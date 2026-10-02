//! Shared mail operations used by both the CLI and the MCP server:
//! enqueue + deliver-or-queue, inbox listing, reading, replying, threads.

use anyhow::{Context, Result, bail};
use iroh::{Endpoint, SecretKey, endpoint::presets};

use crate::client;
use crate::config::{Config, Paths};
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
    thread_id: Option<&str>,
    in_reply_to: Option<&str>,
) -> Result<DeliveryOutcome> {
    let store = Store::open(paths)?;
    if body.len() > config.max_message_bytes {
        bail!(
            "message body is {} bytes, exceeds the {}-byte limit",
            body.len(),
            config.max_message_bytes
        );
    }
    let thread_id = thread_id
        .map(|t| t.to_string())
        .unwrap_or_else(|| ulid::Ulid::generate().to_string());
    let msg = store.enqueue_outgoing(
        me,
        peer_id,
        &thread_id,
        in_reply_to,
        "text/plain",
        body,
        config.retry_base_secs,
    )?;

    let result = if peer_id == me {
        // Loopback: iroh does not support dialing our own NodeId, so a
        // message addressed to ourselves goes straight into our inbox.
        store.record_incoming(&msg)?;
        Ok(crate::util::now_secs())
    } else {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(sk.clone())
            .bind()
            .await
            .map_err(crate::util::de)?;
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
