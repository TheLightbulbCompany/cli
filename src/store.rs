use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    key TEXT PRIMARY KEY, provider TEXT, conversation TEXT, pid INTEGER,
    birth_micros INTEGER, cwd TEXT, title TEXT, status TEXT, seen_at INTEGER
);
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY, request_id TEXT UNIQUE, from_key TEXT, to_key TEXT,
    kind TEXT DEFAULT 'message', thread TEXT, hops INTEGER DEFAULT 0, origin TEXT,
    body TEXT, sent_at INTEGER
);
CREATE TABLE IF NOT EXISTS receipts (
    request_id TEXT PRIMARY KEY, state TEXT, reason TEXT, updated_at INTEGER
);
CREATE TABLE IF NOT EXISTS mailbox (
    id INTEGER PRIMARY KEY, to_key TEXT, message_id TEXT, queued_at INTEGER, expires_at INTEGER
);
CREATE TABLE IF NOT EXISTS team (
    id TEXT PRIMARY KEY, name TEXT, founder_key TEXT, created_at INTEGER
);
PRAGMA user_version = 1;
";

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub key: String,
    pub provider: String,
    pub conversation: Option<String>,
    pub pid: Option<i64>,
    pub birth_micros: Option<i64>,
    pub cwd: String,
    pub title: Option<String>,
    pub status: String,
    pub seen_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub request_id: String,
    pub from_key: String,
    pub to_key: String,
    pub kind: String,
    pub thread: Option<String>,
    pub hops: i64,
    pub origin: Option<String>,
    pub body: String,
    pub sent_at: i64,
}

fn row_session(r: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        key: r.get("key")?,
        provider: r.get("provider")?,
        conversation: r.get("conversation")?,
        pid: r.get("pid")?,
        birth_micros: r.get("birth_micros")?,
        cwd: r.get("cwd")?,
        title: r.get("title")?,
        status: r.get("status")?,
        seen_at: r.get("seen_at")?,
    })
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Store { conn })
    }

    pub fn put_session(&self, s: &Session) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                s.key,
                s.provider,
                s.conversation,
                s.pid,
                s.birth_micros,
                s.cwd,
                s.title,
                s.status,
                s.seen_at
            ],
        )?;
        Ok(())
    }

    pub fn session(&self, key: &str) -> rusqlite::Result<Option<Session>> {
        self.conn
            .query_row("SELECT * FROM sessions WHERE key = ?1", [key], row_session)
            .optional()
    }

    pub fn sessions(&self) -> rusqlite::Result<Vec<Session>> {
        let mut stmt = self.conn.prepare("SELECT * FROM sessions")?;
        let rows = stmt.query_map([], row_session)?;
        rows.collect()
    }

    pub fn delete_session(&self, key: &str) -> rusqlite::Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM sessions WHERE key = ?1", [key])?
            > 0)
    }

    pub fn put_message(&self, m: &Message) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO messages VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                m.id,
                m.request_id,
                m.from_key,
                m.to_key,
                m.kind,
                m.thread,
                m.hops,
                m.origin,
                m.body,
                m.sent_at
            ],
        )?;
        Ok(())
    }

    pub fn message(&self, id: &str) -> rusqlite::Result<Option<Message>> {
        self.conn
            .query_row("SELECT * FROM messages WHERE id = ?1", [id], |r| {
                Ok(Message {
                    id: r.get("id")?,
                    request_id: r.get("request_id")?,
                    from_key: r.get("from_key")?,
                    to_key: r.get("to_key")?,
                    kind: r.get("kind")?,
                    thread: r.get("thread")?,
                    hops: r.get("hops")?,
                    origin: r.get("origin")?,
                    body: r.get("body")?,
                    sent_at: r.get("sent_at")?,
                })
            })
            .optional()
    }

    /// Creates the solo team on first start (a Plan A stub); returns `(id, name)`.
    pub fn ensure_team(
        &self,
        id: &str,
        name: &str,
        created_at: i64,
    ) -> rusqlite::Result<(String, String)> {
        self.conn.execute(
            "INSERT OR IGNORE INTO team VALUES (?1, ?2, ?1, ?3)",
            params![id, name, created_at],
        )?;
        self.conn.query_row(
            "SELECT id, name FROM team WHERE founder_key = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trips_a_session_and_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let session = Session {
            key: "claude:abc".into(),
            provider: "claude".into(),
            conversation: None,
            pid: Some(42),
            birth_micros: Some(1),
            cwd: "/tmp/project".into(),
            title: None,
            status: "idle".into(),
            seen_at: 7,
        };
        store.put_session(&session).unwrap();
        assert_eq!(store.session("claude:abc").unwrap(), Some(session));

        let message = Message {
            id: "m1".into(),
            request_id: "r1".into(),
            from_key: "claude:abc".into(),
            to_key: "codex:def".into(),
            kind: "message".into(),
            thread: None,
            hops: 0,
            origin: None,
            body: "hello".into(),
            sent_at: 9,
        };
        store.put_message(&message).unwrap();
        assert_eq!(store.message("m1").unwrap(), Some(message));
        assert_eq!(store.message("missing").unwrap(), None);
    }
}
