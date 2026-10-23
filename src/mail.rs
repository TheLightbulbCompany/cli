//! Messages, the mailbox, receipts and the loop limit. Time is passed in (`now`, unix seconds).

use crate::net::frames::Frame;
use crate::policy::{self, Policy, Verdict};
use crate::roster::{Member, MemberKind, Roster};
use crate::sessions::{self, Key};
use crate::store::{Message, Store, MSG_RECEIPT};
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

    /// Whether `self` ranks above `other`: receipts only move that way.
    pub fn later_than(self, other: State) -> bool {
        self.rank() > other.rank()
    }

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
        Some(o) => state.later_than(State::parse(&o)?),
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

/// A message decided and ready to store: here (`send`, `receive`) or in the outbox
/// (`outbox::enqueue`).
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
    pub sent_at: i64,
}

/// What `prepare` decided.
#[derive(Debug)]
pub enum Prep {
    /// To be stored.
    Go(Prepared),
    /// Refused, or sent before: this is its receipt.
    Done(Receipt),
}

/// Where a stored message came from: this computer's IPC, or member `short` (`net:<short>`).
#[derive(Clone, Copy)]
pub enum Origin<'a> {
    Local,
    Net(&'a str),
}

impl From<&Prepared> for Message {
    /// `p` as stored here, from this computer (`origin` local).
    fn from(p: &Prepared) -> Message {
        Message {
            id: p.message_id.clone(),
            request_id: p.request_id.clone(),
            from_key: p.from.0.clone(),
            to_key: p.to.0.clone(),
            kind: p.kind.clone(),
            thread: Some(p.thread.clone()),
            hops: p.hops,
            origin: Some("local".into()),
            body: p.text.clone(),
            sent_at: p.sent_at,
        }
    }
}

impl From<&Prepared> for Frame {
    /// `p` as the wire carries it.
    fn from(p: &Prepared) -> Frame {
        Frame::Envelope {
            id: p.message_id.clone(),
            request_id: p.request_id.clone(),
            kind: p.kind.clone(),
            from: p.from.0.clone(),
            to: p.to.0.clone(),
            reply_to: p.reply_to.clone(),
            thread: Some(p.thread.clone()),
            hops: u32::try_from(p.hops).unwrap_or(u32::MAX),
            body: p.text.clone(),
            files: vec![],
            sent_at: p.sent_at,
        }
    }
}

/// Whether `n` sends in the window already reach the loop limit.
fn loop_refused(n: usize) -> bool {
    n >= LOOP_LIMIT as usize
}

/// Whether a message `hops` deep may go: under `MAX_HOPS`, and (unless a `person` sent it) at
/// least `floor`, one past what it answers.
fn hops_ok(hops: i64, floor: Option<i64>, person: bool) -> bool {
    hops < MAX_HOPS && (person || floor.is_none_or(|f| hops >= f))
}

/// Whether `from` is a person at a terminal (`<member>/terminal`).
pub fn is_person(from: &str) -> bool {
    sessions::split_addr(from).is_some_and(|(_, rest)| rest == "terminal")
}

/// When mail for `to` stored at `sent_at` leaves the mailbox: never for a name address (a
/// person's terminal or `listen --as` mailbox), after `HOLD_S` for a session or anything else.
pub(crate) fn expires_for(to: &str, sent_at: i64) -> i64 {
    match sessions::split_addr(to) {
        Some((_, rest)) if sessions::valid_name(rest) => i64::MAX,
        _ => sent_at + HOLD_S as i64,
    }
}

/// Decides `from`'s message `request_id` before anything is stored: `Done` with the receipt
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
) -> Result<Prep> {
    let conn = store.conn();
    let unstored = |reason: &str| {
        Ok(Prep::Done(Receipt {
            request_id: request_id.into(),
            state: State::Refused,
            reason: Some(reason.into()),
            where_: None,
        }))
    };
    let refused = |reason: &str| Ok(Prep::Done(refuse(conn, from, request_id, reason, now)?));
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
    if let Some((old_to, old_text)) = store.sent_as(&from.0, request_id)? {
        if old_to != to.0 || old_text != text {
            // Not persisted: the original's receipt must stay as it was.
            return unstored("request_id_reused");
        }
        let r = receipt(store, from, request_id)?.ok_or("message without a receipt")?;
        return Ok(Prep::Done(r));
    }
    if let Some(r) = receipt(store, from, request_id)? {
        return Ok(Prep::Done(r)); // an earlier refusal; no message row to compare
    }
    if text.len() > MAX_TEXT {
        return refused("too_long");
    }
    let since = now.saturating_sub(LOOP_WINDOW_S) as i64;
    let recent: u32 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE from_key = ?1 AND to_key = ?2 AND sent_at > ?3",
        params![from.0, to.0, since],
        |r| r.get(0),
    )?;
    if loop_refused(recent as usize) {
        return refused("loop");
    }

    let id = ulid::Ulid::generate().to_string();
    let (thread, hops) = match &parent {
        Some((pid, thread, hops, _)) => (thread.clone().unwrap_or(pid.clone()), hops + 1),
        None => {
            // The trigger: the newest message delivered to `from` in the window. Hops carry on
            // from it even when the work is forwarded to a third session.
            let trigger: Option<(String, Option<String>, i64, String)> = conn
                .query_row(
                    &format!(
                        "SELECT m.id, m.thread, m.hops, m.from_key FROM messages m {MSG_RECEIPT} \
                         WHERE m.to_key = ?1 AND r.state IN ('delivered', 'read') AND r.updated_at > ?2 \
                         ORDER BY r.updated_at DESC, m.rowid DESC LIMIT 1"
                    ),
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
    let person = is_person(&from.0);
    let hops = if person { 0 } else { hops };
    if !hops_ok(hops, None, person) {
        return refused("loop");
    }
    Ok(Prep::Go(Prepared {
        message_id: id,
        from: from.clone(),
        to,
        text: text.into(),
        request_id: request_id.into(),
        reply_to: parent.map(|(pid, ..)| pid),
        thread,
        hops,
        kind: "message".into(),
        sent_at: now as i64,
    }))
}

/// Writes `p`'s message (from `origin`) with receipt `state`.
pub(crate) fn put(store: &Store, p: &Prepared, origin: Origin, state: State) -> Result<()> {
    let mut m = Message::from(p);
    if let Origin::Net(short) = origin {
        m.origin = Some(format!("net:{short}"));
    }
    store.put_message(&m)?;
    advance(
        store.conn(),
        &p.from.0,
        &p.request_id,
        state,
        None,
        p.sent_at as u64,
    )
}

/// Answering a message read it.
fn answered(store: &Store, p: &Prepared) -> Result<()> {
    if let Some(pid) = &p.reply_to {
        if store.message(pid)?.is_some_and(|m| m.to_key == p.from.0) {
            mark_read(store, pid, p.sent_at as u64)?;
        }
    }
    Ok(())
}

/// Stores `p` (from `origin`) in its recipient's mailbox, `stored` (`expires_for`); with
/// `mark_reply_read`, the message it answers is read. The caller holds a transaction.
pub(crate) fn record(
    store: &Store,
    p: &Prepared,
    origin: Origin,
    mark_reply_read: bool,
) -> Result<()> {
    put(store, p, origin, State::Stored)?;
    let expires = expires_for(&p.to.0, p.sent_at);
    store.put_mailbox(&p.to.0, &p.message_id, p.sent_at, expires)?;
    if mark_reply_read {
        answered(store, p)?;
    }
    Ok(())
}

/// Keeps `p` as this computer's message for another member, `queued` and in no mailbox (the
/// outbox holds it). The caller holds a transaction.
pub(crate) fn record_queued(store: &Store, p: &Prepared) -> Result<()> {
    put(store, p, Origin::Local, State::Queued)?;
    answered(store, p)
}

/// Stores `p` here, in its recipient's mailbox; its receipt.
pub fn store_local(store: &Store, p: &Prepared) -> Result<Receipt> {
    let tx = store.conn().unchecked_transaction()?;
    record(store, p, Origin::Local, true)?;
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
        Prep::Go(p) => store_local(store, &p),
        Prep::Done(r) => Ok(r),
    }
}

/// What member `peer`'s envelope `env` becomes on this computer (member prefix `me`), checked
/// in this order: a `from` outside `peer`'s prefix is dropped unanswered (`Err`); a cloud
/// member's `from` must be a session of its latest presence (`policy`); a `kind` other than
/// `message`, any `files`, a body over `MAX_TEXT`, or a malformed id, `thread` or `request_id`
/// is `unsupported`; a `to` that is not one address is `policy`, one under another prefix
/// `not_member`; an envelope seen before gets the state it has now and is not delivered again;
/// a `request_id` reused with another text or recipient is `request_id_reused`; `hops` at
/// `MAX_HOPS` or more, or below one past a message of ours it answers (`hops_ok`), or the
/// `LOOP_LIMIT`th send in `LOOP_WINDOW_S` from one principal to `to` (the person for a computer
/// member, the sending session for a cloud member), is `loop`; then `policy::decide`: refused
/// `policy`, or kept under a hold (`holds::hold`, `held`, in no mailbox), or stored for `to`
/// with origin `net:<peer short>`, timed by its arrival (`now`). Held or stored, it is
/// remembered in `seen_ids`.
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
        reply_to,
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
    let ulid = |a: &str| ulid::Ulid::from_string(a).is_ok();
    if !address(from) || sessions::member_of(from) != Some(peer.short.as_str()) {
        return Err("an envelope from outside its member's prefix".into());
    }
    if peer.kind == MemberKind::Cloud && !advertised(store, &peer.key, from)? {
        return refused("policy");
    }
    if kind != "message"
        || !files.is_empty()
        || body.len() > MAX_TEXT
        || !sessions::word(request_id, MAX_REQUEST_ID)
        || !ulid(id)
        || !thread.as_deref().is_none_or(ulid)
    {
        return refused("unsupported");
    }
    if !address(to) {
        return refused("policy");
    }
    if sessions::member_of(to) != Some(me) {
        return refused("not_member");
    }
    if let Some((state, reason)) = store.seen_state(&peer.key, id)? {
        return Ok((State::parse(&state)?, reason));
    }
    if let Some((old_to, old_body)) = store.sent_as(from, request_id)? {
        if old_to != *to || old_body != *body {
            return refused("request_id_reused");
        }
        // The same message under another envelope id: where it is now.
        let sender = Key(from.clone());
        let r = receipt(store, &sender, request_id)?.ok_or("message without a receipt")?;
        return Ok((r.state, r.reason));
    }
    // An answer to (or in the thread of) a message this computer sent is at least one hop
    // past it, whatever `hops` the peer claims; only a person (`/terminal`, fenced by the
    // sending daemon) starts again at 0.
    let floor = store.hop_floor([reply_to.as_deref(), thread.as_deref()])?;
    let hops = i64::from(*hops);
    if !hops_ok(hops, floor, is_person(from)) {
        return refused("loop");
    }
    let since = now.saturating_sub(LOOP_WINDOW_S) as i64;
    let same_principal = |(f, origin): &(String, String)| match peer.kind {
        MemberKind::Cloud => f == from,
        MemberKind::Computer => roster.members.values().any(|m| {
            m.person_id == peer.person_id && origin.strip_prefix("net:") == Some(m.short.as_str())
        }),
    };
    let recent = store.net_mail_to(to, since)?;
    if loop_refused(recent.iter().filter(|r| same_principal(r)).count()) {
        return refused("loop");
    }
    // Not in my own roster (removed): nothing is mine to accept.
    let verdict = match roster.by_short(me) {
        Some(mine) => policy::decide(&Policy::load(store)?, mine, peer),
        None => Verdict::Hold,
    };
    if verdict == Verdict::Refuse {
        return refused("policy");
    }
    let p = Prepared {
        message_id: id.clone(),
        from: Key(from.clone()),
        to: Key(to.clone()),
        text: body.clone(),
        request_id: request_id.clone(),
        reply_to: reply_to.clone(),
        thread: thread.clone().unwrap_or_else(|| id.clone()),
        hops,
        kind: kind.clone(),
        sent_at: now as i64,
    };
    let tx = store.conn().unchecked_transaction()?;
    // Seen with what the sender is told, so a resend makes no second hold or copy.
    let state = if verdict == Verdict::Hold {
        crate::holds::hold(store, &p, &peer.key, now as i64)?;
        State::Held
    } else {
        record(store, &p, Origin::Net(&peer.short), false)?;
        State::Stored
    };
    store.put_seen(&peer.key, id, state.name(), now as i64)?;
    tx.commit()?;
    Ok((state, None))
}

/// Whether `member`'s latest presence names `address` among its sessions, `known` or `kept`
/// addresses.
fn advertised(store: &Store, member: &str, address: &str) -> Result<bool> {
    let presence = store.presence_of(member)?;
    Ok(match presence.and_then(|t| serde_json::from_str(&t).ok()) {
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

/// What senders are owed about the messages they sent here: a `Receipt` for each whose state
/// moved past what they were told, `(sender member, frame)`; `told` once sent.
pub fn untold(store: &Store) -> Result<Vec<(String, Frame)>> {
    let mut out = vec![];
    for u in store.untold()? {
        if State::parse(&u.state)?.later_than(State::parse(&u.told)?) {
            let (id, state, reason) = (u.id, u.state, u.reason);
            out.push((u.sender, Frame::Receipt { id, state, reason }));
        }
    }
    Ok(out)
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
    Ok(())
}

/// Marks message `message_id` read if it reached its recipient (`stored` or `delivered`); a
/// `held` one changes only by its person's answer or its expiry.
pub fn mark_read(store: &Store, message_id: &str, now: u64) -> Result<()> {
    let Some(m) = store.message(message_id)? else {
        return Ok(());
    };
    let from = Key(m.from_key.clone());
    let reached = receipt(store, &from, &m.request_id)?
        .is_some_and(|r| matches!(r.state, State::Stored | State::Delivered));
    if reached {
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
        // Past its hold, it is not handed out even before `expire` runs.
        assert!(take(&s, &k("b"), 1000 + HOLD_S + 1).unwrap().is_empty());
        assert_eq!(state(&s, "r1"), State::Stored);
        expire(&s, 1000 + HOLD_S + 1).unwrap();
        assert_eq!(state(&s, "r1"), State::Expired);
        assert!(take(&s, &k("b"), 1000 + HOLD_S + 2).unwrap().is_empty());
    }

    #[test]
    fn mail_for_a_name_address_never_expires() {
        // `expires_for`, as `a_peers_mail_to_a_name_address_never_expires` pins it for a peer's:
        // a name address never expires (another member's name under its own prefix never
        // reaches this store: the IPC sends it to the outbox); a session, or a name outside the
        // listen rule, keeps the 24 h hold.
        let (_d, s) = setup();
        for (i, to) in ["m/bob", "n/bob", "m/b ob", "m/codex:c"].iter().enumerate() {
            send(
                &s,
                &k("m/terminal"),
                Some(&k(to)),
                "x",
                &format!("r{}", i + 1),
                None,
                1000,
            )
            .unwrap();
        }
        expire(&s, 1000 + 100 * HOLD_S).unwrap();
        assert_eq!(state(&s, "r1"), State::Stored);
        assert_eq!(state(&s, "r2"), State::Stored);
        assert_eq!(state(&s, "r3"), State::Expired);
        assert_eq!(state(&s, "r4"), State::Expired);
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

    /// Member `p` (key `pkey`) in a roster, and its envelope `rid` to `to` from `p/codex:a`.
    fn peer_mail(to: &str, rid: &str) -> (Member, Roster, Frame) {
        let peer = Member {
            key: "pkey".into(),
            short: "p".into(),
            name: "aden".into(),
            computer: "mbp".into(),
            person_id: "pkey".into(),
            role: crate::roster::Role::Member,
            kind: MemberKind::Computer,
            admitted_at: 0,
            proof: None,
            workspace: None,
        };
        let mine = Member {
            key: "mkey".into(),
            short: "m".into(),
            person_id: "mkey".into(),
            ..peer.clone()
        };
        let mut roster = Roster::default();
        roster.members.insert(peer.key.clone(), peer.clone());
        roster.members.insert(mine.key.clone(), mine);
        let env = Frame::Envelope {
            id: ulid::Ulid::generate().to_string(),
            request_id: rid.into(),
            kind: "message".into(),
            from: "p/codex:a".into(),
            to: to.into(),
            reply_to: None,
            thread: None,
            hops: 0,
            body: "x".into(),
            files: vec![],
            sent_at: 0,
        };
        (peer, roster, env)
    }

    /// This computer allows `peer_mail`'s person.
    fn allow_p(s: &Store) {
        policy::set(s, None, "pkey", Verdict::Accept, "local", 0).unwrap();
    }

    #[test]
    fn a_peers_mail_to_a_name_address_never_expires() {
        let (_d, s) = setup();
        allow_p(&s);
        for (i, to) in ["m/terminal", "m/codex:c"].iter().enumerate() {
            let (peer, roster, env) = peer_mail(to, &format!("r{i}"));
            let got = receive(&s, &env, &peer, &roster, "m", 1000).unwrap();
            assert_eq!(got, (State::Stored, None));
        }
        let later = 1000 + 100 * HOLD_S;
        expire(&s, later).unwrap();
        assert_eq!(waiting(&s, &k("m/terminal"), later).unwrap().len(), 1);
        assert!(waiting(&s, &k("m/codex:c"), later).unwrap().is_empty());
    }

    #[test]
    fn a_duplicate_envelope_is_reacked_with_its_reason_and_never_stored_twice() {
        let (_d, s) = setup();
        allow_p(&s);
        let (peer, roster, env) = peer_mail("m/codex:c", "r1");
        let again = || receive(&s, &env, &peer, &roster, "m", 1001).unwrap();
        receive(&s, &env, &peer, &roster, "m", 1000).unwrap();
        advance(
            s.conn(),
            "p/codex:a",
            "r1",
            State::Refused,
            Some("declined"),
            1000,
        )
        .unwrap();
        assert_eq!(again(), (State::Refused, Some("declined".into())));
        // With its message and receipt rows gone, what the sender was told stands.
        s.conn()
            .execute_batch("DELETE FROM messages; DELETE FROM receipts; DELETE FROM mailbox;")
            .unwrap();
        assert_eq!(again(), (State::Stored, None));
        assert_eq!(
            unread(&s, &k("m/codex:c"), 1001).unwrap(),
            0,
            "not stored again"
        );
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
