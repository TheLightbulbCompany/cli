//! Hold notices and relayed decisions (ruling §6): a computer tells its own workspace's member
//! (`policy::my_workspace`, proof in force) of every new hold, message or permission, as a
//! signed `hold_notice` of ids and the sender's label, never text; the member (an embedded
//! instance, the Lightbulb bridge) shows it on the person's phone or web and carries the tap
//! back as a signed `decision` with `decided_by`, the person who tapped.
//!
//! Both ride the outbox table: one row each, `hn:<hold id>` on the holder and `dc:<nonce>` on
//! the member, sent with the mail's backoff to the one member named, again on every `hello`,
//! until that member acks it (`ack {id: <row id>}`, state `delivered` for a notice, `applied`
//! or `not_open` for a decision) or, for a notice, its hold closes; at most `mail::HELD_S`.
//! A decision refused by the holder is not acked: what refused it (a lapsed proof, a roster
//! not yet synced) may heal, and the member's retries end with the hold's 7 days.
//!
//! On the instance, `hold_notice` and `decision_result` are events, `decision.send` the op.
//! Logs carry ids and shorts, never text.

use crate::admission::{self, NOT_OPEN};
use crate::daemon::ipc::{self, Response, BAD_REQUEST, UNSUPPORTED};
use crate::daemon::Daemon;
use crate::holds::{self, Closed, Hold, ALLOW, DECLINE};
use crate::identity::Identity;
use crate::net::frames::Frame;
use crate::net::short;
use crate::roster::{Member, MemberKind, Roster};
use crate::store::Store;
use crate::{dialog, mail, policy, Result};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

const NOTICE: &str = "hn:";
const DECISION: &str = "dc:";
/// The ack states of a notice, and of a decision.
const NOTICE_ACK: &str = "delivered";
const APPLIED: &str = "applied";
/// The scopes a relayed decision may carry.
const RELAYED: [&str; 3] = ["once", "person", "own_cloud"];

/// Whether `id` names a notice's or a decision's outbox row (an envelope id never does).
pub fn is_control(id: &str) -> bool {
    id.starts_with(NOTICE) || id.starts_with(DECISION)
}

/// Whether `state` may ack row `id`.
pub fn ack_ok(id: &str, state: &str) -> bool {
    if id.starts_with(NOTICE) {
        state == NOTICE_ACK
    } else {
        id.starts_with(DECISION) && [APPLIED, NOT_OPEN].contains(&state)
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
    let rows: Vec<(String, String, String)> = {
        let mut stmt = store.conn().prepare(
            "SELECT message_id, to_member, frame FROM outbox WHERE substr(message_id, 1, 3) \
             IN ('hn:', 'dc:') AND next_at <= ?1 AND created_at > ?2",
        )?;
        let rows = stmt.query_map([due_at, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
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

/// Member `key`'s ack of row `id` at `state` (checked by `ack_ok`): the row is done. A
/// decision's ack is the instance's `decision_result` event.
pub(crate) fn acked(d: &Daemon, key: &str, id: &str, state: &str) {
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
        let _ = d
            .events
            .send(json!({"event": "decision_result", "hold_id": hold_id,
            "holding_member": key, "result": state}));
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
/// it, becomes the `hold_notice` event and is acked. A resent one is an event again (the
/// bridge keys on `holding_member` and `hold_id`).
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
        })
    };
    let well_formed = dialog::number(hold_id, 'H').is_some()
        && ["message", "permission"].contains(&kind.as_str());
    if !(ours && well_formed && admission::signed_by(key, f)) {
        return false;
    }
    let _ = d
        .events
        .send(json!({"event": "hold_notice", "hold_id": hold_id,
        "holding_member": key, "holding_computer": holding_computer, "for_person": for_person,
        "target_session": target_session, "from": from, "label": label, "kind": kind,
        "created_at": created_at}));
    eprintln!("hold {hold_id} at {}: notice", short(key));
    let _ = tx.send(ack(notice_row(hold_id), NOTICE_ACK));
    true
}

fn ack(id: String, state: &str) -> Frame {
    Frame::Ack {
        id,
        state: state.into(),
        reason: None,
        limit: None,
    }
}

/// Decision `f` from `signer`, on this computer (`me`), at `now`: applied as the person's
/// answer (`holds::answer`, by `bridge`) only if its `sig` is the signer's, the signer is my
/// own workspace's member (proof in force), it was `decided_by` my person, its
/// `holding_member` is me and its hold is mine, its nonce is new, and its scope fits: `once`
/// for any hold; `person` for a message from a computer member (on a cloud sender it would be
/// `cloud_of`); `own_cloud` only for a message from the signer's own session. `cloud_of` and
/// `always` never. `Some` the answer applied, `None` when its hold was already answered or
/// expired, or its nonce was seen (`not_open`). `Err` names the refusal (nothing changes, the
/// nonce stays unused).
pub fn accept(
    store: &Store,
    roster: &Roster,
    me: &Identity,
    signer: &Member,
    f: &Frame,
    now: i64,
) -> Result<std::result::Result<Option<Closed>, &'static str>> {
    let Frame::Decision {
        hold_id,
        holding_member,
        verdict,
        scope,
        decided_by,
        nonce,
        ..
    } = f
    else {
        return Ok(Err("not a decision"));
    };
    if !admission::signed_by(&signer.key, f) {
        return Ok(Err("signature"));
    }
    if !policy::my_workspace(roster, &me.key, signer) {
        return Ok(Err("not my workspace's member"));
    }
    if *holding_member != me.key
        || Some(decided_by) != roster.members.get(&me.key).map(|m| &m.person_id)
    {
        return Ok(Err("not for me"));
    }
    let allow = match verdict.as_str() {
        ALLOW => true,
        DECLINE => false,
        _ => return Ok(Err("verdict")),
    };
    let Some(h) = dialog::number(hold_id, 'H')
        .map(|n| holds::get(store, n))
        .transpose()?
        .flatten()
    else {
        return Ok(Err("no such hold"));
    };
    let sender = roster.members.get(&h.from_member);
    let fits = match scope.as_str() {
        "once" => true,
        "person" => sender.is_some_and(|m| m.kind == MemberKind::Computer),
        "own_cloud" => h.from_member == signer.key,
        _ => false,
    };
    if !fits {
        return Ok(Err("scope"));
    }
    let seen = format!("{DECISION}{nonce}");
    if store.seen_state(&signer.key, &seen)?.is_some() {
        return Ok(Ok(None));
    }
    let r = holds::answer(store, roster, me, h.id, allow, Some(scope), "bridge", now)?;
    let out = match r {
        Ok(c) => Some(c),
        Err(NOT_OPEN) => None,
        Err(_) => return Ok(Err("scope")),
    };
    store.put_seen(&signer.key, &seen, "decision", now)?;
    Ok(Ok(out))
}

/// On a computer: `accept` under the daemon's locks; an applied or spent decision is acked.
fn on_decision(d: &Arc<Daemon>, key: &str, f: &Frame, tx: &UnboundedSender<Frame>) -> bool {
    let Frame::Decision { hold_id, nonce, .. } = f else {
        return false;
    };
    let r = {
        let team = d.team.lock().unwrap();
        let Some(signer) = team.roster.members.get(key) else {
            return false;
        };
        let store = d.store.lock().unwrap();
        accept(
            &store,
            &team.roster,
            &d.identity,
            signer,
            f,
            ipc::now() as i64,
        )
    };
    let (state, closed) = match r {
        Ok(Ok(Some(c))) => (APPLIED, Some(c)),
        Ok(Ok(None)) => (NOT_OPEN, None),
        Ok(Err(why)) => {
            eprintln!(
                "hold {hold_id}: refused a decision from {} ({why})",
                short(key)
            );
            return true;
        }
        Err(e) => {
            eprintln!("hold {hold_id}: a decision from {}: {e}", short(key));
            return true;
        }
    };
    eprintln!("hold {hold_id}: decision from {}: {state}", short(key));
    let _ = tx.send(ack(format!("{DECISION}{nonce}"), state));
    if let Some(c) = closed {
        holds::after(d, &c, "bridge");
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
