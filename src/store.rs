//! Persistent message store (SQLite, WAL mode). Shared by the daemon, the
//! CLI, and (later) the MCP server and TUI: mail survives process restarts,
//! and the outbox is what gives offline peers at-least-once delivery.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::config::Paths;
use crate::proto::{Audience, Message};
use crate::util::now_secs;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
    msg_key      TEXT PRIMARY KEY,
    remote_id    TEXT NOT NULL,              -- incoming: sender's msg id (dedupe)
    direction    TEXT NOT NULL CHECK (direction IN ('in', 'out')),
    from_id      TEXT NOT NULL,
    to_id        TEXT NOT NULL,
    peer         TEXT NOT NULL,              -- the other endpoint id
    thread_id    TEXT NOT NULL,
    in_reply_to  TEXT,
    audience     TEXT NOT NULL DEFAULT 'agent',
    content_type TEXT NOT NULL DEFAULT 'text/plain',
    body         TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    received_at  INTEGER,
    read_at      INTEGER,
    status       TEXT NOT NULL DEFAULT 'unread',  -- in: unread|read ; out: queued|delivered|failed
    attempts     INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS dedupe_idx ON messages (direction, peer, remote_id);
CREATE INDEX IF NOT EXISTS inbox_idx ON messages (direction, received_at DESC);
";

#[derive(Debug, Clone, Serialize)]
pub struct StoredMessage {
    pub msg_key: String,
    /// For inbound rows: the sender's wire message id (what peers reference
    /// in `in_reply_to`). For outbound rows: same as `msg_key`.
    pub remote_id: String,
    pub direction: String,
    pub from_id: String,
    pub to_id: String,
    pub peer: String,
    pub thread_id: String,
    pub in_reply_to: Option<String>,
    pub audience: String,
    pub content_type: String,
    pub body: String,
    pub created_at: u64,
    pub received_at: Option<u64>,
    pub read_at: Option<u64>,
    pub status: String,
    pub attempts: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub peer: String,
    pub message_count: u64,
    pub unread_count: u64,
    pub last_body: String,
    pub last_at: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub msg_key: String,
    pub peer: String,
    pub message: Message,
    pub attempts: u64,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(paths: &Paths) -> Result<Self> {
        if let Some(parent) = paths.db.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(&paths.db)
            .with_context(|| format!("opening {}", paths.db.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Record an incoming message. Returns false if it is a duplicate
    /// (same sender msg id already seen) — we still ack duplicates.
    pub fn record_incoming(&self, msg: &Message) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let key = ulid::Ulid::generate().to_string();
        let now = now_secs() as i64;
        let n = conn.execute(
            "INSERT OR IGNORE INTO messages
             (msg_key, remote_id, direction, from_id, to_id, peer, thread_id,
              in_reply_to, audience, content_type, body, created_at,
              received_at, status)
             VALUES (?1, ?2, 'in', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'unread')",
            params![
                key,
                msg.id,
                msg.from,
                msg.to,
                msg.from,
                msg.thread_id,
                msg.in_reply_to,
                audience_str(msg.audience),
                msg.content_type,
                msg.body,
                msg.created_at as i64,
                now,
            ],
        )?;
        Ok(n > 0)
    }

    /// Queue an outgoing message. The wire `id` is the row's `msg_key`, so
    /// peers can dedupe retries and reference it in `in_reply_to`.
    pub fn enqueue_outgoing(
        &self,
        me: &str,
        peer: &str,
        thread_id: &str,
        in_reply_to: Option<&str>,
        audience: Audience,
        content_type: &str,
        body: &str,
    ) -> Result<Message> {
        let key = ulid::Ulid::generate().to_string();
        let now = now_secs();
        let msg = Message {
            id: key.clone(),
            thread_id: thread_id.to_string(),
            in_reply_to: in_reply_to.map(|s| s.to_string()),
            from: me.to_string(),
            to: peer.to_string(),
            created_at: now,
            audience,
            content_type: content_type.to_string(),
            body: body.to_string(),
        };
        self.conn.lock().unwrap().execute(
            "INSERT INTO messages
             (msg_key, remote_id, direction, from_id, to_id, peer, thread_id,
              in_reply_to, audience, content_type, body, created_at, status)
             VALUES (?1, ?1, 'out', ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued')",
            params![
                key,
                me,
                peer,
                thread_id,
                in_reply_to,
                audience_str(audience),
                content_type,
                body,
                now as i64,
            ],
        )?;
        Ok(msg)
    }

    pub fn mark_delivered(&self, msg_key: &str, received_at: u64) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE messages SET status = 'delivered', received_at = ?2,
                    next_retry_at = NULL
             WHERE msg_key = ?1 AND direction = 'out'",
            params![msg_key, received_at as i64],
        )?;
        Ok(())
    }

    /// Record a failed delivery attempt and schedule the next retry with
    /// exponential backoff (base * 2^attempts, capped).
    pub fn mark_retry(&self, msg_key: &str, attempts: u64, base_secs: u64, max_secs: u64) -> Result<()> {
        let delay = base_secs
            .saturating_mul(1u64 << attempts.min(16))
            .min(max_secs);
        let next = now_secs() + delay;
        self.conn.lock().unwrap().execute(
            "UPDATE messages SET attempts = ?2, next_retry_at = ?3
             WHERE msg_key = ?1 AND direction = 'out'",
            params![msg_key, attempts as i64, next as i64],
        )?;
        Ok(())
    }

    /// Outgoing messages due for a delivery attempt.
    pub fn due_outgoing(&self, limit: usize) -> Result<Vec<QueuedMessage>> {
        let conn = self.conn.lock().unwrap();
        let now = now_secs() as i64;
        let mut stmt = conn.prepare(
            "SELECT msg_key, peer, thread_id, in_reply_to, audience, content_type,
                    body, created_at, from_id, to_id, attempts
             FROM messages
             WHERE direction = 'out' AND status IN ('queued', 'failed')
               AND attempts < 10
               AND (next_retry_at IS NULL OR next_retry_at <= ?1)
             ORDER BY created_at
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now, limit as i64], |row| {
            let audience: String = row.get(4)?;
            Ok(QueuedMessage {
                msg_key: row.get(0)?,
                peer: row.get(1)?,
                message: Message {
                    id: row.get(0)?,
                    thread_id: row.get(2)?,
                    in_reply_to: row.get(3)?,
                    from: row.get(8)?,
                    to: row.get(9)?,
                    created_at: row.get::<_, i64>(7)? as u64,
                    audience: parse_audience(&audience),
                    content_type: row.get(5)?,
                    body: row.get(6)?,
                },
                attempts: row.get::<_, i64>(10)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn list_inbox(&self, peer: Option<&str>, human_only: bool) -> Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut conditions = vec!["direction = 'in'".to_string()];
        if peer.is_some() {
            conditions.push("peer = ?1".to_string());
        }
        if human_only {
            conditions.push("audience = 'human'".to_string());
        }
        let sql = format!(
            "SELECT msg_key, remote_id, direction, from_id, to_id, peer, thread_id, in_reply_to,
                    audience, content_type, body, created_at, received_at, read_at,
                    status, attempts
             FROM messages WHERE {} ORDER BY received_at DESC LIMIT 500",
            conditions.join(" AND ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = if let Some(peer) = peer {
            stmt.query_map(params![peer], map_stored)?
        } else {
            stmt.query_map(params![], map_stored)?
        };
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Conversation overview: one row per thread, most recent activity first.
    pub fn list_threads(&self) -> Result<Vec<ThreadSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT thread_id, peer, COUNT(*),
                    SUM(CASE WHEN read_at IS NULL THEN 1 ELSE 0 END),
                    (SELECT body FROM messages m2
                      WHERE m2.thread_id = messages.thread_id
                      ORDER BY m2.created_at DESC LIMIT 1),
                    MAX(received_at)
             FROM messages
             WHERE direction = 'in'
             GROUP BY thread_id
             ORDER BY MAX(received_at) DESC
             LIMIT 200",
        )?;
        let rows = stmt.query_map(params![], |row| {
            Ok(ThreadSummary {
                thread_id: row.get(0)?,
                peer: row.get(1)?,
                message_count: row.get::<_, i64>(2)? as u64,
                unread_count: row.get::<_, i64>(3)? as u64,
                last_body: row.get(4)?,
                last_at: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn get(&self, msg_key: &str) -> Result<Option<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT msg_key, remote_id, direction, from_id, to_id, peer, thread_id, in_reply_to,
                    audience, content_type, body, created_at, received_at, read_at,
                    status, attempts
             FROM messages WHERE msg_key = ?1",
            params![msg_key],
            map_stored,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn mark_read(&self, msg_key: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE messages SET read_at = ?2, status = 'read'
             WHERE msg_key = ?1 AND direction = 'in'",
            params![msg_key, now_secs() as i64],
        )?;
        Ok(())
    }
}

fn map_stored(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredMessage> {
    Ok(StoredMessage {
        msg_key: row.get(0)?,
        remote_id: row.get(1)?,
        direction: row.get(2)?,
        from_id: row.get(3)?,
        to_id: row.get(4)?,
        peer: row.get(5)?,
        thread_id: row.get(6)?,
        in_reply_to: row.get(7)?,
        audience: row.get(8)?,
        content_type: row.get(9)?,
        body: row.get(10)?,
        created_at: row.get::<_, i64>(11)? as u64,
        received_at: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        read_at: row.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        status: row.get(14)?,
        attempts: row.get::<_, i64>(15)? as u64,
    })
}

fn audience_str(a: Audience) -> &'static str {
    match a {
        Audience::Agent => "agent",
        Audience::Human => "human",
    }
}

fn parse_audience(s: &str) -> Audience {
    match s {
        "human" => Audience::Human,
        _ => Audience::Agent,
    }
}

/// Test hook: open a store at an explicit path.
#[allow(dead_code)]
pub fn open_at(db_path: &Path) -> Result<Store> {
    let paths = Paths {
        config_dir: db_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| Path::new(".").to_path_buf()),
        data_dir: db_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| Path::new(".").to_path_buf()),
        secret_key: db_path.with_extension("key"),
        allowed_keys: db_path.with_extension("toml"),
        db: db_path.to_path_buf(),
    };
    Store::open(&paths)
}
