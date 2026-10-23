//! Hold notices and relayed decisions (ruling §6): a computer tells its own workspace's member
//! (`Roster::my_workspace_member`, proof in force) of every new hold, message or permission,
//! as a signed `hold_notice` of ids and the sender's label, never text; the member (an
//! embedded instance, the Lightbulb bridge) shows it on the person's phone or web and carries
//! the tap back as a signed `decision` with `decided_by`, the person who tapped.
//!
//! Both ride the outbox (`outbox::due`, the mail's loop and backoff): one row each,
//! `hn:<hold id>` on the holder and `dc:<nonce>` on the member, to the one member named, again
//! on every `hello`, until that member acks it (`ack {id: <row id>}`: state `delivered` for a
//! notice; `applied`, `not_open`, or `refused` with a `REFUSALS` reason for a decision) or,
//! for a notice, its hold closes; at most `mail::HELD_S`. The holder keeps each decision's
//! answer by its nonce and gives a resent copy the same one. Only a decision from a member
//! not (yet) my workspace's is left unanswered: a roster sync or a renewed proof may heal it.
//!
//! On the instance, `hold_notice` and `decision_result` are events, `decision.send` and
//! `holds.pending` the ops. A notice is acked only once the event went to a subscriber, and it
//! stays pending (`holds.pending`) until its decision is applied or found not open, or its
//! hold's own time is up. Logs carry ids and shorts, never text.

use crate::admission::{self, NOT_OPEN};
use crate::daemon::ipc::{self, Response, BAD_REQUEST, UNSUPPORTED};
use crate::daemon::{Cloud, Daemon};
use crate::doing::TEXT_MAX;
use crate::holds::{self, Closed, Hold, ALLOW, DECLINE};
use crate::identity::Identity;
use crate::net::frames::{self, Frame};
use crate::net::short;
use crate::roster::{Member, MemberKind, Roster};
use crate::store::Store;
use crate::{dialog, label, mail, sessions, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

const NOTICE: &str = "hn:";
const DECISION: &str = "dc:";
/// The ack states of a notice, and of a decision.
const DELIVERED: &str = "delivered";
const APPLIED: &str = "applied";
const REFUSED: &str = "refused";
/// Why a decision was refused: a scope that does not fit, a malformed one or a nonce reused on
/// another hold, a signature not the signer's, a hold (or a person) not mine. (`not_open` is a
/// state of its own; as a reason it is accepted, never sent.)
const SCOPE: &str = "scope";
const MALFORMED: &str = "bad_request";
const BAD_SIG: &str = "bad_sig";
const NOT_MINE: &str = "not_mine";
pub const REFUSALS: [&str; 5] = [SCOPE, NOT_OPEN, MALFORMED, BAD_SIG, NOT_MINE];
/// The scopes a relayed decision may carry.
const RELAYED: [&str; 3] = ["once", "person", "own_cloud"];

/// Whether `id` names a notice's or a decision's outbox row (an envelope id never does).
pub fn is_control(id: &str) -> bool {
    id.starts_with(NOTICE) || id.starts_with(DECISION)
}

/// Whether `state` (with `reason`) may ack row `id`.
pub fn ack_ok(id: &str, state: &str, reason: Option<&str>) -> bool {
    match reason {
        _ if id.starts_with(NOTICE) => state == DELIVERED && reason.is_none(),
        None => id.starts_with(DECISION) && [APPLIED, NOT_OPEN].contains(&state),
        Some(r) => id.starts_with(DECISION) && state == REFUSED && REFUSALS.contains(&r),
    }
}

fn notice_row(hold: &str) -> String {
    format!("{NOTICE}{hold}")
}

/// `allow` or `decline`.
fn verdict(v: &str) -> Option<bool> {
    match v {
        ALLOW => Some(true),
        DECLINE => Some(false),
        _ => None,
    }
}

/// The number of hold id `H<n>`.
fn hold_no(id: &str) -> Option<i64> {
    dialog::number(id, 'H')
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
            kind: h.kind().as_str().into(),
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

/// Member `key`'s ack of row `id` at `state` (and `reason`, checked by `ack_ok`): the row is
/// done. A decision's ack is the instance's `decision_result` event; once it was applied or
/// found not open, its notice is no longer pending.
pub(crate) fn acked(d: &Daemon, key: &str, id: &str, state: &str, reason: Option<&str>) {
    let store = d.store.lock().unwrap();
    let got: rusqlite::Result<Option<String>> = store
        .conn()
        .query_row(
            "DELETE FROM outbox WHERE message_id = ?1 AND to_member = ?2 RETURNING frame",
            [id, key],
            |r| r.get(0),
        )
        .optional();
    let frame = match got {
        Ok(Some(f)) => f,
        Ok(None) => {
            return eprintln!(
                "net: dropped an ack from {} for {id} (not sent there)",
                short(key)
            )
        }
        Err(e) => return eprintln!("net: {id}: {e}"),
    };
    eprintln!("net: {id} at {}: {state}", short(key));
    if !id.starts_with(DECISION) {
        return;
    }
    let Ok(Frame::Decision { hold_id, .. }) = serde_json::from_str(&frame) else {
        return;
    };
    if let (Some(c), false) = (&d.cloud, state == REFUSED) {
        let at = (key.to_string(), hold_id.clone());
        c.notices.lock().unwrap().remove(&at);
    }
    let _ = d
        .events
        .send(json!({"event": "decision_result", "hold_id": hold_id,
        "holding_member": key, "result": state, "reason": reason}));
}

/// `from_peer_ok`'s check on a `hold_notice` from `peer`, all it can see without the roster:
/// from a computer, signed by it, no preview, a hold id `H<n>`, a known kind, `target_session`
/// one of the peer's own, `from` an address, and `from` and `label` short printable text.
pub(crate) fn notice_ok(f: &Frame, peer: &Member) -> bool {
    let Frame::HoldNotice {
        hold_id,
        kind,
        target_session,
        from,
        label,
        preview,
        ..
    } = f
    else {
        return false;
    };
    let text =
        |t: &str| !t.is_empty() && t.chars().count() <= TEXT_MAX && t.chars().all(label::printable);
    peer.kind == MemberKind::Computer
        && preview.is_empty()
        && hold_no(hold_id).is_some()
        && ["message", "permission"].contains(&kind.as_str())
        && frames::own(&peer.short, target_session)
        && text(label)
        && text(from)
        && sessions::split_addr(from).is_some()
        && admission::signed_by(&peer.key, f)
}

/// A `hold_notice` (on an instance) or a `decision` (on a computer) from member `key`, checked
/// by `frames::from_peer_ok`; anything else is dropped (counted by kind).
pub(crate) fn on_frame(d: &Arc<Daemon>, key: &str, f: &Frame, tx: &UnboundedSender<Frame>) {
    let served = match (f, &d.cloud) {
        (Frame::HoldNotice { .. }, Some(cloud)) => on_notice(d, cloud, key, f, tx),
        (Frame::Decision { hold_id, nonce, .. }, None) => {
            on_decision(d, key, f, hold_id, nonce, tx);
            true
        }
        _ => false,
    };
    if !served {
        d.peers.dropped(f.kind());
        eprintln!("net: dropped a {} frame from {}", f.kind(), short(key));
    }
}

/// On an instance: a notice (`notice_ok`) from a computer of this workspace, for its own
/// person, under its own computer name, is kept pending and becomes the `hold_notice` event
/// (the frame's fields, `holding_member` the sender); it is acked only when a subscriber got
/// the event (else the holder sends it again). A resent one is an event again (the bridge
/// keys on `holding_member` and `hold_id`).
fn on_notice(d: &Daemon, cloud: &Cloud, key: &str, f: &Frame, tx: &UnboundedSender<Frame>) -> bool {
    let mut event = serde_json::to_value(f).expect("a frame serializes");
    let field = |k: &str| event[k].as_str().unwrap_or_default().to_string();
    let (hold_id, person, computer) = (
        field("hold_id"),
        field("for_person"),
        field("holding_computer"),
    );
    let ours = {
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        r.workspace_computer(&d.identity.key, key, &person)
            .is_some_and(|m| m.computer == computer)
    };
    if !ours {
        return false;
    }
    let o = event.as_object_mut().expect("an object");
    for k in ["t", "preview", "sig"] {
        o.remove(k);
    }
    o.insert("event".into(), "hold_notice".into());
    o.insert("holding_member".into(), key.into());
    // ponytail: in memory, a restart forgets it (the holder re-sends what was never acked);
    // one per hold still open, at most a week.
    let at = (key.to_string(), hold_id.clone());
    cloud.notices.lock().unwrap().insert(at, event.clone());
    if d.events.send(event).is_err() {
        eprintln!("hold {hold_id} at {}: notice, no subscriber", short(key));
        return true;
    }
    eprintln!("hold {hold_id} at {}: notice", short(key));
    let _ = tx.send(ack(notice_row(&hold_id), DELIVERED, None));
    true
}

/// `holds.pending {}` -> `{notices}`: the `hold_notice` events received and not yet applied
/// or found not open, oldest first, for a bridge that attached late; each kept as long as its
/// hold can be open (`holds::PERMISSION_S` for a permission prompt, `mail::HELD_S` for a
/// message). An instance's op only.
pub(crate) fn pending(id: u64, d: &Daemon) -> Response {
    let Some(cloud) = &d.cloud else {
        return Response::err(id, UNSUPPORTED);
    };
    let now = ipc::now() as i64;
    let mut map = cloud.notices.lock().unwrap();
    map.retain(|_, v| {
        let life = if v["kind"] == holds::Kind::Permission.as_str() {
            holds::PERMISSION_S
        } else {
            mail::HELD_S as i64
        };
        v["created_at"].as_i64().is_some_and(|t| t + life > now)
    });
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
#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub state: String,
    pub reason: Option<String>,
    #[serde(skip)]
    pub closed: Option<Closed>,
}

impl Reply {
    fn of(state: &str) -> Reply {
        Reply {
            state: state.into(),
            reason: None,
            closed: None,
        }
    }

    fn refused(why: &str) -> Reply {
        Reply {
            reason: Some(why.into()),
            ..Reply::of(REFUSED)
        }
    }
}

/// A decision's answer as kept by its nonce (`seen_ids`), with the hold it named.
#[derive(Serialize, Deserialize)]
struct Kept {
    hold: String,
    reply: Reply,
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
    let Some(mine) = roster.members.get(&me.key) else {
        return Ok(None);
    };
    if roster
        .my_workspace_member(mine)
        .is_none_or(|w| w.key != signer.key)
    {
        return Ok(None);
    }
    if !admission::signed_by(&signer.key, f) {
        return Ok(Some(Reply::refused(BAD_SIG)));
    }
    let seen = format!("{DECISION}{nonce}");
    let kept: Option<String> = store
        .conn()
        .query_row(
            "SELECT state FROM seen_ids WHERE sender = ?1 AND envelope_id = ?2",
            [&signer.key, &seen],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(kept) = kept {
        let kept: Kept = serde_json::from_str(&kept)?;
        return Ok(Some(if kept.hold == *hold_id {
            kept.reply
        } else {
            Reply::refused(MALFORMED)
        }));
    }
    let reply = judge(store, roster, me, &mine.person_id, signer, f, now)?;
    let kept = Kept {
        hold: hold_id.clone(),
        reply,
    };
    store.put_seen(&signer.key, &seen, &serde_json::to_string(&kept)?, now)?;
    Ok(Some(kept.reply))
}

/// `accept`'s rules past the signer and the nonce, for my person `person`.
fn judge(
    store: &Store,
    roster: &Roster,
    me: &Identity,
    person: &str,
    signer: &Member,
    f: &Frame,
    now: i64,
) -> Result<Reply> {
    let Frame::Decision {
        hold_id,
        holding_member,
        verdict: v,
        scope,
        decided_by,
        ..
    } = f
    else {
        return Ok(Reply::refused(MALFORMED));
    };
    if *holding_member != me.key || decided_by != person {
        return Ok(Reply::refused(NOT_MINE));
    }
    let (Some(allow), Some(n)) = (verdict(v), hold_no(hold_id)) else {
        return Ok(Reply::refused(MALFORMED));
    };
    let Some(h) = holds::get(store, n)? else {
        return Ok(Reply::refused(NOT_MINE));
    };
    let fits = match scope.as_str() {
        "once" => true,
        "person" => roster
            .members
            .get(&h.from_member)
            .is_some_and(|m| m.kind == MemberKind::Computer),
        "own_cloud" => h.from_member == signer.key,
        _ => false,
    };
    if !fits {
        return Ok(Reply::refused(SCOPE));
    }
    Ok(
        match holds::answer(store, roster, me, h.id, allow, Some(scope), "bridge", now)? {
            Ok(c) => Reply {
                closed: Some(c),
                ..Reply::of(APPLIED)
            },
            Err(NOT_OPEN) => Reply::of(NOT_OPEN),
            Err(_) => Reply::refused(SCOPE),
        },
    )
}

/// On a computer: `accept` under the daemon's locks, and its answer acked.
fn on_decision(
    d: &Arc<Daemon>,
    key: &str,
    f: &Frame,
    hold_id: &str,
    nonce: &str,
    tx: &UnboundedSender<Frame>,
) {
    // The peer chose it: logged as plain short text.
    let hold = label::plain(hold_id, 16);
    let r = {
        let team = d.team.lock().unwrap();
        let Some(signer) = team.roster.members.get(key) else {
            return;
        };
        let store = d.store.lock().unwrap();
        let now = ipc::now() as i64;
        accept(&store, &team.roster, &d.identity, signer, f, now)
    };
    let reply = match r {
        Ok(Some(reply)) => reply,
        Ok(None) => {
            let who = short(key);
            return eprintln!("hold {hold}: a decision from {who}, not my workspace's member");
        }
        Err(e) => return eprintln!("hold {hold}: a decision from {}: {e}", short(key)),
    };
    let why = reply.reason.as_deref();
    let (who, state) = (short(key), &reply.state);
    eprintln!(
        "hold {hold}: decision from {who}: {state} {}",
        why.unwrap_or("")
    );
    let _ = tx.send(ack(format!("{DECISION}{nonce}"), state, why));
    if let Some(c) = &reply.closed {
        holds::after(d, c, "bridge");
    }
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
    let (Some(hold_id), Some(holding), Some(v), Some(scope), Some(by)) = (
        s("hold_id"),
        s("holding_member"),
        s("verdict"),
        s("scope"),
        s("decided_by"),
    ) else {
        return Response::err(id, BAD_REQUEST);
    };
    let words = hold_no(hold_id).is_some() && verdict(v).is_some() && RELAYED.contains(&scope);
    let team = d.team.lock().unwrap();
    let theirs = (team.roster)
        .workspace_computer(&d.identity.key, holding, by)
        .is_some();
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
            verdict: v.into(),
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
