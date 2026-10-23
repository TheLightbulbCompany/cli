//! Hold notices and relayed decisions (ruling §6): a computer tells its own workspace's member
//! (`policy::my_workspace`, proof in force) of every new hold, message or permission, as a
//! signed `hold_notice` of ids and the sender's label, never text; the member (an embedded
//! instance, the Lightbulb bridge) shows it on the person's phone or web and carries the tap
//! back as a signed `decision` with `decided_by`, the person who tapped.
//!
//! Both ride the outbox table: one row each, `hn:<hold id>` on the holder and `dc:<nonce>` on
//! the member, sent with the mail's backoff to the one member named, again on every `hello`,
//! until that member acks it (`ack {id: <row id>}`: state `delivered` for a notice; `applied`,
//! `not_open`, or `refused` with a `REFUSALS` reason for a decision) or, for a notice, its
//! hold closes; at most `mail::HELD_S`, then the row goes. The holder keeps each decision's
//! answer by its nonce and gives a resent copy the same one. Only a decision from a member
//! not (yet) my workspace's is left unanswered: a roster sync or a renewed proof may heal it.
//!
//! On the instance, `hold_notice` and `decision_result` are events, `decision.send` and
//! `holds.pending` the ops. A notice is acked only once the event went to a subscriber, and it
//! stays pending (`holds.pending`) until its decision's answer, or `mail::HELD_S`. Logs carry
//! ids and shorts, never text.

use crate::admission::{self, NOT_OPEN};
use crate::daemon::ipc::{self, Response, BAD_REQUEST, UNSUPPORTED};
use crate::daemon::Daemon;
use crate::doing::TEXT_MAX;
use crate::holds::{self, Closed, Hold, ALLOW, DECLINE};
use crate::identity::Identity;
use crate::net::frames::{self, Frame};
use crate::net::short;
use crate::roster::{Member, MemberKind, Roster};
use crate::store::Store;
use crate::{dialog, label, mail, policy, sessions, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

const NOTICE: &str = "hn:";
const DECISION: &str = "dc:";
/// The ack states of a notice, and of a decision.
const NOTICE_ACK: &str = "delivered";
const APPLIED: &str = "applied";
const REFUSED: &str = "refused";
/// Why a decision was refused: a scope that does not fit, a hold no longer open, a malformed
/// one or a nonce reused on another hold, a signature not the signer's, a hold not mine (or a
/// person not mine).
pub const REFUSALS: [&str; 5] = ["scope", NOT_OPEN, "bad_request", "bad_sig", "not_mine"];
/// The scopes a relayed decision may carry.
const RELAYED: [&str; 3] = ["once", "person", "own_cloud"];

/// Whether `id` names a notice's or a decision's outbox row (an envelope id never does).
pub fn is_control(id: &str) -> bool {
    id.starts_with(NOTICE) || id.starts_with(DECISION)
}

/// Whether `state` (with `reason`) may ack row `id`.
pub fn ack_ok(id: &str, state: &str, reason: Option<&str>) -> bool {
    match reason {
        _ if id.starts_with(NOTICE) => state == NOTICE_ACK && reason.is_none(),
        None => id.starts_with(DECISION) && [APPLIED, NOT_OPEN].contains(&state),
        Some(r) => id.starts_with(DECISION) && state == REFUSED && REFUSALS.contains(&r),
    }
}

fn notice_row(hold: &str) -> String {
    format!("{NOTICE}{hold}")
}

fn kind_word(h: &Hold) -> &'static str {
    match h.kind() {
        holds::Kind::Message => "message",
        holds::Kind::Permission => "permission",
    }
}

/// New hold `h` (its sender labelled `label`): a notice for my own workspace's member, if I
/// have one, into the outbox. A free user's holds tell no one.
pub(crate) fn notify(d: &Daemon, roster: &Roster, store: &Store, h: &Hold, label: &str) {
    let Some(me) = roster.members.get(&d.identity.key) else {
        return;
    };
    let Some(w) = roster.my_workspace_member(me) else {
        return;
    };
    let f = admission::signed(
        &d.identity,
        Frame::HoldNotice {
            hold_id: h.name(),
            kind: kind_word(h).into(),
            holding_computer: me.computer.clone(),
            for_person: me.person_id.clone(),
            target_session: h.target_session.clone(),
            from: h.from_session.clone(),
            label: label.into(),
            // Never text (ruling §6): `preview_to_workspace` is not read.
            preview: String::new(),
            created_at: h.created_at,
            sig: String::new(),
        },
    );
    let frame = serde_json::to_string(&f).expect("a frame serializes");
    let put = store.put_outbox(&notice_row(&h.name()), &w.key, &frame, ipc::now() as i64);
    match put {
        Ok(()) => {
            eprintln!("hold {}: notice for {}", h.name(), short(&w.key));
            d.peers.outbox.notify_one();
        }
        Err(e) => eprintln!("hold {}: notice: {e}", h.name()),
    }
}

/// Hold `hold` closed: its notice, if still unsent or unacked, never goes.
pub(crate) fn withdraw(store: &Store, hold: &str) -> rusqlite::Result<()> {
    store.conn().execute(
        "DELETE FROM outbox WHERE message_id = ?1",
        [notice_row(hold)],
    )?;
    Ok(())
}

/// Sends the notices and decisions due to the `online` members (with `all`, whatever their
/// backoff), from `Net::flush`.
pub(crate) fn flush(
    d: &Daemon,
    store: &Store,
    online: &[String],
    all: bool,
    now: u64,
) -> Result<()> {
    let due_at = if all { i64::MAX } else { now as i64 };
    let since = now.saturating_sub(mail::HELD_S) as i64;
    // Past the hold's 7 days nothing can come of a row.
    store.conn().execute(
        "DELETE FROM outbox WHERE substr(message_id, 1, 3) IN ('hn:', 'dc:') \
         AND created_at <= ?1",
        [since],
    )?;
    let rows: Vec<(String, String, String)> = {
        let mut stmt = store.conn().prepare(
            "SELECT message_id, to_member, frame FROM outbox WHERE substr(message_id, 1, 3) \
             IN ('hn:', 'dc:') AND next_at <= ?1",
        )?;
        let rows = stmt.query_map([due_at], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for (id, member, frame) in rows {
        if !online.contains(&member) {
            continue;
        }
        if d.peers.send(&member, serde_json::from_str(&frame)?).is_ok() {
            store.outbox_sent(&id, now as i64)?;
        }
    }
    Ok(())
}

/// Member `key`'s ack of row `id` at `state` (and `reason`, checked by `ack_ok`): the row is
/// done. A decision's ack is the instance's `decision_result` event, and its notice is no
/// longer pending.
pub(crate) fn acked(d: &Daemon, key: &str, id: &str, state: &str, reason: Option<&str>) {
    let store = d.store.lock().unwrap();
    let got: rusqlite::Result<String> = store.conn().query_row(
        "SELECT frame FROM outbox WHERE message_id = ?1 AND to_member = ?2",
        [id, key],
        |r| r.get(0),
    );
    let Ok(frame) = got else {
        return eprintln!(
            "net: dropped an ack from {} for {id} (not sent there)",
            short(key)
        );
    };
    if let Err(e) = store
        .conn()
        .execute("DELETE FROM outbox WHERE message_id = ?1", [id])
    {
        return eprintln!("net: {id}: {e}");
    }
    eprintln!("net: {id} at {}: {state}", short(key));
    if let Ok(Frame::Decision { hold_id, .. }) = serde_json::from_str(&frame) {
        if let Some(c) = &d.cloud {
            let at = (key.to_string(), hold_id.clone());
            c.notices.lock().unwrap().remove(&at);
        }
        let _ = d
            .events
            .send(json!({"event": "decision_result", "hold_id": hold_id,
            "holding_member": key, "result": state, "reason": reason}));
    }
}

/// A `hold_notice` (on an instance) or a `decision` (on a computer) from member `key`, checked
/// by `frames::from_peer_ok`; anything else is dropped (counted by kind).
pub(crate) fn on_frame(d: &Arc<Daemon>, key: &str, f: &Frame, tx: &UnboundedSender<Frame>) {
    let served = match f {
        Frame::HoldNotice { .. } if d.cloud.is_some() => on_notice(d, key, f, tx),
        Frame::Decision { .. } if d.cloud.is_none() => on_decision(d, key, f, tx),
        _ => false,
    };
    if !served {
        d.peers.dropped(f.kind());
        eprintln!("net: dropped a {} frame from {}", f.kind(), short(key));
    }
}

/// On an instance: a notice from a computer of this workspace, for its own person, signed by
/// it, with its own computer name, its own session as the target, and printable short text,
/// is kept pending and becomes the `hold_notice` event; it is acked only when a subscriber got
/// the event (else the holder sends it again). A resent one is an event again (the bridge
/// keys on `holding_member` and `hold_id`).
fn on_notice(d: &Daemon, key: &str, f: &Frame, tx: &UnboundedSender<Frame>) -> bool {
    let Frame::HoldNotice {
        hold_id,
        kind,
        holding_computer,
        for_person,
        target_session,
        from,
        label,
        created_at,
        ..
    } = f
    else {
        return false;
    };
    let ours = {
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        let mine = r
            .members
            .get(&d.identity.key)
            .and_then(|m| m.workspace.as_ref());
        r.members.get(key).is_some_and(|p| {
            p.kind == MemberKind::Computer
                && mine.is_some()
                && p.workspace.as_ref() == mine
                && p.person_id == *for_person
                && p.computer == *holding_computer
                && frames::own(&p.short, target_session)
        })
    };
    let text =
        |t: &str| !t.is_empty() && t.chars().count() <= TEXT_MAX && t.chars().all(label::printable);
    let well_formed = dialog::number(hold_id, 'H').is_some()
        && ["message", "permission"].contains(&kind.as_str())
        && text(label)
        && text(from)
        && sessions::split_addr(from).is_some();
    let (Some(cloud), true) = (
        &d.cloud,
        ours && well_formed && admission::signed_by(key, f),
    ) else {
        return false;
    };
    let event = json!({"event": "hold_notice", "hold_id": hold_id,
        "holding_member": key, "holding_computer": holding_computer, "for_person": for_person,
        "target_session": target_session, "from": from, "label": label, "kind": kind,
        "created_at": created_at});
    // ponytail: in memory, a restart forgets it (the bridge re-reads on attach); one per hold
    // still open, at most a week.
    let at = (key.to_string(), hold_id.clone());
    cloud.notices.lock().unwrap().insert(at, event.clone());
    if d.events.send(event).is_err() {
        eprintln!("hold {hold_id} at {}: notice, no subscriber", short(key));
        return true;
    }
    eprintln!("hold {hold_id} at {}: notice", short(key));
    let _ = tx.send(ack(notice_row(hold_id), NOTICE_ACK, None));
    true
}

/// `holds.pending {}` -> `{notices}`: the `hold_notice` events received and not yet answered
/// by their decision, oldest first, for a bridge that attached late. An instance's op only.
pub(crate) fn pending(id: u64, d: &Daemon) -> Response {
    let Some(cloud) = &d.cloud else {
        return Response::err(id, UNSUPPORTED);
    };
    let since = ipc::now().saturating_sub(mail::HELD_S) as i64;
    let mut map = cloud.notices.lock().unwrap();
    map.retain(|_, v| v["created_at"].as_i64().is_some_and(|t| t > since));
    let mut list: Vec<Value> = map.values().cloned().collect();
    list.sort_by_key(|v| v["created_at"].as_i64());
    Response::ok(id, json!({ "notices": list }))
}

fn ack(id: String, state: &str, reason: Option<&str>) -> Frame {
    Frame::Ack {
        id,
        state: state.into(),
        reason: reason.map(Into::into),
        limit: None,
    }
}

/// What a decision is answered: `state` (`applied`, `not_open`, `refused`), the refusal's
/// `reason`, and the answer when this copy applied it.
#[derive(Debug)]
pub struct Reply {
    pub state: String,
    pub reason: Option<String>,
    pub closed: Option<Closed>,
}

impl Reply {
    fn of(state: &str, reason: Option<&str>) -> Reply {
        Reply {
            state: state.into(),
            reason: reason.map(Into::into),
            closed: None,
        }
    }
}

/// Decision `f` from `signer`, on this computer (`me`), at `now`. `None` (left unanswered,
/// it may heal) unless the signer is my own workspace's member (proof in force). Else `Some`:
/// `refused bad_sig` unless its `sig` is the signer's; a nonce seen before gets the answer its
/// first copy got (`refused bad_request` when it named another hold); else it is applied as the
/// person's answer (`holds::answer`, by `bridge`) only if it was `decided_by` my person, its
/// `holding_member` is me and its hold is mine (`not_mine`), its verdict and hold id are well
/// formed (`bad_request`), and its scope fits (`scope`): `once` for any hold; `person` for a
/// message from a computer member (on a cloud sender it would be `cloud_of`); `own_cloud`
/// only for a message from the signer's own session; `cloud_of` and `always` never. A hold no
/// longer open is `not_open`. That answer is kept by its nonce.
pub fn accept(
    store: &Store,
    roster: &Roster,
    me: &Identity,
    signer: &Member,
    f: &Frame,
    now: i64,
) -> Result<Option<Reply>> {
    let Frame::Decision { hold_id, nonce, .. } = f else {
        return Ok(None);
    };
    if !policy::my_workspace(roster, &me.key, signer) {
        return Ok(None);
    }
    let refused = |why| Ok(Some(Reply::of(REFUSED, Some(why))));
    if !admission::signed_by(&signer.key, f) {
        return refused("bad_sig");
    }
    let seen = format!("{DECISION}{nonce}");
    if let Some((kept, _)) = store.seen_state(&signer.key, &seen)? {
        let kept: Value = serde_json::from_str(&kept).unwrap_or_default();
        if kept["hold"] != hold_id.as_str() {
            return refused("bad_request");
        }
        let state = kept["state"].as_str().unwrap_or(NOT_OPEN);
        return Ok(Some(Reply::of(state, kept["reason"].as_str())));
    }
    let reply = judge(store, roster, me, signer, f, now)?;
    let kept = json!({"hold": hold_id, "state": reply.state, "reason": reply.reason});
    store.put_seen(&signer.key, &seen, &kept.to_string(), now)?;
    Ok(Some(reply))
}

/// `accept`'s rules past the signer and the nonce.
fn judge(
    store: &Store,
    roster: &Roster,
    me: &Identity,
    signer: &Member,
    f: &Frame,
    now: i64,
) -> Result<Reply> {
    let Frame::Decision {
        hold_id,
        holding_member,
        verdict,
        scope,
        decided_by,
        ..
    } = f
    else {
        return Ok(Reply::of(REFUSED, Some("bad_request")));
    };
    let refused = |why| Ok(Reply::of(REFUSED, Some(why)));
    if *holding_member != me.key
        || Some(decided_by) != roster.members.get(&me.key).map(|m| &m.person_id)
    {
        return refused("not_mine");
    }
    let (allow, Some(n)) = (verdict.as_str(), dialog::number(hold_id, 'H')) else {
        return refused("bad_request");
    };
    let allow = match allow {
        ALLOW => true,
        DECLINE => false,
        _ => return refused("bad_request"),
    };
    let Some(h) = holds::get(store, n)? else {
        return refused("not_mine");
    };
    let sender = roster.members.get(&h.from_member);
    let fits = match scope.as_str() {
        "once" => true,
        "person" => sender.is_some_and(|m| m.kind == MemberKind::Computer),
        "own_cloud" => h.from_member == signer.key,
        _ => false,
    };
    if !fits {
        return refused("scope");
    }
    Ok(
        match holds::answer(store, roster, me, h.id, allow, Some(scope), "bridge", now)? {
            Ok(c) => Reply {
                closed: Some(c),
                ..Reply::of(APPLIED, None)
            },
            Err(NOT_OPEN) => Reply::of(NOT_OPEN, None),
            Err(_) => return refused("scope"),
        },
    )
}

/// On a computer: `accept` under the daemon's locks, and its answer acked.
fn on_decision(d: &Arc<Daemon>, key: &str, f: &Frame, tx: &UnboundedSender<Frame>) -> bool {
    let Frame::Decision { hold_id, nonce, .. } = f else {
        return false;
    };
    // The peer chose it: logged only in its shape.
    let hold = if dialog::number(hold_id, 'H').is_some() {
        hold_id.as_str()
    } else {
        "<bad id>"
    };
    let r = {
        let team = d.team.lock().unwrap();
        let Some(signer) = team.roster.members.get(key) else {
            return false;
        };
        let store = d.store.lock().unwrap();
        let now = ipc::now() as i64;
        accept(&store, &team.roster, &d.identity, signer, f, now)
    };
    let reply = match r {
        Ok(Some(reply)) => reply,
        Ok(None) => {
            eprintln!(
                "hold {hold}: a decision from {}, not my workspace's member (yet)",
                short(key)
            );
            return true;
        }
        Err(e) => {
            eprintln!("hold {hold}: a decision from {}: {e}", short(key));
            return true;
        }
    };
    let why = reply.reason.as_deref();
    eprintln!(
        "hold {hold}: decision from {}: {} {}",
        short(key),
        reply.state,
        why.unwrap_or("")
    );
    let _ = tx.send(ack(format!("{DECISION}{nonce}"), &reply.state, why));
    if let Some(c) = &reply.closed {
        holds::after(d, c, "bridge");
    }
    true
}

/// `decision.send {hold_id, holding_member, verdict, scope, decided_by}` -> `{nonce}`: the
/// person `decided_by` (Clerk-verified by the bridge) tapped `verdict` on a hold notice; a
/// signed `decision` goes to the holding computer through the outbox. `bad_request` unless the
/// holding member is a computer of this workspace whose person is `decided_by` (the notice's
/// `for_person`, which the notice was checked against), `scope` is `once`, `person` or
/// `own_cloud`, and `verdict` `allow` or `decline`. An instance's op only.
pub(crate) fn op(id: u64, body: &Value, d: &Daemon) -> Response {
    if d.cloud.is_none() {
        return Response::err(id, UNSUPPORTED);
    }
    let s = |k: &str| body[k].as_str();
    let (Some(hold_id), Some(holding), Some(verdict), Some(scope), Some(by)) = (
        s("hold_id"),
        s("holding_member"),
        s("verdict"),
        s("scope"),
        s("decided_by"),
    ) else {
        return Response::err(id, BAD_REQUEST);
    };
    let words = dialog::number(hold_id, 'H').is_some()
        && [ALLOW, DECLINE].contains(&verdict)
        && RELAYED.contains(&scope);
    let team = d.team.lock().unwrap();
    let members = &team.roster.members;
    let mine = members
        .get(&d.identity.key)
        .and_then(|m| m.workspace.as_ref());
    let theirs = members.get(holding).is_some_and(|m| {
        m.kind == MemberKind::Computer
            && mine.is_some()
            && m.workspace.as_ref() == mine
            && m.person_id == by
    });
    if !(words && theirs) {
        return Response::err(id, BAD_REQUEST);
    }
    let nonce = ulid::Ulid::generate().to_string();
    let now = ipc::now() as i64;
    let f = admission::signed(
        &d.identity,
        Frame::Decision {
            hold_id: hold_id.into(),
            holding_member: holding.into(),
            verdict: verdict.into(),
            scope: scope.into(),
            decided_by: by.into(),
            nonce: nonce.clone(),
            decided_at: now,
            sig: String::new(),
        },
    );
    let store = d.store.lock().unwrap();
    let frame = serde_json::to_string(&f).expect("a frame serializes");
    if let Err(e) = store.put_outbox(&format!("{DECISION}{nonce}"), holding, &frame, now) {
        eprintln!("decision.send: {e}");
        return Response::err(id, ipc::INTERNAL);
    }
    eprintln!("hold {hold_id} at {}: decision queued", short(holding));
    d.peers.outbox.notify_one();
    Response::ok(id, json!({ "nonce": nonce }))
}
