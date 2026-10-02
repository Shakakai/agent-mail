//! Persistent message store (SQLite, WAL mode). Shared by the daemon, the
//! CLI, and (later) the MCP server and TUI: mail survives process restarts,
//! and the outbox is what gives offline peers at-least-once delivery.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::config::Paths;
use crate::proto::Message;
use crate::util::now_secs;

/// Delivery attempts before an outgoing message is marked `failed`.
pub const MAX_ATTEMPTS: u64 = 10;

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
CREATE TABLE IF NOT EXISTS rejections (
    node_id   TEXT NOT NULL,
    last_seen INTEGER NOT NULL,
    count     INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (node_id)
);
-- One-time migration (idempotent): rows exhausted under the old hardcoded
-- filter stay 'queued' forever and were invisible; surface them as failed.
UPDATE messages SET status = 'failed'
 WHERE direction = 'out' AND status = 'queued' AND attempts >= 10;
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
    pub content_type: String,
    pub body: String,
    pub created_at: u64,
    pub received_at: Option<u64>,
    pub read_at: Option<u64>,
    pub status: String,
    pub attempts: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Rejection {
    pub node_id: String,
    pub last_seen: u64,
    pub count: u64,
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
        // Wire protocol v2 dropped the audience column; migrate stores
        // created by v1 (SQLite >= 3.35 supports DROP COLUMN).
        let has_audience = conn.prepare(
            "SELECT 1 FROM pragma_table_info('messages') WHERE name = 'audience'",
        )?.exists([])?;
        if has_audience {
            conn.execute_batch("ALTER TABLE messages DROP COLUMN audience;")?;
        }
        #[cfg(unix)]
        lock_down_db_files(&paths.db);
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
              in_reply_to, content_type, body, created_at,
              received_at, status)
             VALUES (?1, ?2, 'in', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'unread')",
            params![
                key,
                msg.id,
                msg.from,
                msg.to,
                msg.from,
                msg.thread_id,
                msg.in_reply_to,
                msg.content_type,
                msg.body,
                msg.created_at as i64,
                now,
            ],
        )?;
        Ok(n > 0)
    }

    /// Queue an outgoing message. The wire `id` is the row's `msg_key`, so
    /// peers can dedupe retries and reference it in `in_reply_to`. The
    /// first daemon retry is scheduled `first_retry_secs` out so the daemon
    /// never races the interactive delivery attempt that enqueued this row.
    pub fn enqueue_outgoing(
        &self,
        me: &str,
        peer: &str,
        thread_id: &str,
        in_reply_to: Option<&str>,
        content_type: &str,
        body: &str,
        first_retry_secs: u64,
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
            content_type: content_type.to_string(),
            body: body.to_string(),
        };
        self.conn.lock().unwrap().execute(
            "INSERT INTO messages
             (msg_key, remote_id, direction, from_id, to_id, peer, thread_id,
              in_reply_to, content_type, body, created_at, status,
              next_retry_at)
             VALUES (?1, ?1, 'out', ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, 'queued',
                     ?8 + ?9)",
            params![
                key,
                me,
                peer,
                thread_id,
                in_reply_to,
                content_type,
                body,
                now as i64,
                first_retry_secs as i64,
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
    /// exponential backoff (base * 2^attempts, capped). At MAX_ATTEMPTS the
    /// message transitions to `failed`: it leaves the retry loop but stays
    /// visible via `outbox_counts`/`list_outgoing` instead of vanishing.
    pub fn mark_retry(&self, msg_key: &str, attempts: u64, base_secs: u64, max_secs: u64) -> Result<()> {
        let delay = base_secs
            .saturating_mul(1u64 << attempts.min(16))
            .min(max_secs);
        let next = now_secs() + delay;
        self.conn.lock().unwrap().execute(
            "UPDATE messages SET attempts = ?2, next_retry_at = ?3,
                    status = CASE WHEN ?2 >= ?4 THEN 'failed' ELSE 'queued' END
             WHERE msg_key = ?1 AND direction = 'out'",
            params![msg_key, attempts as i64, next as i64, MAX_ATTEMPTS as i64],
        )?;
        Ok(())
    }

    /// Outgoing messages due for a delivery attempt.
    pub fn due_outgoing(&self, limit: usize) -> Result<Vec<QueuedMessage>> {
        let conn = self.conn.lock().unwrap();
        let now = now_secs() as i64;
        let mut stmt = conn.prepare(
            "SELECT msg_key, peer, thread_id, in_reply_to, content_type,
                    body, created_at, from_id, to_id, attempts
             FROM messages
             WHERE direction = 'out' AND status IN ('queued', 'failed')
               AND attempts < ?3
               AND (next_retry_at IS NULL OR next_retry_at <= ?1)
             ORDER BY created_at
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now, limit as i64, MAX_ATTEMPTS as i64], |row| {
            Ok(QueuedMessage {
                msg_key: row.get(0)?,
                peer: row.get(1)?,
                message: Message {
                    id: row.get(0)?,
                    thread_id: row.get(2)?,
                    in_reply_to: row.get(3)?,
                    from: row.get(7)?,
                    to: row.get(8)?,
                    created_at: row.get::<_, i64>(6)? as u64,
                    content_type: row.get(4)?,
                    body: row.get(5)?,
                },
                attempts: row.get::<_, i64>(9)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn list_inbox(&self, peer: Option<&str>) -> Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut conditions = vec!["direction = 'in'".to_string()];
        if peer.is_some() {
            conditions.push("peer = ?1".to_string());
        }
        let sql = format!(
            "SELECT msg_key, remote_id, direction, from_id, to_id, peer, thread_id, in_reply_to,
                    content_type, body, created_at, received_at, read_at,
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
    /// Highest SQLite rowid among inbound messages; drives MCP
    /// subscription notifications (a new inbound message means a new rowid).
    pub fn max_inbound_rowid(&self) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let max: Option<i64> = conn.query_row(
            "SELECT MAX(rowid) FROM messages WHERE direction='in'",
            [],
            |r| r.get(0),
        )?;
        Ok(max)
    }

    pub fn list_threads(&self) -> Result<Vec<ThreadSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT thread_id, peer,
                    COUNT(*),
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

    /// Record a rejected inbound connection attempt (unknown NodeId).
    pub fn record_rejection(&self, node_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO rejections (node_id, last_seen, count) VALUES (?1, ?2, 1)
             ON CONFLICT(node_id) DO UPDATE SET last_seen = ?2, count = count + 1",
            params![node_id, now_secs() as i64],
        )?;
        Ok(())
    }

    pub fn list_rejections(&self) -> Result<Vec<Rejection>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT node_id, last_seen, count FROM rejections ORDER BY last_seen DESC LIMIT 100")?;
        let rows = stmt.query_map(params![], |row| {
            Ok(Rejection {
                node_id: row.get(0)?,
                last_seen: row.get::<_, i64>(1)? as u64,
                count: row.get::<_, i64>(2)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// (pending, failed) counts for outgoing messages. Failed messages are
    /// terminal but counted, so exhaustion is visible rather than silent.
    pub fn outbox_counts(&self) -> (u64, u64) {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT
                   COALESCE(SUM(CASE WHEN status = 'queued'
                                      AND attempts < ?1 THEN 1 ELSE 0 END), 0),
                   COALESCE(SUM(CASE WHEN status = 'failed'
                                      OR (status = 'queued' AND attempts >= ?1)
                                     THEN 1 ELSE 0 END), 0)
                 FROM messages WHERE direction = 'out'",
                params![MAX_ATTEMPTS as i64],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
            )
            .unwrap_or((0, 0))
    }

    /// Outgoing messages awaiting delivery or given up, oldest first.
    pub fn list_outgoing(&self) -> Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT msg_key, remote_id, direction, from_id, to_id, peer, thread_id, in_reply_to,
                    content_type, body, created_at, received_at, read_at,
                    status, attempts
             FROM messages
             WHERE direction = 'out' AND status IN ('queued', 'failed')
             ORDER BY created_at
             LIMIT 500",
        )?;
        let rows = stmt.query_map(params![], map_stored)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Messages of one thread, chronological.
    pub fn thread_messages(&self, thread_id: &str) -> Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT msg_key, remote_id, direction, from_id, to_id, peer, thread_id, in_reply_to,
                    content_type, body, created_at, received_at, read_at,
                    status, attempts
             FROM messages WHERE thread_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![thread_id], map_stored)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Mark every inbound message in a thread as read.
    pub fn mark_thread_read(&self, thread_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE messages SET read_at = ?2, status = 'read'
             WHERE thread_id = ?1 AND direction = 'in' AND read_at IS NULL",
            params![thread_id, now_secs() as i64],
        )?;
        Ok(())
    }

    pub fn get(&self, msg_key: &str) -> Result<Option<StoredMessage>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT msg_key, remote_id, direction, from_id, to_id, peer, thread_id, in_reply_to,
                    content_type, body, created_at, received_at, read_at,
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
        content_type: row.get(8)?,
        body: row.get(9)?,
        created_at: row.get::<_, i64>(10)? as u64,
        received_at: row.get::<_, Option<i64>>(11)?.map(|v| v as u64),
        read_at: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        status: row.get(13)?,
        attempts: row.get::<_, i64>(14)? as u64,
    })
}

/// Mail bodies are plaintext and sensitive; keep the store files readable
/// only by the owner. Applied on every open so existing installs are
/// tightened, not just newly created ones.
#[cfg(unix)]
fn lock_down_db_files(db: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for candidate in [
        db.to_path_buf(),
        db.with_extension("db-wal"),
        db.with_extension("db-shm"),
    ] {
        if let Ok(meta) = std::fs::metadata(&candidate) {
            let mut perms = meta.permissions();
            if perms.mode() & 0o077 != 0 {
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(&candidate, perms);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("am-store-test-{}", ulid::Ulid::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.db");
        (open_at(&db).unwrap(), dir)
    }

    fn sample_msg(id: &str, from: &str, to: &str) -> crate::proto::Message {
        crate::proto::Message {
            id: id.to_string(),
            thread_id: "t1".to_string(),
            in_reply_to: None,
            from: from.to_string(),
            to: to.to_string(),
            created_at: now_secs(),
            content_type: "text/plain".to_string(),
            body: "hello".to_string(),
        }
    }

    #[test]
    fn incoming_dedupes_on_remote_id() {
        let (store, dir) = tmp_store();
        assert!(store.record_incoming(&sample_msg("m1", "peer", "me")).unwrap());
        assert!(!store.record_incoming(&sample_msg("m1", "peer", "me")).unwrap());
        assert_eq!(store.list_inbox(None).unwrap().len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn enqueue_schedules_first_retry_and_hides_from_due() {
        let (store, dir) = tmp_store();
        let msg = store
            .enqueue_outgoing("me", "peer", "t1", None, "text/plain", "hi", 600)
            .unwrap();
        // Not due immediately: the interactive send owns the first attempt.
        assert!(store.due_outgoing(50).unwrap().is_empty());
        assert_eq!(store.outbox_counts(), (1, 0));
        // Delivered rows leave the outbox entirely.
        store.mark_delivered(&msg.id, now_secs()).unwrap();
        assert_eq!(store.outbox_counts(), (0, 0));
        assert!(store.list_outgoing().unwrap().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn zero_first_retry_is_due_at_once() {
        let (store, dir) = tmp_store();
        store
            .enqueue_outgoing("me", "peer", "t1", None, "text/plain", "hi", 0)
            .unwrap();
        assert_eq!(store.due_outgoing(50).unwrap().len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn retries_back_off_then_fail_visibly_at_max_attempts() {
        let (store, dir) = tmp_store();
        let msg = store
            .enqueue_outgoing("me", "peer", "t1", None, "text/plain", "hi", 0)
            .unwrap();
        for attempts in 1..MAX_ATTEMPTS {
            store.mark_retry(&msg.id, attempts, 30, 900).unwrap();
            assert_eq!(store.get(&msg.id).unwrap().unwrap().status, "queued");
        }
        store.mark_retry(&msg.id, MAX_ATTEMPTS, 30, 900).unwrap();
        let row = store.get(&msg.id).unwrap().unwrap();
        assert_eq!(row.status, "failed");
        // Exhausted: out of the retry loop...
        assert!(store.due_outgoing(50).unwrap().is_empty());
        // ...but visible in counts and the outbox listing.
        assert_eq!(store.outbox_counts(), (0, 1));
        assert_eq!(store.list_outgoing().unwrap().len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn loopback_writes_in_copy() {
        let (store, dir) = tmp_store();
        let msg = store
            .enqueue_outgoing("me", "me", "t1", None, "text/plain", "note", 0)
            .unwrap();
        assert!(store.record_incoming(&msg).unwrap());
        store.mark_delivered(&msg.id, now_secs()).unwrap();
        let inbox = store.list_inbox(None).unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].body, "note");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn threads_aggregate_counts() {
        let (store, dir) = tmp_store();
        store
            .record_incoming(&sample_msg("m1", "peer", "me"))
            .unwrap();
        store
            .record_incoming(&sample_msg("m2", "peer", "me"))
            .unwrap();
        let threads = store.list_threads().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].message_count, 2);
        assert_eq!(threads[0].unread_count, 2);
        store.mark_thread_read("t1").unwrap();
        assert_eq!(store.list_threads().unwrap()[0].unread_count, 0);
        std::fs::remove_dir_all(dir).ok();
    }
}

/// Test hook: open a store at an explicit path.
#[cfg(test)]
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
