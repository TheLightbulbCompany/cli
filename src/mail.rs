//! Messages, the mailbox, receipts and the loop limit. Time is passed in (`now`, unix seconds).

use crate::net::frames::Frame;
use crate::policy::{self, Verdict};
use crate::roster::{Member, MemberKind, Roster};
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
/// A message `MAX_HOPS` agent replies deep is refused `loop`: a person must speak.
pub const MAX_HOPS: i64 = 4;

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
    pub fn name(self) -> &'static str {
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

    pub fn parse(s: &str) -> Result<State> {
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
    pub fn rank(self) -> u8 {
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

/// Writes `state` only if it is later than the stored one, and tells the senders of `net:`
/// mail what changed (`tell_senders`).
pub(crate) fn advance(
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
        tell_senders(conn)?;
    }
    Ok(())
}

/// Queues a `Receipt` (`receipt_out`) for each message from a peer whose receipt moved past
/// what its sender was told (`seen_ids.state`), and notes it told them.
// ponytail: scans every `seen_ids` row (7 days of mail from peers) per state change; key it by
// message if that grows.
fn tell_senders(conn: &Connection) -> Result<()> {
    let rows: Vec<(String, String, String, Option<String>, String)> = {
        let mut stmt = conn.prepare(
            "SELECT s.sender, s.envelope_id, r.state, r.reason, s.state FROM seen_ids s \
             JOIN messages m ON m.id = s.envelope_id \
             JOIN receipts r ON r.from_key = m.from_key AND r.request_id = m.request_id \
             WHERE r.state != s.state",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (sender, id, state, reason, told) in rows {
        if State::parse(&state)?.rank() > State::parse(&told)?.rank() {
            conn.execute(
                "INSERT OR REPLACE INTO receipt_out VALUES (?1, ?2, ?3, ?4)",
                params![sender, id, state, reason],
            )?;
            conn.execute(
                "UPDATE seen_ids SET state = ?3 WHERE sender = ?1 AND envelope_id = ?2",
                params![sender, id, state],
            )?;
        }
    }
    Ok(())
}

/// Who `from`'s message `request_id` was stored for; `None` when it stored nothing.
pub fn recipient(store: &Store, from: &Key, request_id: &str) -> Result<Option<String>> {
    Ok(store
        .conn()
        .query_row(
            "SELECT to_key FROM messages WHERE from_key = ?1 AND request_id = ?2",
            [&from.0, request_id],
            |r| r.get(0),
        )
        .optional()?)
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
        recipient(store, from, request_id)?.map(|to| {
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

/// A message decided and ready to store, here (`send`) or in the outbox (`outbox::enqueue`).
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub message_id: String,
    pub from: Key,
    pub to: Key,
    pub text: String,
    pub request_id: String,
    /// The message this answers (resolved from `reply_to`), by id.
    pub reply_to: Option<String>,
    pub thread: String,
    pub hops: i64,
    pub kind: String,
    pub sent_at: u64,
}

/// Decides `from`'s message `request_id` before anything is stored: `Err` with the receipt
/// when it is refused or already sent. With no `to`, it goes to the sender of the `reply_to`
/// message (refused `no_recipient`, nothing stored, when that is unknown). Thread and hops:
/// with `reply_to` (a message id or request id the store knows) the message takes that
/// message's thread and its hops + 1. Without it, the trigger is the message delivered (or
/// read) TO `from` most recently, if that was within `LOOP_WINDOW_S`: hops = trigger.hops + 1,
/// and the thread is the trigger's if `to` is the trigger's sender, else a new one. No trigger:
/// hops 0, new thread. (Forwarding to a third session therefore cannot reset the agent-chain
/// count.) A person at a `/terminal` is always hop 0; hops `MAX_HOPS` or more is refused `loop`.
/// A `request_id` must match `[A-Za-z0-9._-]{1,128}`, else `Refused` (`bad_request_id`, nothing
/// stored). Retrying a `request_id` that was accepted returns its receipt; one that was
/// refused (`loop`, `too_long`) is refused again, whatever text comes with the retry, because a
/// refusal stores no `to`/`text`. Clients mint a new `request_id` after any refusal.
pub fn prepare(
    store: &Store,
    from: &Key,
    to: Option<&Key>,
    text: &str,
    request_id: &str,
    reply_to: Option<&str>,
    now: u64,
) -> Result<std::result::Result<Prepared, Receipt>> {
    let conn = store.conn();
    let unstored = |reason: &str| {
        Ok(Err(Receipt {
            request_id: request_id.into(),
            state: State::Refused,
            reason: Some(reason.into()),
            where_: None,
        }))
    };
    if !sessions::word(request_id, MAX_REQUEST_ID) {
        return unstored("bad_request_id"); // nothing safe to key a receipt on
    }
    // The message this answers: a message id, or a request id `from` received.
    let parent: Option<(String, Option<String>, i64, String)> = match reply_to {
        Some(r) => conn
            .query_row(
                "SELECT id, thread, hops, from_key FROM messages WHERE id = ?1 OR (request_id = ?1 AND to_key = ?2) \
                 ORDER BY id = ?1 DESC, rowid DESC LIMIT 1",
                params![r, from.0],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?, x.get(3)?)),
            )
            .optional()?,
        None => None,
    };
    // No recipient named: the reply goes to whoever sent the parent.
    let to = match (to, &parent) {
        (Some(t), _) => t.clone(),
        (None, Some((_, _, _, sender))) => Key(sender.clone()),
        // Not persisted: a later message may make this reply_to known.
        (None, None) => return unstored("no_recipient"),
    };
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
            return unstored("request_id_reused");
        }
        return Ok(Err(
            receipt(store, from, request_id)?.ok_or("message without a receipt")?
        ));
    }
    if let Some(r) = receipt(store, from, request_id)? {
        return Ok(Err(r)); // an earlier refusal; no message row to compare
    }
    if text.len() > MAX_TEXT {
        return Ok(Err(refuse(conn, from, request_id, "too_long", now)?));
    }
    let since = now.saturating_sub(LOOP_WINDOW_S) as i64;
    let recent: u32 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE from_key = ?1 AND to_key = ?2 AND sent_at > ?3",
        params![from.0, to.0, since],
        |r| r.get(0),
    )?;
    if recent >= LOOP_LIMIT {
        return Ok(Err(refuse(conn, from, request_id, "loop", now)?));
    }

    let id = ulid::Ulid::generate().to_string();
    let (thread, hops) = match &parent {
        Some((pid, thread, hops, _)) => (thread.clone().unwrap_or(pid.clone()), hops + 1),
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
    let person = sessions::split_addr(&from.0).is_some_and(|(_, rest)| rest == "terminal");
    let hops = if person { 0 } else { hops };
    if hops >= MAX_HOPS {
        return Ok(Err(refuse(conn, from, request_id, "loop", now)?));
    }
    Ok(Ok(Prepared {
        message_id: id,
        from: from.clone(),
        to,
        text: text.into(),
        request_id: request_id.into(),
        reply_to: parent.map(|(pid, ..)| pid),
        thread,
        hops,
        kind: "message".into(),
        sent_at: now,
    }))
}

/// Writes `p` as `from`'s message with receipt `state`, in the mailbox until `expires` when
/// given; answering a message read it. The caller holds a transaction.
pub(crate) fn record(
    store: &Store,
    p: &Prepared,
    state: State,
    expires: Option<i64>,
) -> Result<()> {
    let tx = store.conn();
    store.put_message(&Message {
        id: p.message_id.clone(),
        request_id: p.request_id.clone(),
        from_key: p.from.0.clone(),
        to_key: p.to.0.clone(),
        kind: p.kind.clone(),
        thread: Some(p.thread.clone()),
        hops: p.hops,
        origin: Some("local".into()),
        body: p.text.clone(),
        sent_at: p.sent_at as i64,
    })?;
    if let Some(expires) = expires {
        tx.execute(
            "INSERT INTO mailbox (to_key, message_id, queued_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
            params![p.to.0, p.message_id, p.sent_at as i64, expires],
        )?;
    }
    advance(tx, &p.from.0, &p.request_id, state, None, p.sent_at)?;
    if let Some(pid) = &p.reply_to {
        if store.message(pid)?.is_some_and(|m| m.to_key == p.from.0) {
            mark_read(store, pid, p.sent_at)?;
        }
    }
    Ok(())
}

/// Stores `p` here, in its recipient's mailbox; its receipt.
pub fn store_local(store: &Store, p: &Prepared) -> Result<Receipt> {
    // This member's own name address (`<member>/<name>`, the `listen --as` character rule) is a
    // person's mailbox and never expires; any other non-session address keeps the hold.
    let own_name = match (p.from.0.split_once('/'), p.to.0.split_once('/')) {
        (Some((a, _)), Some((b, name))) => a == b && sessions::valid_name(name),
        _ => false,
    };
    let expires = if own_name {
        i64::MAX
    } else {
        (p.sent_at + HOLD_S) as i64
    };
    let tx = store.conn().unchecked_transaction()?;
    record(store, p, State::Stored, Some(expires))?;
    tx.commit()?;
    Ok(receipt(store, &p.from, &p.request_id)?.ok_or("receipt missing after write")?)
}

/// `prepare`, then `store_local`: Plan A's one-computer send.
pub fn send(
    store: &Store,
    from: &Key,
    to: Option<&Key>,
    text: &str,
    request_id: &str,
    reply_to: Option<&str>,
    now: u64,
) -> Result<Receipt> {
    match prepare(store, from, to, text, request_id, reply_to, now)? {
        Ok(p) => store_local(store, &p),
        Err(r) => Ok(r),
    }
}

/// What member `peer`'s envelope `env` becomes on this computer (member prefix `me`), checked
/// in this order: a `from` outside `peer`'s prefix is dropped unanswered (`Err`); a cloud
/// member's `from` must be a session of its latest presence (`policy`); a `kind` other than
/// `message`, any `files`, a body over `MAX_TEXT`, or a malformed id or `request_id` is
/// `unsupported`; a `to` that is not one address is `policy`, one under another prefix
/// `not_member`; an envelope seen before gets the state it has now and is not delivered again;
/// a `request_id` reused with another text or recipient is `request_id_reused`; `hops` at
/// `MAX_HOPS` or more, or the `LOOP_LIMIT`th send in `LOOP_WINDOW_S` from one principal to
/// `to` (the person for a computer member, the sending session for a cloud member), is `loop`;
/// then `policy::decide`. Accepted, it is stored for `to` with origin `net:<peer short>`,
/// timed by its arrival (`now`), and remembered in `seen_ids`.
pub fn receive(
    store: &Store,
    env: &Frame,
    peer: &Member,
    roster: &Roster,
    me: &str,
    now: u64,
) -> Result<(State, Option<String>)> {
    let Frame::Envelope {
        id,
        request_id,
        kind,
        from,
        to,
        thread,
        hops,
        body,
        files,
        ..
    } = env
    else {
        return Err("not an envelope".into());
    };
    let refused = |r: &str| Ok((State::Refused, Some(r.to_string())));
    let address = |a: &str| sessions::qualify("", a).as_deref() == Some(a);
    let prefix = |a: &str| sessions::split_addr(a).map(|(m, _)| m.to_string());
    if !address(from) || prefix(from).as_deref() != Some(peer.short.as_str()) {
        return Err("an envelope from outside its member's prefix".into());
    }
    let conn = store.conn();
    if peer.kind == MemberKind::Cloud && !advertised(store, &peer.key, from)? {
        return refused("policy");
    }
    if kind != "message"
        || !files.is_empty()
        || body.len() > MAX_TEXT
        || !sessions::word(request_id, MAX_REQUEST_ID)
        || ulid::Ulid::from_string(id).is_err()
    {
        return refused("unsupported");
    }
    if !address(to) {
        return refused("policy");
    }
    if prefix(to).as_deref() != Some(me) {
        return refused("not_member");
    }
    if let Some(seen) = conn
        .query_row(
            "SELECT state FROM seen_ids WHERE sender = ?1 AND envelope_id = ?2",
            [&peer.key, id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok((State::parse(&seen)?, None));
    }
    let sender = Key(from.clone());
    if let Some((old_to, old_body)) = conn
        .query_row(
            "SELECT to_key, body FROM messages WHERE from_key = ?1 AND request_id = ?2",
            [from, request_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if old_to != *to || old_body != *body {
            return refused("request_id_reused");
        }
        // The same message under another envelope id: where it is now.
        let r = receipt(store, &sender, request_id)?.ok_or("message without a receipt")?;
        return Ok((r.state, r.reason));
    }
    if i64::from(*hops) >= MAX_HOPS {
        return refused("loop");
    }
    let since = now.saturating_sub(LOOP_WINDOW_S) as i64;
    let recent: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT from_key, origin FROM messages WHERE to_key = ?1 AND sent_at > ?2 \
             AND origin LIKE 'net:%'",
        )?;
        let rows = stmt.query_map(params![to, since], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let same_principal = |(f, origin): &(String, String)| match peer.kind {
        MemberKind::Cloud => f == from,
        MemberKind::Computer => roster.members.values().any(|m| {
            m.person_id == peer.person_id && origin.strip_prefix("net:") == Some(m.short.as_str())
        }),
    };
    if recent.iter().filter(|r| same_principal(r)).count() >= LOOP_LIMIT as usize {
        return refused("loop");
    }
    match policy::decide(store, peer, to) {
        Verdict::Accept => {}
        // ponytail: Task 9 stores the hold and asks the person.
        Verdict::Hold => return Ok((State::Held, None)),
        Verdict::Refuse => return refused("policy"),
    }
    let tx = conn.unchecked_transaction()?;
    store.put_message(&Message {
        id: id.clone(),
        request_id: request_id.clone(),
        from_key: from.clone(),
        to_key: to.clone(),
        kind: kind.clone(),
        thread: Some(thread.clone().unwrap_or_else(|| id.clone())),
        hops: i64::from(*hops),
        origin: Some(format!("net:{}", peer.short)),
        body: body.clone(),
        sent_at: now as i64,
    })?;
    tx.execute(
        "INSERT INTO mailbox (to_key, message_id, queued_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![to, id, now as i64, (now + HOLD_S) as i64],
    )?;
    tx.execute(
        "INSERT INTO seen_ids VALUES (?1, ?2, 'stored', ?3)",
        params![peer.key, id, now as i64],
    )?;
    advance(&tx, from, request_id, State::Stored, None, now)?;
    tx.commit()?;
    Ok((State::Stored, None))
}

/// Whether `member`'s latest presence (`peers_sessions`) names `address` among its sessions,
/// `known` or `kept` addresses.
fn advertised(store: &Store, member: &str, address: &str) -> Result<bool> {
    let text: Option<String> = store
        .conn()
        .query_row(
            "SELECT presence FROM peers_sessions WHERE member = ?1",
            [member],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match text.and_then(|t| serde_json::from_str(&t).ok()) {
        Some(Frame::Presence {
            sessions,
            known,
            kept,
            ..
        }) => {
            sessions.iter().any(|s| s.session_id == address)
                || known.iter().chain(&kept).any(|a| a == address)
        }
        _ => false,
    })
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

/// The addresses with waiting mail, newest mail first.
pub fn waiting_keys(store: &Store, now: u64) -> Result<Vec<String>> {
    let mut stmt = store.conn().prepare(
        "SELECT to_key FROM mailbox WHERE expires_at >= ?1 GROUP BY to_key ORDER BY MAX(id) DESC",
    )?;
    let keys = stmt.query_map([now as i64], |r| r.get(0))?;
    Ok(keys.collect::<rusqlite::Result<_>>()?)
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

/// Marks `read` every message to `to` whose receipt is `delivered` from before `now`: the
/// session's next step after a delivery has had it in front of it.
pub fn mark_read_for(store: &Store, to: &Key, now: u64) -> Result<()> {
    // `updated_at` stays the delivery time: the hops trigger window runs on it.
    store.conn().execute(
        "UPDATE receipts SET state = 'read' WHERE state = 'delivered' AND updated_at < ?2 \
         AND EXISTS (SELECT 1 FROM messages m WHERE m.to_key = ?1 \
         AND m.from_key = receipts.from_key AND m.request_id = receipts.request_id)",
        params![to.0, now as i64],
    )?;
    tell_senders(store.conn())
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
        let r = send(
            &s,
            &k("m/codex:a"),
            Some(&k("m/codex:b")),
            "hi",
            "r1",
            None,
            100,
        )
        .unwrap();
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
        mark_read_for(&s, &k("m/codex:b"), 101).unwrap();
        assert_eq!(
            state(&s, "r1"),
            State::Delivered,
            "not in the second it was taken"
        );
        mark_read_for(&s, &k("m/codex:a"), 103).unwrap();
        assert_eq!(state(&s, "r1"), State::Delivered, "only mail to that key");
        mark_read_for(&s, &k("m/codex:b"), 103).unwrap();
        assert_eq!(state(&s, "r1"), State::Read);
        // never backwards
        advance(s.conn(), "m/codex:a", "r1", State::Stored, None, 104).unwrap();
        assert_eq!(state(&s, "r1"), State::Read);
    }

    #[test]
    fn a_reply_without_a_recipient_goes_to_the_parents_sender() {
        let (_d, s) = setup();
        send(
            &s,
            &k("m/terminal"),
            Some(&k("m/codex:b")),
            "q",
            "r1",
            None,
            1,
        )
        .unwrap();
        take(&s, &k("m/codex:b"), 2).unwrap();
        let r = send(&s, &k("m/codex:b"), None, "a", "r2", Some("r1"), 3).unwrap();
        assert_eq!(r.state, State::Stored);
        assert_eq!(take(&s, &k("m/terminal"), 4).unwrap()[0].body, "a");
        assert_eq!(state(&s, "r1"), State::Read, "answering it read it");
        // No recipient and nothing to answer: refused, nothing stored.
        let r = send(&s, &k("m/codex:b"), None, "a", "r3", Some("nope"), 5).unwrap();
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (State::Refused, Some("no_recipient"))
        );
        assert!(receipt(&s, &k("m/codex:b"), "r3").unwrap().is_none());
    }

    #[test]
    fn thirteenth_send_in_ten_minutes_is_refused_with_loop() {
        let (_d, s) = setup();
        let t = 1000;
        for i in 0..12 {
            let r = send(&s, &k("a"), Some(&k("b")), "x", &format!("r{i}"), None, t).unwrap();
            assert_eq!(r.state, State::Stored);
        }
        let r = send(&s, &k("a"), Some(&k("b")), "x", "r12", None, t + 599).unwrap();
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (State::Refused, Some("loop"))
        );
        assert_eq!(state(&s, "r12"), State::Refused);
        let r = send(&s, &k("a"), Some(&k("b")), "x", "r13", None, t + 600).unwrap();
        assert_eq!(r.state, State::Stored);
    }

    #[test]
    fn refused_sends_do_not_count_toward_the_window() {
        let (_d, s) = setup();
        let big = "x".repeat(MAX_TEXT + 1);
        // 11 accepted, then many refusals: none may push the 12th/13th over early.
        for i in 0..11 {
            send(
                &s,
                &k("a"),
                Some(&k("b")),
                "x",
                &format!("r{i}"),
                None,
                1000,
            )
            .unwrap();
        }
        for i in 0..5 {
            let r = send(
                &s,
                &k("a"),
                Some(&k("b")),
                &big,
                &format!("big{i}"),
                None,
                1001,
            )
            .unwrap();
            assert_eq!(r.reason.as_deref(), Some("too_long"));
        }
        assert_eq!(
            send(&s, &k("a"), Some(&k("b")), "x", "r11", None, 1002)
                .unwrap()
                .state,
            State::Stored
        );
        for i in 0..5 {
            let r = send(
                &s,
                &k("a"),
                Some(&k("b")),
                "x",
                &format!("loop{i}"),
                None,
                1003,
            )
            .unwrap();
            assert_eq!(r.reason.as_deref(), Some("loop"));
        }
        // Only the 12 accepted count: at 1600 they are all outside the window.
        assert_eq!(
            send(&s, &k("a"), Some(&k("b")), "x", "after", None, 1600)
                .unwrap()
                .state,
            State::Stored
        );
    }

    #[test]
    fn a_request_id_with_other_characters_is_refused() {
        let (_d, s) = setup();
        for rid in ["a\"b", "a\nb", "a b", "é"] {
            let r = send(&s, &k("a"), Some(&k("b")), "x", rid, None, 1).unwrap();
            assert_eq!(
                (r.state, r.reason.as_deref()),
                (State::Refused, Some("bad_request_id")),
                "{rid:?}"
            );
            assert!(receipt(&s, &k("a"), rid).unwrap().is_none());
        }
        assert_eq!(
            send(&s, &k("a"), Some(&k("b")), "x", "Ok.id_1-2", None, 1)
                .unwrap()
                .state,
            State::Stored
        );
    }

    #[test]
    fn a_refused_request_id_stays_refused_on_retry() {
        let (_d, s) = setup();
        let big = "x".repeat(MAX_TEXT + 1);
        send(&s, &k("a"), Some(&k("b")), &big, "r1", None, 1).unwrap();
        let r = send(&s, &k("a"), Some(&k("b")), "short now", "r1", None, 2).unwrap();
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (State::Refused, Some("too_long"))
        );
        assert_eq!(
            send(&s, &k("a"), Some(&k("b")), "short now", "r2", None, 3)
                .unwrap()
                .state,
            State::Stored
        );
    }

    #[test]
    fn unknown_reply_to_falls_back_to_the_default_thread_rule() {
        let (_d, s) = setup();
        send(&s, &k("b"), Some(&k("a")), "first", "r1", None, 1).unwrap();
        take(&s, &k("a"), 1).unwrap();
        let first: String = s
            .conn()
            .query_row(
                "SELECT thread FROM messages WHERE request_id='r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        send(
            &s,
            &k("a"),
            Some(&k("b")),
            "re",
            "r2",
            Some("no-such-id"),
            2,
        )
        .unwrap();
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
        let r = send(&s, &k("a"), Some(&k("b")), "x", "r1", None, 1000).unwrap();
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
    fn mail_for_a_name_address_never_expires() {
        let (_d, s) = setup();
        send(
            &s,
            &k("m/terminal"),
            Some(&k("m/bob")),
            "x",
            "r1",
            None,
            1000,
        )
        .unwrap();
        // Another member's name, or a name outside the listen rule, keeps the 24 h hold.
        send(
            &s,
            &k("m/terminal"),
            Some(&k("n/bob")),
            "x",
            "r2",
            None,
            1000,
        )
        .unwrap();
        send(
            &s,
            &k("m/terminal"),
            Some(&k("m/b ob")),
            "x",
            "r3",
            None,
            1000,
        )
        .unwrap();
        expire(&s, 1000 + 100 * HOLD_S).unwrap();
        assert_eq!(state(&s, "r1"), State::Stored);
        assert_eq!(state(&s, "r2"), State::Expired);
        assert_eq!(state(&s, "r3"), State::Expired);
        assert_eq!(take(&s, &k("m/bob"), 1000 + 100 * HOLD_S).unwrap().len(), 1);
    }

    #[test]
    fn resumed_session_gets_the_mail_that_waited() {
        let (_d, s) = setup();
        register(&s, "b", false);
        send(&s, &k("a"), Some(&k("b")), "wait for me", "r1", None, 1000).unwrap();
        register(&s, "b", true);
        let got = take(&s, &k("b"), 1000 + HOLD_S - 1).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "wait for me");
    }

    #[test]
    fn same_request_id_returns_same_receipt_and_mismatch_is_refused() {
        let (_d, s) = setup();
        let first = send(&s, &k("a"), Some(&k("b")), "x", "r1", None, 1).unwrap();
        assert_eq!(
            send(&s, &k("a"), Some(&k("b")), "x", "r1", None, 2).unwrap(),
            first
        );
        for (to, text) in [("c", "x"), ("b", "y")] {
            let r = send(&s, &k("a"), Some(&k(to)), text, "r1", None, 3).unwrap();
            assert_eq!(
                (r.state, r.reason.as_deref()),
                (State::Refused, Some("request_id_reused"))
            );
        }
        assert_eq!(state(&s, "r1"), State::Stored);
        let big = "x".repeat(MAX_TEXT + 1);
        let r = send(&s, &k("a"), Some(&k("b")), &big, "r2", None, 4).unwrap();
        assert_eq!(r.reason.as_deref(), Some("too_long"));
        assert!(
            send(&s, &k("a"), Some(&k("b")), "x", &"r".repeat(129), None, 4)
                .unwrap()
                .state
                == State::Refused
        );
    }

    #[test]
    fn reply_sets_thread_from_the_message_it_answers() {
        let (_d, s) = setup();
        let (a, b) = (k("a"), k("b"));
        send(&s, &a, Some(&b), "q", "r1", None, 1).unwrap();
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
        send(&s, &b, Some(&a), "ans", "r2", Some("r1"), 2).unwrap();
        // c answers by message id
        send(&s, &a, Some(&b), "more", "r3", Some(&m1.0), 3).unwrap();
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
        send(&s, &a, Some(&b), "follow", "r4", None, 4).unwrap();
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
        send(&s, &a, Some(&k("z")), "hello", "r5", None, 5).unwrap();
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
        send(&s, &k("a"), Some(&k("b")), "do it", "r1", None, 1).unwrap();
        take(&s, &k("b"), 2).unwrap();
        send(&s, &k("b"), Some(&k("c")), "you do it", "r2", None, 3).unwrap();
        let (id, thread, hops) = thread_hops(&s, "r2");
        assert_eq!((hops, thread == id), (1, true));
        assert_ne!(thread, thread_hops(&s, "r1").1);
    }

    #[test]
    fn a_reply_without_reply_to_to_the_trigger_sender_keeps_its_thread() {
        let (_d, s) = setup();
        send(&s, &k("a"), Some(&k("b")), "do it", "r1", None, 1).unwrap();
        take(&s, &k("b"), 2).unwrap();
        send(&s, &k("b"), Some(&k("a")), "done", "r2", None, 3).unwrap();
        let (_, thread, hops) = thread_hops(&s, "r2");
        assert_eq!((thread, hops), (thread_hops(&s, "r1").1, 1));
    }

    #[test]
    fn an_old_delivery_is_no_trigger() {
        let (_d, s) = setup();
        send(&s, &k("a"), Some(&k("b")), "x", "r1", None, 1).unwrap();
        take(&s, &k("b"), 100).unwrap();
        mark_read(&s, &thread_hops(&s, "r1").0, 150).unwrap();
        send(
            &s,
            &k("b"),
            Some(&k("c")),
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
        send(
            &s,
            &k("b"),
            Some(&k("c")),
            "z",
            "r3",
            None,
            100 + LOOP_WINDOW_S,
        )
        .unwrap();
        assert_eq!(thread_hops(&s, "r3").2, 0, "delivery 600 s ago is out");
    }

    #[test]
    fn a_message_taken_long_after_sending_is_still_a_trigger() {
        let (_d, s) = setup();
        send(&s, &k("a"), Some(&k("b")), "x", "r1", None, 1).unwrap();
        take(&s, &k("b"), 1000).unwrap();
        send(&s, &k("b"), Some(&k("c")), "y", "r2", None, 1500).unwrap();
        assert_eq!(thread_hops(&s, "r2").2, 1);
    }

    #[test]
    fn two_senders_may_reuse_the_same_request_id() {
        let (_d, s) = setup();
        let r1 = send(&s, &k("a"), Some(&k("c")), "from a", "req-1", None, 1).unwrap();
        let r2 = send(&s, &k("b"), Some(&k("c")), "from b", "req-1", None, 1).unwrap();
        assert_eq!((r1.state, r2.state), (State::Stored, State::Stored));
        // each sender's retry is idempotent within itself; a mismatch is only against its own
        assert_eq!(
            send(&s, &k("b"), Some(&k("c")), "from b", "req-1", None, 2).unwrap(),
            r2
        );
        let r = send(&s, &k("a"), Some(&k("c")), "from b", "req-1", None, 2).unwrap();
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
