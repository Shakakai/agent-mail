//! The agent-mail daemon: owns the IROH endpoint, accepts inbound mail
//! (allowlist-gated), and retries the outbox with exponential backoff.

use anyhow::Result;
use iroh::{Endpoint, SecretKey, endpoint::presets, protocol::{AcceptError, ProtocolHandler, Router}};
use tracing::{info, warn};

use crate::allowlist::AllowList;
use crate::client;
use crate::config::{Config, Paths};
use crate::identity;
use crate::proto::{self, ErrorCode, Frame, FrameBody, Message, read_frame, write_frame};
use crate::store::Store;

#[derive(Debug, Clone)]
pub struct MailHandler {
    pub paths: Paths,
    pub config: Config,
    pub secret_key: SecretKey,
}

impl ProtocolHandler for MailHandler {
    async fn accept(&self, conn: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        let remote = conn.remote_id();

        // Trust gate: re-read the allowlist per connection so edits take
        // effect immediately. Peers not on the list are dropped before any
        // envelope is exchanged.
        let allowed = AllowList::load(&self.paths.allowed_keys)
            .map(|l| l.contains(&remote))
            .unwrap_or(false);
        if !allowed {
            warn!(%remote, "rejected connection: node id not in allowlist");
            return Ok(());
        }

        let store = match Store::open(&self.paths) {
            Ok(s) => s,
            Err(e) => {
                warn!(%remote, "store unavailable: {e:#}");
                return Ok(());
            }
        };
        let me = identity::node_id(&self.secret_key);

        loop {
            let (mut send, mut recv) = match conn.accept_bi().await {
                Ok(streams) => streams,
                Err(_) => break, // connection closed
            };
            let frame = match read_frame(&mut recv, self.config.max_message_bytes).await {
                Ok(f) => f,
                Err(e) => {
                    warn!(%remote, "bad frame: {e:#}");
                    let _ = write_frame(
                        &mut send,
                        &Frame::error(ErrorCode::Malformed, None, format!("{e:#}")),
                    )
                    .await;
                    let _ = send.finish();
                    continue;
                }
            };
            let response = match frame.body {
                FrameBody::Hello { .. } => Frame::hello(),
                FrameBody::Send { msg, .. } => match handle_message(&store, &me, &remote, msg) {
                    Ok(received_at) => Frame {
                        v: proto::PROTOCOL_V,
                        body: FrameBody::Ack {
                            id: ulid::Ulid::generate().to_string(),
                            of: received_at.0,
                            received_at: received_at.1,
                        },
                    },
                    Err((code, message)) => Frame::error(code, None, message),
                },
                _ => Frame::error(ErrorCode::Malformed, None, "expected hello or send"),
            };
            if write_frame(&mut send, &response).await.is_err() {
                break;
            }
            let _ = send.finish();
        }
        Ok(())
    }
}

/// Validate and persist an incoming message. Returns (msg_id, received_at)
/// for the ack, or an (error code, message) pair.
fn handle_message(
    store: &Store,
    me: &str,
    remote: &iroh::EndpointId,
    msg: Message,
) -> std::result::Result<(String, u64), (ErrorCode, String)> {
    let remote_str = remote.to_string();
    if msg.from != remote_str {
        return Err((
            ErrorCode::IdentityMismatch,
            format!("from ({}) does not match connection peer", msg.from),
        ));
    }
    if msg.to != me {
        return Err((
            ErrorCode::IdentityMismatch,
            format!("to ({}) is not this endpoint", msg.to),
        ));
    }
    match store.record_incoming(&msg) {
        Ok(inserted) => {
            if inserted {
                info!(from = %remote_str, thread = %msg.thread_id, "received message");
            }
            Ok((msg.id.clone(), crate::util::now_secs()))
        }
        Err(e) => Err((ErrorCode::Internal, format!("store error: {e:#}"))),
    }
}

pub async fn run(paths: Paths, config: Config) -> Result<()> {
    let secret_key = identity::load(&paths)?;
    let me = identity::node_id(&secret_key);
    info!("agent-mail daemon starting");
    info!("node id: {me}");

    // CRITICAL: bind the endpoint with our persistent secret key. Without
    // this the endpoint gets an ephemeral identity: published discovery
    // records and all connections would use a key our peers don't know.
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret_key.clone())
        .bind()
        .await
        .map_err(crate::util::de)?;
    let router = Router::builder(endpoint.clone())
        .accept(
            proto::ALPN,
            MailHandler {
                paths: paths.clone(),
                config: config.clone(),
                secret_key: secret_key.clone(),
            },
        )
        .spawn();
    endpoint.online().await;
    info!("endpoint online");
    // Log our current address ticket: useful for `send --ticket` debugging
    // and for humans to inspect published addressing info.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    if let Ok(ticket) = serde_json::to_string(&endpoint.addr()) {
        info!("addr ticket: {ticket}");
    }

    let store = Store::open(&paths)?;
    let mut tick =
        tokio::time::interval(std::time::Duration::from_secs(config.daemon_tick_secs.max(1)));

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Err(e) = retry_outbox(&endpoint, &store, &config, &me).await {
                    warn!("outbox retry pass failed: {e:#}");
                }
            }
            res = tokio::signal::ctrl_c() => {
                res?;
                info!("shutting down");
                break;
            }
        }
    }

    router.shutdown().await.map_err(crate::util::de)?;
    Ok(())
}

/// Attempt delivery for every due outbox message.
async fn retry_outbox(
    endpoint: &Endpoint,
    store: &Store,
    config: &Config,
    me: &str,
) -> Result<()> {
    let due = store.due_outgoing(50)?;
    for queued in due {
        let peer = match queued.message.to.parse::<iroh::EndpointId>() {
            Ok(p) => p,
            Err(e) => {
                warn!(peer = %queued.peer, "skipping undialable peer: {e}");
                store.mark_retry(&queued.msg_key, queued.attempts + 1, config.retry_base_secs, config.retry_max_secs)?;
                continue;
            }
        };
        let result = client::deliver(
            endpoint,
            iroh::EndpointAddr::from(peer),
            &queued.message,
        )
        .await;
        match result {
            Ok(received_at) => {
                store.mark_delivered(&queued.msg_key, received_at)?;
                info!(to = %queued.peer, "delivered {}", queued.msg_key);
            }
            Err(e) => {
                store.mark_retry(
                    &queued.msg_key,
                    queued.attempts + 1,
                    config.retry_base_secs,
                    config.retry_max_secs,
                )?;
                warn!(to = %queued.peer, "delivery failed (will retry): {e:#}");
            }
        }
    }
    let _ = me;
    Ok(())
}
