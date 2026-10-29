//! Messages, the mailbox, receipts and the loop limit. Time is passed in (`now`, unix seconds).

use crate::config::Config;
use crate::net::frames::Frame;
use crate::roster::{Member, Roster};
use crate::sessions::{self, Key};
use crate::store::{Message, Store, MSG_RECEIPT};
use crate::workers::{self, Taken};
use crate::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The kinds an envelope may carry.
pub const KINDS: [&str; 4] = ["message", "assign", "result", "cancel"];

pub const LOOP_LIMIT: u32 = 12;
/// Half-open: a send counts while `sent_at > now - LOOP_WINDOW_S`, so one sent at T stops counting at T + 600.
pub const LOOP_WINDOW_S: u64 = 600;
/// What a sender the workspace does not answer is told (`not_a_member`).
pub const NOT_A_MEMBER: &str =
    "This workspace answers its members. Ask an admin to add you, then run `lightbulb login`";
/// Offline mail (and, later, the outbox) waits this long.
pub const HOLD_S: u64 = 86_400;
/// A held message waits this long for the person (nothing holds in Plan A).
pub const HELD_S: u64 = 604_800;
pub const MAX_TEXT: usize = 65_536;
pub const MAX_REQUEST_ID: usize = 128;
/// A message `MAX_HOPS` agent replies deep is refused `loop`: a person must speak.
pub const MAX_HOPS: i64 = 20;
/// What `receive` decided.
#[derive(Debug, PartialEq)]
pub struct Answer {
    pub state: State,
    /// Why, when `refused`.
    pub reason: Option<String>,
    /// The loop limit a `loop` refusal hit (`HOPS`, `RATE`).
    pub limit: Option<String>,
    /// Whether it is new mail (stored now), not a resend or a refusal.
    pub fresh: bool,
    /// What a fresh worker's assignment became (the caller runs its follow-up).
    pub taken: Option<Taken>,
    /// The assignment (message id) a cancel for a fresh worker names.
    pub cancelled: Option<String>,
}

impl Answer {
    fn is(state: State) -> Answer {
        Answer {
            state,
            reason: None,
            limit: None,
            fresh: false,
            taken: None,
            cancelled: None,
        }
    }

    fn refused(reason: &str, limit: Option<&str>) -> Answer {
        Answer {
            reason: Some(reason.into()),
            limit: limit.map(Into::into),
            ..Answer::is(State::Refused)
        }
    }
}

/// Which loop limit a `loop` refusal hit (`Receipt::limit`, the wire's `limit`): `MAX_HOPS` agent
/// replies, or `LOOP_LIMIT` messages in the window.
pub const HOPS: &str = "hops";
pub const RATE: &str = "rate";

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

/// The result that answered an assignment: its `request_id` and `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultRef {
    pub request_id: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub request_id: String,
    pub state: State,
    pub reason: Option<String>,
    /// `"recipient"` (live session) or `"mailbox"` (not live); only while `stored`.
    #[serde(rename = "where")]
    pub where_: Option<String>,
    /// For a `loop` refusal, `HOPS` or `RATE`; `None` when the refuser did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// Who the message was stored for (none for a refusal), and when the receipt last moved.
    #[serde(skip)]
    pub to: Option<String>,
    #[serde(skip)]
    pub at: Option<i64>,
    /// For an assignment, the result that closed it (the oldest reply to it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ResultRef>,
}

/// What a terminal's sender is told about a receipt: the address it went `to` (when known), its
/// request id and the command name to type (`receipt_note`).
pub struct ForPerson<'a> {
    pub to: Option<&'a str>,
    pub rid: &'a str,
    pub cmd: &'a str,
    /// Whether to end with `Check: <cmd> receipt <rid>` (not when `receipt` itself prints it).
    pub check: bool,
}

/// A receipt's reason when a person or agent allowed the held message: `approved by <who>`.
pub const APPROVED: &str = "approved by ";

/// What receipt `r` means in words. With `person`, the sentence for the person who sent it at
/// a terminal (always one). Without, the note an agent's `send_message` and `message_receipt`
/// carry, only for mail on a teammate's computer or a `loop` refusal.
pub fn receipt_note(r: &Value, person: Option<&ForPerson>) -> Option<String> {
    let text = receipt_text(r, person)?;
    Some(
        match r["reason"].as_str().and_then(|w| w.strip_prefix(APPROVED)) {
            Some(who) if person.is_some() => format!(
                "{text} Approved by {}.",
                crate::label::plain(who, usize::MAX)
            ),
            _ => text,
        },
    )
}

/// The `no_worker` note, naming `harness` (a provider) when the address has one.
fn no_worker(harness: &str, hint: &str) -> String {
    if harness.is_empty() {
        format!("that computer does not take fresh workers yet{hint}")
    } else {
        let h = crate::frame::agent_name(harness);
        format!("that computer does not take {h} workers (find_workers lists who does)")
    }
}

fn receipt_text(r: &Value, person: Option<&ForPerson>) -> Option<String> {
    let at = |f: &str| r[f].as_str().unwrap_or_default();
    // A teammate's words, printed to a terminal: one line, nothing it would act on.
    let shown = |f: &str| crate::label::plain(&at(f).replace(['\n', '\t'], " "), usize::MAX);
    let (computer, who) = (shown("where"), shown("person"));
    let remote = r["person"].is_string();
    let until = || crate::date::local_time(r["until"].as_i64().unwrap_or_default());
    let limit = || match at("limit") {
        RATE => format!("loop limit: {LOOP_LIMIT} messages in 10 minutes to this session"),
        // A person's message is hop 0, so `MAX_HOPS - 1` agent replies get through.
        HOPS => format!(
            "paused after {} agent replies; a person must speak",
            MAX_HOPS - 1
        ),
        _ => "paused by a loop limit".into(),
    };
    let Some(p) = person else {
        let (computer, person) = (at("where"), at("person"));
        return Some(match at("state") {
            "refused" if at("reason") == "loop" => limit(),
            "refused" if at("reason") == "no_worker" => format!("not sent: {}", no_worker(at("harness"), " (find_workers lists who takes assignments)")),
            "refused" if at("reason") == "busy" && remote => format!("not sent: {person}'s computer is running all the workers it allows; try later or another worker"),
            "refused" if at("reason") == "declined" && remote => format!("not sent: {person} declined to start a worker"),
            "queued" if remote && r["waking"] == true => {
                format!("queued: {computer} is waking up and gets it in a moment.")
            }
            "queued" if remote => {
                let why = if r["online"] == true {
                    "has not confirmed yet"
                } else {
                    "is offline"
                };
                format!("queued: {computer} {why}. Will retry until {}.", until())
            }
            "stored" if remote => format!("stored on {computer}."),
            "held" if remote => format!("held until {person} allows it"),
            _ => return None,
        });
    };
    let check = if p.check {
        format!(" Check: {} receipt {}", p.cmd, p.rid)
    } else {
        String::new()
    };
    let name = p.to.map(|t| t.split_once('/').map_or(t, |(_, n)| n));
    // A name address (`inbox`), not a session's.
    let mailbox = name.filter(|n| !n.contains(':'));
    Some(match at("state") {
        "refused" if at("reason") == "loop" => format!("Not sent: {}.", limit()),
        "refused" => format!(
            "Not sent: {}.",
            match at("reason") {
                "policy" => "they do not take messages from you".to_string(),
                "declined" if remote => format!("{who} declined to start a worker"),
                "declined" => "they declined it".into(),
                "busy" => format!("{who}'s computer is running all the workers it allows; try later or another worker"),
                "not_member" => "that computer is not in your team".into(),
                "not_a_member" => NOT_A_MEMBER.into(),
                "request_id_reused" => "that request id was used for another message".into(),
                "unsupported" => "their computer cannot take it".into(),
                "no_worker" => no_worker(p.to.and_then(new_harness).unwrap_or_default(), ""),
                "bad_request" => "the assignment's terms were refused".into(),
                _ => shown("reason").replace('_', " "),
            }
        ),
        "held" => format!("Not yet sent: held until {who} allows it.{check}"),
        "queued" if remote && r["waking"] == true => {
            format!("Sent. {computer} is waking up and gets it in a moment.{check}")
        }
        "queued" if remote => {
            let why = if r["online"] == true {
                "has not confirmed it yet"
            } else {
                "is offline"
            };
            format!(
                "Not delivered yet: {computer} {why}. Lightbulb keeps trying until {}.{check}",
                until()
            )
        }
        "queued" => format!("Not delivered yet.{check}"),
        "stored" if remote => match mailbox {
            Some(n) => format!(
                "Sent. Stored on {computer} in the \"{}\" mailbox.{check}",
                crate::label::plain(n, usize::MAX)
            ),
            // A session's address; unknown (`receipt` has only the id): no guess.
            None if name.is_some() => {
                format!("Sent. Stored on {computer} for the session's next step.{check}")
            }
            None => format!("Sent. Stored on {computer}.{check}"),
        },
        "stored" if computer == "recipient" => "Sent. The session sees it at its next step.".into(),
        "stored" => match name {
            Some(n) if mailbox.is_some() => format!(
                "Sent. Waiting in the \"{}\" mailbox until someone reads it.",
                crate::label::plain(n, usize::MAX)
            ),
            Some(_) => "Sent. It waits until that session is back.".into(),
            None => "Sent. It waits until it is read.".into(),
        },
        "delivered" if remote => format!("Delivered to {who} on {computer}."),
        "delivered" => "Delivered.".into(),
        "read" if remote => format!("Read by {who} on {computer}."),
        "read" => "Read.".into(),
        // A hold nobody answered (`not_allowed`), the sender's outbox gave up (`unreachable`), or
        // it waited unread in the mailbox (or its session ended while held).
        "expired" if at("reason") == "not_allowed" => {
            "Nobody answered the Allow for 7 days; the message expired.".into()
        }
        "expired" if at("reason") == "unreachable" => {
            "Not delivered: the other computer could not be reached for 24 hours.".into()
        }
        "expired" => format!(
            "{} did not come back within 24 hours; the message expired.",
            crate::label::plain(name.unwrap_or("The session"), usize::MAX)
        ),
        other => format!("Sent ({}).", crate::label::plain(other, usize::MAX)),
    })
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

/// Writes `state` (with `reason` and, for a `loop` refusal, its `limit`) only if it is later
/// than the stored one: whether it did.
pub(crate) fn advance(
    conn: &Connection,
    from: &str,
    request_id: &str,
    state: State,
    reason: Option<&str>,
    limit: Option<&str>,
    now: u64,
) -> Result<bool> {
    let old = conn
        .query_row(
            "SELECT state, reason FROM receipts WHERE from_key = ?1 AND request_id = ?2",
            [from, request_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let moves = match &old {
        Some((o, _)) => state.later_than(State::parse(o)?),
        None => true,
    };
    // Who approved a held message stays on the receipt as it moves on.
    let kept = old
        .and_then(|(_, r)| r)
        .filter(|r| reason.is_none() && r.starts_with(APPROVED));
    let reason = reason.or(kept.as_deref());
    if moves {
        if state == State::Read {
            // `updated_at` stays the delivery time: the hops trigger window runs on it.
            conn.execute(
                "UPDATE receipts SET state = 'read' WHERE from_key = ?1 AND request_id = ?2",
                [from, request_id],
            )?;
        } else {
            conn.execute(
                "INSERT OR REPLACE INTO receipts (from_key, request_id, state, reason, updated_at, \
                 \"limit\") VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![from, request_id, state.name(), reason, now as i64, limit],
            )?;
        }
    }
    Ok(moves)
}

/// The stored receipt for `from`'s `request_id`, with `where_` worked out from the recipient's
/// liveness now, and the recipient and the time it last moved (`to`, `at`) read with it.
pub fn receipt(store: &Store, from: &Key, request_id: &str) -> Result<Option<Receipt>> {
    let row = store
        .conn()
        .query_row(
            "SELECT r.state, r.reason, r.\"limit\", r.updated_at, m.to_key, m.id FROM receipts r \
             LEFT JOIN messages m ON m.from_key = r.from_key AND m.request_id = r.request_id \
             WHERE r.from_key = ?1 AND r.request_id = ?2",
            [&from.0, request_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((state, reason, limit, at, to, id)) = row else {
        return Ok(None);
    };
    let state = State::parse(&state)?;
    let where_ = to.as_deref().filter(|_| state == State::Stored).map(|to| {
        if live(store, to) {
            "recipient"
        } else {
            "mailbox"
        }
        .to_string()
    });
    let result = match id {
        Some(id) => store.result_of(&id)?,
        None => None,
    }
    .map(|(request_id, status)| ResultRef { request_id, status });
    Ok(Some(Receipt {
        request_id: request_id.into(),
        state,
        reason,
        where_,
        limit,
        to,
        at,
        result,
    }))
}

pub(crate) fn refuse(
    conn: &Connection,
    from: &Key,
    request_id: &str,
    reason: &str,
    limit: Option<&str>,
    now: u64,
) -> Result<Receipt> {
    advance(
        conn,
        &from.0,
        request_id,
        State::Refused,
        Some(reason),
        limit,
        now,
    )?;
    Ok(Receipt {
        request_id: request_id.into(),
        state: State::Refused,
        reason: Some(reason.into()),
        where_: None,
        limit: limit.map(Into::into),
        to: None,
        at: Some(now as i64),
        result: None,
    })
}

/// The terms of an assignment and its result, stored with the message as JSON.
#[derive(Default, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_when: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Meta {
    pub fn is_empty(&self) -> bool {
        *self == Meta::default()
    }
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
    pub meta: Meta,
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
    /// A hosted app's session on this computer (`link`): never framed as the person's own.
    App,
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
            reply_to: p.reply_to.clone(),
            meta: p.meta.clone(),
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
            meta: p.meta.clone(),
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

/// Whether `m` is an assignment addressed to `to`, or to `<member>/<provider>:new` when `to`
/// is a session of that harness (the worker started for it).
fn answers_assign(m: &Message, to: &str) -> bool {
    m.kind == "assign" && (m.to_key == to || Some(&m.to_key) == new_key_of(to).as_ref())
}

/// `<m>/<p>:<conv>` as the address a worker for it was asked at: `<m>/<p>:new`.
pub fn new_key_of(session: &str) -> Option<String> {
    let (member, rest) = sessions::split_addr(session)?;
    let (provider, conv) = rest.split_once(':')?;
    (sessions::PROVIDERS.contains(&provider) && conv != "new")
        .then(|| format!("{member}/{provider}:new"))
}

/// The harness of a `<member>/<provider>:new` address.
pub fn new_harness(to: &str) -> Option<&str> {
    let (_, rest) = sessions::split_addr(to)?;
    rest.strip_suffix(":new")
        .filter(|p| sessions::PROVIDERS.contains(p))
}

/// Whether `to` is `<member>/<provider>:new`: a request for a fresh worker session.
pub fn is_new_worker(to: &str) -> bool {
    sessions::split_addr(to)
        .and_then(|(_, rest)| rest.strip_suffix(":new"))
        .is_some_and(|p| sessions::PROVIDERS.contains(&p))
}

/// The rule for sending `kind` with `meta` from `from` to `to`, answering `parent`:
/// `Err(reason)` is the refusal. An `assign` carries its terms (`done_when`, a deadline;
/// `repo`/`branch` short); a `result` answers an assignment made to `from`, goes back to its
/// asker and has a known status; a `cancel` is the asker's alone.
pub(crate) fn check_kind(
    kind: &str,
    meta: &Meta,
    parent: Option<&Message>,
    from: &str,
    to: &str,
) -> std::result::Result<(), &'static str> {
    let too_long = |v: &Option<String>, max| v.as_ref().is_some_and(|s| s.chars().count() > max);
    match kind {
        // No build sends a plain message with terms or a status.
        "message" if meta.is_empty() => Ok(()),
        "message" => Err("bad_request"),
        "assign" => match &meta.done_when {
            Some(d)
                if !d.is_empty()
                    && !too_long(&meta.done_when, 4_096)
                    && meta.deadline_at.is_some()
                    && !too_long(&meta.repo, 256)
                    && !too_long(&meta.branch, 256) =>
            {
                Ok(())
            }
            _ => Err("bad_request"),
        },
        "result" => {
            if !parent.is_some_and(|p| answers_assign(p, from) && p.from_key == to) {
                return Err("policy");
            }
            let status = meta.status.as_deref();
            let ok = matches!(status, None | Some("done" | "failed" | "cancelled"))
                && match meta.reason.as_deref() {
                    None => true,
                    Some(r) => {
                        status == Some("failed") && ["timed_out", "stopped", "error"].contains(&r)
                    }
                };
            if ok {
                Ok(())
            } else {
                Err("bad_request")
            }
        }
        "cancel" if parent.is_some_and(|p| p.kind == "assign" && p.from_key == from) => Ok(()),
        "cancel" => Err("policy"),
        _ => Err("unsupported"),
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
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    store: &Store,
    from: &Key,
    to: Option<&Key>,
    text: &str,
    request_id: &str,
    reply_to: Option<&str>,
    kind: &str,
    mut meta: Meta,
    now: u64,
) -> Result<Prep> {
    let conn = store.conn();
    let unstored = |reason: &str| {
        Ok(Prep::Done(Receipt {
            request_id: request_id.into(),
            state: State::Refused,
            reason: Some(reason.into()),
            where_: None,
            limit: None,
            to: None,
            at: None,
            result: None,
        }))
    };
    let refused = |reason: &str| {
        Ok(Prep::Done(refuse(
            conn, from, request_id, reason, None, now,
        )?))
    };
    let looped = |limit: &str| {
        let r = refuse(conn, from, request_id, "loop", Some(limit), now)?;
        Ok(Prep::Done(r))
    };
    if !sessions::word(request_id, MAX_REQUEST_ID) {
        return unstored("bad_request_id"); // nothing safe to key a receipt on
    }
    // A running worker's reply naming its assignment (by request id or id) answers that one,
    // whichever other assignment to its `:new` address shares the request id.
    let worker = store.worker_by_session(&from.0)?;
    let worker = worker.filter(|w| w.ended_at.is_none());
    let own = match (reply_to, &worker) {
        (Some(r), Some(w)) => store
            .message(&w.assignment)?
            .filter(|a| a.request_id == r || a.id == r),
        _ => None,
    };
    // A worker answers no other `:new` assignment.
    let new_key = new_key_of(&from.0).filter(|_| worker.is_none());
    // The message this answers: a message id, or a request id `from` received.
    let parent: Option<Message> = match reply_to {
        _ if own.is_some() => own,
        Some(r) => {
            let id: Option<String> = conn
                .query_row(
                    // A cancel names the asker's own assignment by request id; any other
                    // reply names a message sent to `from`.
                    // A worker started for `<m>/<p>:new` answers it as its own session.
                    "SELECT id FROM messages WHERE id = ?1 OR (request_id = ?1 AND \
                     CASE WHEN ?3 = 'cancel' THEN from_key = ?2 ELSE (to_key = ?2 OR to_key = ?4) \
                     END) ORDER BY id = ?1 DESC, rowid DESC LIMIT 1",
                    params![r, from.0, kind, new_key],
                    |x| x.get(0),
                )
                .optional()?;
            match id {
                Some(id) => store.message(&id)?,
                None => None,
            }
        }
        None => None,
    };
    // No recipient named: the reply goes to whoever sent the parent.
    let to = match (to, &parent) {
        (Some(t), _) => t.clone(),
        // A cancel goes to the assignment's target, not back to its asker.
        (None, Some(p)) if kind == "cancel" => Key(p.to_key.clone()),
        (None, Some(p)) => Key(p.from_key.clone()),
        // Not persisted: a later message may make this reply_to known.
        (None, None) => return unstored("no_recipient"),
    };
    if let Some((old_to, same)) = store.sent_as(&from.0, request_id, text)? {
        if old_to != to.0 || !same {
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
        return looped(RATE);
    }

    // Only an assignment (taken or refused where the worker would run) and its cancel go to
    // a fresh worker's address.
    if is_new_worker(&to.0) && !matches!(kind, "assign" | "cancel") {
        return refused("no_worker");
    }
    // A plain message answering an assignment addressed to `from`, sent back to its asker, is
    // its result.
    let mut kind = kind;
    if kind == "message"
        && parent
            .as_ref()
            .is_some_and(|p| answers_assign(p, &from.0) && p.from_key == to.0)
    {
        kind = "result";
        meta.status.get_or_insert_with(|| "done".into());
    }
    if let Err(reason) = check_kind(kind, &meta, parent.as_ref(), &from.0, &to.0) {
        return refused(reason);
    }

    let id = ulid::Ulid::generate().to_string();
    let (thread, hops) = match &parent {
        Some(p) => (p.thread.clone().unwrap_or(p.id.clone()), p.hops + 1),
        None => {
            // The trigger: the newest message delivered to `from` in the window. Hops carry on
            // from it even when the work is forwarded to a third session.
            let trigger: Option<(String, Option<String>, i64, String)> = conn
                .query_row(
                    &format!(
                        "SELECT m.id, m.thread, m.hops, m.from_key FROM messages m {MSG_RECEIPT} \
                         WHERE m.to_key = ?1 AND m.kind NOT IN ('result', 'cancel') \
                         AND r.state IN ('delivered', 'read') AND r.updated_at > ?2 \
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
        return looped(HOPS);
    }
    Ok(Prep::Go(Prepared {
        message_id: id,
        from: from.clone(),
        to,
        text: text.into(),
        request_id: request_id.into(),
        reply_to: parent.map(|p| p.id),
        thread,
        hops,
        kind: kind.into(),
        meta,
        sent_at: now as i64,
    }))
}

/// Writes `p`'s message (from `origin`) with receipt `state`.
pub(crate) fn put(store: &Store, p: &Prepared, origin: Origin, state: State) -> Result<()> {
    let mut m = Message::from(p);
    match origin {
        Origin::Net(short) => m.origin = Some(format!("net:{short}")),
        Origin::App => m.origin = Some(APP_ORIGIN.into()),
        Origin::Local => {}
    }
    store.put_message(&m)?;
    let at = p.sent_at as u64;
    advance(
        store.conn(),
        &p.from.0,
        &p.request_id,
        state,
        None,
        None,
        at,
    )?;
    Ok(())
}

/// Answering a message read it.
fn answered(store: &Store, p: &Prepared) -> Result<()> {
    if let Some(pid) = &p.reply_to {
        if store.message(pid)?.is_some_and(|m| m.to_key == p.from.0) {
            mark_read(store, pid)?;
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

/// A stored message's `origin` when a hosted app's session on this computer sent it.
pub const APP_ORIGIN: &str = "app";

/// Stores `p` here, in its recipient's mailbox; its receipt.
pub fn store_local(store: &Store, p: &Prepared) -> Result<Receipt> {
    store_local_from(store, p, Origin::Local)
}

/// `store_local`, from `origin` (`Local`, or `App`).
pub fn store_local_from(store: &Store, p: &Prepared, origin: Origin) -> Result<Receipt> {
    let tx = store.conn().unchecked_transaction()?;
    record(store, p, origin, true)?;
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
    match prepare(
        store,
        from,
        to,
        text,
        request_id,
        reply_to,
        "message",
        Meta::default(),
        now,
    )? {
        Prep::Go(p) => store_local(store, &p),
        Prep::Done(r) => Ok(r),
    }
}

/// What member `peer`'s envelope `env` becomes on this computer (member prefix `me`), checked
/// in this order: a `from` outside `peer`'s prefix is dropped unanswered (`Err`); a `kind` outside `KINDS` (`message`, `assign`, `result`, `cancel`; the `app_*` kinds never reach here: `net::receive` drops them), any `files`, a body over `MAX_TEXT`, or a malformed id, `thread` or `request_id`
/// is `unsupported`; a `to` that is not one address is `policy`, one under another prefix
/// `not_member`; a `to` of the form `<member>/<provider>:new` is `no_worker` (this build starts
/// no workers); an envelope seen before gets the state it has now and is not delivered again;
/// a `request_id` reused with another text or recipient is `request_id_reused`; a kind whose
/// rules fail (`check_kind`, with `reply_to` resolved to the stored message) is refused with
/// its reason; `hops` at
/// `MAX_HOPS` or more, or below one past a message of ours it answers (`hops_ok`), or the
/// `LOOP_LIMIT`th send in `LOOP_WINDOW_S` from one member to `to`, is `loop`; then
/// stored for `to` with origin `net:<peer short>`, timed by its arrival (`now`), and
/// remembered in `seen_ids`. (No one holds a member's message: membership is the gate.) On a workspace member, `declared` are its sessions: new mail for
/// any other is `policy` (after the resend checks, so a resend gets the state it has). With
/// `members` (the member keys the workspace answers: those whose sign-in proof names one of its
/// people, `proof::admitted`), mail from a peer whose key
/// is not listed is `not_a_member`: refused, no hold.
#[allow(clippy::too_many_arguments)]
pub fn receive(
    store: &Store,
    env: &Frame,
    peer: &Member,
    roster: &Roster,
    me: &str,
    declared: Option<&std::collections::HashSet<String>>,
    members: Option<&std::collections::HashSet<String>>,
    config: &Config,
    now: u64,
) -> Result<Answer> {
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
        meta,
        ..
    } = env
    else {
        return Err("not an envelope".into());
    };
    let refused = |r: &str| Ok(Answer::refused(r, None));
    let looped = |l: &str| Ok(Answer::refused("loop", Some(l)));
    let address = |a: &str| sessions::qualify("", a).as_deref() == Some(a);
    let ulid = |a: &str| ulid::Ulid::from_string(a).is_ok();
    if !address(from) || sessions::member_of(from) != Some(peer.short.as_str()) {
        return Err("an envelope from outside its member's prefix".into());
    }
    if !KINDS.contains(&kind.as_str())
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
    if is_new_worker(to) && !matches!(kind.as_str(), "assign" | "cancel") {
        return refused("no_worker");
    }
    if let Some((state, reason)) = store.seen_state(&peer.key, id)? {
        return Ok(Answer {
            reason,
            ..Answer::is(State::parse(&state)?)
        });
    }
    if let Some((old_to, same)) = store.sent_as(from, request_id, body)? {
        if old_to != *to || !same {
            return refused("request_id_reused");
        }
        // The same message under another envelope id: where it is now.
        let sender = Key(from.clone());
        let r = receipt(store, &sender, request_id)?.ok_or("message without a receipt")?;
        return Ok(Answer {
            reason: r.reason,
            limit: r.limit,
            ..Answer::is(r.state)
        });
    }
    // `reply_to` is a message id on the wire.
    let parent = match reply_to {
        Some(r) => store.message(r)?,
        None => None,
    };
    if let Err(reason) = check_kind(kind, meta, parent.as_ref(), from, to) {
        return refused(reason);
    }
    if declared.is_some_and(|d| !d.contains(to)) {
        return refused("policy");
    }
    if members.is_some_and(|m| !m.contains(&peer.key)) {
        return refused("not_a_member");
    }
    // An answer to (or in the thread of) a message this computer sent is at least one hop
    // past it, whatever `hops` the peer claims; only a person (`/terminal`, fenced by the
    // sending daemon) starts again at 0.
    let floor = store.hop_floor([reply_to.as_deref(), thread.as_deref()])?;
    let hops = i64::from(*hops);
    if !hops_ok(hops, floor, is_person(from)) {
        return looped(HOPS);
    }
    let since = now.saturating_sub(LOOP_WINDOW_S) as i64;
    let same_member =
        |(_, origin): &(String, String)| origin.strip_prefix("net:") == Some(peer.short.as_str());
    let recent = store.net_mail_to(to, since)?;
    if loop_refused(recent.iter().filter(|r| same_member(r)).count()) {
        return looped(RATE);
    }
    // Not in my own roster (removed): nothing here is mine to accept. Every member of the
    // network is trusted: membership is the gate.
    if roster.by_short(me).is_none() {
        return refused("not_member");
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
        meta: meta.clone(),
        sent_at: now as i64,
    };
    let tx = store.conn().unchecked_transaction()?;
    // A cancel reaches the worker module only for an assignment to a fresh worker.
    let worker_cancel = parent.as_ref().is_some_and(|p| is_new_worker(&p.to_key));
    if is_new_worker(to) && (kind == "assign" || worker_cancel) {
        // A fresh worker's address has no mailbox: the assignment is refused, held for the
        // person or started; its cancel is stored and reaches the worker module.
        let mut taken = None;
        let mut cancelled = None;
        let state = if kind == "assign" {
            let mut m = Message::from(&p);
            m.origin = Some(format!("net:{}", peer.short));
            let t = workers::take_assign(store, config, me, &m, now as i64)?;
            let state = match t {
                Taken::Refused(r) => return refused(r), // nothing written: the tx rolls back
                Taken::Held { .. } => State::Held,
                Taken::Start => State::Stored,
            };
            taken = Some(t);
            state
        } else {
            put(store, &p, Origin::Net(&peer.short), State::Stored)?;
            cancelled = p.reply_to.clone();
            State::Stored
        };
        store.put_seen(&peer.key, id, state.name(), now as i64)?;
        tx.commit()?;
        return Ok(Answer {
            fresh: true,
            taken,
            cancelled,
            ..Answer::is(state)
        });
    }
    // Seen with what the sender is told, so a resend makes no second copy.
    let state = State::Stored;
    record(store, &p, Origin::Net(&peer.short), false)?;
    store.put_seen(&peer.key, id, state.name(), now as i64)?;
    tx.commit()?;
    Ok(Answer {
        fresh: true,
        ..Answer::is(state)
    })
}

/// What senders are owed about the messages they sent here: a `Receipt` for each whose state
/// moved past what they were told, `(sender member, frame)`; `told` once sent.
pub fn untold(store: &Store) -> Result<Vec<(String, Frame)>> {
    let mut out = vec![];
    for u in store.untold()? {
        if State::parse(&u.state)?.later_than(State::parse(&u.told)?) {
            let (id, state, reason, limit) = (u.id, u.state, u.reason, None);
            out.push((
                u.sender,
                Frame::Receipt {
                    id,
                    state,
                    reason,
                    limit,
                },
            ));
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

/// The messages to member `short`'s names (`<short>/<name>`) that were delivered and are not
/// yet read, oldest first: taken from the mailbox, their text kept until `mark_read`. Only
/// those delivered within `outbox::TTL_S` of `now`; `expire_delivered` ends the rest.
pub fn delivered_unread(store: &Store, short: &str, now: u64) -> Result<Vec<Message>> {
    let since = now.saturating_sub(crate::outbox::TTL_S) as i64;
    let ids = delivered_ids(store, short, "r.updated_at >= ?3", since)?;
    let mut out = vec![];
    for id in ids {
        out.extend(store.message(&id)?);
    }
    Ok(out)
}

/// Member `short`'s delivered-unread mail (`delivered_unread`) delivered more than
/// `outbox::TTL_S` before `now` becomes `expired` (its text is `forget`'s then): a turn that
/// never completes is not handed out for ever.
pub fn expire_delivered(store: &Store, short: &str, now: u64) -> Result<()> {
    let before = now.saturating_sub(crate::outbox::TTL_S) as i64;
    let tx = store.conn().unchecked_transaction()?;
    for id in delivered_ids(store, short, "r.updated_at < ?3", before)? {
        if let Some(m) = store.message(&id)? {
            advance(
                &tx,
                &m.from_key,
                &m.request_id,
                State::Expired,
                None,
                None,
                now,
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Closes member `me`'s unanswered assignments whose deadline (`meta.deadline_at`) is at or
/// before `now`: each gets a `failed` / `timed_out` result from the assignee to the asker,
/// stored in the asker's mailbox, one transaction each (the caller holds the store lock); one
/// that fails is logged by id and skipped. Returns each asker's key and the timeout result's
/// message id, to wake. A real result arriving later is ordinary mail: `result_of` keeps
/// naming the oldest, this one.
pub fn expire_assignments(store: &Store, me: &str, now: u64) -> Result<Vec<(Key, String)>> {
    let mut askers = vec![];
    for a in store.open_assignments_past(&format!("{me}/"), now as i64)? {
        let at = a.meta.deadline_at.unwrap_or_default();
        let p = Prepared {
            message_id: ulid::Ulid::generate().to_string(),
            from: Key(a.to_key.clone()),
            to: Key(a.from_key.clone()),
            text: format!(
                "deadline passed: no result from {} by {}",
                a.to_key,
                crate::date::local_time(at)
            ),
            request_id: format!("timeout-{}", a.id),
            reply_to: Some(a.id.clone()),
            thread: a.thread.clone().unwrap_or_else(|| a.id.clone()),
            hops: a.hops + 1,
            kind: "result".into(),
            meta: Meta {
                status: Some("failed".into()),
                reason: Some("timed_out".into()),
                ..Meta::default()
            },
            sent_at: now as i64,
        };
        // One transaction each: an assignment that cannot be closed never stops the others.
        let stored = (|| -> Result<()> {
            let tx = store.conn().unchecked_transaction()?;
            record(store, &p, Origin::Local, false)?;
            Ok(tx.commit()?)
        })();
        match stored {
            Ok(()) => askers.push((Key(a.from_key), p.message_id)),
            Err(e) => eprintln!("mail: assignment {} not timed out: {e}", a.id),
        }
    }
    Ok(askers)
}

/// The ids of `delivered_unread`'s messages for which `bound` (on `r.updated_at`, the delivery
/// time, against `?3` = `at`) holds, oldest first.
fn delivered_ids(store: &Store, short: &str, bound: &str, at: i64) -> Result<Vec<String>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT m.id FROM messages m JOIN receipts r \
         ON r.from_key = m.from_key AND r.request_id = m.request_id \
         WHERE r.state = 'delivered' AND m.body IS NOT NULL AND substr(m.to_key, 1, ?2) = ?1 \
         AND {bound} AND NOT EXISTS (SELECT 1 FROM mailbox b WHERE b.message_id = m.id) \
         ORDER BY m.rowid"
    ))?;
    let prefix = format!("{short}/");
    let ids = stmt.query_map(params![prefix, prefix.len() as i64, at], |r| {
        r.get::<_, String>(0)
    })?;
    Ok(ids.collect::<rusqlite::Result<_>>()?)
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
            advance(
                &tx,
                &m.from_key,
                &m.request_id,
                State::Delivered,
                None,
                None,
                now,
            )?;
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

/// Marks message `message_id` read if it reached its recipient (`stored` or `delivered`): a
/// `held` receipt moves only by its person's answer or its expiry (`holds`).
pub fn mark_read(store: &Store, message_id: &str) -> Result<()> {
    // `updated_at` stays the delivery time: the hops trigger window runs on it.
    store.conn().execute(
        "UPDATE receipts SET state = 'read' WHERE state IN ('stored', 'delivered') \
         AND (from_key, request_id) = (SELECT from_key, request_id FROM messages WHERE id = ?1)",
        [message_id],
    )?;
    Ok(())
}

/// Drops the text of every message nothing waits on any more: its receipt is final (`read`,
/// `refused` or `expired`) and no mailbox or outbox row names it. The row stays (loop limits,
/// threads and receipts read it) with the text's digest (`body_sha`, for a retry's compare);
/// a person's own terminal mailbox keeps its text.
// ponytail: a scan of the rows still holding text on the expiry timer; a trigger if it shows.
pub fn forget(store: &Store) -> Result<()> {
    let tx = store.conn().unchecked_transaction()?;
    let done: Vec<(String, String)> = {
        let mut stmt = tx.prepare(
        "SELECT id, body FROM messages WHERE body IS NOT NULL AND to_key NOT LIKE '%/terminal' \
         AND EXISTS (SELECT 1 FROM receipts r WHERE r.from_key = messages.from_key \
             AND r.request_id = messages.request_id AND r.state IN ('read', 'refused', 'expired')) \
         AND NOT EXISTS (SELECT 1 FROM mailbox b WHERE b.message_id = messages.id) \
         AND NOT EXISTS (SELECT 1 FROM outbox o WHERE o.message_id = messages.id)",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (id, body) in done {
        tx.execute(
            "UPDATE messages SET body = NULL, body_sha = ?2 WHERE id = ?1",
            params![id, crate::store::body_sha(&body)],
        )?;
    }
    tx.commit()?;
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
        advance(&tx, &from, &request_id, State::Expired, None, None, now)?;
    }
    tx.execute("DELETE FROM mailbox WHERE expires_at < ?1", [now as i64])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_no_worker_refusal_has_a_note() {
        let r = serde_json::json!({"state": "refused", "reason": "no_worker", "person": "aden", "where": "mac"});
        assert_eq!(receipt_note(&r, None).as_deref(), Some("not sent: that computer does not take fresh workers yet (find_workers lists who takes assignments)"));
    }

    #[test]
    fn worker_receipts_name_the_person_in_both_notes() {
        let r = |state: &str, reason: Option<&str>| serde_json::json!({"state": state, "reason": reason, "person": "aden", "where": "mac"});
        let note = |v| receipt_note(&v, None).unwrap();
        assert_eq!(note(r("held", None)), "held until aden allows it");
        let to_aden = ForPerson {
            to: Some("m/claude:new"),
            rid: "r1",
            cmd: "lightbulb",
            check: false,
        };
        assert_eq!(
            receipt_note(&r("held", None), Some(&to_aden)).unwrap(),
            "Not yet sent: held until aden allows it."
        );
        assert_eq!(
            note(r("refused", Some("declined"))),
            "not sent: aden declined to start a worker"
        );
        assert!(note(r("refused", Some("busy"))).contains("running all the workers it allows"));
        let p = ForPerson {
            to: Some("m/claude:new"),
            rid: "r1",
            cmd: "lightbulb",
            check: false,
        };
        let said = receipt_note(&r("refused", Some("declined")), Some(&p)).unwrap();
        assert_eq!(said, "Not sent: aden declined to start a worker.");
    }

    #[test]
    fn a_bad_request_refusal_names_the_terms() {
        let p = ForPerson {
            to: Some("m/codex:b"),
            rid: "r1",
            cmd: "lightbulb",
            check: false,
        };
        let r = serde_json::json!({"state": "refused", "reason": "bad_request"});
        assert_eq!(
            receipt_note(&r, Some(&p)).as_deref(),
            Some("Not sent: the assignment's terms were refused.")
        );
    }

    #[test]
    fn notes_name_the_loop_limit_or_say_a_limit() {
        let r = |limit: Option<&str>| {
            receipt_note(
                &serde_json::json!({"state": "refused", "reason": "loop", "limit": limit}),
                None,
            )
            .unwrap()
        };
        assert_eq!(
            r(Some("hops")),
            "paused after 19 agent replies; a person must speak"
        );
        assert_eq!(
            r(Some("rate")),
            "loop limit: 12 messages in 10 minutes to this session"
        );
        assert_eq!(r(None), "paused by a loop limit");
        assert_eq!(
            receipt_note(
                &serde_json::json!({"state": "refused", "reason": "policy"}),
                None
            ),
            None
        );
        assert_eq!(
            receipt_note(
                &serde_json::json!({"state": "stored", "where": "recipient"}),
                None
            ),
            None
        );
    }

    use crate::{os, store::Session};

    fn setup() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        (dir, store)
    }

    #[test]
    fn one_bad_assignment_never_stops_the_expiry_of_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let assign = |id: &str, from: &str| Message {
            id: id.into(),
            request_id: format!("r-{id}"),
            from_key: from.into(),
            to_key: "w/b".into(),
            kind: "assign".into(),
            thread: None,
            hops: 0,
            origin: None,
            body: "x".into(),
            sent_at: 1,
            reply_to: None,
            meta: Meta {
                deadline_at: Some(5),
                ..Meta::default()
            },
        };
        store.put_message(&assign("a1", "me/x")).unwrap();
        store.put_message(&assign("a2", "me/y")).unwrap();
        // Takes the first timeout's (from_key, request_id) so its store trips UNIQUE.
        let mut squat = assign("squat", "w/b");
        squat.request_id = "timeout-a1".into();
        squat.kind = "message".into();
        store.put_message(&squat).unwrap();
        let woken = expire_assignments(&store, "me", 10).unwrap();
        let [(key, id)] = &woken[..] else {
            panic!("{woken:?}")
        };
        assert_eq!(key, &Key("me/y".into()));
        assert_eq!(
            store.message(id).unwrap().unwrap().reply_to.as_deref(),
            Some("a2")
        );
        assert!(store.result_of("a2").unwrap().is_some());
        assert!(store.result_of("a1").unwrap().is_none());
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
                repo: None,
                branch: None,
                handle: None,
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
        advance(s.conn(), "m/codex:a", "r1", State::Stored, None, None, 104).unwrap();
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
        assert_eq!(r.limit.as_deref(), Some(RATE));
        assert_eq!(
            receipt(&s, &k("a"), "r12")
                .unwrap()
                .unwrap()
                .limit
                .as_deref(),
            Some(RATE)
        );
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
    fn the_reply_past_max_hops_is_refused_as_hops() {
        let (_d, s) = setup();
        send(&s, &k("a"), Some(&k("b")), "x", "r0", None, 1).unwrap();
        let mut r = None;
        for i in 1..=MAX_HOPS {
            let (from, to) = if i % 2 == 1 { ("b", "a") } else { ("a", "b") };
            let parent = format!("r{}", i - 1);
            let rid = format!("r{i}");
            let got = send(&s, &k(from), Some(&k(to)), "x", &rid, Some(&parent), 2).unwrap();
            r = Some((from, rid, got));
        }
        let (from, rid, got) = r.unwrap();
        assert_eq!(got.reason.as_deref(), Some("loop"));
        let stored = receipt(&s, &k(from), &rid).unwrap().unwrap();
        assert_eq!(stored.limit.as_deref(), Some(HOPS));
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
        mark_read(&s, &thread_hops(&s, "r1").0).unwrap();
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
            admitted_at: 0,
            by: None,
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
            meta: Default::default(),
        };
        (peer, roster, env)
    }

    fn peer_env(from: &str, to: &str, rid: &str, kind: &str, meta: Meta) -> Frame {
        let (_, _, mut env) = peer_mail(to, rid);
        if let Frame::Envelope {
            from: f,
            kind: k,
            meta: m,
            ..
        } = &mut env
        {
            (*f, *k, *m) = (from.into(), kind.into(), meta);
        }
        env
    }

    #[test]
    fn a_peers_assign_is_stored_with_its_meta_and_a_new_address_is_no_worker() {
        let (_d, s) = setup();
        let (peer, roster, _) = peer_mail("m/claude:2", "x");
        let meta = terms("green", 1060);
        let env = peer_env("p/codex:a", "m/claude:2", "r1", "assign", meta);
        let got = receive(
            &s,
            &env,
            &peer,
            &roster,
            "m",
            None,
            None,
            &Config::default(),
            1000,
        )
        .unwrap();
        assert_eq!(
            got,
            Answer {
                fresh: true,
                ..Answer::is(State::Stored)
            }
        );
        let Frame::Envelope { id, .. } = &env else {
            unreachable!()
        };
        let stored = s.message(id).unwrap().unwrap();
        assert_eq!(stored.meta.done_when.as_deref(), Some("green"));
        assert_eq!(stored.kind, "assign");
        let env = peer_env(
            "p/codex:a",
            "m/claude:new",
            "r2",
            "assign",
            terms("green", 1060),
        );
        let got = receive(
            &s,
            &env,
            &peer,
            &roster,
            "m",
            None,
            None,
            &Config::default(),
            1000,
        )
        .unwrap();
        assert_eq!(
            (got.state, got.reason.as_deref()),
            (State::Refused, Some("no_worker"))
        );
    }

    #[test]
    fn a_peers_result_must_answer_an_assign_sent_to_it() {
        let (_d, s) = setup();
        let (peer, roster, _) = peer_mail("m/codex:1", "x");
        let a = go(prep(
            &s,
            "m/codex:1",
            Some("p/claude:9"),
            "a1",
            None,
            "assign",
            terms("green", 1060),
            900,
        ));
        store_local(&s, &a).unwrap();
        let done = Meta {
            status: Some("done".into()),
            ..Default::default()
        };
        let mut env = peer_env("p/claude:9", "m/codex:1", "r1", "result", done.clone());
        if let Frame::Envelope { reply_to, hops, .. } = &mut env {
            (*reply_to, *hops) = (Some(a.message_id.clone()), a.hops as u32 + 1);
        }
        // A result to a session other than the asker is refused.
        let mut third = peer_env("p/claude:9", "m/cursor:2", "r0", "result", done.clone());
        if let Frame::Envelope { reply_to, hops, .. } = &mut third {
            (*reply_to, *hops) = (Some(a.message_id.clone()), a.hops as u32 + 1);
        }
        let got = receive(
            &s,
            &third,
            &peer,
            &roster,
            "m",
            None,
            None,
            &Config::default(),
            1000,
        )
        .unwrap();
        assert_eq!(
            (got.state, got.reason.as_deref()),
            (State::Refused, Some("policy"))
        );
        let got = receive(
            &s,
            &env,
            &peer,
            &roster,
            "m",
            None,
            None,
            &Config::default(),
            1000,
        )
        .unwrap();
        assert_eq!(got.state, State::Stored);
        let Frame::Envelope { id, .. } = &env else {
            unreachable!()
        };
        let stored = s.message(id).unwrap().unwrap();
        assert_eq!(stored.meta.status.as_deref(), Some("done"));
        assert!(stored.hops >= 1);
        // A result to a plain message is not an answer to an assignment.
        let plain = peer_env("p/claude:9", "m/codex:1", "r2", "message", Meta::default());
        receive(
            &s,
            &plain,
            &peer,
            &roster,
            "m",
            None,
            None,
            &Config::default(),
            1000,
        )
        .unwrap();
        let Frame::Envelope { id: plain_id, .. } = &plain else {
            unreachable!()
        };
        let mut env = peer_env("p/claude:9", "m/codex:1", "r3", "result", done);
        if let Frame::Envelope { reply_to, hops, .. } = &mut env {
            (*reply_to, *hops) = (Some(plain_id.clone()), 1);
        }
        let got = receive(
            &s,
            &env,
            &peer,
            &roster,
            "m",
            None,
            None,
            &Config::default(),
            1000,
        )
        .unwrap();
        assert_eq!(
            (got.state, got.reason.as_deref()),
            (State::Refused, Some("policy"))
        );
    }

    #[test]
    fn a_peers_unknown_kind_is_unsupported() {
        let (_d, s) = setup();
        let (peer, roster, _) = peer_mail("m/codex:c", "x");
        let env = peer_env("p/codex:a", "m/codex:c", "r1", "poke", Meta::default());
        let got = receive(
            &s,
            &env,
            &peer,
            &roster,
            "m",
            None,
            None,
            &Config::default(),
            1000,
        )
        .unwrap();
        assert_eq!(
            (got.state, got.reason.as_deref()),
            (State::Refused, Some("unsupported"))
        );
    }

    #[test]
    fn a_peers_mail_to_a_name_address_never_expires() {
        let (_d, s) = setup();
        for (i, to) in ["m/terminal", "m/codex:c"].iter().enumerate() {
            let (peer, roster, env) = peer_mail(to, &format!("r{i}"));
            let got = receive(
                &s,
                &env,
                &peer,
                &roster,
                "m",
                None,
                None,
                &Config::default(),
                1000,
            )
            .unwrap();
            let fresh = Answer {
                fresh: true,
                ..Answer::is(State::Stored)
            };
            assert_eq!(got, fresh);
        }
        let later = 1000 + 100 * HOLD_S;
        expire(&s, later).unwrap();
        assert_eq!(waiting(&s, &k("m/terminal"), later).unwrap().len(), 1);
        assert!(waiting(&s, &k("m/codex:c"), later).unwrap().is_empty());
    }

    #[test]
    fn a_duplicate_envelope_is_reacked_with_its_reason_and_never_stored_twice() {
        let (_d, s) = setup();
        let (peer, roster, env) = peer_mail("m/codex:c", "r1");
        let again = || {
            let r = receive(
                &s,
                &env,
                &peer,
                &roster,
                "m",
                None,
                None,
                &Config::default(),
                1001,
            )
            .unwrap();
            (r.state, r.reason, r.fresh)
        };
        receive(
            &s,
            &env,
            &peer,
            &roster,
            "m",
            None,
            None,
            &Config::default(),
            1000,
        )
        .unwrap();
        advance(
            s.conn(),
            "p/codex:a",
            "r1",
            State::Refused,
            Some("declined"),
            None,
            1000,
        )
        .unwrap();
        assert_eq!(again(), (State::Refused, Some("declined".into()), false));
        // With its message and receipt rows gone, what the sender was told stands.
        s.conn()
            .execute_batch("DELETE FROM messages; DELETE FROM receipts; DELETE FROM mailbox;")
            .unwrap();
        assert_eq!(again(), (State::Stored, None, false));
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
        mark_read(&s, &a_id).unwrap();
        let got = |f: &str| receipt(&s, &k(f), "req-1").unwrap().unwrap().state;
        assert_eq!((got("a"), got("b")), (State::Read, State::Delivered));
        assert!(receipt(&s, &k("c"), "req-1").unwrap().is_none());
    }

    #[test]
    fn a_read_message_loses_its_text_but_the_terminal_keeps_its_own() {
        let (_d, s) = setup();
        register(&s, "m/codex:b", true);
        let body = |rid: &str| -> Option<String> {
            let q = "SELECT body FROM messages WHERE request_id = ?1";
            s.conn().query_row(q, [rid], |r| r.get(0)).unwrap()
        };
        send(
            &s,
            &k("m/codex:a"),
            Some(&k("m/codex:b")),
            "hi",
            "r1",
            None,
            1,
        )
        .unwrap();
        send(
            &s,
            &k("m/codex:b"),
            Some(&k("m/terminal")),
            "yo",
            "r2",
            None,
            1,
        )
        .unwrap();
        forget(&s).unwrap();
        assert_eq!(body("r1").as_deref(), Some("hi"), "still waiting");
        take(&s, &k("m/codex:b"), 2).unwrap();
        forget(&s).unwrap();
        assert_eq!(body("r1").as_deref(), Some("hi"), "delivered, not read yet");
        mark_read_for(&s, &k("m/codex:b"), 3).unwrap();
        forget(&s).unwrap();
        assert_eq!(body("r1"), None);
        // The row stays: a retry of the same request is still answered by its receipt.
        let r = send(
            &s,
            &k("m/codex:a"),
            Some(&k("m/codex:b")),
            "hi",
            "r1",
            None,
            4,
        )
        .unwrap();
        assert_eq!(r.state, State::Read);
        // Another text under it is still a reused request id: compared by its digest.
        let r = send(
            &s,
            &k("m/codex:a"),
            Some(&k("m/codex:b")),
            "ho",
            "r1",
            None,
            4,
        )
        .unwrap();
        assert_eq!(
            (r.state, r.reason.as_deref()),
            (State::Refused, Some("request_id_reused"))
        );
        // The person's own terminal keeps its history, read or not.
        take(&s, &k("m/terminal"), 2).unwrap();
        advance(s.conn(), "m/codex:b", "r2", State::Read, None, None, 3).unwrap();
        forget(&s).unwrap();
        assert_eq!(body("r2").as_deref(), Some("yo"));
    }

    #[test]
    fn a_parked_workspace_reads_as_waking_not_offline() {
        let p = ForPerson {
            to: Some("w/friday"),
            rid: "r1",
            cmd: "lightbulb",
            check: false,
        };
        let r = |waking: bool| {
            serde_json::json!({"state": "queued", "where": "Acme", "person": "Acme",
                "online": false, "waking": waking, "until": 0})
        };
        assert_eq!(
            receipt_note(&r(true), Some(&p)).unwrap(),
            "Sent. Acme is waking up and gets it in a moment."
        );
        assert!(receipt_note(&r(false), Some(&p))
            .unwrap()
            .contains("is offline"));
        assert_eq!(
            receipt_note(&r(true), None).unwrap(),
            "queued: Acme is waking up and gets it in a moment."
        );
    }

    #[test]
    fn an_expiry_says_who_did_not_come_back() {
        let p = ForPerson {
            to: Some("m/codex:b"),
            rid: "r1",
            cmd: "lightbulb",
            check: false,
        };
        let note = |r: serde_json::Value| receipt_note(&r, Some(&p)).unwrap();
        assert_eq!(
            note(serde_json::json!({"state": "expired"})),
            "codex:b did not come back within 24 hours; the message expired."
        );
        assert_eq!(
            note(serde_json::json!({"state": "expired", "reason": "unreachable"})),
            "Not delivered: the other computer could not be reached for 24 hours."
        );
        assert_eq!(
            note(serde_json::json!({"state": "expired", "reason": "not_allowed"})),
            "Nobody answered the Allow for 7 days; the message expired."
        );
        // The approval stays on the receipt as it moves to delivered.
        let dir = tempfile::tempdir().unwrap();
        let s = crate::store::Store::open(&dir.path().join("state.db")).unwrap();
        advance(
            s.conn(),
            "f",
            "r",
            State::Stored,
            Some("approved by m/claude-1"),
            None,
            1,
        )
        .unwrap();
        advance(s.conn(), "f", "r", State::Delivered, None, None, 2).unwrap();
        let kept: Option<String> = s
            .conn()
            .query_row("SELECT reason FROM receipts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept.as_deref(), Some("approved by m/claude-1"));
        assert_eq!(
            note(serde_json::json!({"state": "stored", "reason": "approved by m/claude-1"})),
            "Sent. It waits until that session is back. Approved by m/claude-1."
        );
    }

    fn terms(done_when: &str, deadline: i64) -> Meta {
        Meta {
            done_when: Some(done_when.into()),
            deadline_at: Some(deadline),
            ..Default::default()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prep(
        s: &Store,
        from: &str,
        to: Option<&str>,
        rid: &str,
        reply_to: Option<&str>,
        kind: &str,
        meta: Meta,
        now: u64,
    ) -> Prep {
        let to = to.map(k);
        prepare(
            s,
            &k(from),
            to.as_ref(),
            "t",
            rid,
            reply_to,
            kind,
            meta,
            now,
        )
        .unwrap()
    }

    fn go(p: Prep) -> Prepared {
        match p {
            Prep::Go(p) => p,
            Prep::Done(r) => panic!("refused: {r:?}"),
        }
    }

    fn refused_for(p: Prep, reason: &str) {
        match p {
            Prep::Done(r) => assert_eq!(
                (r.state, r.reason.as_deref()),
                (State::Refused, Some(reason))
            ),
            Prep::Go(p) => panic!("expected {reason}, got {p:?}"),
        }
    }

    /// Stores an assign a/codex:1 -> a/claude:2 as request `rid`; returns its message id.
    fn assigned(s: &Store, rid: &str) -> String {
        let p = go(prep(
            s,
            "a/codex:1",
            Some("a/claude:2"),
            rid,
            None,
            "assign",
            terms("tests pass", 10),
            100,
        ));
        store_local(s, &p).unwrap();
        p.message_id
    }

    #[test]
    fn an_assign_needs_done_when_and_a_deadline() {
        let (_d, s) = setup();
        let bare = prep(
            &s,
            "a/codex:1",
            Some("a/claude:2"),
            "r1",
            None,
            "assign",
            Meta::default(),
            100,
        );
        refused_for(bare, "bad_request");
        let p = go(prep(
            &s,
            "a/codex:1",
            Some("a/claude:2"),
            "r2",
            None,
            "assign",
            terms("x", 10),
            100,
        ));
        assert_eq!(p.kind, "assign");
        assert_eq!(p.meta.deadline_at, Some(10));
    }

    #[test]
    fn a_reply_to_an_assignment_is_its_result_marked_done() {
        let (_d, s) = setup();
        let aid = assigned(&s, "r1");
        let p = go(prep(
            &s,
            "a/claude:2",
            None,
            "r2",
            Some("r1"),
            "message",
            Meta::default(),
            101,
        ));
        assert_eq!(p.kind, "result");
        assert_eq!(p.meta.status.as_deref(), Some("done"));
        assert_eq!(p.hops, 1);
        assert_eq!(p.reply_to, Some(aid));
    }

    #[test]
    fn a_reply_to_someone_elses_assignment_stays_a_message() {
        let (_d, s) = setup();
        let aid = assigned(&s, "r1");
        let p = go(prep(
            &s,
            "a/cursor:3",
            Some("a/codex:1"),
            "r2",
            Some(&aid),
            "message",
            Meta::default(),
            101,
        ));
        assert_eq!(p.kind, "message");
        assert!(p.meta.is_empty());
    }

    #[test]
    fn a_reply_to_an_assignment_sent_to_a_third_session_stays_a_message() {
        let (_d, s) = setup();
        let aid = assigned(&s, "r1");
        let p = go(prep(
            &s,
            "a/claude:2",
            Some("a/cursor:3"),
            "r2",
            Some(&aid),
            "message",
            Meta::default(),
            101,
        ));
        assert_eq!(p.kind, "message");
        assert!(p.meta.is_empty());
        // An explicit result may only go back to the asker.
        let r = prep(
            &s,
            "a/claude:2",
            Some("a/cursor:3"),
            "r3",
            Some(&aid),
            "result",
            Meta::default(),
            101,
        );
        refused_for(r, "policy");
    }

    #[test]
    fn a_plain_message_carries_no_meta() {
        let (_d, s) = setup();
        let with = |rid: &str, meta: Meta| {
            prep(
                &s,
                "a/codex:1",
                Some("a/claude:2"),
                rid,
                None,
                "message",
                meta,
                100,
            )
        };
        let status = Meta {
            status: Some("done".into()),
            ..Default::default()
        };
        refused_for(with("r1", status), "bad_request");
        refused_for(with("r2", terms("x", 10)), "bad_request");
        assert_eq!(go(with("r3", Meta::default())).kind, "message");
    }

    #[test]
    fn a_result_with_a_bad_status_is_refused() {
        let (_d, s) = setup();
        assigned(&s, "r1");
        let with = |rid: &str, status: &str, reason: Option<&str>| {
            prep(
                &s,
                "a/claude:2",
                None,
                rid,
                Some("r1"),
                "result",
                Meta {
                    status: Some(status.into()),
                    reason: reason.map(Into::into),
                    ..Default::default()
                },
                101,
            )
        };
        refused_for(with("r2", "maybe", None), "bad_request");
        refused_for(with("r3", "failed", Some("bored")), "bad_request");
        assert_eq!(go(with("r4", "failed", Some("timed_out"))).kind, "result");
    }

    #[test]
    fn only_the_asker_may_cancel() {
        let (_d, s) = setup();
        let aid = assigned(&s, "r1");
        let cancel = |from: &str, rid: &str, reply_to: &str| {
            prep(
                &s,
                from,
                None,
                rid,
                Some(reply_to),
                "cancel",
                Meta::default(),
                101,
            )
        };
        refused_for(cancel("a/claude:2", "r2", &aid), "policy");
        // The asker names the assignment by its request id, or by its message id.
        let p = go(cancel("a/codex:1", "r3", "r1"));
        assert_eq!((p.kind.as_str(), p.reply_to), ("cancel", Some(aid.clone())));
        assert_eq!(p.to, k("a/claude:2"));
        let p = go(cancel("a/codex:1", "r4", &aid));
        assert_eq!((p.kind.as_str(), p.to), ("cancel", k("a/claude:2")));
    }

    #[test]
    fn only_an_assign_or_its_cancel_may_go_to_a_new_worker_address() {
        let (_d, s) = setup();
        let to = Some("a/codex:new");
        let assign = prep(
            &s,
            "a/claude:2",
            to,
            "r1",
            None,
            "assign",
            terms("x", 10),
            100,
        );
        assert_eq!(go(assign).to, k("a/codex:new"));
        refused_for(
            prep(
                &s,
                "a/claude:2",
                to,
                "r2",
                None,
                "message",
                Meta::default(),
                100,
            ),
            "no_worker",
        );
        assert!(is_new_worker("a/codex:new"));
        assert!(!is_new_worker("a/codex:1"));
        assert!(!is_new_worker("a/other:new"));
    }

    /// Stores the assign `from` -> `to` (a worker address) as request `rid` in no mailbox.
    fn assigned_new(s: &Store, from: &str, to: &str, rid: &str) -> Prepared {
        let p = go(prep(
            s,
            from,
            Some(to),
            rid,
            None,
            "assign",
            terms("x", 1060),
            900,
        ));
        put(s, &p, Origin::Local, State::Stored).unwrap();
        p
    }

    #[test]
    fn a_new_assignment_is_answered_by_the_worker_started_for_it() {
        let (_d, s) = setup();
        let a = assigned_new(&s, "a/codex:1", "m/claude:new", "r1");
        assert_eq!(new_key_of("m/claude:w1").as_deref(), Some("m/claude:new"));
        assert_eq!(new_key_of("m/claude:new"), None);
        assert_eq!(new_key_of("m/inbox"), None);
        // A plain reply from the worker (by request id) finds the assignment and is a result.
        let p = go(prep(
            &s,
            "m/claude:w1",
            None,
            "w-r1",
            Some("r1"),
            "message",
            Meta::default(),
            901,
        ));
        assert_eq!((p.kind.as_str(), p.to.0.as_str()), ("result", "a/codex:1"));
        assert_eq!(p.reply_to.as_deref(), Some(a.message_id.as_str()));
        assert_eq!(p.meta.status.as_deref(), Some("done"));
        // Another harness's session did not get this assignment.
        refused_for(
            prep(
                &s,
                "m/codex:x",
                Some("a/codex:1"),
                "w-r2",
                Some("r1"),
                "result",
                Meta::default(),
                901,
            ),
            "policy",
        );
    }

    #[test]
    fn a_workers_reply_binds_to_its_own_assignment() {
        let (_d, s) = setup();
        // Two askers, one request id, one worker address.
        let first = assigned_new(&s, "a/codex:1", "m/claude:new", "r1");
        let second = assigned_new(&s, "b/codex:2", "m/claude:new", "r1");
        assert_ne!(first.message_id, second.message_id);
        s.insert_worker(&crate::store::Worker {
            id: 0,
            assignment: first.message_id.clone(),
            session: "m/claude:w1".into(),
            provider: "claude".into(),
            folder: "/f".into(),
            pid: None,
            started_at: 1,
            ended_at: None,
            state: "running".into(),
            by: None,
        })
        .unwrap();
        // By request id or by message id, the worker's reply is the first assignment's result.
        for (rid, reply_to) in [("w-1", "r1"), ("w-2", first.message_id.as_str())] {
            let p = go(prep(
                &s,
                "m/claude:w1",
                None,
                rid,
                Some(reply_to),
                "message",
                Meta::default(),
                901,
            ));
            assert_eq!((p.kind.as_str(), p.to.0.as_str()), ("result", "a/codex:1"));
            assert_eq!(p.reply_to.as_deref(), Some(first.message_id.as_str()));
        }
    }

    #[test]
    fn receive_accepts_a_result_from_the_workers_real_key() {
        let (_d, s) = setup();
        let (peer, roster, _) = peer_mail("m/codex:1", "x");
        // The asker's copy: addressed to the host's `:new`.
        let a = assigned_new(&s, "m/codex:1", "p/claude:new", "a1");
        let done = Meta {
            status: Some("done".into()),
            ..Default::default()
        };
        let mut env = peer_env("p/claude:abc", "m/codex:1", "r1", "result", done);
        if let Frame::Envelope { reply_to, hops, .. } = &mut env {
            (*reply_to, *hops) = (Some(a.message_id.clone()), a.hops as u32 + 1);
        }
        let got = receive(
            &s,
            &env,
            &peer,
            &roster,
            "m",
            None,
            None,
            &Config::default(),
            1000,
        )
        .unwrap();
        assert_eq!(got.state, State::Stored);
    }

    #[test]
    fn take_assign_refuses_holds_or_starts() {
        let (dir, s) = setup();
        let me = crate::identity::Identity::load_or_create(dir.path()).unwrap();
        let assign = |rid: &str, from: &str| Message {
            id: ulid::Ulid::generate().to_string(),
            request_id: rid.into(),
            from_key: from.into(),
            to_key: "m/claude:new".into(),
            kind: "assign".into(),
            thread: None,
            hops: 0,
            origin: None,
            body: "x".into(),
            sent_at: 1,
            reply_to: None,
            meta: terms("x", 100),
        };
        let mut config = Config::default();
        let take =
            |config: &Config, a: &Message| workers::take_assign(&s, config, "m", a, 10).unwrap();
        let st = |a: &Message| state(&s, &a.request_id);
        // Not offered, nor for this harness.
        let a = assign("r1", "p/codex:a");
        assert_eq!(take(&config, &a), Taken::Refused("no_worker"));
        config.workers_offer = "codex".into(); // another harness's worker is not this one
        assert_eq!(take(&config, &a), Taken::Refused("no_worker"));
        config.workers_offer = "claude".into();
        // Busy at the cap.
        config.workers_max = 1;
        s.insert_worker(&crate::store::Worker {
            id: 0,
            assignment: "x".into(),
            session: "m/claude:w".into(),
            provider: "claude".into(),
            folder: "/f".into(),
            pid: None,
            started_at: 1,
            ended_at: None,
            state: "running".into(),
            by: None,
        })
        .unwrap();
        let b = assign("r2", "p/codex:a");
        assert_eq!(take(&config, &b), Taken::Refused("busy"));
        config.workers_max = 3;
        // Refused by policy, nothing written.
        workers::set_policy(&s, &me, "q", "refused", "me", 1).unwrap();
        let c = assign("r3", "q/codex:a");
        assert_eq!(take(&config, &c), Taken::Refused("declined"));
        assert!(s.message(&c.id).unwrap().is_none());
        // Ask: held, with a hold row and receipt.
        let d = assign("r4", "p/codex:b");
        let Taken::Held { hold } = take(&config, &d) else {
            panic!()
        };
        let h = crate::holds::get(&s, hold).unwrap().unwrap();
        assert_eq!(
            (h.message.id, h.from_member, h.state),
            (d.id.clone(), "p".into(), "open".into())
        );
        assert_eq!(st(&d), State::Held);
        // Allow (the standing policy): stored, to start.
        workers::set_policy(&s, &me, "p", "always", "me", 2).unwrap();
        let e = assign("r5", "p/codex:c");
        assert_eq!(take(&config, &e), Taken::Start);
        assert_eq!(st(&e), State::Stored);
        // A peer's 30-day deadline is stored as this host's most: now + 14 400.
        let f = Message {
            meta: terms("x", 10 + 30 * 86_400),
            ..assign("r6", "p/codex:d")
        };
        assert_eq!(take(&config, &f), Taken::Start);
        let stored = s.message(&f.id).unwrap().unwrap();
        assert_eq!(stored.meta.deadline_at, Some(10 + 14_400));
        // An earlier one stays as asked.
        assert_eq!(
            s.message(&e.id).unwrap().unwrap().meta.deadline_at,
            Some(100)
        );
    }

    #[test]
    fn a_replayed_assign_to_a_worker_address_is_answered_held_once() {
        let (_d, s) = setup();
        let (peer, roster, _) = peer_mail("m/claude:new", "x");
        let config = Config {
            workers_offer: "claude".into(),
            ..Config::default()
        };
        let env = peer_env(
            "p/codex:a",
            "m/claude:new",
            "r1",
            "assign",
            terms("x", 1060),
        );
        let first = receive(&s, &env, &peer, &roster, "m", None, None, &config, 1000).unwrap();
        assert_eq!((first.state, first.fresh), (State::Held, true));
        assert!(matches!(first.taken, Some(Taken::Held { .. })));
        // The same envelope again, and the same request under another envelope id.
        let again = receive(&s, &env, &peer, &roster, "m", None, None, &config, 1001).unwrap();
        assert_eq!(
            (again.state, again.fresh, again.taken),
            (State::Held, false, None)
        );
        let other = peer_env(
            "p/codex:a",
            "m/claude:new",
            "r1",
            "assign",
            terms("x", 1060),
        );
        let again = receive(&s, &other, &peer, &roster, "m", None, None, &config, 1002).unwrap();
        assert_eq!((again.state, again.fresh), (State::Held, false));
        let holds: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM holds", [], |r| r.get(0))
            .unwrap();
        assert_eq!(holds, 1);
    }

    #[test]
    fn results_and_cancels_are_not_hop_triggers() {
        let (_d, s) = setup();
        let a = go(prep(
            &s,
            "a/codex:1",
            Some("a/claude:2"),
            "r1",
            None,
            "assign",
            terms("x", 10),
            100,
        ));
        assert_eq!(a.hops, 0);
        store_local(&s, &a).unwrap();
        let res = go(prep(
            &s,
            "a/claude:2",
            None,
            "r2",
            Some("r1"),
            "result",
            Meta::default(),
            101,
        ));
        assert_eq!(res.hops, 1);
        store_local(&s, &res).unwrap();
        take(&s, &k("a/codex:1"), 102).unwrap();
        let next = go(prep(
            &s,
            "a/codex:1",
            Some("a/claude:2"),
            "r3",
            None,
            "assign",
            terms("y", 10),
            103,
        ));
        assert_eq!(next.hops, 0);
        // Control: a plain message delivered to codex is a trigger.
        let (_d2, s2) = setup();
        let m = go(prep(
            &s2,
            "a/claude:2",
            Some("a/codex:1"),
            "q1",
            None,
            "message",
            Meta::default(),
            100,
        ));
        store_local(&s2, &m).unwrap();
        take(&s2, &k("a/codex:1"), 101).unwrap();
        let after = go(prep(
            &s2,
            "a/codex:1",
            Some("a/claude:2"),
            "q2",
            None,
            "message",
            Meta::default(),
            102,
        ));
        assert_eq!(after.hops, 1);
    }
}
