//! Messages, the mailbox, receipts and the loop limit. Time is passed in (`now`, unix seconds).

use crate::sessions::{self, Key};
use crate::store::{Message, Store};
use crate::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const LOOP_LIMIT: u32 = 12;
/// Half-open: a send counts while `sent_at > now - LOOP_WINDOW_S`, so one sent at T stops counting at T + 600.
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

    fn parse(s: &str) -> Result<State> {
        Ok(match s {
            "queued" => State::Queued,
            "stored" => State::Stored,
            "delivered" => State::Delivered,
            "read" => State::Read,
            "held" => State::Held,
            "refused" => State::Refused,
            "expired" => State::Expired,
            _ => return Err(format!("unknown receipt state {s:?}").into()),
        })
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
    from: &str,
    request_id: &str,
    state: State,
    reason: Option<&str>,
    now: u64,
) -> Result<()> {
    let old = conn
        .query_row(
            "SELECT state FROM receipts WHERE from_key = ?1 AND request_id = ?2",
            [from, request_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    if match old {
        Some(o) => State::parse(&o)?.rank() < state.rank(),
        None => true,
    } {
        if state == State::Read {
            // `updated_at` stays the delivery time: the hops trigger window runs on it.
            conn.execute(
                "UPDATE receipts SET state = 'read' WHERE from_key = ?1 AND request_id = ?2",
                [from, request_id],
            )?;
        } else {
            conn.execute(
                "INSERT OR REPLACE INTO receipts VALUES (?1, ?2, ?3, ?4, ?5)",
                params![from, request_id, state.name(), reason, now as i64],
            )?;
        }
    }
    Ok(())
}

/// The stored receipt for `from`'s `request_id`, with `where_` worked out from the recipient's liveness now.
pub fn receipt(store: &Store, from: &Key, request_id: &str) -> Result<Option<Receipt>> {
    let conn = store.conn();
    let row = conn
        .query_row(
            "SELECT state, reason FROM receipts WHERE from_key = ?1 AND request_id = ?2",
            [&from.0, request_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let Some((state, reason)) = row else {
        return Ok(None);
    };
    let state = State::parse(&state)?;
    let where_ = if state == State::Stored {
        let to: Option<String> = conn
            .query_row(
                "SELECT to_key FROM messages WHERE from_key = ?1 AND request_id = ?2",
                [&from.0, request_id],
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

fn refuse(
    conn: &Connection,
    from: &Key,
    request_id: &str,
    reason: &str,
    now: u64,
) -> Result<Receipt> {
    advance(conn, &from.0, request_id, State::Refused, Some(reason), now)?;
    Ok(Receipt {
        request_id: request_id.into(),
        state: State::Refused,
        reason: Some(reason.into()),
        where_: None,
    })
}

/// Stores `text` for `to`. Thread and hops: with `reply_to` (a message id or request id the store
/// knows) the message takes that message's thread and its hops + 1. Without it, the trigger is the
/// message delivered (or read) TO `from` most recently, if that was within `LOOP_WINDOW_S`: hops = trigger.hops + 1, and
/// the thread is the trigger's if `to` is the trigger's sender, else a new one. No trigger: hops 0,
/// new thread. (Forwarding to a third session therefore cannot reset the agent-chain count.)
/// A `request_id` must match `[A-Za-z0-9._-]{1,128}`, else `Refused` (`bad_request_id`, nothing stored).
/// Retrying a `request_id` that was accepted returns its receipt; one that was
/// refused (`loop`, `too_long`) is refused again, whatever text comes with the retry, because a
/// refusal stores no `to`/`text`. Clients mint a new `request_id` after any refusal.
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
    let ok_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    if request_id.is_empty()
        || request_id.len() > MAX_REQUEST_ID
        || !request_id.chars().all(ok_char)
    {
        // Nothing safe to key a receipt on.
        return Ok(Receipt {
            request_id: request_id.into(),
            state: State::Refused,
            reason: Some("bad_request_id".into()),
            where_: None,
        });
    }
    if let Some((old_to, old_text)) = conn
        .query_row(
            "SELECT to_key, body FROM messages WHERE from_key = ?1 AND request_id = ?2",
            [&from.0, request_id],
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
        return Ok(receipt(store, from, request_id)?.ok_or("message without a receipt")?);
    }
    if let Some(r) = receipt(store, from, request_id)? {
        return Ok(r); // an earlier refusal; no message row to compare
    }
    if text.len() > MAX_TEXT {
        return refuse(conn, from, request_id, "too_long", now);
    }
    let since = now.saturating_sub(LOOP_WINDOW_S) as i64;
    let recent: u32 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE from_key = ?1 AND to_key = ?2 AND sent_at > ?3",
        params![from.0, to.0, since],
        |r| r.get(0),
    )?;
    if recent >= LOOP_LIMIT {
        return refuse(conn, from, request_id, "loop", now);
    }

    let id = ulid::Ulid::generate().to_string();
    let parent: Option<(String, Option<String>, i64)> = match reply_to {
        Some(r) => conn
            .query_row(
                "SELECT id, thread, hops FROM messages WHERE id = ?1 OR (request_id = ?1 AND to_key = ?2) \
                 ORDER BY id = ?1 DESC, rowid DESC LIMIT 1",
                params![r, from.0],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)),
            )
            .optional()?,
        None => None,
    };
    let (thread, hops) = match parent {
        Some((pid, thread, hops)) => (thread.unwrap_or(pid), hops + 1),
        None => {
            // The trigger: the newest message delivered to `from` in the window. Hops carry on
            // from it even when the work is forwarded to a third session.
            let trigger: Option<(String, Option<String>, i64, String)> = conn
                .query_row(
                    "SELECT m.id, m.thread, m.hops, m.from_key FROM messages m \
                     JOIN receipts r ON r.from_key = m.from_key AND r.request_id = m.request_id \
                     WHERE m.to_key = ?1 AND r.state IN ('delivered', 'read') AND r.updated_at > ?2 \
                     ORDER BY r.updated_at DESC, m.rowid DESC LIMIT 1",
                    params![from.0, since],
                    |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)),
                )
                .optional()?;
            match trigger {
                Some((tid, thread, hops, sender)) if sender == to.0 => {
                    (thread.unwrap_or(tid), hops + 1)
                }
                Some((_, _, hops, _)) => (id.clone(), hops + 1),
                None => (id.clone(), 0),
            }
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
    advance(&tx, &from.0, request_id, State::Stored, None, now)?;
    tx.commit()?;
    Ok(receipt(store, from, request_id)?.ok_or("receipt missing after write")?)
}

/// The messages waiting for `to`, oldest first; each becomes `delivered` and leaves the mailbox.
pub fn take(store: &Store, to: &Key, now: u64) -> Result<Vec<Message>> {
    take_where(store, to, None, now)
}

/// `take` for the one message `message_id`; `None` when it no longer waits.
pub fn take_one(store: &Store, to: &Key, message_id: &str, now: u64) -> Result<Option<Message>> {
    Ok(take_where(store, to, Some(message_id), now)?.pop())
}

/// The messages waiting for `to`, oldest first, left where they are.
pub fn waiting(store: &Store, to: &Key, now: u64) -> Result<Vec<Message>> {
    let mut stmt = store.conn().prepare(
        "SELECT message_id FROM mailbox WHERE to_key = ?1 AND expires_at >= ?2 ORDER BY id",
    )?;
    let ids = stmt.query_map(params![to.0, now as i64], |r| r.get::<_, String>(0))?;
    let mut out = vec![];
    for id in ids {
        out.extend(store.message(&id?)?);
    }
    Ok(out)
}

fn take_where(store: &Store, to: &Key, only: Option<&str>, now: u64) -> Result<Vec<Message>> {
    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    let rows: Vec<(i64, String)> = {
        let mut stmt = tx.prepare(
            "SELECT id, message_id FROM mailbox WHERE to_key = ?1 AND expires_at >= ?2 \
             AND (?3 IS NULL OR message_id = ?3) ORDER BY id",
        )?;
        let rows = stmt.query_map(params![to.0, now as i64, only], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut out = vec![];
    for (row_id, message_id) in rows {
        if let Some(m) = store.message(&message_id)? {
            advance(&tx, &m.from_key, &m.request_id, State::Delivered, None, now)?;
            out.push(m);
        }
        tx.execute("DELETE FROM mailbox WHERE id = ?1", [row_id])?;
    }
    tx.commit()?;
    Ok(out)
}

/// How many messages wait for `to`, without taking them.
pub fn unread(store: &Store, to: &Key, now: u64) -> Result<u64> {
    let n: i64 = store.conn().query_row(
        "SELECT COUNT(*) FROM mailbox WHERE to_key = ?1 AND expires_at >= ?2",
        params![to.0, now as i64],
        |r| r.get(0),
    )?;
    Ok(n as u64)
}

pub fn mark_read(store: &Store, message_id: &str, now: u64) -> Result<()> {
    if let Some(m) = store.message(message_id)? {
        advance(
            store.conn(),
            &m.from_key,
            &m.request_id,
            State::Read,
            None,
            now,
        )?;
    }
    Ok(())
}

/// Mail that waited past `HOLD_S` becomes `expired` and leaves the mailbox.
pub fn expire(store: &Store, now: u64) -> Result<()> {
    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    let due: Vec<(String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT m.from_key, m.request_id FROM mailbox b JOIN messages m ON m.id = b.message_id WHERE b.expires_at < ?1",
        )?;
        let rows = stmt.query_map([now as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (from, request_id) in due {
        advance(&tx, &from, &request_id, State::Expired, None, now)?;
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
                executable: None,
            })
            .unwrap();
    }

    fn k(s: &str) -> Key {
        Key(s.into())
    }

    fn state(store: &Store, rid: &str) -> State {
        let from: String = store
            .conn()
            .query_row(
                "SELECT from_key FROM receipts WHERE request_id = ?1",
                [rid],
                |r| r.get(0),
            )
            .unwrap();
        receipt(store, &Key(from), rid).unwrap().unwrap().state
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
        advance(s.conn(), "m/codex:a", "r1", State::Stored, None, 104).unwrap();
        assert_eq!(state(&s, "r1"), State::Read);
    }

    #[test]
    fn thirteenth_send_in_ten_minutes_is_refused_with_loop() {
        let (_d, s) = setup();
        let t = 1000;
        for i in 0..12 {
            let r = send(&s, &k("a"), &k("b"), "x", &format!("r{i}"), None, t).unwrap();
            assert_eq!(r.state, State::Stored);
        }
        let r = send(&s, &k("a"), &k("b"), "x", "r12", None, t + 599).unwrap();
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (State::Refused, Some("loop"))
        );
        assert_eq!(state(&s, "r12"), State::Refused);
        let r = send(&s, &k("a"), &k("b"), "x", "r13", None, t + 600).unwrap();
        assert_eq!(r.state, State::Stored);
    }

    #[test]
    fn refused_sends_do_not_count_toward_the_window() {
        let (_d, s) = setup();
        let big = "x".repeat(MAX_TEXT + 1);
        // 11 accepted, then many refusals: none may push the 12th/13th over early.
        for i in 0..11 {
            send(&s, &k("a"), &k("b"), "x", &format!("r{i}"), None, 1000).unwrap();
        }
        for i in 0..5 {
            let r = send(&s, &k("a"), &k("b"), &big, &format!("big{i}"), None, 1001).unwrap();
            assert_eq!(r.reason.as_deref(), Some("too_long"));
        }
        assert_eq!(
            send(&s, &k("a"), &k("b"), "x", "r11", None, 1002)
                .unwrap()
                .state,
            State::Stored
        );
        for i in 0..5 {
            let r = send(&s, &k("a"), &k("b"), "x", &format!("loop{i}"), None, 1003).unwrap();
            assert_eq!(r.reason.as_deref(), Some("loop"));
        }
        // Only the 12 accepted count: at 1600 they are all outside the window.
        assert_eq!(
            send(&s, &k("a"), &k("b"), "x", "after", None, 1600)
                .unwrap()
                .state,
            State::Stored
        );
    }

    #[test]
    fn a_request_id_with_other_characters_is_refused() {
        let (_d, s) = setup();
        for rid in ["a\"b", "a\nb", "a b", "é"] {
            let r = send(&s, &k("a"), &k("b"), "x", rid, None, 1).unwrap();
            assert_eq!(
                (r.state, r.reason.as_deref()),
                (State::Refused, Some("bad_request_id")),
                "{rid:?}"
            );
            assert!(receipt(&s, &k("a"), rid).unwrap().is_none());
        }
        assert_eq!(
            send(&s, &k("a"), &k("b"), "x", "Ok.id_1-2", None, 1)
                .unwrap()
                .state,
            State::Stored
        );
    }

    #[test]
    fn a_refused_request_id_stays_refused_on_retry() {
        let (_d, s) = setup();
        let big = "x".repeat(MAX_TEXT + 1);
        send(&s, &k("a"), &k("b"), &big, "r1", None, 1).unwrap();
        let r = send(&s, &k("a"), &k("b"), "short now", "r1", None, 2).unwrap();
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (State::Refused, Some("too_long"))
        );
        assert_eq!(
            send(&s, &k("a"), &k("b"), "short now", "r2", None, 3)
                .unwrap()
                .state,
            State::Stored
        );
    }

    #[test]
    fn unknown_reply_to_falls_back_to_the_default_thread_rule() {
        let (_d, s) = setup();
        send(&s, &k("b"), &k("a"), "first", "r1", None, 1).unwrap();
        take(&s, &k("a"), 1).unwrap();
        let first: String = s
            .conn()
            .query_row(
                "SELECT thread FROM messages WHERE request_id='r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        send(&s, &k("a"), &k("b"), "re", "r2", Some("no-such-id"), 2).unwrap();
        let (t, h): (String, i64) = s
            .conn()
            .query_row(
                "SELECT thread, hops FROM messages WHERE request_id='r2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((t, h), (first, 1));
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
        // no reply_to: the trigger is the newest message delivered to a (r2, from b)
        take(&s, &a, 3).unwrap();
        send(&s, &a, &b, "follow", "r4", None, 4).unwrap();
        let (t, h): (String, i64) = s
            .conn()
            .query_row(
                "SELECT thread, hops FROM messages WHERE request_id='r4'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((t, h), (m1.1.clone(), 2));
        // trigger came from someone else: a fresh thread, hops still carry on
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
        assert_ne!(t, m1.1);
        let h: i64 = s
            .conn()
            .query_row("SELECT hops FROM messages WHERE request_id='r5'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(h, 2);
    }

    fn thread_hops(s: &Store, rid: &str) -> (String, String, i64) {
        s.conn()
            .query_row(
                "SELECT id, thread, hops FROM messages WHERE request_id = ?1",
                [rid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    #[test]
    fn forwarding_without_reply_to_keeps_hop_ancestry() {
        let (_d, s) = setup();
        send(&s, &k("a"), &k("b"), "do it", "r1", None, 1).unwrap();
        take(&s, &k("b"), 2).unwrap();
        send(&s, &k("b"), &k("c"), "you do it", "r2", None, 3).unwrap();
        let (id, thread, hops) = thread_hops(&s, "r2");
        assert_eq!((hops, thread == id), (1, true));
        assert_ne!(thread, thread_hops(&s, "r1").1);
    }

    #[test]
    fn a_reply_without_reply_to_to_the_trigger_sender_keeps_its_thread() {
        let (_d, s) = setup();
        send(&s, &k("a"), &k("b"), "do it", "r1", None, 1).unwrap();
        take(&s, &k("b"), 2).unwrap();
        send(&s, &k("b"), &k("a"), "done", "r2", None, 3).unwrap();
        let (_, thread, hops) = thread_hops(&s, "r2");
        assert_eq!((thread, hops), (thread_hops(&s, "r1").1, 1));
    }

    #[test]
    fn an_old_delivery_is_no_trigger() {
        let (_d, s) = setup();
        send(&s, &k("a"), &k("b"), "x", "r1", None, 1).unwrap();
        take(&s, &k("b"), 100).unwrap();
        mark_read(&s, &thread_hops(&s, "r1").0, 150).unwrap();
        send(
            &s,
            &k("b"),
            &k("c"),
            "y",
            "r2",
            None,
            100 + LOOP_WINDOW_S - 1,
        )
        .unwrap();
        assert_eq!(
            thread_hops(&s, "r2").2,
            1,
            "inside the window, even after read"
        );
        send(&s, &k("b"), &k("c"), "z", "r3", None, 100 + LOOP_WINDOW_S).unwrap();
        assert_eq!(thread_hops(&s, "r3").2, 0, "delivery 600 s ago is out");
    }

    #[test]
    fn a_message_taken_long_after_sending_is_still_a_trigger() {
        let (_d, s) = setup();
        send(&s, &k("a"), &k("b"), "x", "r1", None, 1).unwrap();
        take(&s, &k("b"), 1000).unwrap();
        send(&s, &k("b"), &k("c"), "y", "r2", None, 1500).unwrap();
        assert_eq!(thread_hops(&s, "r2").2, 1);
    }

    #[test]
    fn two_senders_may_reuse_the_same_request_id() {
        let (_d, s) = setup();
        let r1 = send(&s, &k("a"), &k("c"), "from a", "req-1", None, 1).unwrap();
        let r2 = send(&s, &k("b"), &k("c"), "from b", "req-1", None, 1).unwrap();
        assert_eq!((r1.state, r2.state), (State::Stored, State::Stored));
        // each sender's retry is idempotent within itself; a mismatch is only against its own
        assert_eq!(
            send(&s, &k("b"), &k("c"), "from b", "req-1", None, 2).unwrap(),
            r2
        );
        let r = send(&s, &k("a"), &k("c"), "from b", "req-1", None, 2).unwrap();
        assert_eq!(r.reason.as_deref(), Some("request_id_reused"));
        // receipts are separate: reading a's message leaves b's delivered
        take(&s, &k("c"), 3).unwrap();
        let a_id: String = s
            .conn()
            .query_row("SELECT id FROM messages WHERE from_key = 'a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        mark_read(&s, &a_id, 4).unwrap();
        let got = |f: &str| receipt(&s, &k(f), "req-1").unwrap().unwrap().state;
        assert_eq!((got("a"), got("b")), (State::Read, State::Delivered));
        assert!(receipt(&s, &k("c"), "req-1").unwrap().is_none());
    }
}
