//! Holds: a message from another member parked (its body in `messages`, receipt `held`, in no
//! mailbox) until its person answers, or a permission prompt the Lightbulb app filed. One
//! `holds` row each (`state` `open`, `allowed`, `declined`, `expired`; `shown` the dialog's
//! text), one dialog `H<n>` each, a signed `approvals` record per answer.

use crate::admission::NOT_OPEN;
use crate::daemon::ipc::{self, refused, Caller, Response, BAD_REQUEST, INTERNAL, NOT_FOUND};
use crate::daemon::Daemon;
use crate::frame::{agent_name, defang, Sender};
use crate::identity::Identity;
use crate::mail::{self, Origin, Prepared, State};
use crate::policy::{self, Policy, Verdict};
use crate::roster::{Member, MemberKind, Roster};
use crate::sessions::{self, Key};
use crate::store::{row_message, Message, Store};
use crate::{dialog, label, Result};
use rusqlite::params;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;

/// What an answer covers: this message, every message from its person (`people`), my own
/// workspace's member (`agents_send_without_asking`), or another workspace (`cloud_of`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Once,
    Person,
    OwnCloud,
    CloudOf,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Once => "once",
            Scope::Person => "person",
            Scope::OwnCloud => "own_cloud",
            Scope::CloudOf => "cloud_of",
        }
    }
}

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
/// A hold whose target session has been gone this long expires (`session_ended`).
pub const GONE_S: i64 = 86_400;
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

/// Keeps `p` from member `peer` on hold: its message stored `held`, in no mailbox. The caller
/// holds a transaction.
pub fn hold(store: &Store, p: &Prepared, peer: &Member, now: i64) -> Result<Hold> {
    mail::put(store, p, Origin::Net(&peer.short), State::Held)?;
    let mut m = Message::from(p);
    m.origin = Some(format!("net:{}", peer.short));
    insert(store, m, &peer.key, now)
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

/// `h`'s dialog: its text, its buttons (`Always allow` only for a cloud sender) and the
/// sender's label in it.
pub fn dialog(
    store: &Store,
    roster: &Roster,
    me: &str,
    h: &Hold,
) -> Result<(String, Vec<&'static str>, String)> {
    let who = label::render(roster, me, &h.from_session);
    let quote = preview(&h.message.body);
    let agent = agent(&h.target_session);
    let text = if h.kind() == Kind::Permission {
        format!("{agent} asks: \"{quote}\"")
    } else {
        let folder = store
            .session(&h.target_session)?
            .and_then(|s| {
                Some(preview(
                    &std::path::Path::new(&s.cwd).file_name()?.to_string_lossy(),
                ))
            })
            .filter(|f| !f.is_empty())
            .map_or(String::new(), |f| format!(" in {f}"));
        message_text(&who, &h.target_session, &agent, &folder, &quote)
    };
    let mut buttons = vec!["Decline", "Allow"];
    if always_scope(roster, me, h).is_some() {
        buttons.push("Always allow");
    }
    Ok((text, buttons, who))
}

/// A held message's dialog words: `who` wants to message the session (`agent`, in `folder`)
/// or the name at `target`, then the `quote`.
fn message_text(who: &str, target: &str, agent: &str, folder: &str, quote: &str) -> String {
    let member = sessions::member_of(target).unwrap_or_default();
    match Sender::of(member, target) {
        Sender::Name(n) => {
            format!("{who} wants to send a message to \"{n}\" on this computer.\n\"{quote}\"")
        }
        _ => format!("{who} wants to message your {agent}{folder}.\n\"{quote}\""),
    }
}

/// What "Always allow" means for `h`'s sender: `OwnCloud` for my workspace's member, `CloudOf`
/// for another cloud member, nothing for a computer (or a permission hold).
pub fn always_scope(roster: &Roster, me: &str, h: &Hold) -> Option<Scope> {
    let from = roster.members.get(&h.from_member)?;
    if h.kind() != Kind::Message || from.kind != MemberKind::Cloud {
        return None;
    }
    Some(if policy::my_workspace(roster, me, &from.key, None) {
        Scope::OwnCloud
    } else {
        Scope::CloudOf
    })
}

/// What a plain Allow means: the sender's person for a computer member, this message for a
/// cloud member (and a permission prompt).
pub fn allow_scope(roster: &Roster, h: &Hold) -> Scope {
    match roster.members.get(&h.from_member) {
        Some(m) if h.kind() == Kind::Message && m.kind == MemberKind::Computer => Scope::Person,
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

impl Closed {
    pub fn verdict(&self) -> &'static str {
        if self.allow {
            ALLOW
        } else {
            DECLINE
        }
    }
}

/// The person's answer to hold `id` (`by` `local` or `bridge`): `Err(not_found)`,
/// `Err(not_open)` once answered or expired, `Err(bad_request)` for a scope that does not fit
/// the sender. `scope` is a `Scope` word, `always` (`always_scope`), or none (`allow_scope`);
/// `OwnCloud` fits only my workspace's member, `CloudOf` only another cloud member, `Person`
/// only a message. Allow puts the message in its recipient's mailbox (`stored`) and, past
/// `Once`, allows its sender (`policy::set`); decline makes it `refused declined`. Either way
/// a signed `approvals` record keeps the text the dialog showed.
#[allow(clippy::too_many_arguments)]
pub fn answer(
    store: &Store,
    roster: &Roster,
    me: &Identity,
    id: i64,
    allow: bool,
    scope: Option<&str>,
    by: &str,
    now: i64,
) -> Result<std::result::Result<Closed, &'static str>> {
    let Some(h) = get(store, id)? else {
        return Ok(Err(NOT_FOUND));
    };
    if !h.is_open() {
        return Ok(Err(NOT_OPEN));
    }
    let always = always_scope(roster, &me.key, &h);
    let from = roster.members.get(&h.from_member);
    let scope = match scope {
        None => allow_scope(roster, &h),
        Some("always") => match always {
            Some(s) => s,
            None => return Ok(Err(BAD_REQUEST)),
        },
        Some(word) => match [Scope::Once, Scope::Person, Scope::OwnCloud, Scope::CloudOf]
            .into_iter()
            .find(|s| s.as_str() == word)
        {
            Some(Scope::Once) => Scope::Once,
            Some(Scope::Person) if h.kind() == Kind::Message && from.is_some() => Scope::Person,
            Some(s) if always == Some(s) => s,
            _ => return Ok(Err(BAD_REQUEST)),
        },
    };
    // The text the person saw; one never shown (no dialog yet) is what it would show now.
    let shown = match &h.shown {
        Some(t) => t.clone(),
        None => dialog(store, roster, &me.key, &h)?.0,
    };
    let c = Closed {
        hold: h,
        allow,
        scope,
    };
    let (verdict, m) = (c.verdict(), &c.hold.message);
    let tx = store.conn().unchecked_transaction()?;
    let state = if allow { "allowed" } else { "declined" };
    store.conn().execute(
        "UPDATE holds SET state = ?2 WHERE id = ?1",
        params![id, state],
    )?;
    crate::decision::withdraw(store, &c.hold.name())?;
    if c.hold.kind() == Kind::Message {
        let (f, rid, at) = (&m.from_key, &m.request_id, now as u64);
        if allow {
            store.put_mailbox(&m.to_key, &m.id, now, mail::expires_for(&m.to_key, now))?;
            mail::advance(store.conn(), f, rid, State::Stored, None, None, at)?;
            if let (Some(f), false) = (from, scope == Scope::Once) {
                policy::set(store, roster, &me.key, f, Verdict::Accept, by, now)?;
            }
        } else {
            mail::advance(
                store.conn(),
                f,
                rid,
                State::Refused,
                Some("declined"),
                None,
                at,
            )?;
        }
    }
    let nonce = ulid::Ulid::generate().to_string();
    let mut record = json!({"hold": c.hold.name(), "verdict": verdict, "scope": scope,
        "by": by, "nonce": nonce, "decided_at": now, "member": me.key, "text": shown});
    record["sig"] = me.sign(&record).into();
    store.conn().execute(
        "INSERT INTO approvals VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            verdict,
            scope.as_str(),
            by,
            nonce,
            now,
            record.to_string()
        ],
    )?;
    tx.commit()?;
    Ok(Ok(c))
}

/// Closes the open holds past their time at `now`: a message whose target session has been
/// gone (ended, or with no row here) `GONE_S` (`expired`, reason `session_ended`) or that
/// waited `mail::HELD_S` (`expired`); a permission prompt whose session is gone or that waited
/// `PERMISSION_S`. Returns them.
// ponytail: a session with no row (discovered only, never registered) counts as gone from
// the hold's start; register it to keep its holds the full 7 days.
pub fn expire(store: &Store, now: i64) -> Result<Vec<Hold>> {
    let tx = store.conn().unchecked_transaction()?;
    let mut out = vec![];
    for (h, gone_since) in open_rows(store)? {
        let ended = gone_since.is_some_and(|t| t <= now - GONE_S);
        let (due, reason) = match h.kind() {
            Kind::Permission => (
                gone_since.is_some() || h.created_at <= now - PERMISSION_S,
                None,
            ),
            Kind::Message if ended => (true, Some("session_ended")),
            Kind::Message => (h.created_at <= now - mail::HELD_S as i64, None),
        };
        if !due {
            continue;
        }
        store
            .conn()
            .execute("UPDATE holds SET state = 'expired' WHERE id = ?1", [h.id])?;
        crate::decision::withdraw(store, &h.name())?;
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
    let kept = dialog(store, roster, me, h).and_then(|(text, buttons, who)| {
        store.conn().execute(
            "UPDATE holds SET shown = ?2 WHERE id = ?1",
            params![h.id, text],
        )?;
        Ok((text, buttons, who))
    });
    let (text, buttons, who) = match kept {
        Ok(t) => t,
        Err(e) => return eprintln!("hold {}: {e}", h.name()),
    };
    // No preview: subscribers include agents.
    let _ = d
        .events
        .send(json!({"event": "held", "id": h.name(), "kind": h.kind(),
        "target_session": h.target_session, "from": h.from_session, "label": who}));
    crate::decision::notify(d, roster, store, h, &who);
    let args = dialog::args("Lightbulb Connect", &text, &buttons);
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

/// `answer` under the daemon's locks (from `surface`), then `closed`, and an allowed message
/// pushed.
fn answer_here(
    d: &Arc<Daemon>,
    id: i64,
    allow: bool,
    scope: Option<&str>,
    by: &str,
    surface: &'static str,
) -> Result<std::result::Result<Closed, &'static str>> {
    let r = {
        let team = d.team.lock().unwrap();
        let store = d.store.lock().unwrap();
        let now = ipc::now() as i64;
        answer(&store, &team.roster, &d.identity, id, allow, scope, by, now)?
    };
    if let Ok(c) = &r {
        after(d, c, by, Some(surface));
    }
    Ok(r)
}

/// The daemon's side of an answer `by` someone (outside the locks): `closed`, and an allowed
/// message pushed.
pub(crate) fn after(d: &Arc<Daemon>, c: &Closed, by: &str, surface: Option<&'static str>) {
    closed(d, &c.hold, c.verdict(), Some(by), surface);
    if c.allow && c.hold.kind() == Kind::Message {
        ipc::stored(
            d,
            &Key(c.hold.message.to_key.clone()),
            Some(&c.hold.message.id),
        );
    }
}

/// Expires what is due (`expire`) and closes their dialogs; from the 60 s mail timer.
pub fn expire_here(d: &Daemon, store: &Store, now: i64) -> Result<()> {
    for h in expire(store, now)? {
        closed(d, &h, "expired", None, None);
    }
    Ok(())
}

/// Dialog `H<n>` answered `button`: `Decline`, `Allow` or `Always allow`.
pub fn on_dialog(d: &Arc<Daemon>, n: i64, button: &str) {
    let always = (button == "Always allow").then_some("always");
    if let Err(e) = answer_here(d, n, button != "Decline", always, "local", "dialog") {
        eprintln!("hold H{n}: {e}");
    }
}

/// The hold and policy ops: `holds`, `hold.get`, `hold.answer`, `hold.file`, `policy.get`,
/// `policy.set` (all but `policy.get` person-only: previews are a teammate's text).
pub fn op(id: u64, op: &str, body: &Value, d: &Arc<Daemon>, caller: Caller) -> Response {
    if op != "policy.get" && !caller.is_person() {
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
                json!({"id": h.name(), "kind": h.kind(), "target_session": h.target_session,
                    "from": h.from_session, "label": label::render(r, me, &h.from_session),
                    "body": h.message.body, "state": h.state, "created_at": h.created_at,
                    "target_birth_micros": birth, "allow": allow_scope(r, &h),
                    "always": always_scope(r, me, &h)}),
            )
        }
        // `hold.answer {hold, verdict: allow|decline, scope?}` -> `{id, verdict, scope}`;
        // `scope` is a `Scope` word or `always`.
        "hold.answer" => {
            let allow = match body["verdict"].as_str() {
                Some(ALLOW) => true,
                Some(DECLINE) => false,
                _ => return Ok(Response::err(id, BAD_REQUEST)),
            };
            let scope = match &body["scope"] {
                Value::Null => None,
                Value::String(s) => Some(s.as_str()),
                _ => return Ok(Response::err(id, BAD_REQUEST)),
            };
            let Some(n) = hold_id else {
                return Ok(Response::err(id, NOT_FOUND));
            };
            match answer_here(d, n, allow, scope, "local", "cli")? {
                Ok(c) => Response::ok(
                    id,
                    json!({"id": c.hold.name(), "verdict": c.verdict(), "scope": c.scope}),
                ),
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
        "policy.get" => Response::ok(id, json!(Policy::load(&d.store.lock().unwrap())?)),
        // `policy.set {person_id, value: accept|hold|refuse}` or `{agents_send_without_asking}`
        // -> the policy. A member's person goes where its kind reads (`policy::set`); a person
        // no member is, in `people`.
        "policy.set" => {
            let team = d.team.lock().unwrap();
            let store = d.store.lock().unwrap();
            let now = ipc::now() as i64;
            if let Some(on) = body["agents_send_without_asking"].as_bool() {
                policy::set_switch(&store, on, "local", now)?;
            } else if body["value"] == "forget" {
                let Some(pid) = body["person_id"].as_str() else {
                    return Ok(Response::err(id, BAD_REQUEST));
                };
                policy::forget(&store, pid)?;
            } else {
                let value = body["value"].as_str().and_then(Verdict::parse);
                let (Some(pid), Some(v)) = (body["person_id"].as_str(), value) else {
                    return Ok(Response::err(id, BAD_REQUEST));
                };
                let r = &team.roster;
                // My own person is always accepted (`decide`): a row would only mislead.
                if r.members.get(me).is_some_and(|m| m.person_id == pid) {
                    return Ok(Response::err(id, "own_person"));
                }
                match r.members.values().find(|m| m.person_id == pid) {
                    Some(m) => policy::set(&store, r, me, m, v, "local", now)?,
                    None if pid.starts_with("ws:") => {
                        policy::set_person(&store, pid, v, "local", now)?
                    }
                    // Nobody `decide` would look up: stored, it would be inert.
                    None => return Ok(Response::err(id, NOT_FOUND)),
                }
            }
            Response::ok(id, json!(Policy::load(&store)?))
        }
        _ => Response::err(id, ipc::UNKNOWN_OP),
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_hold_for_a_name_says_so_without_mailbox_jargon() {
        assert_eq!(
            super::message_text(
                "demo2, at the terminal",
                "abc/inbox",
                "inbox mailbox",
                "",
                "hi"
            ),
            "demo2, at the terminal wants to send a message to \"inbox\" on this computer.\n\"hi\""
        );
        assert_eq!(
            super::message_text("bob's Codex", "abc/codex:t", "Codex", " in app", "hi"),
            "bob's Codex wants to message your Codex in app.\n\"hi\""
        );
    }
}
