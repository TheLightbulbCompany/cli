use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// Migration `i` takes the db to `user_version` `i + 1`. Forward only; each one only adds
/// tables or columns, and runs once (in the transaction that moves `user_version`).
const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5];

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

/// A session's end: its row stays a day (`presence`'s `kept`) with `ended_at` set.
const V3: &str = "ALTER TABLE sessions ADD COLUMN ended_at INTEGER;";

/// The text a hold's dialog showed, which its `approvals` record signs.
const V4: &str = "ALTER TABLE holds ADD COLUMN shown TEXT;";
/// Which loop limit a `loop` refusal hit (`mail::HOPS`, `mail::RATE`), when known.
const V5: &str = "ALTER TABLE receipts ADD COLUMN \"limit\" TEXT;";

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
        body: r.get("body")?,
        sent_at: r.get("sent_at")?,
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
                "State is from a newer version (v{version}). Install a newer lightbulb-connect than {}.",
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

    /// Writes `s` as a live row (an ended one is live again).
    pub fn put_session(&self, s: &Session) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (key, provider, conversation, pid, birth_micros, cwd, \
             title, status, seen_at, executable) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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

    /// The rows that have not ended.
    pub fn sessions(&self) -> rusqlite::Result<Vec<Session>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM sessions WHERE ended_at IS NULL")?;
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

    /// Ends `key`'s row at `now`; `false` when there is no live one.
    pub fn end_session(&self, key: &str, now: i64) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE sessions SET ended_at = ?2 WHERE key = ?1 AND ended_at IS NULL",
            params![key, now],
        )? > 0)
    }

    /// The rows that ended at `since` or later, `(key, ended_at, cwd)`; deletes older ends.
    pub fn ended_since(&self, since: i64) -> rusqlite::Result<Vec<(String, i64, String)>> {
        self.conn
            .execute("DELETE FROM sessions WHERE ended_at < ?1", [since])?;
        let mut stmt = self
            .conn
            .prepare("SELECT key, ended_at, cwd FROM sessions WHERE ended_at IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
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
            "INSERT INTO join_requests (ticket_id, inviter, candidate, name, computer, kind,
             created_at, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                j.ticket_id,
                j.inviter,
                j.candidate,
                j.name,
                j.computer,
                j.kind,
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
            "SELECT id, ticket_id, inviter, candidate, name, computer, kind, created_at
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
                kind: r.get(6)?,
                created_at: r.get(7)?,
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
    /// `from`'s message `request_id` as stored: `(to, body)`.
    pub fn sent_as(
        &self,
        from: &str,
        request_id: &str,
    ) -> rusqlite::Result<Option<(String, String)>> {
        self.conn
            .query_row(
                "SELECT to_key, body FROM messages WHERE from_key = ?1 AND request_id = ?2",
                [from, request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
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

    /// The frames for `members` due at `now`: `(row id, member, frame)`. A message's while still
    /// `queued`; a row that is no message (a hold notice or decision, `decision`) while
    /// created after `since`.
    pub fn outbox_due(
        &self,
        members: &[String],
        now: i64,
        since: i64,
    ) -> rusqlite::Result<Vec<(String, String, String)>> {
        if members.is_empty() {
            return Ok(vec![]);
        }
        let marks = vec!["?"; members.len()].join(", ");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT o.message_id, o.to_member, o.frame FROM outbox o \
             LEFT JOIN messages m ON m.id = o.message_id LEFT {MSG_RECEIPT} \
             WHERE (r.state = 'queued' OR (m.id IS NULL AND o.created_at > ?)) \
             AND o.next_at <= ? AND o.to_member IN ({marks})"
        ))?;
        let args = [&since as &dyn rusqlite::ToSql, &now]
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
    pub kind: String,
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
        assert!(store.end_session("k", 9).unwrap());
        assert!(!store.end_session("k", 9).unwrap());
        assert_eq!(store.session("k").unwrap(), None);
        assert!(store.holds("k").unwrap());
        assert_eq!(
            store.ended_since(9).unwrap(),
            [("k".into(), 9, "/p".into())]
        );
        assert!(store.ended_since(10).unwrap().is_empty());
        assert!(!store.holds("k").unwrap());
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
