//! Shared mail operations used by both the CLI and the MCP server:
//! enqueue + deliver-or-queue, inbox listing, reading, replying, threads.

use anyhow::{Context, Result, bail};
use iroh::SecretKey;
use std::path::Path;

use crate::client;
use crate::config::{Config, Paths};
use crate::proto::{Attachment, Message, MAX_ATTACHMENT_BYTES, MAX_WIRE_BYTES};
use crate::store::{Store, StoredMessage, ThreadSummary};

#[derive(Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeliveryOutcome {
    Delivered {
        msg_key: String,
        peer: String,
        received_at: u64,
    },
    Queued {
        msg_key: String,
        peer: String,
        note: String,
    },
}

/// Enqueue an outgoing message and attempt immediate delivery. On failure
/// the message stays queued for the daemon's retry loop.
pub async fn send_message(
    paths: &Paths,
    config: &Config,
    sk: &SecretKey,
    me: &str,
    peer_id: &str,
    body: &str,
    attachments: Vec<Attachment>,
    thread_id: Option<&str>,
    in_reply_to: Option<&str>,
) -> Result<DeliveryOutcome> {
    let store = Store::open(paths)?;
    let msg = Message {
        id: ulid::Ulid::generate().to_string(),
        thread_id: thread_id
            .map(|t| t.to_string())
            .unwrap_or_else(|| ulid::Ulid::generate().to_string()),
        in_reply_to: in_reply_to.map(|s| s.to_string()),
        from: me.to_string(),
        to: peer_id.to_string(),
        created_at: crate::util::now_secs(),
        content_type: "text/plain".to_string(),
        body: body.to_string(),
        attachments,
    };
    validate_message(config, &msg)?;
    store.enqueue_outgoing(&msg, config.retry_base_secs)?;

    let result = if peer_id == me {
        // Loopback: iroh does not support dialing our own NodeId, so a
        // message addressed to ourselves goes straight into our inbox.
        store.record_incoming(&msg)?;
        Ok(crate::util::now_secs())
    } else {
        let endpoint = crate::net::bind_endpoint(config, sk).await?;
        let r = client::deliver(
            &endpoint,
            iroh::EndpointAddr::from(peer_id.parse::<iroh::EndpointId>()?),
            &msg,
        )
        .await;
        endpoint.close().await;
        r
    };

    match result {
        Ok(received_at) => {
            store.mark_delivered(&msg.id, received_at)?;
            Ok(DeliveryOutcome::Delivered {
                msg_key: msg.id,
                peer: peer_id.to_string(),
                received_at,
            })
        }
        Err(e) => {
            store.mark_retry(&msg.id, 0, config.retry_base_secs, config.retry_max_secs)?;
            Ok(DeliveryOutcome::Queued {
                msg_key: msg.id,
                peer: peer_id.to_string(),
                note: format!("queued for retry by the daemon ({e:#})"),
            })
        }
    }
}

/// Validate a message against the configured body limit and the wire
/// limits (per-attachment 20 MiB, total frame 64 MiB). Shared by the CLI
/// and the MCP server before anything reaches the outbox.
pub fn validate_message(config: &Config, msg: &Message) -> Result<()> {
    if msg.body.len() > config.max_message_bytes {
        bail!(
            "message body is {} bytes, exceeds the {}-byte limit",
            msg.body.len(),
            config.max_message_bytes
        );
    }
    for a in &msg.attachments {
        if a.size > MAX_ATTACHMENT_BYTES {
            bail!(
                "attachment `{}` is {} bytes, exceeds the {}-byte limit",
                a.name,
                a.size,
                MAX_ATTACHMENT_BYTES
            );
        }
    }
    let wire_estimate = msg.body.len() as u64
        + msg
            .attachments
            .iter()
            .map(|a| a.data_base64.len() as u64)
            .sum::<u64>()
        + 4096;
    if wire_estimate > MAX_WIRE_BYTES as u64 {
        bail!(
            "message with attachments is ~{} bytes on the wire, exceeds the {}-byte frame limit",
            wire_estimate,
            MAX_WIRE_BYTES
        );
    }
    Ok(())
}

/// Build an attachment from a file on disk: payload base64-encoded,
/// name from the file name, content type inferred from a few common
/// extensions (default <code>application/octet-stream</code>).
pub fn attachment_from_file(path: &Path) -> Result<Attachment> {
    use base64::Engine;
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading attachment {}", path.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "attachment".to_string());
    let content_type = match path.extension().map(|e| e.to_string_lossy().to_lowercase()) {
        Some(ref e) if e == "json" => "application/json",
        Some(ref e) if e == "txt" || e == "log" || e == "md" => "text/plain",
        Some(ref e) if e == "png" => "image/png",
        Some(ref e) if e == "jpg" || e == "jpeg" => "image/jpeg",
        Some(ref e) if e == "pdf" => "application/pdf",
        Some(ref e) if e == "zip" => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string();
    Ok(Attachment {
        name,
        content_type,
        size: bytes.len() as u64,
        data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
    })
}

pub fn list_inbox(
    paths: &Paths,
    peer: Option<&str>,
    unread_only: bool,
) -> Result<Vec<StoredMessage>> {
    let store = Store::open(paths)?;
    let peer_id = match peer {
        Some(p) => {
            let list = crate::allowlist::AllowList::load(&paths.allowed_keys)?;
            Some(
                list.resolve(p)
                    .with_context(|| format!("`{p}` is not in the allowlist"))?
                    .node_id
                    .clone(),
            )
        }
        None => None,
    };
    let mut rows = store.list_inbox(peer_id.as_deref())?;
    if unread_only {
        rows.retain(|r| r.read_at.is_none());
    }
    Ok(rows)
}

pub fn read_message(paths: &Paths, msg_key: &str) -> Result<StoredMessage> {
    let store = Store::open(paths)?;
    let mut msg = store
        .get(msg_key)?
        .with_context(|| format!("no message `{msg_key}`"))?;
    if msg.direction == "in" && msg.read_at.is_none() {
        store.mark_read(&msg.msg_key)?;
        msg.read_at = Some(crate::util::now_secs());
        msg.status = "read".to_string();
    }
    Ok(msg)
}

pub fn list_threads(paths: &Paths) -> Result<Vec<ThreadSummary>> {
    Store::open(paths)?.list_threads()
}

/// Reply to a received message, continuing its thread. Returns the wire
/// `in_reply_to` (the original sender's message id) and thread context.
pub fn reply_context(paths: &Paths, msg_key: &str) -> Result<(String, String, String)> {
    let store = Store::open(paths)?;
    let original = store
        .get(msg_key)?
        .with_context(|| format!("no message `{msg_key}`"))?;
    if original.direction != "in" {
        anyhow::bail!("can only reply to received messages");
    }
    Ok((
        original.from_id.clone(),
        original.thread_id.clone(),
        original.remote_id.clone(),
    ))
}
