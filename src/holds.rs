//! Holds: a message from another member parked (its body in `messages`, receipt `held`, in no
//! mailbox) until its person answers, or a permission prompt the Lightbulb app filed. One
//! `holds` row each (`state` `open`, `allowed`, `declined`, `expired`), one dialog `H<n>`
//! each, a signed `approvals` record per answer.

use crate::admission::NOT_OPEN;
use crate::daemon::ipc::{self, refused, Response, BAD_REQUEST, INTERNAL, NOT_FOUND};
use crate::daemon::{net_ops, Daemon};
use crate::frame::{agent_name, defang, Sender};
use crate::identity::Identity;
use crate::mail::{self, Origin, Prepared, State};
use crate::policy::{self, Policy, Verdict};
use crate::roster::{MemberKind, Roster};
use crate::sessions::{self, Key};
use crate::store::{Message, Store};
use crate::{dialog, label, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

/// What an answer covers: this message, every message from its person (`people`), my own
/// workspace's member (`agents_send_without_asking`), or another workspace (`cloud_of`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Once,
    Person,
    OwnCloud,
    CloudOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Message,
    Permission,
}

/// A permission hold lasts this long unanswered.
pub const PERMISSION_S: i64 = 600;
/// A hold whose target session has been gone this long expires (`session_ended`).
pub const GONE_S: i64 = 86_400;

#[derive(Debug, Clone, PartialEq)]
pub struct Hold {
    pub id: i64,
    pub kind: Kind,
    pub message: Message,
    pub target_session: String,
    pub from_session: String,
    /// The sender's member key (this computer's own for a permission hold).
    pub from_member: String,
    pub created_at: i64,
    pub state: String,
}

impl Hold {
    pub fn name(&self) -> String {
        format!("H{}", self.id)
    }
}

/// `H<n>`'s `n`.
pub fn parse_id(s: &str) -> Option<i64> {
    s.strip_prefix('H')?.parse().ok().filter(|n| *n > 0)
}

/// Keeps `p` (from member `from_member`) on hold: its message stored `held`, in no mailbox.
/// The caller holds a transaction.
pub fn hold(store: &Store, p: &Prepared, from_member: &str, now: i64) -> Result<i64> {
    let short = sessions::member_of(&p.from.0).unwrap_or_default();
    mail::put(store, p, Origin::Net(short), State::Held)?;
    insert(store, &p.message_id, &p.to.0, &p.from.0, from_member, now)
}

fn insert(store: &Store, mid: &str, to: &str, from: &str, member: &str, now: i64) -> Result<i64> {
    store.conn().execute(
        "INSERT INTO holds (message_id, target_session, from_session, from_member, created_at, \
         state) VALUES (?1, ?2, ?3, ?4, ?5, 'open')",
        params![mid, to, from, member, now],
    )?;
    Ok(store.conn().last_insert_rowid())
}

/// Files a permission hold for `session`'s `prompt` (its message is `kind` permission, from
/// and to the session, origin `permission`, with no receipt).
pub fn file_permission(store: &Store, session: &str, prompt: &str, now: i64) -> Result<Hold> {
    let id = ulid::Ulid::generate().to_string();
    let tx = store.conn().unchecked_transaction()?;
    store.put_message(&Message {
        id: id.clone(),
        request_id: format!("permission-{id}"),
        from_key: session.into(),
        to_key: session.into(),
        kind: "permission".into(),
        thread: None,
        hops: 0,
        origin: Some("permission".into()),
        body: prompt.into(),
        sent_at: now,
    })?;
    let n = insert(store, &id, session, session, "", now)?;
    tx.commit()?;
    get(store, n)?.ok_or_else(|| "hold missing after write".into())
}

const SELECT: &str = "SELECT id, message_id, target_session, from_session, from_member, \
                      created_at, state FROM holds";

fn rows(store: &Store, filter: &str, arg: rusqlite::types::Value) -> Result<Vec<Hold>> {
    let mut stmt = store
        .conn()
        .prepare(&format!("{SELECT} {filter} ORDER BY id"))?;
    let raw = stmt.query_map([arg], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;
    let mut out = vec![];
    for row in raw {
        let (id, mid, target_session, from_session, from_member, created_at, state) = row?;
        let Some(message) = store.message(&mid)? else {
            continue;
        };
        let kind = match message.kind.as_str() {
            "permission" => Kind::Permission,
            _ => Kind::Message,
        };
        out.push(Hold {
            id,
            kind,
            message,
            target_session,
            from_session,
            from_member,
            created_at,
            state,
        });
    }
    Ok(out)
}

pub fn get(store: &Store, id: i64) -> Result<Option<Hold>> {
    Ok(rows(store, "WHERE id = ?1", id.into())?.pop())
}

/// The open holds, oldest first.
pub fn open(store: &Store) -> Result<Vec<Hold>> {
    rows(store, "WHERE state = ?1", "open".to_string().into())
}

/// The hold of envelope `message_id`, if it made one.
pub fn by_message(store: &Store, message_id: &str) -> Result<Option<Hold>> {
    Ok(rows(
        store,
        "WHERE message_id = ?1",
        message_id.to_string().into(),
    )?
    .pop())
}

/// The first 160 characters of `text` as one defanged, printable line.
fn preview(text: &str) -> String {
    defang(text)
        .chars()
        .map(|c| if matches!(c, '\n' | '\t') { ' ' } else { c })
        .filter(|c| label::printable(*c))
        .take(160)
        .collect()
}

/// The sender as this computer's person is told it.
fn sender(roster: &Roster, me: &str, h: &Hold) -> String {
    label::render(roster, me, &h.from_session)
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

/// The dialog text for `h` (and what its `approvals` record keeps).
// ponytail: worked out again at answer time, so a roster rename in between shows in the record.
pub fn text(store: &Store, roster: &Roster, me: &str, h: &Hold) -> Result<String> {
    let quote = preview(&h.message.body);
    let agent = agent(&h.target_session);
    if h.kind == Kind::Permission {
        return Ok(format!("{agent} asks: \"{quote}\""));
    }
    let folder = store
        .session(&h.target_session)?
        .and_then(|s| {
            let name = std::path::Path::new(&s.cwd).file_name()?;
            Some(preview(&name.to_string_lossy()))
        })
        .filter(|f| !f.is_empty())
        .map_or(String::new(), |f| format!(" in {f}"));
    let who = sender(roster, me, h);
    Ok(format!(
        "{who} wants to message your {agent}{folder}.\n\"{quote}\""
    ))
}

/// What "Always allow" means for `h`'s sender: `OwnCloud` for my workspace's member, `CloudOf`
/// for another cloud member, nothing for a computer (or a permission hold).
pub fn always_scope(roster: &Roster, me: &str, h: &Hold) -> Option<Scope> {
    let from = roster.members.get(&h.from_member)?;
    if h.kind != Kind::Message || from.kind != MemberKind::Cloud {
        return None;
    }
    let mine = roster
        .members
        .get(me)
        .and_then(|m| roster.my_workspace_member(m))
        .is_some_and(|w| w.key == from.key);
    Some(if mine {
        Scope::OwnCloud
    } else {
        Scope::CloudOf
    })
}

/// What a plain Allow means: the sender's person for a computer member, this message for a
/// cloud member (and a permission prompt).
pub fn allow_scope(roster: &Roster, h: &Hold) -> Scope {
    match roster.members.get(&h.from_member) {
        Some(m) if h.kind == Kind::Message && m.kind == MemberKind::Computer => Scope::Person,
        _ => Scope::Once,
    }
}

/// An answered hold.
#[derive(Debug, Clone)]
pub struct Closed {
    pub hold: Hold,
    pub allow: bool,
    pub scope: Scope,
}

/// The person's answer to hold `id` (`by` `local` or `bridge`): `Err(not_found)`,
/// `Err(not_open)` once answered or expired, `Err(bad_request)` for a scope that does not fit
/// the sender (`OwnCloud` only for my workspace's member, `CloudOf` only for another cloud
/// member, `Person` only for a message). `scope` defaults to `allow_scope`. Allow puts the
/// message in its recipient's mailbox (`stored`) and writes the scope's policy; decline makes
/// it `refused declined`. Either way a signed `approvals` record keeps the text shown.
#[allow(clippy::too_many_arguments)]
pub fn answer(
    store: &Store,
    roster: &Roster,
    me: &Identity,
    id: i64,
    allow: bool,
    scope: Option<Scope>,
    by: &str,
    now: i64,
) -> Result<std::result::Result<Closed, &'static str>> {
    let Some(h) = get(store, id)? else {
        return Ok(Err(NOT_FOUND));
    };
    if h.state != "open" {
        return Ok(Err(NOT_OPEN));
    }
    let scope = scope.unwrap_or_else(|| allow_scope(roster, &h));
    let from = roster.members.get(&h.from_member);
    let fits = match scope {
        Scope::Once => true,
        Scope::Person => h.kind == Kind::Message && from.is_some(),
        s => always_scope(roster, &me.key, &h) == Some(s),
    };
    if !fits {
        return Ok(Err(BAD_REQUEST));
    }
    let shown = text(store, roster, &me.key, &h)?;
    let verdict = if allow { "allow" } else { "decline" };
    let tx = store.conn().unchecked_transaction()?;
    let state = if allow { "allowed" } else { "declined" };
    store.conn().execute(
        "UPDATE holds SET state = ?2 WHERE id = ?1",
        params![id, state],
    )?;
    let m = &h.message;
    if h.kind == Kind::Message {
        if allow {
            store.put_mailbox(&m.to_key, &m.id, now, mail::expires_for(&m.to_key, now))?;
            let rid = &m.request_id;
            mail::advance(
                store.conn(),
                &m.from_key,
                rid,
                State::Stored,
                None,
                now as u64,
            )?;
            let pid = from.map_or("", |f| f.person_id.as_str());
            match scope {
                Scope::Once => {}
                Scope::Person => policy::set(store, "people", pid, Verdict::Accept, by, now)?,
                Scope::CloudOf => policy::set(store, "cloud_of", pid, Verdict::Accept, by, now)?,
                Scope::OwnCloud => policy::set_switch(store, true, by, now)?,
            }
        } else {
            let (f, rid) = (&m.from_key, &m.request_id);
            mail::advance(
                store.conn(),
                f,
                rid,
                State::Refused,
                Some("declined"),
                now as u64,
            )?;
        }
    }
    let nonce = ulid::Ulid::generate().to_string();
    let mut record = json!({"hold": h.name(), "verdict": verdict, "scope": scope, "by": by,
        "nonce": nonce, "decided_at": now, "member": me.key, "text": shown});
    record["sig"] = me.sign(&record).into();
    store.conn().execute(
        "INSERT INTO approvals VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            verdict,
            json!(scope).as_str(),
            by,
            nonce,
            now,
            record.to_string()
        ],
    )?;
    tx.commit()?;
    Ok(Ok(Closed {
        hold: h,
        allow,
        scope,
    }))
}

/// Closes the open holds past their time at `now`: a message whose target session has been
/// gone (ended, or with no row here) `GONE_S` (`expired`, reason `session_ended`) or that
/// waited `mail::HELD_S` (`expired`); a permission prompt whose session is gone or that waited
/// `PERMISSION_S`. Returns their ids.
// ponytail: a session with no row (discovered only, never registered) counts as gone from
// the hold's start; register it to keep its holds the full 7 days.
pub fn expire(store: &Store, now: i64) -> Result<Vec<i64>> {
    let tx = store.conn().unchecked_transaction()?;
    let mut out = vec![];
    for h in open(store)? {
        // When the target session went: its row's end, else (no row) the hold's start.
        let row: Option<Option<i64>> = store
            .conn()
            .query_row(
                "SELECT ended_at FROM sessions WHERE key = ?1",
                [&h.target_session],
                |r| r.get(0),
            )
            .optional()?;
        let ended = row.unwrap_or(Some(h.created_at));
        let gone = ended.is_some_and(|t| t <= now - GONE_S);
        let reason = match h.kind {
            Kind::Permission if ended.is_some() || h.created_at <= now - PERMISSION_S => None,
            Kind::Message if gone => Some("session_ended"),
            Kind::Message if h.created_at <= now - mail::HELD_S as i64 => None,
            _ => continue,
        };
        store
            .conn()
            .execute("UPDATE holds SET state = 'expired' WHERE id = ?1", [h.id])?;
        if h.kind == Kind::Message {
            let m = &h.message;
            let (f, rid) = (&m.from_key, &m.request_id);
            mail::advance(store.conn(), f, rid, State::Expired, reason, now as u64)?;
        }
        out.push(h.id);
    }
    tx.commit()?;
    Ok(out)
}

/// The daemon's side of a new hold: subscribers get `held`, the person a dialog.
pub fn shown(d: &Daemon, roster: &Roster, store: &Store, h: &Hold) {
    let me = &d.identity.key;
    let Ok(text) = text(store, roster, me, h) else {
        return;
    };
    let mut buttons = vec!["Decline", "Allow"];
    if always_scope(roster, me, h).is_some() {
        buttons.push("Always allow");
    }
    let _ = d
        .events
        .send(json!({"event": "held", "id": h.name(), "kind": h.kind,
        "target_session": h.target_session, "from": h.from_session,
        "label": sender(roster, me, h), "preview": preview(&h.message.body)}));
    d.dialogs.show(
        &h.name(),
        dialog::args("Lightbulb Connect", &text, &buttons),
    );
    eprintln!("hold {}: open", h.name());
}

/// The daemon's side of a closed hold (`verdict` `allow`, `decline` or `expired`; `by` null for
/// an expiry): its dialog withdrawn, subscribers told `hold_closed`.
fn closed(d: &Daemon, h: &Hold, verdict: &str, by: Option<&str>) {
    d.dialogs.withdraw(&h.name());
    let _ = d
        .events
        .send(json!({"event": "hold_closed", "id": h.name(), "by": by,
        "verdict": verdict}));
    eprintln!("hold {}: {verdict}", h.name());
}

/// `answer` under the daemon's locks, then `closed`.
fn answer_here(
    d: &Arc<Daemon>,
    id: i64,
    allow: bool,
    scope: Option<Scope>,
    by: &str,
) -> Result<std::result::Result<Closed, &'static str>> {
    let r = {
        let team = d.team.lock().unwrap();
        let store = d.store.lock().unwrap();
        let now = ipc::now() as i64;
        answer(&store, &team.roster, &d.identity, id, allow, scope, by, now)?
    };
    if let Ok(c) = &r {
        closed(
            d,
            &c.hold,
            if allow { "allow" } else { "decline" },
            Some(by),
        );
        if allow && c.hold.kind == Kind::Message {
            ipc::stored(d, &Key(c.hold.message.to_key.clone()));
        }
    }
    Ok(r)
}

/// Expires what is due (`expire`) and closes their dialogs; from the 60 s mail timer.
pub fn expire_here(d: &Daemon, store: &Store, now: i64) -> Result<()> {
    for id in expire(store, now)? {
        if let Some(h) = get(store, id)? {
            closed(d, &h, "expired", None);
        }
    }
    Ok(())
}

/// Dialog `H<n>` answered `button`: `Decline`, `Allow` or `Always allow`.
pub fn on_dialog(d: &Arc<Daemon>, n: i64, button: &str) {
    let scope = match button {
        "Always allow" => {
            let team = d.team.lock().unwrap();
            let h = get(&d.store.lock().unwrap(), n).ok().flatten();
            h.and_then(|h| always_scope(&team.roster, &d.identity.key, &h))
        }
        _ => None,
    };
    if let Err(e) = answer_here(d, n, button != "Decline", scope, "local") {
        eprintln!("hold H{n}: {e}");
    }
}

/// The hold and policy ops: `holds`, `hold.get`, `hold.answer`, `hold.file`, `policy.get`,
/// `policy.set` (all but `holds` and `policy.get` person-only).
pub fn op(id: u64, op: &str, body: &Value, d: &Arc<Daemon>, peer_pid: Option<u32>) -> Response {
    let person = matches!(op, "hold.get" | "hold.answer" | "hold.file" | "policy.set");
    if person && !peer_pid.is_some_and(net_ops::person_only) {
        return refused(id, "not_a_person");
    }
    match run(id, op, body, d) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{op}: {e}");
            Response::err(id, INTERNAL)
        }
    }
}

fn run(id: u64, op: &str, body: &Value, d: &Arc<Daemon>) -> Result<Response> {
    let me = &d.identity.key;
    let hold_id = body["hold"].as_str().and_then(parse_id);
    Ok(match op {
        // `holds {}` -> `{holds: [{id, kind, target_session, from, label, preview, created_at}]}`
        "holds" => {
            let team = d.team.lock().unwrap();
            let store = d.store.lock().unwrap();
            let list: Vec<Value> = open(&store)?
                .iter()
                .map(|h| {
                    json!({"id": h.name(), "kind": h.kind, "target_session": h.target_session,
                        "from": h.from_session, "label": sender(&team.roster, me, h),
                        "preview": preview(&h.message.body), "created_at": h.created_at})
                })
                .collect();
            Response::ok(id, json!({ "holds": list }))
        }
        // `hold.get {hold}` -> the hold with its `body`, `state`, `target_birth_micros`, and the
        // scopes its Allow and Always allow mean (`allow`, `always`, null when none).
        "hold.get" => {
            let team = d.team.lock().unwrap();
            let store = d.store.lock().unwrap();
            let Some(h) = hold_id.map(|n| get(&store, n)).transpose()?.flatten() else {
                return Ok(Response::err(id, NOT_FOUND));
            };
            let birth = store
                .session(&h.target_session)?
                .and_then(|s| s.birth_micros);
            let r = &team.roster;
            Response::ok(
                id,
                json!({"id": h.name(), "kind": h.kind, "target_session": h.target_session,
                    "from": h.from_session, "label": sender(r, me, &h), "body": h.message.body,
                    "state": h.state, "created_at": h.created_at, "target_birth_micros": birth,
                    "allow": allow_scope(r, &h), "always": always_scope(r, me, &h)}),
            )
        }
        // `hold.answer {hold, verdict: allow|decline, scope?}` -> `{id, verdict, scope}`.
        "hold.answer" => {
            let allow = match body["verdict"].as_str() {
                Some("allow") => true,
                Some("decline") => false,
                _ => return Ok(Response::err(id, BAD_REQUEST)),
            };
            let Ok(scope) = serde_json::from_value::<Option<Scope>>(body["scope"].clone()) else {
                return Ok(Response::err(id, BAD_REQUEST));
            };
            let Some(n) = hold_id else {
                return Ok(Response::err(id, NOT_FOUND));
            };
            match answer_here(d, n, allow, scope, "local")? {
                Ok(c) => Response::ok(
                    id,
                    json!({"id": c.hold.name(), "verdict": body["verdict"], "scope": c.scope}),
                ),
                Err(code) => Response::err(id, code),
            }
        }
        // `hold.file {kind: "permission", session, prompt}` -> `{hold}`: the app asks the
        // person to answer an agent's permission prompt (dialog, or phone via the bridge).
        "hold.file" => {
            let session = body["session"]
                .as_str()
                .and_then(|s| sessions::qualify(&d.identity.short, s));
            let prompt = body["prompt"].as_str().unwrap_or_default();
            let mine = session
                .as_deref()
                .is_some_and(|s| sessions::member_of(s) == Some(d.identity.short.as_str()));
            if body["kind"] != "permission"
                || !mine
                || prompt.is_empty()
                || prompt.len() > mail::MAX_TEXT
            {
                return Ok(Response::err(id, BAD_REQUEST));
            }
            let team = d.team.lock().unwrap();
            let store = d.store.lock().unwrap();
            let h = file_permission(
                &store,
                &session.unwrap_or_default(),
                prompt,
                ipc::now() as i64,
            )?;
            shown(d, &team.roster, &store, &h);
            Response::ok(id, json!({ "hold": h.name() }))
        }
        "policy.get" => Response::ok(id, json!(Policy::load(&d.store.lock().unwrap())?)),
        // `policy.set {person_id, scope: people|cloud_of, value}` or
        // `{agents_send_without_asking}` -> the policy.
        "policy.set" => {
            let store = d.store.lock().unwrap();
            let now = ipc::now() as i64;
            if let Some(on) = body["agents_send_without_asking"].as_bool() {
                policy::set_switch(&store, on, "local", now)?;
            } else {
                let value = serde_json::from_value::<Verdict>(body["value"].clone());
                let (Some(pid), Some(scope), Ok(v)) =
                    (body["person_id"].as_str(), body["scope"].as_str(), value)
                else {
                    return Ok(Response::err(id, BAD_REQUEST));
                };
                if policy::set(&store, scope, pid, v, "local", now).is_err() {
                    return Ok(Response::err(id, BAD_REQUEST));
                }
            }
            Response::ok(id, json!(Policy::load(&store)?))
        }
        _ => Response::err(id, ipc::UNKNOWN_OP),
    })
}
