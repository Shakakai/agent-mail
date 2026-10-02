//! MCP server (stdio): the agent-facing interface to agent-mail.
//! Agents get identity, send/read/reply, thread overview, and (optionally,
//! when enabled in config.toml) trust-list management.

use anyhow::Result;
use iroh::SecretKey;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ErrorData;
use rmcp::model::{
    ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ResourceUpdatedNotification,
    ResourceUpdatedNotificationParam, ResourcesCapability, ServerCapabilities, ServerConfig,
    ServerNotification, SubscribeRequestParams, SubscriptionFilter, UnsubscribeRequestParams,
};
use rmcp::service::{RequestContext, SubscriptionContext, SubscriptionSink};
use rmcp::tool;
use rmcp::tool_handler;
use rmcp::tool_router;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::{Peer, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::allowlist::{AllowList, PeerEntry};
use crate::config::{Config, Paths};
use crate::daemon;
use crate::identity;
use crate::ops;
use crate::store::Store;

/// Resource URI clients subscribe to for new-mail notifications.
pub const INBOX_URI: &str = "agent-mail://inbox";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendParams {
    /// Peer NodeId or allowlist name.
    pub peer: String,
    /// Message body (plain text).
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListInboxParams {
    /// Only unread messages.
    #[serde(default)]
    pub unread_only: Option<bool>,
    /// Filter by peer NodeId or allowlist name.
    #[serde(default)]
    pub peer: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadParams {
    /// Message key (from list_inbox).
    pub msg_key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplyParams {
    /// Message key of the message being replied to.
    pub msg_key: String,
    /// Reply body (plain text).
    pub body: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AllowAddParams {
    /// Peer NodeId (64-char hex).
    pub node_id: String,
    /// Friendly label.
    #[serde(default)]
    pub name: Option<String>,
    /// Mark as a human peer.
    #[serde(default)]
    pub human: Option<bool>,
}

/// One subscriber to `agent-mail://inbox`: either a legacy
/// `resources/subscribe` client (tracked by its peer) or a 2026-07-28
/// `subscriptions/listen` stream (tracked by its filter-enforcing sink).
#[derive(Clone)]
enum Subscriber {
    Legacy(Peer<RoleServer>),
    Sink(SubscriptionSink),
}

type Subscriptions = Arc<Mutex<Vec<(String, Subscriber)>>>;

#[derive(Clone)]
pub struct MailMcp {
    paths: Paths,
    config: Config,
    secret_key: SecretKey,
    me: String,
    subs: Subscriptions,
}

impl MailMcp {
    pub fn new(paths: Paths, config: Config) -> Result<Self> {
        let secret_key = identity::load(&paths)?;
        let me = identity::node_id(&secret_key);
        Ok(Self {
            paths,
            config,
            secret_key,
            me,
            subs: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn bad_request(msg: impl Into<String>) -> ErrorData {
        ErrorData::new(rmcp::model::ErrorCode::INVALID_PARAMS, msg.into(), None)
    }

    fn internal(msg: impl Into<String>) -> ErrorData {
        ErrorData::new(rmcp::model::ErrorCode::INTERNAL_ERROR, msg.into(), None)
    }

    fn resolve_peer(&self, query: &str) -> std::result::Result<String, ErrorData> {
        let list = AllowList::load(&self.paths.allowed_keys)
            .map_err(|e| Self::internal(format!("allowlist error: {e:#}")))?;
        list.resolve(query)
            .map(|e| e.node_id.clone())
            .ok_or_else(|| {
                Self::bad_request(format!(
                    "`{query}` is not in the allowlist — ask a human to run `agent-mail allow add`"
                ))
            })
    }

    fn json<T: serde::Serialize>(v: &T) -> String {
        serde_json::to_string_pretty(v).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}

#[tool_router]
impl MailMcp {
    #[tool(description = "Get this agent's agent-mail identity (NodeId) and where to find the allowlist. Share the NodeId with peers so they can allow this agent.")]
    fn get_identity(&self) -> String {
        Self::json(&serde_json::json!({
            "node_id": self.me,
            "allowlist": self.paths.allowed_keys,
            "note": "peers must add this NodeId to their allowlist (and vice versa) before mail flows",
        }))
    }

    #[tool(description = "List allowlisted peers (NodeId, name, human flag).")]
    fn allow_list(&self) -> std::result::Result<String, ErrorData> {
        let list = AllowList::load(&self.paths.allowed_keys)
            .map_err(|e| Self::internal(format!("{e:#}")))?;
        Ok(Self::json(&list.entries))
    }

    #[tool(description = "Add a peer to the allowlist. Disabled by default: requires mcp_allow_trust_changes = true in config.toml.")]
    fn allow_add(&self, Parameters(p): Parameters<AllowAddParams>) -> std::result::Result<String, ErrorData> {
        if !self.config.mcp_allow_trust_changes {
            return Err(Self::bad_request(
                "trust changes via MCP are disabled (set mcp_allow_trust_changes = true in config.toml)",
            ));
        }
        let mut list = AllowList::load_or_create(&self.paths.allowed_keys)
            .map_err(|e| Self::internal(format!("{e:#}")))?;
        list.add(PeerEntry {
            node_id: p.node_id.clone(),
            name: p.name.clone(),
            human: p.human.unwrap_or(false),
        })
        .map_err(|e| Self::bad_request(format!("{e:#}")))?;
        Ok(Self::json(&serde_json::json!({"added": p.node_id, "name": p.name})))
    }

    #[tool(description = "Send a message to an allowlisted peer. Delivers immediately if the peer is online, otherwise queues for the daemon to retry (at-least-once). Returns the message key and status.")]
    async fn send_message(
        &self,
        Parameters(p): Parameters<SendParams>,
    ) -> std::result::Result<String, ErrorData> {
        if p.body.trim().is_empty() {
            return Err(Self::bad_request("message body is empty"));
        }
        let peer_id = self.resolve_peer(&p.peer)?;
        let outcome = ops::send_message(
            &self.paths,
            &self.config,
            &self.secret_key,
            &self.me,
            &peer_id,
            &p.body,
            None,
            None,
        )
        .await
        .map_err(|e| Self::internal(format!("{e:#}")))?;
        Ok(Self::json(&outcome))
    }

    #[tool(description = "List received messages (newest first), with body previews. Optionally filter to unread or a specific peer.")]
    fn list_inbox(&self, Parameters(p): Parameters<ListInboxParams>) -> std::result::Result<String, ErrorData> {
        let rows = ops::list_inbox(
            &self.paths,
            p.peer.as_deref(),
            p.unread_only.unwrap_or(false),
        )
        .map_err(|e| Self::internal(format!("{e:#}")))?;
        Ok(Self::json(&rows))
    }

    #[tool(description = "Read a full message by its key (marks it read). Returns from/to/thread/timestamps/body.")]
    fn read_message(&self, Parameters(p): Parameters<ReadParams>) -> std::result::Result<String, ErrorData> {
        let msg = ops::read_message(&self.paths, &p.msg_key)
            .map_err(|e| Self::internal(format!("{e:#}")))?;
        Ok(Self::json(&msg))
    }

    #[tool(description = "Reply to a received message, continuing its thread. Returns the new message key and delivery status.")]
    async fn reply(
        &self,
        Parameters(p): Parameters<ReplyParams>,
    ) -> std::result::Result<String, ErrorData> {
        if p.body.trim().is_empty() {
            return Err(Self::bad_request("reply body is empty"));
        }
        let (peer_id, thread_id, reply_to) =
            ops::reply_context(&self.paths, &p.msg_key).map_err(|e| Self::internal(format!("{e:#}")))?;
        let outcome = ops::send_message(
            &self.paths,
            &self.config,
            &self.secret_key,
            &self.me,
            &peer_id,
            &p.body,
            Some(&thread_id),
            Some(&reply_to),
        )
        .await
        .map_err(|e| Self::internal(format!("{e:#}")))?;
        Ok(Self::json(&outcome))
    }

    #[tool(description = "Conversation overview: one entry per thread with the latest message preview, message count, and unread count.")]
    fn list_threads(&self) -> std::result::Result<String, ErrorData> {
        let threads = ops::list_threads(&self.paths).map_err(|e| Self::internal(format!("{e:#}")))?;
        Ok(Self::json(&threads))
    }

    #[tool(description = "List outgoing messages not yet confirmed delivered: queued (the daemon retries with backoff) and failed (gave up after 10 attempts). Includes per-message attempt counts so exhaustion is visible. Use this to check on mail you sent before ending a task.")]
    fn list_outbox(&self) -> std::result::Result<String, ErrorData> {
        let store = Store::open(&self.paths).map_err(|e| Self::internal(format!("{e:#}")))?;
        let rows = store.list_outgoing().map_err(|e| Self::internal(format!("{e:#}")))?;
        let (pending, failed) = store.outbox_counts();
        Ok(Self::json(&serde_json::json!({
            "pending": pending,
            "failed": failed,
            "messages": rows,
        })))
    }
}

#[tool_handler]
impl ServerHandler for MailMcp {
    fn get_info(&self) -> ServerConfig {
        let mut caps = ServerCapabilities::builder().enable_tools().build();
        // Advertise that clients may subscribe to resources (pre-2026-07-28
        // clients use `resources/subscribe`; newer ones use
        // `subscriptions/listen`). Both paths are implemented below.
        let mut resources = ResourcesCapability::default();
        resources.subscribe = Some(true);
        caps.resources = Some(resources);
        ServerConfig::new(caps)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult {
            result_type: None,
            meta: None,
            next_cursor: None,
            ttl_ms: None,
            cache_scope: None,
            resources: vec![Resource::new(INBOX_URI, "inbox")
                .with_description(
                    "Recent inbound agent-mail messages as JSON. Subscribe for \
                     notifications/resources/updated when new mail arrives.",
                )
                .with_mime_type("application/json")],
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResponse, ErrorData> {
        if request.uri != INBOX_URI {
            return Err(Self::bad_request(format!(
                "unknown resource `{}`; only `{INBOX_URI}` is available",
                request.uri
            )));
        }
        let messages = ops::list_inbox(&self.paths, None, false)
            .map_err(|e| Self::internal(format!("{e:#}")))?;
        let text = serde_json::to_string_pretty(&messages)
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
        let content = ResourceContents::text(text, INBOX_URI).with_mime_type("application/json");
        Ok(ReadResourceResult::new(vec![content]).into())
    }

    /// Legacy (pre-2026-07-28) resource subscription. The watcher task sends
    /// `notifications/resources/updated` to these peers on new inbound mail.
    #[allow(deprecated)]
    async fn subscribe(
        &self,
        request: SubscribeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<(), ErrorData> {
        if request.uri != INBOX_URI {
            return Err(Self::bad_request(format!(
                "cannot subscribe to `{}`; only `{INBOX_URI}` is subscribable",
                request.uri
            )));
        }
        self.subs
            .lock()
            .unwrap()
            .push((INBOX_URI.to_string(), Subscriber::Legacy(context.peer)));
        Ok(())
    }

    #[allow(deprecated)]
    async fn unsubscribe(
        &self,
        request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<(), ErrorData> {
        if request.uri != INBOX_URI {
            return Err(Self::bad_request(format!(
                "cannot unsubscribe from `{}`; only `{INBOX_URI}` is tracked",
                request.uri
            )));
        }
        self.subs.lock().unwrap().retain(|(uri, sub)| {
            !(uri == INBOX_URI && matches!(sub, Subscriber::Legacy(_)))
        });
        Ok(())
    }

    /// 2026-07-28 subscriptions: accept listen streams that opt in to
    /// `agent-mail://inbox` resource updates.
    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        let uris = requested.resource_subscriptions.as_ref()?;
        if uris.iter().any(|u| u == INBOX_URI) {
            Some(
                SubscriptionFilter::builder()
                    .resource_subscription(INBOX_URI)
                    .build(),
            )
        } else {
            None
        }
    }

    /// Hold the listen stream open until the client cancels it, forwarding
    /// inbox updates through the sink while alive.
    async fn listen(&self, context: SubscriptionContext) -> std::result::Result<(), ErrorData> {
        self.subs.lock().unwrap().push((
            INBOX_URI.to_string(),
            Subscriber::Sink(context.sink().clone()),
        ));
        context.cancelled().await;
        let id = context.sink().id().clone();
        self.subs.lock().unwrap().retain(|(_, sub)| match sub {
            Subscriber::Sink(sink) => sink.id() != &id,
            Subscriber::Legacy(_) => true,
        });
        Ok(())
    }
}

/// Poll the store for new inbound mail and notify every subscriber of
/// `agent-mail://inbox`. Runs for the lifetime of the MCP server process.
async fn inbox_watcher(paths: Paths, subs: Subscriptions) {
    let mut last: i64 = -1;
    loop {
        let tick = Store::open(&paths)
            .and_then(|store| store.max_inbound_rowid())
            .map(|max| max.unwrap_or(0));
        match tick {
            Ok(cur) => {
                if last >= 0 && cur > last {
                    notify_inbox_subs(&subs).await;
                }
                last = last.max(cur);
            }
            Err(e) => tracing::warn!("subscription watcher: {e:#}"),
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}

async fn notify_inbox_subs(subs: &Subscriptions) {
    let targets: Vec<(usize, Subscriber)> = {
        let guard = subs.lock().unwrap();
        guard
            .iter()
            .enumerate()
            .filter(|(_, (uri, _))| uri == INBOX_URI)
            .map(|(i, (_, sub))| (i, sub.clone()))
            .collect()
    };
    let mut dead = Vec::new();
    for (full_idx, sub) in targets {
        let notification = || {
            ServerNotification::ResourceUpdatedNotification(
                ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam::new(
                    INBOX_URI,
                )),
            )
        };
        let ok = match &sub {
            Subscriber::Legacy(peer) => peer.send_notification(notification()).await.is_ok(),
            Subscriber::Sink(sink) => sink.send(notification()).await.is_ok(),
        };
        if !ok {
            tracing::debug!("dropping dead inbox subscriber");
            dead.push(full_idx);
        }
    }
    if !dead.is_empty() {
        let mut guard = subs.lock().unwrap();
        for full_idx in dead.into_iter().rev() {
            if full_idx < guard.len() {
                guard.remove(full_idx);
            }
        }
    }
}

pub async fn run(paths: Paths, config: Config) -> Result<()> {
    let server = MailMcp::new(paths.clone(), config.clone())?;
    tokio::spawn(inbox_watcher(paths.clone(), server.subs.clone()));

    // Embed the mail daemon: it starts with the MCP server, so peers can
    // reach us and the outbox drains whenever an agent is connected, and it
    // shuts down automatically when the stdio session ends. A daemon
    // failure is logged but does not kill the server — read/send tools that
    // dial on demand keep working.
    let daemon_shutdown = Arc::new(tokio::sync::Notify::new());
    let daemon_task = {
        let shutdown = daemon_shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = daemon::run_until(paths, config, shutdown).await {
                tracing::error!("embedded daemon exited: {e:#}");
            }
        })
    };

    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    // Stdio closed: stop the daemon and give it a moment to close its
    // endpoint cleanly before the process exits.
    daemon_shutdown.notify_one();
    if tokio::time::timeout(Duration::from_secs(5), daemon_task).await.is_err() {
        tracing::warn!("embedded daemon did not shut down within 5s");
    }
    Ok(())
}
