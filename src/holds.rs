//! Holds: a message from another member parked (its body in `messages`, receipt `held`, in no
//! mailbox) until its person answers, or a permission prompt the Lightbulb app filed. One
//! `holds` row each (`state` `open`, `allowed`, `declined`, `expired`; `shown` the dialog's
//! text), one dialog `H<n>` each, a signed `approvals` record per answer.

use crate::admission::NOT_OPEN;
use crate::daemon::ipc::{self, refused, Caller, Response, BAD_REQUEST, INTERNAL, NOT_FOUND};
use crate::daemon::Daemon;
use crate::frame::{agent_name, defang, Sender};
use crate::identity::Identity;
use crate::mail::{self, State};
use crate::roster::Roster;
use crate::sessions;
use crate::store::{row_message, Message, Store};
use crate::{dialog, label, Result};
use rusqlite::params;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Message,
    Permission,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Message => "message",
            Kind::Permission => "permission",
        }
    }
}

/// A permission hold lasts this long unanswered.
pub const PERMISSION_S: i64 = 600;
pub const ALLOW: &str = "allow";
pub const DECLINE: &str = "decline";

#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    pub id: i64,
    pub message: Message,
    pub target_session: String,
    pub from_session: String,
    /// The sender's member key (empty for a permission hold).
    pub from_member: String,
    pub created_at: i64,
    pub state: String,
    /// The dialog's text, once shown.
    pub shown: Option<String>,
}

impl Hold {
    pub fn name(&self) -> String {
        format!("H{}", self.id)
    }

    pub fn kind(&self) -> Kind {
        match self.message.kind.as_str() {
            "permission" => Kind::Permission,
            _ => Kind::Message,
        }
    }

    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

/// The `holds` row for `message` (from its sender to its recipient), from `member`.
fn insert(store: &Store, message: Message, member: &str, now: i64) -> Result<Hold> {
    store.conn().execute(
        "INSERT INTO holds (message_id, target_session, from_session, from_member, created_at, \
         state) VALUES (?1, ?2, ?3, ?4, ?5, 'open')",
        params![message.id, message.to_key, message.from_key, member, now],
    )?;
    Ok(Hold {
        id: store.conn().last_insert_rowid(),
        target_session: message.to_key.clone(),
        from_session: message.from_key.clone(),
        message,
        from_member: member.into(),
        created_at: now,
        state: "open".into(),
        shown: None,
    })
}

/// Files a permission hold for `session`'s `prompt` (its message is `kind` permission, from
/// and to the session, origin `permission`, with no receipt).
pub fn file_permission(store: &Store, session: &str, prompt: &str, now: i64) -> Result<Hold> {
    let id = ulid::Ulid::generate().to_string();
    let m = Message {
        request_id: format!("permission-{id}"),
        id,
        from_key: session.into(),
        to_key: session.into(),
        kind: "permission".into(),
        thread: None,
        hops: 0,
        origin: Some("permission".into()),
        body: prompt.into(),
        sent_at: now,
    };
    let tx = store.conn().unchecked_transaction()?;
    store.put_message(&m)?;
    let h = insert(store, m, "", now)?;
    tx.commit()?;
    Ok(h)
}

/// The holds `filter` selects, oldest first, each with when its target session went (its
/// row's end; no row: since the hold began).
fn rows(
    store: &Store,
    filter: &str,
    arg: &dyn rusqlite::ToSql,
) -> Result<Vec<(Hold, Option<i64>)>> {
    let mut stmt = store.conn().prepare(&format!(
        "SELECT m.*, h.id AS h_id, h.target_session AS h_to, h.from_session AS h_from, \
         h.from_member AS h_member, h.created_at AS h_at, h.state AS h_state, \
         h.shown AS h_shown, s.key IS NOT NULL AS s_row, s.ended_at AS s_end \
         FROM holds h JOIN messages m ON m.id = h.message_id \
         LEFT JOIN sessions s ON s.key = h.target_session {filter} ORDER BY h.id"
    ))?;
    let raw = stmt.query_map([arg], |r| {
        let created_at = r.get("h_at")?;
        let gone_since = if r.get("s_row")? {
            r.get("s_end")?
        } else {
            Some(created_at)
        };
        let h = Hold {
            id: r.get("h_id")?,
            message: row_message(r)?,
            target_session: r.get("h_to")?,
            from_session: r.get("h_from")?,
            from_member: r.get("h_member")?,
            created_at,
            state: r.get("h_state")?,
            shown: r.get("h_shown")?,
        };
        Ok((h, gone_since))
    })?;
    Ok(raw.collect::<rusqlite::Result<_>>()?)
}

pub fn get(store: &Store, id: i64) -> Result<Option<Hold>> {
    Ok(rows(store, "WHERE h.id = ?1", &id)?.pop().map(|(h, _)| h))
}

fn open_rows(store: &Store) -> Result<Vec<(Hold, Option<i64>)>> {
    rows(store, "WHERE h.state = ?1", &"open")
}

/// The open holds, oldest first.
pub fn open(store: &Store) -> Result<Vec<Hold>> {
    Ok(open_rows(store)?.into_iter().map(|(h, _)| h).collect())
}

/// The first 160 characters of `text` as one printable, defanged line: what cannot be shown
/// goes first, so nothing hidden can shield a forged prefix from `defang`.
fn preview(text: &str) -> String {
    let shown: String = text
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || label::printable(*c))
        .collect();
    defang(&shown)
        .chars()
        .map(|c| if matches!(c, '\n' | '\t') { ' ' } else { c })
        .take(160)
        .collect()
}

/// `session`'s agent as the person reads it: `Claude Code`, `desk mailbox`, `terminal`.
fn agent(session: &str) -> String {
    let member = sessions::member_of(session).unwrap_or_default();
    match Sender::of(member, session) {
        Sender::Agent(p) => agent_name(p),
        Sender::Name(n) => format!("{n} mailbox"),
        Sender::Terminal => "terminal".into(),
    }
}

/// `h`'s dialog: its text, its buttons and the asking session's label in it.
pub fn dialog(roster: &Roster, me: &str, h: &Hold) -> (String, Vec<&'static str>, String) {
    let who = label::render(roster, me, &h.from_session);
    let text = format!(
        "{} asks: \"{}\"",
        agent(&h.target_session),
        preview(&h.message.body)
    );
    (text, vec!["Decline", "Allow"], who)
}

/// An answered hold.
#[derive(Debug, Clone)]
pub struct Closed {
    pub hold: Hold,
    pub allow: bool,
}

impl Closed {
    pub fn verdict(&self) -> &'static str {
        if self.allow {
            ALLOW
        } else {
            DECLINE
        }
    }
}

/// The person's answer to permission hold `id` (`by` `local` or `bridge`): `Err(not_found)`,
/// `Err(not_open)` once answered or expired. A signed `approvals` record keeps the text the
/// dialog showed.
pub fn answer(
    store: &Store,
    roster: &Roster,
    me: &Identity,
    id: i64,
    allow: bool,
    by: &str,
    now: i64,
) -> Result<std::result::Result<Closed, &'static str>> {
    let Some(h) = get(store, id)? else {
        return Ok(Err(NOT_FOUND));
    };
    // (A message hold of an older build is not answerable; `expire` closes it.)
    if !h.is_open() || h.kind() != Kind::Permission {
        return Ok(Err(NOT_OPEN));
    }
    // The text the person saw; one never shown (no dialog yet) is what it would show now.
    let shown = match &h.shown {
        Some(t) => t.clone(),
        None => dialog(roster, &me.key, &h).0,
    };
    let c = Closed { hold: h, allow };
    let verdict = c.verdict();
    let tx = store.conn().unchecked_transaction()?;
    let state = if allow { "allowed" } else { "declined" };
    store.conn().execute(
        "UPDATE holds SET state = ?2 WHERE id = ?1",
        params![id, state],
    )?;
    let nonce = ulid::Ulid::generate().to_string();
    let mut record = json!({"hold": c.hold.name(), "verdict": verdict, "scope": "once",
        "by": by, "nonce": nonce, "decided_at": now, "member": me.key, "text": shown});
    record["sig"] = me.sign(&record).into();
    store.conn().execute(
        "INSERT INTO approvals VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, verdict, "once", by, nonce, now, record.to_string()],
    )?;
    tx.commit()?;
    Ok(Ok(c))
}

/// Closes the open holds past their time at `now`: a permission prompt whose session is gone or
/// that waited `PERMISSION_S`, and any message hold of an older build. Returns them.
pub fn expire(store: &Store, now: i64) -> Result<Vec<Hold>> {
    let tx = store.conn().unchecked_transaction()?;
    let mut out = vec![];
    for (h, gone_since) in open_rows(store)? {
        // A message hold of an older build is closed at once: nothing answers it any more.
        let (due, reason) = match h.kind() {
            Kind::Permission => (
                gone_since.is_some() || h.created_at <= now - PERMISSION_S,
                None,
            ),
            Kind::Message => (true, Some("not_allowed")),
        };
        if !due {
            continue;
        }
        store
            .conn()
            .execute("UPDATE holds SET state = 'expired' WHERE id = ?1", [h.id])?;
        if h.kind() == Kind::Message {
            let m = &h.message;
            let (f, rid) = (&m.from_key, &m.request_id);
            mail::advance(
                store.conn(),
                f,
                rid,
                State::Expired,
                reason,
                None,
                now as u64,
            )?;
        }
        out.push(h);
    }
    tx.commit()?;
    Ok(out)
}

/// The daemon's side of a new hold: its dialog text kept (`shown`), subscribers told `held`,
/// the person shown the dialog.
pub fn shown(d: &Daemon, roster: &Roster, store: &Store, h: &Hold) {
    let me = &d.identity.key;
    let (text, buttons, who) = dialog(roster, me, h);
    let kept = store
        .conn()
        .execute(
            "UPDATE holds SET shown = ?2 WHERE id = ?1",
            params![h.id, text],
        )
        .map(|_| (text, buttons, who));
    let (text, buttons, who) = match kept {
        Ok(t) => t,
        Err(e) => return eprintln!("hold {}: {e}", h.name()),
    };
    // No preview: subscribers include agents.
    let _ = d
        .events
        .send(json!({"event": "held", "id": h.name(), "kind": h.kind(),
        "target_session": h.target_session, "from": h.from_session, "label": who}));
    let args = dialog::args("Lightbulb", &text, &buttons);
    d.dialogs.show(&h.name(), args);
    eprintln!("hold {}: open", h.name());
}

/// The daemon's side of a closed hold (`verdict` `allow`, `decline` or `expired`; `by` null for
/// an expiry; `surface` the one that answered, if this computer's): its dialog withdrawn,
/// subscribers told `hold_closed`.
fn closed(d: &Daemon, h: &Hold, verdict: &str, by: Option<&str>, surface: Option<&'static str>) {
    d.dialogs.withdraw(&h.name());
    let event = json!({"event": "hold_closed", "id": h.name(), "by": by, "verdict": verdict});
    let _ = d.events.send(event);
    match surface {
        Some(s) => {
            d.answered_by.lock().unwrap().insert(h.name(), s);
            eprintln!("hold {}: {verdict} (from the {s})", h.name());
        }
        None => eprintln!("hold {}: {verdict}", h.name()),
    }
}

/// `answer` under the daemon's locks (from `surface`), then `closed`.
fn answer_here(
    d: &Arc<Daemon>,
    id: i64,
    allow: bool,
    by: &str,
    surface: &'static str,
) -> Result<std::result::Result<Closed, &'static str>> {
    let r = {
        let team = d.team.lock().unwrap();
        let store = d.store.lock().unwrap();
        let now = ipc::now() as i64;
        answer(&store, &team.roster, &d.identity, id, allow, by, now)?
    };
    if let Ok(c) = &r {
        closed(d, &c.hold, c.verdict(), Some(by), Some(surface));
    }
    Ok(r)
}

/// Expires what is due (`expire`) and closes their dialogs; from the 60 s mail timer.
pub fn expire_here(d: &Daemon, store: &Store, now: i64) -> Result<()> {
    for h in expire(store, now)? {
        closed(d, &h, "expired", None, None);
    }
    Ok(())
}

/// Dialog `H<n>` answered `button`: `Decline` or `Allow`.
pub fn on_dialog(d: &Arc<Daemon>, n: i64, button: &str) {
    if let Err(e) = answer_here(d, n, button != "Decline", "local", "dialog") {
        eprintln!("hold H{n}: {e}");
    }
}

/// An instance's word for a hold id it does not have (a computer's is `not_found`).
const HOLD_NOT_FOUND: &str = "hold_not_found";

/// The hold ops: `holds`, `hold.get`, `hold.answer`, `hold.file` (person-only). An
/// instance's caller (the bridge, for the workspace's people) may run `holds`, `hold.get` and
/// `hold.answer`, the last with `decided_by`, the person who answered.
pub fn op(id: u64, op: &str, body: &Value, d: &Arc<Daemon>, caller: Caller) -> Response {
    let instance = caller == Caller::Instance && matches!(op, "holds" | "hold.get" | "hold.answer");
    // An agent may answer a hold (the answer records its session); nothing else here.
    let mut agent = None;
    if !instance && !caller.is_person() {
        // Only a resolved agent session may answer; an unresolved caller is refused.
        let actor = (op == "hold.answer")
            .then(|| caller.actor(&d.store.lock().unwrap()))
            .flatten();
        let Some(actor) = actor else {
            return refused(id, "not_a_person");
        };
        agent = Some(actor);
    }
    match run(id, op, body, d, instance, agent.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{op}: {e}");
            Response::err(id, INTERNAL)
        }
    }
}

fn run(
    id: u64,
    op: &str,
    body: &Value,
    d: &Arc<Daemon>,
    instance: bool,
    agent: Option<&str>,
) -> Result<Response> {
    let me = &d.identity.key;
    let missing = if instance { HOLD_NOT_FOUND } else { NOT_FOUND };
    let hold_id = body["hold"].as_str().and_then(|s| dialog::number(s, 'H'));
    Ok(match op {
        // `holds {}` -> `{holds: [{id, kind, target_session, from, label, verified, preview,
        // created_at}]}` (`verified` as the label says it)
        "holds" => {
            let team = d.team.lock().unwrap();
            let store = d.store.lock().unwrap();
            let list: Vec<Value> = open(&store)?
                .iter()
                .map(|h| {
                    let from = sessions::member_of(&h.from_session).unwrap_or_default();
                    let m = team.roster.by_short(from);
                    json!({"id": h.name(), "kind": h.kind(), "target_session": h.target_session,
                        "from": h.from_session,
                        "label": label::render(&team.roster, me, &h.from_session),
                        "verified": m.is_some_and(|m| label::verified(m, me)),
                        "preview": preview(&h.message.body), "created_at": h.created_at})
                })
                .collect();
            Response::ok(id, json!({ "holds": list }))
        }
        // `hold.get {hold}` -> the hold with its `body`, `state` and `target_birth_micros`.
        "hold.get" => {
            let team = d.team.lock().unwrap();
            let store = d.store.lock().unwrap();
            let Some(h) = hold_id.map(|n| get(&store, n)).transpose()?.flatten() else {
                return Ok(Response::err(id, missing));
            };
            let birth = store
                .session(&h.target_session)?
                .and_then(|s| s.birth_micros);
            let r = &team.roster;
            Response::ok(
                id,
                json!({"id": h.name(), "kind": h.kind(), "target_session": h.target_session,
                    "from": h.from_session, "label": label::render(r, me, &h.from_session),
                    "body": h.message.body, "state": h.state, "created_at": h.created_at,
                    "target_birth_micros": birth}),
            )
        }
        // `hold.answer {hold, verdict: allow|decline}` -> `{id, verdict}`. On an instance also
        // `decided_by`, recorded as the answer's `by` (a computer's is `local`, an agent's its session).
        "hold.answer" => {
            let (by, surface) = match body["decided_by"].as_str() {
                _ if !instance => (agent.unwrap_or("local"), "cli"),
                Some(p) if !p.is_empty() => (p, "bridge"),
                _ => return Ok(Response::err(id, BAD_REQUEST)),
            };
            let allow = match body["verdict"].as_str() {
                Some(ALLOW) => true,
                Some(DECLINE) => false,
                _ => return Ok(Response::err(id, BAD_REQUEST)),
            };
            let Some(n) = hold_id else {
                return Ok(Response::err(id, missing));
            };
            match answer_here(d, n, allow, by, surface)? {
                Ok(c) => Response::ok(id, json!({"id": c.hold.name(), "verdict": c.verdict()})),
                Err(NOT_FOUND) => Response::err(id, missing),
                Err(code) => {
                    let by = d.answered_by.lock().unwrap().get(&format!("H{n}")).copied();
                    Response::err_with(id, code, json!({ "answered_in": by }))
                }
            }
        }
        // `hold.file {kind: "permission", session, prompt}` -> `{hold}`: the app asks the
        // person to answer an agent's permission prompt (dialog, or phone via the bridge).
        "hold.file" => {
            let short = &d.identity.short;
            let session = body["session"]
                .as_str()
                .and_then(|s| sessions::qualify(short, s))
                .filter(|s| sessions::member_of(s) == Some(short.as_str()));
            let prompt = body["prompt"].as_str().unwrap_or_default();
            let fits = body["kind"] == "permission"
                && !prompt.is_empty()
                && prompt.len() <= mail::MAX_TEXT;
            let (Some(session), true) = (session, fits) else {
                return Ok(Response::err(id, BAD_REQUEST));
            };
            let team = d.team.lock().unwrap();
            let store = d.store.lock().unwrap();
            if store.session(&session)?.is_none() {
                return Ok(refused(id, "unknown_session"));
            }
            let h = file_permission(&store, &session, prompt, ipc::now() as i64)?;
            shown(d, &team.roster, &store, &h);
            Response::ok(id, json!({ "hold": h.name() }))
        }
        _ => Response::err(id, ipc::UNKNOWN_OP),
    })
}
