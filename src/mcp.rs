//! MCP server (stdio): the agent-facing interface to agent-mail.
//! Agents get identity, send/read/reply, thread overview, and (optionally,
//! when enabled in config.toml) trust-list management.

use anyhow::Result;
use iroh::SecretKey;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::ErrorData;
use rmcp::tool;
use rmcp::tool_router;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::allowlist::{AllowList, PeerEntry};
use crate::config::{Config, Paths};
use crate::identity;
use crate::ops;
use crate::proto::Audience;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendParams {
    /// Peer NodeId or allowlist name.
    pub peer: String,
    /// Message body (plain text).
    pub body: String,
    /// Intended audience on the receiving side: "agent" (default) or "human".
    #[serde(default)]
    pub audience: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListInboxParams {
    /// Only unread messages.
    #[serde(default)]
    pub unread_only: Option<bool>,
    /// Only messages addressed to humans.
    #[serde(default)]
    pub human_only: Option<bool>,
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

#[derive(Clone)]
pub struct MailMcp {
    paths: Paths,
    config: Config,
    secret_key: SecretKey,
    me: String,
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

    fn parse_audience(s: Option<&str>) -> std::result::Result<Audience, ErrorData> {
        match s.unwrap_or("agent") {
            "agent" => Ok(Audience::Agent),
            "human" => Ok(Audience::Human),
            other => Err(Self::bad_request(format!(
                "audience must be `agent` or `human`, got `{other}`"
            ))),
        }
    }

    fn json<T: serde::Serialize>(v: &T) -> String {
        serde_json::to_string_pretty(v).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }
}

#[tool_router(server_handler)]
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
        let audience = Self::parse_audience(p.audience.as_deref())?;
        let outcome = ops::send_message(
            &self.paths,
            &self.config,
            &self.secret_key,
            &self.me,
            &peer_id,
            audience,
            &p.body,
            None,
            None,
        )
        .await
        .map_err(|e| Self::internal(format!("{e:#}")))?;
        Ok(Self::json(&outcome))
    }

    #[tool(description = "List received messages (newest first), with body previews. Optionally filter to unread, human-audience, or a specific peer.")]
    fn list_inbox(&self, Parameters(p): Parameters<ListInboxParams>) -> std::result::Result<String, ErrorData> {
        let rows = ops::list_inbox(
            &self.paths,
            p.peer.as_deref(),
            p.unread_only.unwrap_or(false),
            p.human_only.unwrap_or(false),
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
            Audience::Agent,
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
}

pub async fn run(paths: Paths, config: Config) -> Result<()> {
    let server = MailMcp::new(paths, config)?;
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
