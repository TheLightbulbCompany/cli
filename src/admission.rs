//! Admission: how a stranger becomes a member. The candidate dials a member its ticket lists
//! on `JOIN_ALPN` and sends a signed `join_request`; that member checks it, asks its person
//! (or admits at once for an `open` link), writes the `admit`, and answers a signed
//! `join_decision`, followed after an allow by the whole roster in `roster` pages. The
//! candidate checks every entry itself before that roster replaces its solo one.

use crate::daemon::ipc::{self, Response, BAD_REQUEST, INTERNAL, NOT_FOUND, UNSUPPORTED};
use crate::daemon::TeamState;
use crate::daemon::{net_ops, user_name, Daemon};
use crate::dialog;
use crate::identity::{self, Identity};
use crate::net::frames::{self, Frame};
use crate::net::{self, JOIN_ALPN};
use crate::proof::{self, Proof, ProofKind};
use crate::roster::{self, Collision, Entry, Kind, MemberKind, Role, Roster};
use crate::store::{Join, Store};
use crate::ticket::{self, Ticket};
use data_encoding::HEXLOWER;
use iroh::endpoint::{Connection, SendStream, VarInt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

pub const NOT_OPEN: &str = "not_open";
pub const ALREADY_IN_TEAM: &str = "already_in_team";
pub const BAD_TICKET: &str = "bad_ticket";
pub const EXPIRED_TICKET: &str = "expired_ticket";
pub const NAME_TAKEN: &str = "name_taken";
pub const KEY_COLLISION: &str = "key_collision";
pub const UNREACHABLE: &str = "unreachable";
pub const DECLINED: &str = "declined";
pub const TOO_MANY: &str = "too_many_requests";
/// The person did not answer within the wait: the candidate is told so and stops asking.
pub const NOT_ANSWERED: &str = "not_answered";
/// A new computer of a signed-in person whose other computers are all offline (a `decline`).
pub const YOUR_COMPUTER_OFFLINE: &str = "your_computer_offline";
/// Not on the wire: `check`'s word that another computer of the candidate's person decides.
const FORWARD: &str = "forward";
/// Every reason a `join_decision` may carry; another reads as `refused`.
const REASONS: &[&str] = &[
    BAD_TICKET,
    EXPIRED_TICKET,
    NAME_TAKEN,
    KEY_COLLISION,
    DECLINED,
    TOO_MANY,
    NOT_ANSWERED,
    YOUR_COMPUTER_OFFLINE,
    UNSUPPORTED,
    BAD_REQUEST,
    INTERNAL,
    net_ops::NOT_ADMIN,
];

/// A dial, a first frame, a `roster` page, the goodbye: each gets this long.
const STEP: Duration = Duration::from_secs(10);
/// Open requests at once: per team, per inviter, per candidate.
const LIMITS: (usize, usize, usize) = (20, 5, 1);
/// The roster a candidate takes in, at most.
const MAX_ENTRIES: usize = 100_000;

/// `Ok` allows; `Err(reason)` refuses (`declined`: the person said no).
pub type Verdict = Result<(), &'static str>;

/// `v` as the decision's `verdict` and as the request's state.
fn words(v: Verdict) -> (&'static str, &'static str) {
    match v {
        Ok(()) => ("allow", "admitted"),
        Err(DECLINED | YOUR_COMPUTER_OFFLINE) => ("decline", "declined"),
        Err(_) => ("refused", "refused"),
    }
}

/// The requests this computer forwarded to another computer of their person, by candidate:
/// that computer's key and the waiting connection task.
pub type Forwards = Mutex<HashMap<String, (String, oneshot::Sender<Verdict>)>>;

/// A request waiting for its person, and the connection task to tell.
pub struct Pending {
    req: Request,
    tx: oneshot::Sender<Verdict>,
}

/// The waiting requests by row id (`J<id>`). First answer wins: whoever takes the entry.
pub type Waiting = Mutex<HashMap<i64, Pending>>;

#[derive(Clone)]
struct Request {
    ticket: Ticket,
    proof: String,
    candidate: String,
    name: String,
    computer: String,
    kind: String,
    /// The candidate's sign-in proof (`kind=workspace` for a cloud member).
    token: Option<String>,
}

/// `req`'s proof, verified at `now` for its candidate: `None` without one, `Some(Err)` when it
/// does not verify.
fn claims(req: &Request, now: i64) -> Option<crate::Result<Proof>> {
    let t = req.token.as_deref()?;
    Some(proof::verify(t, &req.candidate, now))
}

/// The person `req`'s verified computer proof names, when one of `r`'s computers is theirs:
/// the candidate is that person's new computer.
fn same_person(r: &Roster, req: &Request, now: i64) -> Option<String> {
    let c = claims(req, now)?.ok()?;
    (req.kind == "computer" && c.kind == ProofKind::Computer && !r.computers_of(&c.sub).is_empty())
        .then_some(c.sub)
}

/// The admission dialog's text for a workspace proof: from the proof alone.
pub fn workspace_prompt(c: &Proof) -> String {
    let clean = |s: &str| -> String { s.chars().filter(|c| crate::label::printable(*c)).collect() };
    let ws = c
        .workspace
        .as_ref()
        .map_or(String::new(), |w| clean(&w.name));
    let mut agents: Vec<String> = c.agents.iter().take(3).map(|a| clean(a)).collect();
    if c.agents.len() > 3 {
        agents.push("…".into());
    }
    let them = if agents.is_empty() {
        String::new()
    } else {
        format!(" ({})", agents.join(", "))
    };
    format!(
        "Lightbulb workspace \"{ws}\" wants to join as a member. Its agents{them} will appear in your directory, and it will see this network's directory."
    )
}

/// What the person is asked about `req`, as plain text (`label::printable`).
fn prompt(r: &Roster, req: &Request, now: i64) -> String {
    let text = match claims(req, now).and_then(|c| c.ok()) {
        Some(c) if c.kind == ProofKind::Workspace => workspace_prompt(&c),
        _ if same_person(r, req, now).is_some() => {
            format!("Your new Mac {} wants to join. Allow?", req.computer)
        }
        _ => format!("{} wants to join your network", req.name),
    };
    text.chars()
        .filter(|c| crate::label::printable(*c))
        .collect()
}

/// What a `join_request` proves it holds: hex sha256(secret ‖ candidate).
pub fn proof(secret: &str, candidate: &str) -> String {
    HEXLOWER.encode(&Sha256::digest(format!("{secret}{candidate}").as_bytes()))
}

/// `id`'s signed `join_request` for ticket `t`.
pub fn request(id: &Identity, t: &Ticket, name: &str, computer: &str, kind: &str) -> Frame {
    request_with(id, t, name, computer, kind, None)
}

/// `request`, carrying the candidate's sign-in proof `token`.
pub fn request_with(
    id: &Identity,
    t: &Ticket,
    name: &str,
    computer: &str,
    kind: &str,
    token: Option<&str>,
) -> Frame {
    signed(
        id,
        Frame::JoinRequest {
            ticket: ticket::encode(t),
            secret_proof: proof(&t.secret, &id.key),
            candidate: id.key.clone(),
            name: name.into(),
            computer: computer.into(),
            kind: kind.into(),
            proof: token.map(Into::into),
            sig: String::new(),
        },
    )
}

/// `f` with its `sig`: `id`'s signature over it (`sig` left out).
pub fn signed(id: &Identity, f: Frame) -> Frame {
    let mut v = serde_json::to_value(&f).expect("a frame serializes");
    v["sig"] = id.sign(&v).into();
    serde_json::from_value(v).expect("the same frame")
}

pub(crate) fn signed_by(key: &str, f: &Frame) -> bool {
    let v = serde_json::to_value(f).expect("a frame serializes");
    identity::verify(key, &v, v["sig"].as_str().unwrap_or_default())
}

// ---- the admitting member ----------------------------------------------------------------

/// One `JOIN_ALPN` connection: the request, its checks, the person's answer, the decision.
/// A request whose `candidate` is not the peer, or not signed by it, is dropped unanswered.
pub async fn serve(d: Arc<Daemon>, conn: Connection) {
    let peer = identity::key_of(conn.remote_id().as_bytes());
    let first = async {
        let (send, mut recv) = conn.accept_bi().await.ok()?;
        Some((send, frames::read(&mut recv).await.ok()?))
    };
    let got = tokio::time::timeout(STEP, first).await.ok().flatten();
    let Some((send, f)) = got else {
        conn.close(VarInt::from_u32(0), b"");
        return;
    };
    let genuine = signed_by(&peer, &f);
    let raw = f.clone();
    let Frame::JoinRequest {
        ticket,
        secret_proof,
        candidate,
        name,
        computer,
        kind,
        proof: token,
        ..
    } = f
    else {
        conn.close(VarInt::from_u32(0), b"");
        return;
    };
    if candidate != peer || !genuine {
        eprintln!(
            "join: dropped a forged join request from {}",
            net::short(&peer)
        );
        conn.close(VarInt::from_u32(0), b"");
        return;
    }
    let Ok(ticket) = ticket::decode(&ticket) else {
        eprintln!("join: refused {} ({BAD_TICKET})", net::short(&peer));
        return tell(&d, &conn, send, "", &peer, Err(BAD_TICKET)).await;
    };
    let ticket_id = ticket.ticket_id.clone();
    // Admitted already (its roster lost on the way): the roster again, nothing written.
    if d.team.lock().unwrap().roster.members.contains_key(&peer) {
        eprintln!(
            "join: {} is a member; resending the roster",
            net::short(&peer)
        );
        return tell(&d, &conn, send, &ticket_id, &peer, Ok(())).await;
    }
    let req = Request {
        ticket,
        proof: secret_proof,
        candidate,
        name,
        computer,
        kind,
        token,
    };
    let gone = async {
        conn.closed().await;
    };
    let verdict = match open(&d, &req) {
        Err(FORWARD) => forward(&d, &conn, raw, &req).await,
        Err(r) => Some(Err(r)),
        Ok((_, Some(v))) => Some(v),
        Ok((n, None)) => wait(&d, gone, n, req).await,
    };
    let Some(verdict) = verdict else {
        return conn.close(VarInt::from_u32(0), b"");
    };
    if let Err(r) = verdict {
        eprintln!("join: refused {} ({r})", net::short(&peer));
    }
    tell(&d, &conn, send, &ticket_id, &peer, verdict).await;
}

/// Checks `req` and stores it as an `open` row within the limits; an `open` link's request is
/// admitted at once, under the same locks. Its row id, and its verdict when decided already.
fn open(d: &Daemon, req: &Request) -> Result<(i64, Option<Verdict>), &'static str> {
    let mut team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let now = ipc::now() as i64;
    let once = check(&team.roster, &store, req, &d.identity.key, now)?;
    let fail = |e: rusqlite::Error| {
        eprintln!("join: {e}");
        INTERNAL
    };
    // A request older than the wait (and the step its waiter may lag) is one its candidate
    // stopped waiting for.
    let window = (d.peers.knobs().join_wait + STEP).as_secs() as i64;
    store.expire_joins(now - window, now).map_err(fail)?;
    let [all, by_inviter, by_candidate] = store
        .open_counts(&req.ticket.inviter, &req.candidate)
        .map_err(fail)?;
    if all >= LIMITS.0 || by_inviter >= LIMITS.1 || by_candidate >= LIMITS.2 {
        return Err(TOO_MANY);
    }
    // A cloud candidate, and a person's new computer, always wait for an Allow.
    let ask = req.ticket.mode != "open"
        || req.kind == "cloud"
        || same_person(&team.roster, req, now).is_some();
    let row = Join {
        id: 0,
        ticket_id: req.ticket.ticket_id.clone(),
        inviter: req.ticket.inviter.clone(),
        candidate: req.candidate.clone(),
        name: req.name.clone(),
        computer: req.computer.clone(),
        kind: req.kind.clone(),
        created_at: now,
    };
    let n = store.put_join(&row).map_err(fail)?;
    eprintln!("join: J{n} open for {}", net::short(&req.candidate));
    if ask {
        return Ok((n, None));
    }
    let v = try_admit(d, &mut team, &store, n, req, once, now);
    close(&store, n, v);
    Ok((n, Some(v)))
}

/// Whether this computer may admit `req` into `r` now: the ticket and its live link, the
/// secret, the kind and its proof (a cloud member needs a `workspace` one; a computer's, if
/// any, must verify), a free name and prefix, and our authority: over that link, or (a person's
/// new computer) being that person's computer, else `FORWARD`. Whether the link is a once link.
fn check(
    r: &Roster,
    store: &Store,
    req: &Request,
    me: &str,
    now: i64,
) -> Result<bool, &'static str> {
    let t = &req.ticket;
    ticket::verify(t, r).map_err(|_| BAD_TICKET)?;
    let link = &r.links[&t.ticket_id];
    if link.expires_at <= now {
        return Err(EXPIRED_TICKET);
    }
    // `verify` proved `t.secret` is the link's; the `used_by` hint refuses a spent once link
    // early (the roster's first applied admit is what consumes it).
    if req.proof != proof(&t.secret, &req.candidate)
        || store.link_used(&t.ticket_id).unwrap_or(true)
    {
        return Err(BAD_TICKET);
    }
    let cloud = match req.kind.as_str() {
        "computer" => false,
        "cloud" => true,
        _ => return Err(UNSUPPORTED),
    };
    // Decided from the proof, never from what the request says of itself.
    let c = match claims(req, now) {
        None if cloud => return Err(UNSUPPORTED),
        None => None,
        Some(Ok(c)) if (c.kind == ProofKind::Workspace) == cloud => Some(c),
        Some(_) if cloud => return Err(UNSUPPORTED),
        Some(_) => return Err(BAD_REQUEST),
    };
    let name = match c.as_ref().and_then(|c| c.workspace.as_ref()) {
        Some(w) if cloud => w.name.as_str(),
        _ => req.name.as_str(),
    };
    if !fits(name) || !fits(&req.computer) {
        return Err(BAD_REQUEST);
    }
    let same = same_person(r, req, now);
    match r.collides(name, &req.candidate, same.as_deref()) {
        Some(Collision::Name) => return Err(NAME_TAKEN),
        Some(Collision::Key) => return Err(KEY_COLLISION),
        None => {}
    }
    let mine = r.members.get(me);
    if let Some(p) = same {
        return match mine {
            Some(m) if m.kind == MemberKind::Computer && m.person_id == p => Ok(link.once),
            _ => Err(FORWARD),
        };
    }
    let authorised = mine.is_some_and(|m| {
        m.kind == MemberKind::Computer && (m.role != Role::Member || (link.signer == me && !cloud))
    });
    if !authorised {
        return Err(net_ops::NOT_ADMIN);
    }
    Ok(link.once)
}

/// A name or computer name: trimmed, 1-64 characters, no control characters.
fn fits(s: &str) -> bool {
    !s.is_empty() && s.trim() == s && s.chars().count() <= 64 && !s.chars().any(char::is_control)
}

/// Writes request `n`'s checked `admit`; for a `once` link, marks it used here.
fn try_admit(
    d: &Daemon,
    team: &mut TeamState,
    store: &Store,
    n: i64,
    req: &Request,
    once: bool,
    now: i64,
) -> Verdict {
    let mut body = json!({"member": req.candidate, "name": req.name, "computer": req.computer,
        "person_id": req.candidate, "kind": req.kind, "role": "member",
        "ticket_id": req.ticket.ticket_id});
    // `check` verified it: the person (or `ws:<id>`) it names, and a workspace's own name.
    if let Some(Ok(c)) = claims(req, now) {
        if let Some(w) = c.workspace.as_ref().filter(|_| req.kind == "cloud") {
            body["name"] = w.name.clone().into();
        }
        body["person_id"] = c.sub.into();
        body["proof"] = req.token.clone().into();
    }
    if let Err(e) = net_ops::write(d, team, store, Kind::Admit, body, now) {
        eprintln!("join: J{n}: {e}");
        return Err(INTERNAL);
    }
    if !team.roster.members.contains_key(&req.candidate) {
        return Err(BAD_TICKET);
    }
    if once {
        let _ = store.use_link(&req.ticket.ticket_id, &req.candidate);
    }
    Ok(())
}

/// Closes request `n`'s row with `v`'s state.
fn close(store: &Store, n: i64, v: Verdict) {
    let state = words(v).1;
    let _ = store.close_join(n, state);
    eprintln!("join: J{n} {state}");
}

/// The person allowed request `n`: checked again (the roster may have moved), then admitted.
fn admit(d: &Daemon, n: i64, req: &Request) -> Verdict {
    let mut team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let now = ipc::now() as i64;
    let v = check(&team.roster, &store, req, &d.identity.key, now)
        .and_then(|once| try_admit(d, &mut team, &store, n, req, once, now));
    close(&store, n, v);
    v
}

/// Shows request `n` to the person and waits for an answer (dialog or `team.admit`), the
/// candidate to hang up (`gone`), or the wait to run out (`not_answered`); `None` when the
/// candidate is gone.
async fn wait(d: &Daemon, gone: impl Future<Output = ()>, n: i64, req: Request) -> Option<Verdict> {
    let (tx, mut rx) = oneshot::channel();
    let text = prompt(&d.team.lock().unwrap().roster, &req, ipc::now() as i64);
    // Before the dialog: a headless one answers at once.
    d.admissions.lock().unwrap().insert(n, Pending { req, tx });
    let id = format!("J{n}");
    let buttons = ["Decline", "Allow"];
    d.dialogs
        .show(&id, dialog::args("Lightbulb Connect", &text, &buttons));
    let gone = tokio::select! {
        v = &mut rx => return v.ok(),
        _ = gone => true,
        _ = tokio::time::sleep(d.peers.knobs().join_wait) => false,
    };
    // Gave up, unless an answer took it first: then its verdict is on the way.
    if d.admissions.lock().unwrap().remove(&n).is_none() {
        return rx.await.ok();
    }
    d.dialogs.withdraw(&id);
    let _ = d.store.lock().unwrap().close_join(n, "expired");
    eprintln!("join: J{n} expired");
    (!gone).then_some(Err(NOT_ANSWERED))
}

/// The person's answer to request `n`, from the dialog or `team.admit`: its name and verdict,
/// or `None` when it is not waiting (answered, or its candidate gone).
fn answer(d: &Daemon, n: i64, allow: bool) -> Option<(String, Verdict)> {
    let p = d.admissions.lock().unwrap().remove(&n)?;
    d.dialogs.withdraw(&format!("J{n}"));
    let v = if allow {
        admit(d, n, &p.req)
    } else {
        close(&d.store.lock().unwrap(), n, Err(DECLINED));
        Err(DECLINED)
    };
    let _ = p.tx.send(v);
    Some((p.req.name, v))
}

/// Sends the signed decision (after an allow, the whole roster), then waits for the candidate
/// to close.
async fn tell(
    d: &Daemon,
    conn: &Connection,
    mut send: SendStream,
    ticket_id: &str,
    candidate: &str,
    v: Verdict,
) {
    let decision = Frame::JoinDecision {
        ticket_id: ticket_id.into(),
        candidate: candidate.into(),
        verdict: words(v).0.into(),
        reason: v.err().map(Into::into),
        sig: String::new(),
    };
    let mut out = vec![signed(&d.identity, decision)];
    if v.is_ok() {
        out.extend(net::pages(net::delta(&d.team.lock().unwrap().roster, &[])));
    }
    for f in &out {
        if frames::write(&mut send, f).await.is_err() {
            break;
        }
    }
    let _ = send.finish();
    let _ = tokio::time::timeout(STEP, conn.closed()).await;
    conn.close(VarInt::from_u32(0), b"");
}

/// A person's new computer, asking this computer: hands the candidate's own signed request to
/// that person's first online computer (replay order) and waits for its decision, as `wait`
/// does for the person here; after an allow, until its `admit` has synced here. `None` when
/// the candidate is gone; `your_computer_offline` when none of the person's is online.
async fn forward(d: &Daemon, conn: &Connection, raw: Frame, req: &Request) -> Option<Verdict> {
    let now = ipc::now() as i64;
    let to = {
        let t = d.team.lock().unwrap();
        let person = same_person(&t.roster, req, now)?;
        let ms = t.roster.computers_of(&person);
        ms.into_iter()
            .map(|m| m.key.clone())
            .find(|k| d.peers.is_online(k))
    };
    let cand = net::short(&req.candidate).to_string();
    let Some(to) = to else {
        eprintln!("join: {cand}'s person has no computer online");
        return Some(Err(YOUR_COMPUTER_OFFLINE));
    };
    let (tx, rx) = oneshot::channel();
    let pending = (to.clone(), tx);
    d.forwards
        .lock()
        .unwrap()
        .insert(req.candidate.clone(), pending);
    if d.peers.send(&to, raw).is_err() {
        d.forwards.lock().unwrap().remove(&req.candidate);
        return Some(Err(YOUR_COMPUTER_OFFLINE));
    }
    eprintln!("join: {cand} forwarded to {}", net::short(&to));
    // That computer gives up at its wait; we a step later.
    let v = tokio::select! {
        v = rx => v.ok(),
        _ = conn.closed() => None,
        _ = tokio::time::sleep(d.peers.knobs().join_wait + STEP) => Some(Err(NOT_ANSWERED)),
    };
    d.forwards.lock().unwrap().remove(&req.candidate);
    let v = v?;
    let deadline = tokio::time::Instant::now() + STEP;
    while v.is_ok()
        && !d
            .team
            .lock()
            .unwrap()
            .roster
            .members
            .contains_key(&req.candidate)
    {
        if tokio::time::Instant::now() > deadline {
            eprintln!("join: {cand}'s admit did not arrive");
            return Some(Err(INTERNAL));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Some(v)
}

/// A join frame member `from` sent: a `join_request` it forwarded (a new computer of this
/// computer's person, `forward`), checked, asked and admitted as one that came here, and
/// answered with a signed `join_decision`; or that decision, for a request we forwarded to it.
/// Either signed otherwise is dropped.
pub async fn from_member(d: Arc<Daemon>, from: String, f: Frame) {
    match f {
        Frame::JoinRequest { .. } => {
            let Some((req, ticket_id)) = forwarded(&f) else {
                return eprintln!("join: dropped a forged request from {}", net::short(&from));
            };
            let verdict = match open(&d, &req) {
                // Never forwarded twice: we are not that person's computer.
                Err(FORWARD) => Some(Err(net_ops::NOT_ADMIN)),
                Err(r) => Some(Err(r)),
                Ok((_, Some(v))) => Some(v),
                Ok((n, None)) => wait(&d, std::future::pending(), n, req.clone()).await,
            };
            let Some(v) = verdict else { return };
            let decision = Frame::JoinDecision {
                ticket_id,
                candidate: req.candidate,
                verdict: words(v).0.into(),
                reason: v.err().map(Into::into),
                sig: String::new(),
            };
            let _ = d.peers.send(&from, signed(&d.identity, decision));
        }
        Frame::JoinDecision {
            ref candidate,
            ref verdict,
            ref reason,
            ..
        } if signed_by(&from, &f) => {
            let mut fw = d.forwards.lock().unwrap();
            if fw.get(candidate).is_none_or(|(to, _)| *to != from) {
                return;
            }
            let (_, tx) = fw.remove(candidate).expect("present");
            let v = match verdict.as_str() {
                "allow" => Ok(()),
                "decline" => Err(DECLINED),
                _ => Err(known(reason.as_deref())),
            };
            let _ = tx.send(v);
        }
        _ => {}
    }
}

/// The request in a forwarded `join_request`, when its candidate signed it, and its ticket id.
fn forwarded(f: &Frame) -> Option<(Request, String)> {
    let Frame::JoinRequest {
        ticket,
        secret_proof,
        candidate,
        name,
        computer,
        kind,
        proof,
        ..
    } = f
    else {
        return None;
    };
    let ticket = ticket::decode(ticket)
        .ok()
        .filter(|_| signed_by(candidate, f))?;
    let id = ticket.ticket_id.clone();
    let req = Request {
        ticket,
        proof: secret_proof.clone(),
        candidate: candidate.clone(),
        name: name.clone(),
        computer: computer.clone(),
        kind: kind.clone(),
        token: proof.clone(),
    };
    Some((req, id))
}

/// `reason` when it is one a `join_decision` may carry, else `refused`.
fn known(reason: Option<&str>) -> &'static str {
    REASONS
        .iter()
        .find(|r| reason == Some(**r))
        .copied()
        .unwrap_or("refused")
}

/// Routes the person's dialog answers by id: `J<n>` to admission, `H<n>` to holds. `None`
/// (gave up, no GUI) leaves the request waiting for `approve`/`decline` until its candidate gives up.
pub async fn route(d: Arc<Daemon>) {
    let Some(mut answers) = d.dialog_answers.lock().unwrap().take() else {
        return;
    };
    while let Some((id, button)) = answers.recv().await {
        let Some(b) = button else { continue };
        if let Some(n) = dialog::number(&id, 'J') {
            answer(&d, n, b == "Allow");
        } else if let Some(n) = dialog::number(&id, 'H') {
            crate::holds::on_dialog(&d, n, &b);
        } else {
            eprintln!("dialog {id}: nothing waits for it");
        }
    }
}

/// `team.join_requests {}` -> `{requests: [{id, candidate, name, computer, created_at}]}`:
/// the open ones.
pub fn requests_op(id: u64, d: &Daemon) -> Response {
    match d.store.lock().unwrap().open_joins() {
        Ok(rows) => {
            let requests: Vec<Value> = rows
                .iter()
                .map(|j| {
                    json!({"id": format!("J{}", j.id), "candidate": j.candidate, "name": j.name,
                        "computer": j.computer, "created_at": j.created_at})
                })
                .collect();
            Response::ok(id, json!({ "requests": requests }))
        }
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// `team.admit {request: "J<n>", verdict: "allow"|"decline"}` -> `{name, state, reason?}`
/// (person-only); `not_open` once it was answered or its candidate gave up.
pub fn admit_op(id: u64, body: &Value, d: &Daemon) -> Response {
    let allow = match body["verdict"].as_str() {
        Some("allow") => true,
        Some("decline") => false,
        _ => return Response::err(id, BAD_REQUEST),
    };
    let Some(n) = body["request"]
        .as_str()
        .and_then(|r| dialog::number(r, 'J'))
    else {
        return Response::err(id, BAD_REQUEST);
    };
    match answer(d, n, allow) {
        Some((name, v)) => Response::ok(
            id,
            json!({"name": name, "state": words(v).1, "reason": v.err()}),
        ),
        None => match d.store.lock().unwrap().join_exists(n) {
            Ok(true) => Response::err(id, NOT_OPEN),
            Ok(false) => Response::err(id, NOT_FOUND),
            Err(_) => Response::err(id, INTERNAL),
        },
    }
}

// ---- the candidate -----------------------------------------------------------------------

enum Outcome {
    Admitted,
    /// `not_answered`: no decision within the wait.
    Refused(&'static str),
    /// This member cannot decide (`not_admin`, `bad_ticket`: it may lack a fresh link, or the
    /// connection broke, `None`): try the next.
    Next(Option<&'static str>),
}

/// `team.join {ticket, name?, kind?, proof?}` -> `{state: admitted|waiting|refused, reason?,
/// team, online}`. Only a solo computer joins; it asks the ticket's members in order, one at a
/// time, carrying `proof` (else this computer's own sign-in proof, if any).
pub async fn join(id: u64, body: &Value, d: &Arc<Daemon>) -> Response {
    let Some(link) = body["ticket"].as_str() else {
        return Response::err(id, BAD_REQUEST);
    };
    let Ok(_joining) = d.joining.try_lock() else {
        return Response::err(id, BAD_REQUEST);
    };
    let name = body["name"]
        .as_str()
        .map_or_else(user_name, str::to_string)
        .trim()
        .to_string();
    let kind = body["kind"].as_str().unwrap_or("computer");
    let me = &d.identity.key;
    let done = |state: &str, reason: Option<&str>, team: &str, online: usize| {
        Response::ok(
            id,
            json!({"state": state, "reason": reason, "team": team, "online": online}),
        )
    };
    let (computer, team, solo) = {
        let t = d.team.lock().unwrap();
        let computer = t.config.computer.trim().to_string();
        (computer, t.roster.team_name.clone(), t.roster.is_solo(me))
    };
    if !solo {
        return done("refused", Some(ALREADY_IN_TEAM), &team, 0);
    }
    let Ok(t) = ticket::decode(link) else {
        return done("refused", Some(BAD_TICKET), "", 0);
    };
    let Some(ep) = d.peers.endpoint.get() else {
        return Response::err(id, INTERNAL);
    };
    // Its proof: the one given (the workspace member's), else this computer's sign-in.
    let token = body["proof"].as_str().map(String::from).or_else(|| {
        let t = d.team.lock().unwrap();
        let own = t.roster.members.get(me).and_then(|m| m.proof.as_ref());
        own.map(|p| p.token.clone())
    });
    let req = request_with(&d.identity, &t, &name, &computer, kind, token.as_deref());
    // The last listed member's refusal, when every reachable one refused.
    let mut last = None;
    for m in t.members.iter().filter(|m| *m != me) {
        let Some(addr) = d.peers.addr(m) else {
            continue;
        };
        let Ok(Ok(conn)) = tokio::time::timeout(STEP, ep.connect(addr, JOIN_ALPN)).await else {
            eprintln!("join: dial {} failed", net::short(m));
            continue;
        };
        let outcome = attempt(d, &conn, &req, m, &t).await;
        conn.close(VarInt::from_u32(0), b"");
        match outcome {
            Outcome::Next(r) => last = r.or(last),
            Outcome::Refused(NOT_ANSWERED) => return done("waiting", None, "", 0),
            Outcome::Refused(r) => return done("refused", Some(r), "", 0),
            Outcome::Admitted => {
                eprintln!("join: admitted by {}", net::short(m));
                // The dial loop connects to the team; give it a moment for the count.
                let _ = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let hello = d.peers.connected.notified();
                        if !d.peers.online().is_empty() {
                            return;
                        }
                        hello.await;
                    }
                })
                .await;
                let team = d.team.lock().unwrap().roster.team_name.clone();
                return done("admitted", None, &team, d.peers.online().len() + 1);
            }
        }
    }
    done("refused", Some(last.unwrap_or(UNREACHABLE)), "", 0)
}

/// One member's answer to `req` on `conn`.
async fn attempt(d: &Daemon, conn: &Connection, req: &Frame, m: &str, t: &Ticket) -> Outcome {
    let Ok((mut send, mut recv)) = conn.open_bi().await else {
        return Outcome::Next(None);
    };
    if frames::write(&mut send, req).await.is_err() {
        return Outcome::Next(None);
    }
    // The member gives up at its wait; we a step later.
    let wait = d.peers.knobs().join_wait + STEP;
    let f = match tokio::time::timeout(wait, frames::read(&mut recv)).await {
        Err(_) => return Outcome::Refused(NOT_ANSWERED),
        Ok(Err(_)) => return Outcome::Next(None),
        Ok(Ok(f)) => f,
    };
    let genuine = signed_by(m, &f);
    let Frame::JoinDecision {
        ticket_id,
        candidate,
        verdict,
        reason,
        ..
    } = f
    else {
        return Outcome::Next(None);
    };
    if !genuine || candidate != d.identity.key || ticket_id != t.ticket_id {
        return Outcome::Next(None);
    }
    let reason = known(reason.as_deref());
    match verdict.as_str() {
        "allow" => {}
        "decline" if reason == YOUR_COMPUTER_OFFLINE => return Outcome::Refused(reason),
        "decline" => return Outcome::Refused(DECLINED),
        _ if reason == net_ops::NOT_ADMIN || reason == BAD_TICKET => {
            return Outcome::Next(Some(reason))
        }
        _ => return Outcome::Refused(reason),
    }
    // ponytail: a roster lost after an allow tries the next member, which may then find the
    // name taken; the person runs `join` again with another name.
    let mut entries = vec![];
    loop {
        match tokio::time::timeout(STEP, frames::read(&mut recv)).await {
            Ok(Ok(Frame::Roster { entries: e, more })) if entries.len() < MAX_ENTRIES => {
                entries.extend(e);
                if !more {
                    break;
                }
            }
            _ => return Outcome::Next(None),
        }
    }
    match take_roster(d, &entries, &t.team_id) {
        Ok(true) => Outcome::Admitted,
        Ok(false) => Outcome::Refused(BAD_TICKET),
        Err(e) => {
            eprintln!("join: taking the roster: {e}");
            Outcome::Refused(INTERNAL)
        }
    }
}

/// Replaces this computer's solo roster with `entries` (parents first), each checked by
/// `roster::accept`, when their replay is team `team_id` with this computer in it; else
/// nothing changes. Whether it did.
fn take_roster(d: &Daemon, entries: &[Entry], team_id: &str) -> crate::Result<bool> {
    let mut team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let me = &d.identity.key;
    // Another join may have finished meanwhile.
    if !team.roster.is_solo(me) {
        return Ok(false);
    }
    let now = ipc::now() as i64;
    // Rolled back when it drops uncommitted.
    let tx = store.conn().unchecked_transaction()?;
    store.clear_roster()?;
    for e in entries {
        roster::accept(&store, e, now)?;
    }
    let new = roster::replay(&roster::entries(&store)?);
    if new.team_id != team_id || !new.members.contains_key(me) {
        return Ok(false);
    }
    tx.commit()?;
    net_ops::install(d, &mut team, &store, new)?;
    Ok(true)
}
