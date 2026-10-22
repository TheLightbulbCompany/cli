//! Admission: how a stranger becomes a member. The candidate dials a member its ticket lists
//! on `JOIN_ALPN` and sends a signed `join_request`; that member checks it, asks its person
//! (or admits at once for an `open` link), writes the `admit`, and answers a signed
//! `join_decision`, followed after an allow by the whole roster in `roster` pages. The
//! candidate checks every entry itself before that roster replaces its solo one.

use crate::daemon::ipc::{self, Response, BAD_REQUEST, INTERNAL, NOT_FOUND, UNSUPPORTED};
use crate::daemon::{net_ops, user_name, Daemon};
use crate::dialog;
use crate::identity::{self, Identity};
use crate::net::frames::{self, Frame};
use crate::net::{self, JOIN_ALPN};
use crate::roster::{self, Entry, Kind, MemberKind, Role, Roster};
use crate::store::{Join, Store};
use crate::ticket::{self, Ticket};
use data_encoding::HEXLOWER;
use iroh::endpoint::{Connection, SendStream, VarInt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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

/// A dial, a first frame, a `roster` page, the goodbye: each gets this long.
const STEP: Duration = Duration::from_secs(10);
/// Open requests at once: per team, per inviter, per candidate.
const LIMITS: (usize, usize, usize) = (20, 5, 1);
/// The roster a candidate takes in, at most.
const MAX_ENTRIES: usize = 100_000;

/// `Ok` allows; `Err(reason)` refuses (`declined`: the person said no).
type Verdict = Result<(), &'static str>;

/// A request waiting for its person, and the connection task to tell.
pub struct Pending {
    req: Request,
    tx: oneshot::Sender<Verdict>,
}

/// The waiting requests by row id (`J<id>`). First answer wins: whoever takes the entry.
pub type Waiting = Mutex<HashMap<i64, Pending>>;

struct Request {
    ticket: Ticket,
    proof: String,
    candidate: String,
    name: String,
    computer: String,
    kind: String,
}

/// What a `join_request` proves it holds: hex sha256(secret ‖ candidate).
pub fn proof(secret: &str, candidate: &str) -> String {
    HEXLOWER.encode(&Sha256::digest(format!("{secret}{candidate}").as_bytes()))
}

/// `id`'s signed `join_request` for ticket `t`.
pub fn request(id: &Identity, t: &Ticket, name: &str, computer: &str, kind: &str) -> Frame {
    signed(
        id,
        Frame::JoinRequest {
            ticket: ticket::encode(t),
            secret_proof: proof(&t.secret, &id.key),
            candidate: id.key.clone(),
            name: name.into(),
            computer: computer.into(),
            kind: kind.into(),
            proof: None,
            sig: String::new(),
        },
    )
}

fn signed(id: &Identity, f: Frame) -> Frame {
    let mut v = serde_json::to_value(&f).expect("a frame serializes");
    v["sig"] = id.sign(&v).into();
    serde_json::from_value(v).expect("the same frame")
}

fn signed_by(key: &str, f: &Frame) -> bool {
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
    let Frame::JoinRequest {
        ticket,
        secret_proof,
        candidate,
        name,
        computer,
        kind,
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
    // Admitted already (its roster lost on the way): the roster again, nothing written.
    if d.team.lock().unwrap().roster.members.contains_key(&peer) {
        eprintln!(
            "join: {} is a member; resending the roster",
            net::short(&peer)
        );
        let ticket_id = ticket::decode(&ticket).map_or_else(|_| String::new(), |t| t.ticket_id);
        tell(&d, &conn, send, &ticket_id, &peer, Ok(())).await;
        return;
    }
    let ticket_id;
    let verdict = match ticket::decode(&ticket) {
        Err(_) => {
            ticket_id = String::new();
            Err(BAD_TICKET)
        }
        Ok(ticket) => {
            ticket_id = ticket.ticket_id.clone();
            let req = Request {
                ticket,
                proof: secret_proof,
                candidate,
                name,
                computer,
                kind,
            };
            match open(&d, &req) {
                Err(r) => Err(r),
                Ok(n) if req.ticket.mode == "open" => admit(&d, n, &req),
                Ok(n) => match wait(&d, &conn, n, req).await {
                    Some(v) => v,
                    None => {
                        conn.close(VarInt::from_u32(0), b"");
                        return;
                    }
                },
            }
        }
    };
    if let Err(r) = verdict {
        eprintln!("join: refused {} ({r})", net::short(&peer));
    }
    tell(&d, &conn, send, &ticket_id, &peer, verdict).await;
}

/// Checks `req` and stores it as an `open` row within the limits: its row id.
fn open(d: &Daemon, req: &Request) -> Result<i64, &'static str> {
    let team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let now = ipc::now() as i64;
    check(&team.roster, &store, req, &d.identity.key, now)?;
    let fail = |e: rusqlite::Error| {
        eprintln!("join: {e}");
        INTERNAL
    };
    // A request older than the wait (and the step its waiter may lag) is one its candidate
    // stopped waiting for.
    let window = (d.peers.knobs().join_wait + STEP).as_secs() as i64;
    store.expire_joins(now - window).map_err(fail)?;
    let open = store.open_joins().map_err(fail)?;
    let count = |f: &dyn Fn(&Join) -> bool| open.iter().filter(|j| f(j)).count();
    if open.len() >= LIMITS.0
        || count(&|j| j.inviter == req.ticket.inviter) >= LIMITS.1
        || count(&|j| j.candidate == req.candidate) >= LIMITS.2
    {
        return Err(TOO_MANY);
    }
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
    Ok(n)
}

/// Whether this computer may admit `req` into `r` now: the ticket and its live link, the
/// secret, the kind, a free name and prefix, and our authority over that link. Whether the
/// link is a once link.
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
    // ponytail: a cloud member needs a `kind=bridge` proof (Task 13).
    if req.kind != "computer" {
        return Err(UNSUPPORTED);
    }
    if !label(&req.name) || !label(&req.computer) {
        return Err(BAD_REQUEST);
    }
    let name = req.name.to_lowercase();
    if r.members.values().any(|m| m.name.to_lowercase() == name) {
        return Err(NAME_TAKEN);
    }
    if r.members
        .values()
        .any(|m| req.candidate.starts_with(&m.short))
    {
        return Err(KEY_COLLISION);
    }
    let authorised = r.members.get(me).is_some_and(|m| {
        m.kind == MemberKind::Computer && (m.role != Role::Member || link.signer == me)
    });
    if !authorised {
        return Err(net_ops::NOT_ADMIN);
    }
    Ok(link.once)
}

/// A name or computer name: trimmed, 1-64 characters, no control characters.
fn label(s: &str) -> bool {
    !s.is_empty() && s.trim() == s && s.chars().count() <= 64 && !s.chars().any(char::is_control)
}

/// Writes request `n`'s `admit` after checking it again (the roster may have moved since).
fn admit(d: &Daemon, n: i64, req: &Request) -> Verdict {
    let mut team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let now = ipc::now() as i64;
    let v = (|| {
        let once = check(&team.roster, &store, req, &d.identity.key, now)?;
        let body = json!({"member": req.candidate, "name": req.name, "computer": req.computer,
            "person_id": req.candidate, "kind": "computer", "role": "member",
            "ticket_id": req.ticket.ticket_id});
        if let Err(e) = net_ops::write(d, &mut team, &store, Kind::Admit, body, now) {
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
    })();
    let _ = store.close_join(n, if v.is_ok() { "admitted" } else { "refused" });
    eprintln!(
        "join: J{n} {}",
        if v.is_ok() { "admitted" } else { "refused" }
    );
    v
}

/// Shows request `n` to the person and waits for an answer (dialog or `team.admit`), the
/// candidate to hang up, or the wait to run out (`not_answered`); `None` when the candidate is
/// gone.
async fn wait(d: &Daemon, conn: &Connection, n: i64, req: Request) -> Option<Verdict> {
    let (tx, mut rx) = oneshot::channel();
    let text = format!("{} wants to join your network", req.name);
    // Before the dialog: a headless one answers at once.
    d.admissions.lock().unwrap().insert(n, Pending { req, tx });
    let id = format!("J{n}");
    let buttons = ["Decline", "Allow"];
    d.dialogs
        .show(&id, dialog::args("Lightbulb Network", &text, &buttons));
    let gone = tokio::select! {
        v = &mut rx => return v.ok(),
        _ = conn.closed() => true,
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
        let _ = d.store.lock().unwrap().close_join(n, "declined");
        eprintln!("join: J{n} declined");
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
    let (verdict, reason) = match v {
        Ok(()) => ("allow", None),
        Err(DECLINED) => ("decline", Some(DECLINED)),
        Err(r) => ("refused", Some(r)),
    };
    let decision = Frame::JoinDecision {
        ticket_id: ticket_id.into(),
        candidate: candidate.into(),
        verdict: verdict.into(),
        reason: reason.map(Into::into),
        sig: String::new(),
    };
    let mut out = vec![signed(&d.identity, decision)];
    if v.is_ok() {
        let team = d.team.lock().unwrap();
        out.extend(net::pages(
            team.roster.log().iter().map(|(_, e)| e.clone()).collect(),
        ));
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

/// Routes the person's dialog answers by id: `J<n>` to admission. `None` (gave up, no GUI)
/// leaves the request waiting for `approve`/`decline` until its candidate gives up.
pub async fn route(d: Arc<Daemon>) {
    let Some(mut answers) = d.dialog_answers.lock().unwrap().take() else {
        return;
    };
    while let Some((id, button)) = answers.recv().await {
        match id.split_at_checked(1) {
            Some(("J", n)) => {
                if let (Ok(n), Some(b)) = (n.parse(), button) {
                    answer(&d, n, b == "Allow");
                }
            }
            // Task 9 adds `H<n>`, a hold's answer.
            _ => eprintln!("dialog {id}: nothing waits for it"),
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
    let n = body["request"]
        .as_str()
        .and_then(|s| s.strip_prefix('J'))
        .and_then(|s| s.parse::<i64>().ok());
    let allow = match body["verdict"].as_str() {
        Some("allow") => true,
        Some("decline") => false,
        _ => return Response::err(id, BAD_REQUEST),
    };
    let Some(n) = n else {
        return Response::err(id, BAD_REQUEST);
    };
    match answer(d, n, allow) {
        Some((name, v)) => {
            let state = match v {
                Ok(()) => "admitted",
                Err(DECLINED) => "declined",
                Err(_) => "refused",
            };
            Response::ok(id, json!({"name": name, "state": state, "reason": v.err()}))
        }
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
    Refused(String),
    /// No decision within the wait.
    Waiting,
    /// This member cannot decide (`not_admin`, `bad_ticket`: it may lack a fresh link, or the
    /// connection broke, `None`): try the next.
    Next(Option<String>),
}

/// Set while a `team.join` runs: one at a time.
static JOINING: AtomicBool = AtomicBool::new(false);

struct Joining;

impl Drop for Joining {
    fn drop(&mut self) {
        JOINING.store(false, Ordering::SeqCst);
    }
}

/// `team.join {ticket, name?, kind?}` -> `{state: admitted|waiting|refused, reason?, team,
/// online}`. Only a solo computer joins; it asks the ticket's members in order, one at a time.
pub async fn join(id: u64, body: &Value, d: &Arc<Daemon>) -> Response {
    let Some(link) = body["ticket"].as_str() else {
        return Response::err(id, BAD_REQUEST);
    };
    if JOINING.swap(true, Ordering::SeqCst) {
        return Response::err(id, BAD_REQUEST);
    }
    let _joining = Joining;
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
        let solo = t.roster.members.len() == 1 && t.roster.members.contains_key(me);
        let computer = t.config.computer.trim().to_string();
        (computer, t.roster.team_name.clone(), solo)
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
    let req = request(&d.identity, &t, &name, &computer, kind);
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
            Outcome::Next(r) => {
                last = r.or(last);
                continue;
            }
            Outcome::Waiting => return done("waiting", None, "", 0),
            Outcome::Refused(r) => return done("refused", Some(&r), "", 0),
            Outcome::Admitted => {
                // The dial loop connects to the team; give it a moment for the count.
                let deadline = Instant::now() + Duration::from_secs(5);
                while d.peers.online().is_empty() && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                let team = d.team.lock().unwrap().roster.team_name.clone();
                eprintln!("join: admitted by {}", net::short(m));
                return done("admitted", None, &team, d.peers.online().len() + 1);
            }
        }
    }
    done(
        "refused",
        Some(last.as_deref().unwrap_or(UNREACHABLE)),
        "",
        0,
    )
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
        Err(_) => return Outcome::Waiting,
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
    match (verdict.as_str(), reason.as_deref()) {
        ("allow", _) => {}
        ("decline", _) => return Outcome::Refused(DECLINED.into()),
        (_, Some(NOT_ANSWERED)) => return Outcome::Waiting,
        (_, Some(r @ (net_ops::NOT_ADMIN | BAD_TICKET))) => return Outcome::Next(Some(r.into())),
        (_, r) => return Outcome::Refused(r.unwrap_or("refused").into()),
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
        Ok(false) => Outcome::Refused(BAD_TICKET.into()),
        Err(e) => {
            eprintln!("join: taking the roster: {e}");
            Outcome::Refused(INTERNAL.into())
        }
    }
}

/// Replaces this computer's solo roster with `entries` (parents first), each checked by
/// `roster::accept`, when their replay is team `team_id` with this computer in it; else
/// nothing changes. Whether it did.
fn take_roster(d: &Daemon, entries: &[Entry], team_id: &str) -> crate::Result<bool> {
    let mut team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    // Another join may have finished meanwhile.
    if team.roster.members.len() != 1 {
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
    if new.team_id != team_id || !new.members.contains_key(&d.identity.key) {
        return Ok(false);
    }
    tx.commit()?;
    net_ops::install(d, &mut team, &store, new)?;
    Ok(true)
}
