//! Messages, the mailbox, receipts and the loop limit. Time is passed in (`now`, unix seconds).

use crate::sessions::{self, Key};
use crate::store::{Message, Store};
use crate::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const LOOP_LIMIT: u32 = 12;
pub const LOOP_WINDOW_S: u64 = 600;
/// Offline mail (and, later, the outbox) waits this long.
pub const HOLD_S: u64 = 86_400;
/// A held message waits this long for the person (nothing holds in Plan A).
pub const HELD_S: u64 = 604_800;
pub const MAX_TEXT: usize = 65_536;
pub const MAX_REQUEST_ID: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Queued,
    Stored,
    Delivered,
    Read,
    Held,
    Refused,
    Expired,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Stored => "stored",
            State::Delivered => "delivered",
            State::Read => "read",
            State::Held => "held",
            State::Refused => "refused",
            State::Expired => "expired",
        }
    }

    fn parse(s: &str) -> State {
        match s {
            "queued" => State::Queued,
            "stored" => State::Stored,
            "delivered" => State::Delivered,
            "read" => State::Read,
            "held" => State::Held,
            "expired" => State::Expired,
            _ => State::Refused,
        }
    }

    /// Receipts only move to a higher rank.
    fn rank(self) -> u8 {
        match self {
            State::Queued => 0,
            State::Held => 1,
            State::Stored => 2,
            State::Delivered => 3,
            State::Read => 4,
            State::Refused | State::Expired => 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub request_id: String,
    pub state: State,
    pub reason: Option<String>,
    /// `"recipient"` (live session) or `"mailbox"` (not live); only while `stored`.
    #[serde(rename = "where")]
    pub where_: Option<String>,
}

/// Live means a registered row whose process is the registered one.
/// ponytail: discovered-only sessions (never registered) count as not live here; delivery is unaffected.
fn live(store: &Store, key: &str) -> bool {
    store
        .session(key)
        .ok()
        .flatten()
        .and_then(|s| {
            Some((
                u32::try_from(s.pid?).ok()?,
                u64::try_from(s.birth_micros?).ok()?,
            ))
        })
        .is_some_and(|(pid, birth)| sessions::alive(pid, birth))
}

/// Writes `state` only if it is later than the stored one.
fn advance(
    conn: &Connection,
    request_id: &str,
    state: State,
    reason: Option<&str>,
    now: u64,
) -> Result<()> {
    let old = conn
        .query_row(
            "SELECT state FROM receipts WHERE request_id = ?1",
            [request_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if old.is_none_or(|o| State::parse(&o).rank() < state.rank()) {
        conn.execute(
            "INSERT OR REPLACE INTO receipts VALUES (?1, ?2, ?3, ?4)",
            params![request_id, state.name(), reason, now as i64],
        )?;
    }
    Ok(())
}

/// The stored receipt for `request_id`, with `where_` worked out from the recipient's liveness now.
pub fn receipt(store: &Store, request_id: &str) -> Result<Option<Receipt>> {
    let conn = store.conn();
    let row = conn
        .query_row(
            "SELECT state, reason FROM receipts WHERE request_id = ?1",
            [request_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let Some((state, reason)) = row else {
        return Ok(None);
    };
    let state = State::parse(&state);
    let where_ = if state == State::Stored {
        let to: Option<String> = conn
            .query_row(
                "SELECT to_key FROM messages WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        to.map(|to| {
            if live(store, &to) {
                "recipient"
            } else {
                "mailbox"
            }
            .to_string()
        })
    } else {
        None
    };
    Ok(Some(Receipt {
        request_id: request_id.into(),
        state,
        reason,
        where_,
    }))
}

fn refuse(conn: &Connection, request_id: &str, reason: &str, now: u64) -> Result<Receipt> {
    advance(conn, request_id, State::Refused, Some(reason), now)?;
    Ok(Receipt {
        request_id: request_id.into(),
        state: State::Refused,
        reason: Some(reason.into()),
        where_: None,
    })
}

pub fn send(
    store: &Store,
    from: &Key,
    to: &Key,
    text: &str,
    request_id: &str,
    reply_to: Option<&str>,
    now: u64,
) -> Result<Receipt> {
    let conn = store.conn();
    if request_id.is_empty() || request_id.len() > MAX_REQUEST_ID {
        // Nothing to key a receipt on.
        return Ok(Receipt {
            request_id: request_id.into(),
            state: State::Refused,
            reason: Some("bad_request_id".into()),
            where_: None,
        });
    }
    if let Some((old_to, old_text)) = conn
        .query_row(
            "SELECT to_key, body FROM messages WHERE request_id = ?1",
            [request_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if old_to != to.0 || old_text != text {
            // Not persisted: the original's receipt must stay as it was.
            return Ok(Receipt {
                request_id: request_id.into(),
                state: State::Refused,
                reason: Some("request_id_reused".into()),
                where_: None,
            });
        }
        return Ok(receipt(store, request_id)?.expect("a message always has a receipt"));
    }
    if let Some(r) = receipt(store, request_id)? {
        return Ok(r); // an earlier refusal; no message row to compare
    }
    if text.len() > MAX_TEXT {
        return refuse(conn, request_id, "too_long", now);
    }
    let since = now.saturating_sub(LOOP_WINDOW_S) as i64;
    let recent: u32 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE from_key = ?1 AND to_key = ?2 AND sent_at > ?3",
        params![from.0, to.0, since],
        |r| r.get(0),
    )?;
    if recent >= LOOP_LIMIT {
        return refuse(conn, request_id, "loop", now);
    }

    let id = ulid::Ulid::generate().to_string();
    let parent: Option<(String, Option<String>, i64)> = match reply_to {
        Some(r) => conn
            .query_row(
                "SELECT id, thread, hops FROM messages WHERE id = ?1 OR request_id = ?1",
                [r],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)),
            )
            .optional()?,
        None => None,
    };
    let (thread, hops) = match parent {
        Some((pid, thread, hops)) => (thread.unwrap_or(pid), hops + 1),
        None => {
            let latest: Option<(String, Option<String>)> = conn
                .query_row(
                    "SELECT id, thread FROM messages WHERE from_key = ?1 AND to_key = ?2 ORDER BY rowid DESC LIMIT 1",
                    params![to.0, from.0],
                    |x| Ok((x.get(0)?, x.get(1)?)),
                )
                .optional()?;
            (
                latest.map_or_else(|| id.clone(), |(lid, t)| t.unwrap_or(lid)),
                0,
            )
        }
    };
    let tx = conn.unchecked_transaction()?;
    store.put_message(&Message {
        id: id.clone(),
        request_id: request_id.into(),
        from_key: from.0.clone(),
        to_key: to.0.clone(),
        kind: "message".into(),
        thread: Some(thread),
        hops,
        origin: Some("local".into()),
        body: text.into(),
        sent_at: now as i64,
    })?;
    tx.execute(
        "INSERT INTO mailbox (to_key, message_id, queued_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![to.0, id, now as i64, (now + HOLD_S) as i64],
    )?;
    advance(&tx, request_id, State::Stored, None, now)?;
    tx.commit()?;
    Ok(receipt(store, request_id)?.expect("just written"))
}

/// The messages waiting for `to`, oldest first; each becomes `delivered` and leaves the mailbox.
pub fn take(store: &Store, to: &Key, now: u64) -> Result<Vec<Message>> {
    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    let rows: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(
            "SELECT id, message_id FROM mailbox WHERE to_key = ?1 AND expires_at >= ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![to.0, now as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = vec![];
    for (row_id, message_id) in rows {
        if let Some(m) = store.message(&message_id)? {
            advance(&tx, &m.request_id, State::Delivered, None, now)?;
            out.push(m);
        }
        tx.execute("DELETE FROM mailbox WHERE id = ?1", [row_id])?;
    }
    tx.commit()?;
    Ok(out)
}

pub fn mark_read(store: &Store, message_id: &str, now: u64) -> Result<()> {
    if let Some(m) = store.message(message_id)? {
        advance(store.conn(), &m.request_id, State::Read, None, now)?;
    }
    Ok(())
}

/// Mail that waited past `HOLD_S` becomes `expired` and leaves the mailbox.
pub fn expire(store: &Store, now: u64) -> Result<()> {
    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    let due: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT m.request_id FROM mailbox b JOIN messages m ON m.id = b.message_id WHERE b.expires_at < ?1",
        )?;
        let rows = stmt.query_map([now as i64], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for request_id in due {
        advance(&tx, &request_id, State::Expired, None, now)?;
    }
    tx.execute("DELETE FROM mailbox WHERE expires_at < ?1", [now as i64])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{os, store::Session};

    fn setup() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        (dir, store)
    }

    /// A registered session; `alive` picks this process's real birth time, else a wrong one.
    fn register(store: &Store, key: &str, alive: bool) {
        let birth = os::process(std::process::id()).unwrap().birth_micros;
        store
            .put_session(&Session {
                key: key.into(),
                provider: "codex".into(),
                conversation: Some("c".into()),
                pid: Some(std::process::id().into()),
                birth_micros: Some(if alive { birth as i64 } else { 1 }),
                cwd: "/".into(),
                title: None,
                status: "idle".into(),
                seen_at: 0,
            })
            .unwrap();
    }

    fn k(s: &str) -> Key {
        Key(s.into())
    }

    fn state(store: &Store, rid: &str) -> State {
        receipt(store, rid).unwrap().unwrap().state
    }

    #[test]
    fn send_stores_then_take_delivers_then_mark_read() {
        let (_d, s) = setup();
        register(&s, "m/codex:b", true);
        let r = send(&s, &k("m/codex:a"), &k("m/codex:b"), "hi", "r1", None, 100).unwrap();
        assert_eq!(
            (r.state, r.where_.as_deref()),
            (State::Stored, Some("recipient"))
        );
        let got = take(&s, &k("m/codex:b"), 101).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].body.as_str(), got[0].origin.as_deref()),
            ("hi", Some("local"))
        );
        assert_eq!(state(&s, "r1"), State::Delivered);
        assert!(take(&s, &k("m/codex:b"), 102).unwrap().is_empty());
        mark_read(&s, &got[0].id, 103).unwrap();
        assert_eq!(state(&s, "r1"), State::Read);
        // never backwards
        advance(s.conn(), "r1", State::Stored, None, 104).unwrap();
        assert_eq!(state(&s, "r1"), State::Read);
    }

    #[test]
    fn thirteenth_send_in_ten_minutes_is_refused_with_loop() {
        let (_d, s) = setup();
        for i in 0..12 {
            let r = send(&s, &k("a"), &k("b"), "x", &format!("r{i}"), None, 1000 + i).unwrap();
            assert_eq!(r.state, State::Stored);
        }
        let r = send(&s, &k("a"), &k("b"), "x", "r12", None, 1100).unwrap();
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (State::Refused, Some("loop"))
        );
        assert_eq!(state(&s, "r12"), State::Refused);
        // refused sends do not count toward the window
        let r = send(&s, &k("a"), &k("b"), "x", "r13", None, 1000 + 600 + 11).unwrap();
        assert_eq!(r.state, State::Stored);
    }

    #[test]
    fn mail_for_a_gone_session_expires_after_24h() {
        let (_d, s) = setup();
        register(&s, "b", false);
        let r = send(&s, &k("a"), &k("b"), "x", "r1", None, 1000).unwrap();
        assert_eq!(
            (r.state, r.where_.as_deref()),
            (State::Stored, Some("mailbox"))
        );
        expire(&s, 1000 + HOLD_S).unwrap();
        assert_eq!(state(&s, "r1"), State::Stored);
        expire(&s, 1000 + HOLD_S + 1).unwrap();
        assert_eq!(state(&s, "r1"), State::Expired);
        assert!(take(&s, &k("b"), 1000 + HOLD_S + 2).unwrap().is_empty());
    }

    #[test]
    fn resumed_session_gets_the_mail_that_waited() {
        let (_d, s) = setup();
        register(&s, "b", false);
        send(&s, &k("a"), &k("b"), "wait for me", "r1", None, 1000).unwrap();
        register(&s, "b", true);
        let got = take(&s, &k("b"), 1000 + HOLD_S - 1).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "wait for me");
    }

    #[test]
    fn same_request_id_returns_same_receipt_and_mismatch_is_refused() {
        let (_d, s) = setup();
        let first = send(&s, &k("a"), &k("b"), "x", "r1", None, 1).unwrap();
        assert_eq!(
            send(&s, &k("a"), &k("b"), "x", "r1", None, 2).unwrap(),
            first
        );
        for (to, text) in [("c", "x"), ("b", "y")] {
            let r = send(&s, &k("a"), &k(to), text, "r1", None, 3).unwrap();
            assert_eq!(
                (r.state, r.reason.as_deref()),
                (State::Refused, Some("request_id_reused"))
            );
        }
        assert_eq!(state(&s, "r1"), State::Stored);
        let big = "x".repeat(MAX_TEXT + 1);
        let r = send(&s, &k("a"), &k("b"), &big, "r2", None, 4).unwrap();
        assert_eq!(r.reason.as_deref(), Some("too_long"));
        assert!(
            send(&s, &k("a"), &k("b"), "x", &"r".repeat(129), None, 4)
                .unwrap()
                .state
                == State::Refused
        );
    }

    #[test]
    fn reply_sets_thread_from_the_message_it_answers() {
        let (_d, s) = setup();
        let (a, b) = (k("a"), k("b"));
        send(&s, &a, &b, "q", "r1", None, 1).unwrap();
        let m1 = s
            .conn()
            .query_row(
                "SELECT id, thread FROM messages WHERE request_id='r1'",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .unwrap();
        assert_eq!(m1.0, m1.1, "first message opens its own thread");
        // b answers by request id: same thread, one hop deeper
        send(&s, &b, &a, "ans", "r2", Some("r1"), 2).unwrap();
        // c answers by message id
        send(&s, &a, &b, "more", "r3", Some(&m1.0), 3).unwrap();
        let rows: Vec<(String, i64)> = ["r2", "r3"]
            .iter()
            .map(|rid| {
                s.conn()
                    .query_row(
                        "SELECT thread, hops FROM messages WHERE request_id=?1",
                        [rid],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .unwrap()
            })
            .collect();
        assert_eq!(rows, vec![(m1.1.clone(), 1), (m1.1.clone(), 1)]);
        // no reply_to: thread of the newest inbound from the target (r2 -> a is from b)
        send(&s, &a, &b, "follow", "r4", None, 4).unwrap();
        let (t, h): (String, i64) = s
            .conn()
            .query_row(
                "SELECT thread, hops FROM messages WHERE request_id='r4'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((t, h), (m1.1, 0));
        // nothing inbound: a fresh thread
        send(&s, &a, &k("z"), "hello", "r5", None, 5).unwrap();
        let (id, t): (String, String) = s
            .conn()
            .query_row(
                "SELECT id, thread FROM messages WHERE request_id='r5'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(id, t);
    }
}
