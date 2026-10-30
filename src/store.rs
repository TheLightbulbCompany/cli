use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Migration `i` takes the db to `user_version` `i + 1`. Forward only; each one only adds
/// tables or columns, but V12, which drops and recreates `memory_fts` (stemmed) from the
/// entries; each runs once (in the transaction that moves `user_version`).
const MIGRATIONS: &[&str] = &[
    V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15, V16,
];

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

/// The network tables. `team_link` holds one row per link minted on this computer (`secret`
/// NULL for a once link, which keeps only `secret_hash`).
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
CREATE TABLE IF NOT EXISTS approvals (
    hold_id INTEGER, verdict TEXT, scope TEXT, \"by\" TEXT, nonce TEXT UNIQUE,
    decided_at INTEGER, record TEXT
);
CREATE TABLE IF NOT EXISTS grants (grant_id TEXT PRIMARY KEY, \"grant\" TEXT, expires_at INTEGER);
";

/// A session's end: its row stays a day (`presence`'s `kept`) with `ended_at` set.
const V3: &str = "ALTER TABLE sessions ADD COLUMN ended_at INTEGER;";

/// The text a hold's dialog showed, which its `approvals` record signs.
const V4: &str = "ALTER TABLE holds ADD COLUMN shown TEXT;";
/// Which loop limit a `loop` refusal hit (`mail::HOPS`, `mail::RATE`), when known.
const V5: &str = "ALTER TABLE receipts ADD COLUMN \"limit\" TEXT;";
/// A session's repo and branch as its hook read them (the daemon never reads a session's
/// folder), its `who` handle, the sessions registered under a hidden folder (kept out of
/// discovery too), and the index a mailbox's reply-window query reads by.
/// (Edited in place while unreleased: no installed build has written V6.)
const V6: &str = "
ALTER TABLE sessions ADD COLUMN repo TEXT;
ALTER TABLE sessions ADD COLUMN branch TEXT;
ALTER TABLE sessions ADD COLUMN handle TEXT;
CREATE TABLE IF NOT EXISTS hidden_sessions (key TEXT PRIMARY KEY, updated_at INTEGER);
CREATE INDEX IF NOT EXISTS messages_to_sent ON messages (to_key, sent_at);
";
/// A message's text digest, kept when `mail::forget` drops the text, so a retry under the same
/// request id still compares.
const V7: &str = "ALTER TABLE messages ADD COLUMN body_sha TEXT;";
/// Retired (the person links' key -> person map): kept so the schema version still counts.
const V8: &str =
    "CREATE TABLE IF NOT EXISTS persons (key TEXT PRIMARY KEY, person TEXT, at INTEGER);";
/// The memory block last handed to each session at its start (`lightbulb memory last`).
const V10: &str =
    "CREATE TABLE IF NOT EXISTS memory_last (session TEXT PRIMARY KEY, block TEXT, at INTEGER);";
/// Local capture (`capture`): a coding session and its tool calls. `summary` is at most 200
/// characters, or `[refused: <pattern>]`.
const V11: &str = "
CREATE TABLE IF NOT EXISTS capture_sessions (
    id TEXT PRIMARY KEY, harness TEXT NOT NULL, computer TEXT NOT NULL, repo TEXT, branch TEXT,
    started INTEGER NOT NULL, ended INTEGER, prompt_tokens INTEGER, completion_tokens INTEGER
);
CREATE TABLE IF NOT EXISTS capture_tool_calls (
    session_id TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL, tool TEXT NOT NULL,
    summary TEXT, duration_ms INTEGER, ok INTEGER NOT NULL, PRIMARY KEY (session_id, seq)
);
";
/// Memory search stems words (`porter`: tests/test, deploys/deploy), so the index is rebuilt
/// from the entries as the triggers write it; V9's triggers stay. `memory_seen`: the highest
/// seq a peer has reported holding for one of our own keys, so an entry written after a
/// restored `state.db` never reuses a number peers already hold.
const V12: &str = "
DROP TABLE IF EXISTS memory_fts;
CREATE VIRTUAL TABLE memory_fts USING fts5(
    subject, fact, content='memory_entries', content_rowid='rowid', tokenize='porter unicode61'
);
INSERT INTO memory_fts(rowid, subject, fact)
    SELECT rowid, json_extract(entry, '$.op.subject'), json_extract(entry, '$.op.fact')
    FROM memory_entries;
CREATE TABLE IF NOT EXISTS memory_seen (author TEXT PRIMARY KEY, seq INTEGER NOT NULL);
";
/// One embedding per held `remember` (`memory_vec`: `DIM` little-endian f32s); absent when no
/// model was installed then (`memory::embed_missing` fills it in).
const V13: &str = "
CREATE TABLE IF NOT EXISTS memory_vec (
    id TEXT PRIMARY KEY REFERENCES memory_entries(id), v BLOB NOT NULL
);
";
/// The session's own turns (`capture::record_turn`), on this computer only: the text, a
/// full-text index over it, and one embedding per turn when a model was installed. `nudged`
/// marks a session the end-of-session nudge has already looked at.
const V14: &str = "
CREATE TABLE IF NOT EXISTS capture_turns (
    rowid INTEGER PRIMARY KEY, session_id TEXT NOT NULL, seq INTEGER NOT NULL, role TEXT NOT NULL,
    text TEXT NOT NULL, at INTEGER NOT NULL, UNIQUE (session_id, seq)
);
CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
    text, content='capture_turns', content_rowid='rowid', tokenize='porter unicode61'
);
CREATE TRIGGER IF NOT EXISTS turns_ai AFTER INSERT ON capture_turns BEGIN
    INSERT INTO turns_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER IF NOT EXISTS turns_ad AFTER DELETE ON capture_turns BEGIN
    INSERT INTO turns_fts(turns_fts, rowid, text) VALUES ('delete', old.rowid, old.text);
    DELETE FROM turns_vec WHERE turn = old.rowid;
END;
CREATE TABLE IF NOT EXISTS turns_vec (turn INTEGER PRIMARY KEY, v BLOB NOT NULL);
ALTER TABLE capture_sessions ADD COLUMN nudged INTEGER NOT NULL DEFAULT 0;
";
/// Why a held memory entry has no body when it was refused rather than forgotten
/// (`refused:<pattern>`: a peer's entry that looks like a credential).
const V15: &str = "ALTER TABLE memory_entries ADD COLUMN reason TEXT;";

/// Why a session ended, and what a waiting one waits for.
const V16: &str =
    "ALTER TABLE sessions ADD COLUMN reason TEXT; ALTER TABLE sessions ADD COLUMN waiting_for TEXT;";
/// Memory: signed entries numbered per author, with a full-text index over `subject` and `fact`
/// (`memory`). `rowid` is explicit so it survives `VACUUM` (the FTS index points at it). A
/// tombstoned row keeps its id and seq and drops its body (`entry` NULL); `memory_tombstones`
/// holds every id a held `Forget` names, so a body that arrives later is stored without its text.
const V9: &str = "
CREATE TABLE IF NOT EXISTS memory_entries (
    rowid INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, author TEXT, seq INTEGER, entry TEXT,
    received_at INTEGER, tombstone INTEGER DEFAULT 0, UNIQUE(author, seq)
);
CREATE TABLE IF NOT EXISTS memory_tombstones (target TEXT PRIMARY KEY);
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    subject, fact, content='memory_entries', content_rowid='rowid'
);
CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory_entries BEGIN
    INSERT INTO memory_fts(rowid, subject, fact)
    VALUES (new.rowid, json_extract(new.entry, '$.op.subject'), json_extract(new.entry, '$.op.fact'));
END;
CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, subject, fact)
    VALUES ('delete', old.rowid, json_extract(old.entry, '$.op.subject'), json_extract(old.entry, '$.op.fact'));
END;
CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, subject, fact)
    VALUES ('delete', old.rowid, json_extract(old.entry, '$.op.subject'), json_extract(old.entry, '$.op.fact'));
    INSERT INTO memory_fts(rowid, subject, fact)
    VALUES (new.rowid, json_extract(new.entry, '$.op.subject'), json_extract(new.entry, '$.op.fact'));
END;
CREATE VIEW IF NOT EXISTS memory_heads AS
    SELECT author, MAX(seq) AS seq FROM memory_entries WHERE seq IS NOT NULL GROUP BY author;
";

/// The digest `messages.body_sha` keeps: hex sha256 of the text.
pub fn body_sha(text: &str) -> String {
    use sha2::{Digest, Sha256};
    data_encoding::HEXLOWER.encode(&Sha256::digest(text.as_bytes()))
}

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
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// `who`'s `<provider>-<n>`, given when the row is first written and kept for its life
    /// (`put_session` sets it; what a caller passes is ignored).
    pub handle: Option<String>,
    /// Why it ended (`end_session`); none while it is live.
    pub reason: Option<String>,
    /// When it ended (`end_session`); none while it is live.
    pub ended_at: Option<i64>,
    /// What a `waiting` session waits for its person on.
    pub waiting_for: Option<String>,
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
        repo: r.get("repo")?,
        branch: r.get("branch")?,
        handle: r.get("handle")?,
        reason: r.get("reason")?,
        ended_at: r.get("ended_at")?,
        waiting_for: r.get("waiting_for")?,
    })
}

/// A `messages` row, read by column name (so `m.*` in a join reads too).
pub(crate) fn row_message(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get("id")?,
        request_id: r.get("request_id")?,
        from_key: r.get("from_key")?,
        to_key: r.get("to_key")?,
        kind: r.get("kind")?,
        thread: r.get("thread")?,
        hops: r.get("hops")?,
        origin: r.get("origin")?,
        // `NULL` once `mail::forget` dropped it.
        body: r.get::<_, Option<String>>("body")?.unwrap_or_default(),
        sent_at: r.get("sent_at")?,
    })
}

pub struct Store {
    conn: Connection,
    /// The state dir the db is in: where memory looks for the embedding model.
    dir: PathBuf,
}

impl Store {
    /// Opens `path`, migrating it forward; refuses a db written by a newer version.
    pub fn open(path: &Path) -> crate::Result<Store> {
        let conn = Connection::open(path)?;
        // A second connection (a test, a tool run beside the daemon) waits for the daemon's
        // write instead of failing with "database is locked".
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: usize = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > MIGRATIONS.len() {
            return Err(format!(
                "State is from a newer version (v{version}). Install a newer Lightbulb than {}.",
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
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        Ok(Store { conn, dir })
    }

    /// The state dir this store's db is in (`embed_model` looks for the model there).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// A consistent copy of the database at `path` (`VACUUM INTO` a temporary file beside it,
    /// then renamed over it).
    pub fn snapshot(&self, path: &Path) -> crate::Result<()> {
        let tmp = path.with_extension("snapshot-tmp");
        let _ = std::fs::remove_file(&tmp);
        let to = tmp.to_str().ok_or("the snapshot path is not UTF-8")?;
        self.conn.execute("VACUUM INTO ?1", [to])?;
        // Mail bodies: the person's own, like the state dir.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Writes `s` as a live row (an ended one is live again). A new agent session's row gets the
    /// handle `<provider>-<n>`, one past its provider's highest, which it then keeps.
    /// ponytail: a number frees up again only when its row is purged (a week after it ended)
    /// and it was the highest.
    pub fn put_session(&self, s: &Session) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (key, provider, conversation, pid, birth_micros, cwd, title, \
             status, seen_at, executable, repo, branch, waiting_for, handle) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
             CASE WHEN ?2 = '' THEN NULL ELSE ?2 || '-' || (SELECT 1 + coalesce(max( \
             CAST(substr(handle, length(?2) + 2) AS INTEGER)), 0) FROM sessions \
             WHERE provider = ?2) END) \
             ON CONFLICT(key) DO UPDATE SET provider = ?2, conversation = ?3, pid = ?4, \
             birth_micros = ?5, cwd = ?6, title = ?7, status = ?8, seen_at = ?9, \
             executable = ?10, repo = ?11, branch = ?12, waiting_for = ?13, ended_at = NULL, \
             reason = NULL, \
             handle = coalesce(handle, excluded.handle)",
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
                s.executable,
                s.repo,
                s.branch,
                s.waiting_for
            ],
        )?;
        Ok(())
    }

    /// Marks `key` as registered under a hidden folder at `now` (`hidden`), or no longer;
    /// whether the mark changed. A mark not renewed for a day goes with `prune_hidden`, unless its session is live.
    pub fn set_hidden(&self, key: &str, hidden: bool, now: i64) -> rusqlite::Result<bool> {
        let was = self
            .conn
            .query_row(
                "SELECT 1 FROM hidden_sessions WHERE key = ?1",
                [key],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if hidden {
            self.conn.execute(
                "INSERT OR REPLACE INTO hidden_sessions VALUES (?1, ?2)",
                params![key, now],
            )?;
        } else {
            self.conn
                .execute("DELETE FROM hidden_sessions WHERE key = ?1", [key])?;
        }
        Ok(was != hidden)
    }

    /// Deletes the hidden marks last renewed before `since` but those of `live` sessions: a
    /// live one stays hidden however long it idles without a hook event.
    pub fn prune_hidden(
        &self,
        since: i64,
        live: &std::collections::HashSet<&str>,
    ) -> rusqlite::Result<()> {
        let old: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT key FROM hidden_sessions WHERE updated_at < ?1")?;
            let rows = stmt.query_map([since], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for k in old.iter().filter(|k| !live.contains(k.as_str())) {
            self.conn
                .execute("DELETE FROM hidden_sessions WHERE key = ?1", [k])?;
        }
        Ok(())
    }

    /// Keeps `block` as the last memory block `session` was handed.
    pub fn set_memory_last(&self, session: &str, block: &str, at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO memory_last VALUES (?1, ?2, ?3)",
            params![session, block, at],
        )?;
        Ok(())
    }

    /// The block `session` was last handed, else the most recent one any session was.
    pub fn memory_last(&self, session: Option<&str>) -> rusqlite::Result<Option<String>> {
        use rusqlite::OptionalExtension;
        self.conn
            .query_row(
                "SELECT block FROM memory_last WHERE session = ?1
                 UNION ALL SELECT block FROM (SELECT block FROM memory_last ORDER BY at DESC LIMIT 1)
                 LIMIT 1",
                [session],
                |r| r.get(0),
            )
            .optional()
    }

    /// The keys `set_hidden` marked.
    pub fn hidden_keys(&self) -> rusqlite::Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT key FROM hidden_sessions")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// `key`'s row, unless it ended.
    pub fn session(&self, key: &str) -> rusqlite::Result<Option<Session>> {
        self.conn
            .query_row(
                "SELECT * FROM sessions WHERE key = ?1 AND ended_at IS NULL",
                [key],
                row_session,
            )
            .optional()
    }

    /// `key`'s row, ended or not: what a revived row keeps.
    pub fn any_session(&self, key: &str) -> rusqlite::Result<Option<Session>> {
        self.conn
            .query_row("SELECT * FROM sessions WHERE key = ?1", [key], row_session)
            .optional()
    }

    /// The rows that have not ended.
    pub fn sessions(&self) -> rusqlite::Result<Vec<Session>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM sessions WHERE ended_at IS NULL ORDER BY rowid")?;
        let rows = stmt.query_map([], row_session)?;
        rows.collect()
    }

    /// Whether this computer has a row for `key`, ended or not.
    pub fn holds(&self, key: &str) -> rusqlite::Result<bool> {
        self.conn
            .query_row("SELECT 1 FROM sessions WHERE key = ?1", [key], |_| Ok(()))
            .optional()
            .map(|r| r.is_some())
    }

    /// Ends `key`'s row at `now` with `reason` (and `title`, when given); `false` when there is
    /// no live one.
    pub fn end_session(
        &self,
        key: &str,
        now: i64,
        reason: &str,
        title: Option<&str>,
    ) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE sessions SET ended_at = ?2, reason = ?3, \
             title = coalesce(?4, title) WHERE key = ?1 AND ended_at IS NULL",
            params![key, now, reason, title],
        )? > 0)
    }

    /// A row born ended at `at`, for a job first seen finished: no process, and no handle (one
    /// comes if it is ever resumed), since hundreds of old jobs must not use up `who`'s numbers.
    /// `false`, changing nothing, when `key` has a row: a job ends once.
    pub fn put_ended_job(
        &self,
        key: &str,
        conversation: &str,
        cwd: &str,
        title: Option<&str>,
        at: i64,
    ) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO sessions (key, provider, conversation, cwd, title, status, \
             seen_at, reason, ended_at) VALUES (?1, 'claude', ?2, ?3, ?4, 'idle', ?5, 'job-done', ?5)",
            params![key, conversation, cwd, title, at],
        )? > 0)
    }

    /// The keys of the rows born ended (`put_ended_job`), or ended with no process.
    pub fn ended_job_keys(&self) -> rusqlite::Result<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT key FROM sessions WHERE ended_at IS NOT NULL AND pid IS NULL")?;
        let keys = stmt.query_map([], |r| r.get(0))?;
        keys.collect()
    }

    /// The ended rows, newest end first; deletes those that ended before `since` unless their
    /// key is in `keep` (a listing still shows it).
    pub fn ended_since(
        &self,
        since: i64,
        keep: &HashSet<String>,
    ) -> rusqlite::Result<Vec<Session>> {
        let mut stale = self
            .conn
            .prepare("SELECT key FROM sessions WHERE ended_at < ?1")?;
        let stale: Vec<String> = stale
            .query_map([since], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for key in stale.iter().filter(|k| !keep.contains(*k)) {
            self.conn
                .execute("DELETE FROM sessions WHERE key = ?1", [key])?;
        }
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM sessions WHERE ended_at IS NOT NULL ORDER BY ended_at DESC")?;
        let rows = stmt.query_map([], row_session)?;
        rows.collect()
    }

    /// Writes `member`'s latest presence (`text`, its JSON).
    pub fn put_presence(
        &self,
        member: &str,
        seq: u64,
        text: &str,
        seen_at: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO peers_sessions VALUES (?1, ?2, ?3, ?4)",
            params![
                member,
                i64::try_from(seq).unwrap_or(i64::MAX),
                text,
                seen_at
            ],
        )?;
        Ok(())
    }

    /// Every stored presence, `(member, JSON, seen_at)`.
    pub fn peers_presence(&self) -> rusqlite::Result<Vec<(String, String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT member, presence, seen_at FROM peers_sessions")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    pub fn touch_presence(&self, member: &str, seen_at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE peers_sessions SET seen_at = ?2 WHERE member = ?1",
            params![member, seen_at],
        )?;
        Ok(())
    }

    pub fn drop_presence(&self, member: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM peers_sessions WHERE member = ?1", [member])?;
        Ok(())
    }

    pub fn put_message(&self, m: &Message) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO messages (id, request_id, from_key, to_key, kind, thread, hops, origin, \
             body, sent_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
            .query_row("SELECT * FROM messages WHERE id = ?1", [id], row_message)
            .optional()
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

    /// Records a link minted here. A once link (`hashed`) keeps only its secret's hash.
    pub fn put_link(
        &self,
        ticket_id: &str,
        secret: &str,
        hashed: Option<&str>,
        mode: &str,
        expires_at: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO team_link VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
            params![
                ticket_id,
                hashed.is_none().then_some(secret),
                hashed,
                mode,
                hashed.is_some(),
                expires_at
            ],
        )?;
        Ok(())
    }

    /// Forgets the team: every roster entry and every link minted here.
    pub fn clear_roster(&self) -> rusqlite::Result<()> {
        self.conn
            .execute_batch("DELETE FROM roster; DELETE FROM team_link;")
    }

    /// Notes that once link `ticket_id`, if minted here, admitted `by`.
    pub fn use_link(&self, ticket_id: &str, by: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE team_link SET used_by = ?2 WHERE ticket_id = ?1",
            [ticket_id, by],
        )?;
        Ok(())
    }

    pub fn link_used(&self, ticket_id: &str) -> rusqlite::Result<bool> {
        self.conn
            .prepare("SELECT 1 FROM team_link WHERE ticket_id = ?1 AND used_by IS NOT NULL")?
            .exists([ticket_id])
    }

    /// Stores `j` as a new `open` join request; its id.
    pub fn put_join(&self, j: &Join) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO join_requests (ticket_id, inviter, candidate, name, computer,
             created_at, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                j.ticket_id,
                j.inviter,
                j.candidate,
                j.name,
                j.computer,
                j.created_at,
                OPEN
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Moves join request `id` from `open` to `state` (nothing if it is no longer open).
    pub fn close_join(&self, id: i64, state: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE join_requests SET state = ?2 WHERE id = ?1 AND state = ?3",
            params![id, state, OPEN],
        )?;
        Ok(())
    }

    /// Marks `expired` every open join request created before `before`, and deletes the
    /// closed ones older than 30 days.
    pub fn expire_joins(&self, before: i64, now: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE join_requests SET state = 'expired' WHERE state = ?2 AND created_at < ?1",
            params![before, OPEN],
        )?;
        self.conn.execute(
            "DELETE FROM join_requests WHERE state != ?2 AND created_at < ?1",
            params![now - JOIN_KEEP_S, OPEN],
        )?;
        Ok(())
    }

    /// The open join requests: in all, by `inviter`, and by `candidate`.
    pub fn open_counts(&self, inviter: &str, candidate: &str) -> rusqlite::Result<[usize; 3]> {
        self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(inviter = ?1), 0), COALESCE(SUM(candidate = ?2), 0)
             FROM join_requests WHERE state = ?3",
            params![inviter, candidate, OPEN],
            |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?]),
        )
    }

    pub fn open_joins(&self) -> rusqlite::Result<Vec<Join>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, ticket_id, inviter, candidate, name, computer, created_at
             FROM join_requests WHERE state = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([OPEN], |r| {
            Ok(Join {
                id: r.get(0)?,
                ticket_id: r.get(1)?,
                inviter: r.get(2)?,
                candidate: r.get(3)?,
                name: r.get(4)?,
                computer: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    pub fn join_exists(&self, id: i64) -> rusqlite::Result<bool> {
        self.conn
            .prepare("SELECT 1 FROM join_requests WHERE id = ?1")?
            .exists([id])
    }
}

/// Joins each `messages m` row to its receipt `r` (the sender's `receipts` row).
pub const MSG_RECEIPT: &str =
    "JOIN receipts r ON r.from_key = m.from_key AND r.request_id = m.request_id";

/// A peer's message whose receipt moved past what its sender was told (`told`).
pub struct Untold {
    pub sender: String,
    pub id: String,
    pub state: String,
    pub reason: Option<String>,
    pub told: String,
}

/// Mail between members: the mailbox, what peers sent (`seen_ids`) and the outbox.
impl Store {
    /// Whether capture session `id` (session `key`) gets the end-of-turn nudge now: it made a
    /// tool call, was never nudged, wrote no memory entry and has no mail waiting. A yes marks
    /// it (`nudged` = `now`), so it never comes twice.
    /// ponytail: the entry check scans memory_entries' JSON; an index on the source session if
    /// memory grows past tens of thousands of entries.
    pub fn nudge(&self, id: &str, key: &str, now: i64) -> rusqlite::Result<bool> {
        let n = self.conn().execute(
            "UPDATE capture_sessions SET nudged = ?3 WHERE id = ?1 AND nudged = 0 \
             AND EXISTS (SELECT 1 FROM capture_tool_calls WHERE session_id = ?1) \
             AND NOT EXISTS (SELECT 1 FROM memory_entries \
                 WHERE json_extract(entry, '$.op.source.session') = ?2) \
             AND NOT EXISTS (SELECT 1 FROM mailbox WHERE to_key = ?2 AND expires_at >= ?3)",
            params![id, key, now],
        )?;
        Ok(n == 1)
    }

    /// `from`'s message `request_id` as stored: `(to, whether its text is `text`)`, the text
    /// compared by its digest once `mail::forget` dropped it.
    pub fn sent_as(
        &self,
        from: &str,
        request_id: &str,
        text: &str,
    ) -> rusqlite::Result<Option<(String, bool)>> {
        self.conn
            .query_row(
                "SELECT to_key, body, body_sha FROM messages WHERE from_key = ?1 AND request_id = ?2",
                [from, request_id],
                |r| {
                    let (body, sha): (Option<String>, Option<String>) = (r.get(1)?, r.get(2)?);
                    let same = match body {
                        Some(b) => b == text,
                        None => sha.as_deref() == Some(body_sha(text).as_str()),
                    };
                    Ok((r.get(0)?, same))
                },
            )
            .optional()
    }

    /// The thread of `from`'s message `request_id`, if it has one here.
    pub fn thread_of(&self, from: &str, request_id: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT thread FROM messages WHERE from_key = ?1 AND request_id = ?2",
                [from, request_id],
                |r| r.get(0),
            )
            .optional()
            .map(Option::flatten)
    }

    pub fn put_mailbox(&self, to: &str, id: &str, at: i64, expires: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO mailbox (to_key, message_id, queued_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![to, id, at, expires],
        )?;
        Ok(())
    }

    /// `member`'s stored presence, as JSON.
    pub fn presence_of(&self, member: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT presence FROM peers_sessions WHERE member = ?1",
                [member],
                |r| r.get(0),
            )
            .optional()
    }

    /// Envelope `id`'s receipt now, `(state, reason)`, if `sender` sent it here; what the sender
    /// was last told when its message is gone.
    pub fn seen_state(
        &self,
        sender: &str,
        id: &str,
    ) -> rusqlite::Result<Option<(String, Option<String>)>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT COALESCE(r.state, s.state), r.reason FROM seen_ids s \
                     LEFT JOIN messages m ON m.id = s.envelope_id LEFT {MSG_RECEIPT} \
                     WHERE s.sender = ?1 AND s.envelope_id = ?2"
                ),
                [sender, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// `sender`'s envelope `id` was seen at `at`, and `told` its state.
    pub fn put_seen(&self, sender: &str, id: &str, told: &str, at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO seen_ids VALUES (?1, ?2, ?3, ?4)",
            params![sender, id, told, at],
        )?;
        Ok(())
    }

    /// One past the deepest of the messages `ids` this computer sent (`origin` local).
    pub fn hop_floor(&self, ids: [Option<&str>; 2]) -> rusqlite::Result<Option<i64>> {
        self.conn.query_row(
            "SELECT MAX(hops) + 1 FROM messages WHERE id IN (?1, ?2) AND origin = 'local'",
            params![ids[0], ids[1]],
            |r| r.get(0),
        )
    }

    /// The peers' mail to `to` sent after `since`: `(from, origin)`.
    pub fn net_mail_to(&self, to: &str, since: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_key, origin FROM messages WHERE to_key = ?1 AND sent_at > ?2 \
             AND origin LIKE 'net:%'",
        )?;
        let rows = stmt.query_map(params![to, since], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Peers' messages whose receipt moved since the sender was told.
    pub fn untold(&self) -> rusqlite::Result<Vec<Untold>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT s.sender, s.envelope_id, r.state, r.reason, s.state FROM seen_ids s \
             JOIN messages m ON m.id = s.envelope_id {MSG_RECEIPT} WHERE r.state != s.state"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(Untold {
                sender: r.get(0)?,
                id: r.get(1)?,
                state: r.get(2)?,
                reason: r.get(3)?,
                told: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn set_told(&self, sender: &str, id: &str, state: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE seen_ids SET state = ?3 WHERE sender = ?1 AND envelope_id = ?2",
            [sender, id, state],
        )?;
        Ok(())
    }

    pub fn put_outbox(&self, id: &str, member: &str, frame: &str, at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO outbox VALUES (?1, ?2, ?3, 0, ?4, ?4)",
            params![id, member, frame, at],
        )?;
        Ok(())
    }

    /// The frames for `members` due at `now`, each message's while still `queued`: `(row id,
    /// member, frame)`.
    pub fn outbox_due(
        &self,
        members: &[String],
        now: i64,
    ) -> rusqlite::Result<Vec<(String, String, String)>> {
        if members.is_empty() {
            return Ok(vec![]);
        }
        let marks = vec!["?"; members.len()].join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT o.message_id, o.to_member, o.frame FROM outbox o \
             JOIN messages m ON m.id = o.message_id LEFT {MSG_RECEIPT} \
             WHERE r.state = 'queued' AND o.next_at <= ? AND o.to_member IN ({marks})"
        ))?;
        let args = [&now as &dyn rusqlite::ToSql]
            .into_iter()
            .chain(members.iter().map(|m| m as &dyn rusqlite::ToSql));
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect()
    }

    /// Outbox row `id` went out at `now`: the next try is 1, 2, 4 … at most 60 s later.
    pub fn outbox_sent(&self, id: &str, now: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE outbox SET attempts = attempts + 1, next_at = ?2 + MIN(60, 1 << MIN(attempts, 6)) \
             WHERE message_id = ?1",
            params![id, now],
        )?;
        Ok(())
    }

    /// The sender's message behind outbox row `id`, if it went to `member`: `(from, request_id)`.
    pub fn outbox_sender(
        &self,
        id: &str,
        member: &str,
    ) -> rusqlite::Result<Option<(String, String)>> {
        self.conn
            .query_row(
                "SELECT m.from_key, m.request_id FROM outbox o JOIN messages m ON m.id = o.message_id \
                 WHERE o.message_id = ?1 AND o.to_member = ?2",
                [id, member],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// The messages still `queued` in the outbox since `before` or earlier: `(from, request_id,
    /// envelope id)`.
    pub fn outbox_stale(&self, before: i64) -> rusqlite::Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT m.from_key, m.request_id, o.message_id FROM outbox o \
             JOIN messages m ON m.id = o.message_id \
             {MSG_RECEIPT} WHERE r.state = 'queued' AND o.created_at <= ?1"
        ))?;
        let rows = stmt.query_map([before], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// Drops outbox rows final before `final_before` or created before `created_before`, and
    /// `seen_ids` from before `seen_before`.
    pub fn prune_mail(
        &self,
        final_before: i64,
        created_before: i64,
        seen_before: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            &format!(
                "DELETE FROM outbox WHERE created_at < ?2 OR message_id IN (SELECT o.message_id \
                 FROM outbox o JOIN messages m ON m.id = o.message_id {MSG_RECEIPT} \
                 WHERE r.state IN ('read', 'refused', 'expired') AND r.updated_at < ?1)"
            ),
            [final_before, created_before],
        )?;
        self.conn
            .execute("DELETE FROM seen_ids WHERE seen_at < ?1", [seen_before])?;
        Ok(())
    }
}

/// A join request waiting for its person.
const OPEN: &str = "open";
/// How long answered join requests are kept.
const JOIN_KEEP_S: i64 = 30 * 86_400;

/// A `join_requests` row (`id` is unset until stored).
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub id: i64,
    pub ticket_id: String,
    pub inviter: String,
    pub candidate: String,
    pub name: String,
    pub computer: String,
    pub created_at: i64,
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
            repo: None,
            branch: None,
            handle: None,
            reason: None,
            ended_at: None,
            waiting_for: None,
        };
        store.put_session(&session).unwrap();
        let handle = Some("claude-1".into());
        assert_eq!(
            store.session("claude:abc").unwrap(),
            Some(Session { handle, ..session })
        );

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
        assert_eq!(user_version(&path), MIGRATIONS.len() as i64);
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
    fn v2_db_migrates_to_v3_keeping_its_sessions_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute_batch(MIGRATIONS[1]).unwrap();
            conn.execute_batch(
                "INSERT INTO sessions VALUES ('k', 'codex', 'c', 1, 1, '/p', NULL, 'idle', 7, NULL);
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(user_version(&path), MIGRATIONS.len() as i64);
        assert_eq!(store.sessions().unwrap().len(), 1);
        assert!(store.end_session("k", 9, "process-exited", None).unwrap());
        assert!(!store.end_session("k", 9, "process-exited", None).unwrap());
        assert_eq!(store.session("k").unwrap(), None);
        assert!(store.holds("k").unwrap());
        let ended = store.ended_since(9, &HashSet::new()).unwrap();
        assert_eq!(
            ended
                .iter()
                .map(|e| (e.key.as_str(), e.cwd.as_str()))
                .collect::<Vec<_>>(),
            [("k", "/p")]
        );
        assert!(store.ended_since(10, &HashSet::new()).unwrap().is_empty());
        assert!(!store.holds("k").unwrap());
    }

    fn waiting_row(key: &str) -> Session {
        Session {
            key: key.into(),
            provider: "claude".into(),
            conversation: None,
            pid: None,
            birth_micros: None,
            cwd: "/".into(),
            title: None,
            status: "idle".into(),
            seen_at: 1,
            executable: None,
            repo: None,
            branch: None,
            handle: None,
            reason: None,
            ended_at: None,
            waiting_for: None,
        }
    }

    #[test]
    fn ending_keeps_reason_and_title_and_prunes_after_the_window_unless_kept() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        s.put_session(&waiting_row("k/claude:a")).unwrap();
        s.put_session(&waiting_row("k/claude:b")).unwrap();
        assert!(s
            .end_session("k/claude:a", 1_000, "moved-on", Some("fix the thing"))
            .unwrap());
        assert!(s
            .end_session("k/claude:b", 1_000, "job-done", None)
            .unwrap());
        let ended = s.ended_since(500, &HashSet::new()).unwrap();
        assert_eq!(ended.len(), 2);
        assert_eq!((ended[0].ended_at, ended[0].seen_at), (Some(1_000), 1));
        assert_eq!(
            (ended[0].reason.as_deref(), ended[0].title.as_deref()),
            (Some("moved-on"), Some("fix the thing"))
        );
        // Past the window: `a` goes, `b` is kept because the listing still shows it.
        let ended = s
            .ended_since(2_000, &HashSet::from(["k/claude:b".to_string()]))
            .unwrap();
        assert_eq!(
            ended.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
            ["k/claude:b"]
        );
        assert!(!s.holds("k/claude:a").unwrap());
    }

    #[test]
    fn a_waiting_row_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let row = Session {
            status: "waiting".into(),
            waiting_for: Some("permission prompt".into()),
            ..waiting_row("k/claude:a")
        };
        s.put_session(&row).unwrap();
        let got = s.session("k/claude:a").unwrap().unwrap();
        assert_eq!(
            (got.status.as_str(), got.waiting_for.as_deref()),
            ("waiting", Some("permission prompt"))
        );
        // Ended, then registered again: live again, the reason cleared.
        s.end_session("k/claude:a", 5, "moved-on", None).unwrap();
        s.put_session(&row).unwrap();
        let back = s.session("k/claude:a").unwrap().unwrap();
        assert_eq!((back.reason, back.ended_at), (None, None));
    }

    #[test]
    fn a_rewritten_row_keeps_its_handle_and_an_ended_one_its_number() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let row = |key: &str, provider: &str| Session {
            key: key.into(),
            provider: provider.into(),
            conversation: None,
            pid: None,
            birth_micros: None,
            cwd: "/".into(),
            title: None,
            status: "idle".into(),
            seen_at: 1,
            executable: None,
            repo: Some("r".into()),
            branch: None,
            handle: None,
            reason: None,
            ended_at: None,
            waiting_for: None,
        };
        for (k, p) in [
            ("a", "codex"),
            ("b", "claude"),
            ("c", "codex"),
            ("d", "codex"),
        ] {
            store.put_session(&row(k, p)).unwrap();
        }
        store.put_session(&row("a", "codex")).unwrap();
        store.end_session("c", 5, "process-exited", None).unwrap();
        let got: Vec<_> = store
            .sessions()
            .unwrap()
            .into_iter()
            .map(|s| (s.key, s.handle, s.repo))
            .collect();
        let h = |k: &str, n: &str| (k.to_string(), Some(n.to_string()), Some("r".to_string()));
        assert_eq!(
            got,
            [h("a", "codex-1"), h("b", "claude-1"), h("d", "codex-3")]
        );
        // An ended row still holds its number; a new one takes the next.
        store.put_session(&row("e", "codex")).unwrap();
        let e = store.session("e").unwrap().unwrap();
        assert_eq!(e.handle.as_deref(), Some("codex-4"));
    }

    #[test]
    fn a_hidden_mark_says_when_it_changed_and_goes_a_day_after_its_last_renewal() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let none = std::collections::HashSet::new();
        assert!(store.set_hidden("k", true, 10).unwrap());
        assert!(!store.set_hidden("k", true, 20).unwrap());
        store.prune_hidden(20, &none).unwrap();
        assert!(store.hidden_keys().unwrap().contains("k"));
        // A live session's mark stays however old.
        store.prune_hidden(21, &["k"].into()).unwrap();
        assert!(store.hidden_keys().unwrap().contains("k"));
        store.prune_hidden(21, &none).unwrap();
        assert!(store.hidden_keys().unwrap().is_empty());
        assert!(!store.set_hidden("k", false, 30).unwrap());
    }

    #[test]
    fn newer_db_refuses_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 17;")
            .unwrap();
        let err = Store::open(&path).err().unwrap().to_string();
        assert!(err.contains("State is from a newer version (v17)"), "{err}");
        assert_eq!(user_version(&path), 17);
    }
}
