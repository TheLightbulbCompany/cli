use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// Migration `i` takes the db to `user_version` `i + 1`. Forward only; each one only adds
/// tables, so it is safe to run again.
const MIGRATIONS: &[&str] = &[V1, V2];

const V1: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    key TEXT PRIMARY KEY, provider TEXT, conversation TEXT, pid INTEGER,
    birth_micros INTEGER, cwd TEXT, title TEXT, status TEXT, seen_at INTEGER, executable TEXT
);
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY, request_id TEXT, from_key TEXT, to_key TEXT,
    kind TEXT DEFAULT 'message', thread TEXT, hops INTEGER DEFAULT 0, origin TEXT,
    body TEXT, sent_at INTEGER, UNIQUE (from_key, request_id)
);
CREATE TABLE IF NOT EXISTS receipts (
    from_key TEXT, request_id TEXT, state TEXT, reason TEXT, updated_at INTEGER,
    PRIMARY KEY (from_key, request_id)
);
CREATE TABLE IF NOT EXISTS mailbox (
    id INTEGER PRIMARY KEY, to_key TEXT, message_id TEXT, queued_at INTEGER, expires_at INTEGER
);
CREATE TABLE IF NOT EXISTS team (
    id TEXT PRIMARY KEY, name TEXT, founder_key TEXT, created_at INTEGER
);
";

/// The network tables. `team_link` holds the current team link secret (`team.link_secret`).
const V2: &str = "
CREATE TABLE IF NOT EXISTS roster (hash TEXT PRIMARY KEY, entry TEXT, received_at INTEGER);
CREATE TABLE IF NOT EXISTS team_link (
    ticket_id TEXT PRIMARY KEY, secret TEXT, secret_hash TEXT, mode TEXT, once INTEGER,
    expires_at INTEGER, used_by TEXT
);
CREATE TABLE IF NOT EXISTS join_requests (
    id INTEGER PRIMARY KEY, ticket_id TEXT, inviter TEXT, candidate TEXT, name TEXT,
    computer TEXT, kind TEXT, proof TEXT, via TEXT, created_at INTEGER, state TEXT
);
CREATE TABLE IF NOT EXISTS peers_sessions (
    member TEXT PRIMARY KEY, seq INTEGER, presence TEXT, seen_at INTEGER
);
CREATE TABLE IF NOT EXISTS outbox (
    message_id TEXT PRIMARY KEY, to_member TEXT, frame TEXT, attempts INTEGER,
    next_at INTEGER, created_at INTEGER
);
CREATE TABLE IF NOT EXISTS receipt_out (
    member TEXT, envelope_id TEXT, state TEXT, reason TEXT, PRIMARY KEY (member, envelope_id)
);
CREATE TABLE IF NOT EXISTS seen_ids (
    sender TEXT, envelope_id TEXT, state TEXT, seen_at INTEGER, PRIMARY KEY (sender, envelope_id)
);
CREATE TABLE IF NOT EXISTS holds (
    id INTEGER PRIMARY KEY, message_id TEXT, target_session TEXT, from_session TEXT,
    from_member TEXT, created_at INTEGER, state TEXT
);
CREATE TABLE IF NOT EXISTS policy (
    scope TEXT, person_id TEXT, value TEXT, set_by TEXT, updated_at INTEGER,
    PRIMARY KEY (scope, person_id)
);
CREATE TABLE IF NOT EXISTS approvals (
    hold_id INTEGER, verdict TEXT, scope TEXT, \"by\" TEXT, nonce TEXT UNIQUE,
    decided_at INTEGER, record TEXT
);
CREATE TABLE IF NOT EXISTS grants (grant_id TEXT PRIMARY KEY, \"grant\" TEXT, expires_at INTEGER);
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
    /// The agent process's executable, as the registering hook found it.
    pub executable: Option<String>,
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
        executable: r.get("executable")?,
    })
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens `path`, migrating it forward; refuses a db written by a newer version.
    pub fn open(path: &Path) -> crate::Result<Store> {
        let conn = Connection::open(path)?;
        let version: usize = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > MIGRATIONS.len() {
            return Err(format!(
                "State is from a newer version (v{version}). Install a newer lightbulb-network than {}.",
                env!("CARGO_PKG_VERSION")
            )
            .into());
        }
        for (i, m) in MIGRATIONS.iter().enumerate().skip(version) {
            // One transaction: `user_version` moves only once all of it ran; a failure rolls
            // back when `tx` drops.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(m)?;
            tx.pragma_update(None, "user_version", i + 1)?;
            tx.commit()?;
        }
        Ok(Store { conn })
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn put_session(&self, s: &Session) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                s.key,
                s.provider,
                s.conversation,
                s.pid,
                s.birth_micros,
                s.cwd,
                s.title,
                s.status,
                s.seen_at,
                s.executable
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

    /// Rewrites the Plan A `team` row from the roster.
    pub fn set_team(&self, id: &str, name: &str, founder: &str, at: i64) -> rusqlite::Result<()> {
        self.conn.execute_batch("DELETE FROM team")?;
        self.conn.execute(
            "INSERT INTO team VALUES (?1, ?2, ?3, ?4)",
            params![id, name, founder, at],
        )?;
        Ok(())
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
            executable: Some("/bin/claude".into()),
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

    fn user_version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn v1_db_migrates_to_v2_adding_tables_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(
                "INSERT INTO messages (id, request_id, from_key, to_key, body, sent_at)
                 VALUES ('m1', 'r1', 'a', 'b', 'hello', 9);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let message = Message {
            id: "m1".into(),
            request_id: "r1".into(),
            from_key: "a".into(),
            to_key: "b".into(),
            kind: "message".into(),
            thread: None,
            hops: 0,
            origin: None,
            body: "hello".into(),
            sent_at: 9,
        };
        Store::open(&path).unwrap();
        let store = Store::open(&path).unwrap();
        assert_eq!(user_version(&path), 2);
        assert_eq!(store.message("m1").unwrap(), Some(message));
        let holds: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'holds'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(holds, 1);
    }

    #[test]
    fn newer_db_refuses_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 9;")
            .unwrap();
        let err = Store::open(&path).err().unwrap().to_string();
        assert!(err.contains("State is from a newer version (v9)"), "{err}");
        assert_eq!(user_version(&path), 9);
    }
}
