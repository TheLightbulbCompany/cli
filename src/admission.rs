//! Admission: how a stranger becomes a member. The candidate dials a member its ticket lists
//! on `JOIN_ALPN` and sends a signed `join_request`; that member checks it, asks its person
//! (or admits at once for an `open` link), writes the `admit`, and answers a signed
//! `join_decision`, followed after an allow by the whole roster in `roster` pages. The
//! candidate checks every entry itself before that roster replaces its solo one.

use crate::daemon::ipc::{self, Response, BAD_REQUEST, INTERNAL, UNSUPPORTED};
use crate::daemon::TeamState;
use crate::daemon::{net_ops, user_name, Daemon};
use crate::dialog;
use crate::identity::{self, Identity};
use crate::net::frames::{self, Frame};
use crate::net::{self, JOIN_ALPN};
use crate::roster::{self, Collision, Entry, Kind, Roster};
use crate::store::{Join, Store};
use crate::ticket::{self, Ticket};
use data_encoding::HEXLOWER;
use iroh::endpoint::{Connection, SendStream, VarInt};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;

pub const NOT_OPEN: &str = "not_open";
pub const ALREADY_IN_TEAM: &str = "already_in_team";
pub const BAD_TICKET: &str = "bad_ticket";
/// A code that is no code, or that no inviter serves (wrong, used, or the inviter is off).
pub const NO_CODE: &str = "no_code";
pub const EXPIRED_TICKET: &str = "expired_ticket";
pub const NAME_TAKEN: &str = "name_taken";
pub const KEY_COLLISION: &str = "key_collision";
pub const UNREACHABLE: &str = "unreachable";
pub const DECLINED: &str = "declined";
/// A name or computer name with characters a dialog cannot show as they are (`fits`).
pub const BAD_NAME: &str = "bad_name";
pub const TOO_MANY: &str = "too_many_requests";
/// The person did not answer within the wait: the candidate is told so and stops asking.
pub const NOT_ANSWERED: &str = "not_answered";
/// An agent's join: its person pressed Don't join. This computer's own, never on the wire.
pub const PERSON_SAID_NO: &str = "person_said_no";
/// An agent's join with no answer from a dialog (none, as over SSH, or it timed out). Likewise.
pub const NEEDS_A_PERSON: &str = "needs_a_person";
/// `NEEDS_A_PERSON` as a sentence for the agent.
pub const NEEDS_A_PERSON_TEXT: &str = "Lightbulb could not get an answer from your person on this computer. Ask your person to run lightbulb join in their own terminal.";
/// `PERSON_SAID_NO` as a sentence for the agent.
pub const PERSON_SAID_NO_TEXT: &str = "Your person chose not to join this network.";
/// Every reason a `join_decision` may carry; another reads as `refused`.
const REASONS: &[&str] = &[
    BAD_TICKET,
    EXPIRED_TICKET,
    NAME_TAKEN,
    KEY_COLLISION,
    DECLINED,
    BAD_NAME,
    TOO_MANY,
    NOT_ANSWERED,
    UNSUPPORTED,
    BAD_REQUEST,
    INTERNAL,
    net_ops::NOT_ADMIN,
];

/// A dial, a first frame, a `roster` page, the goodbye: each gets this long.
const STEP: Duration = Duration::from_secs(10);
/// Between dial rounds while a rung workspace wakes.
const WAKE_RETRY: Duration = Duration::from_secs(5);
/// How long a member's silence on a join request means it is asking its person.
const ASKING: Duration = Duration::from_secs(1);
/// How long an agent's join waits for its person: past the dialog's own 600 s.
const CONFIRM_WAIT: Duration = Duration::from_secs(610);
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
        Err(DECLINED) => ("decline", "declined"),
        Err(_) => ("refused", "refused"),
    }
}

#[derive(Clone)]
pub struct Request {
    ticket: Ticket,
    proof: String,
    candidate: String,
    name: String,
    computer: String,
}

impl Request {
    /// The request in join frame `f`, when it is one and its ticket decodes; that the candidate
    /// signed it is the caller's check.
    fn from_frame(f: &Frame) -> Option<Request> {
        let Frame::JoinRequest {
            ticket,
            secret_proof,
            candidate,
            name,
            computer,
            ..
        } = f
        else {
            return None;
        };
        Some(Request {
            ticket: ticket::decode(ticket).ok()?,
            proof: secret_proof.clone(),
            candidate: candidate.clone(),
            name: name.clone(),
            computer: computer.clone(),
        })
    }
}

/// What a `join_request` proves it holds: hex sha256(secret ‖ candidate).
pub fn proof(secret: &str, candidate: &str) -> String {
    HEXLOWER.encode(&Sha256::digest(format!("{secret}{candidate}").as_bytes()))
}

/// `id`'s signed `join_request` for ticket `t`.
pub fn request(id: &Identity, t: &Ticket, name: &str, computer: &str) -> Frame {
    signed(
        id,
        Frame::JoinRequest {
            ticket: ticket::encode(t),
            secret_proof: proof(&t.secret, &id.key),
            candidate: id.key.clone(),
            name: name.into(),
            computer: computer.into(),
            sig: String::new(),
        },
    )
}

/// `f` with its `sig`: `id`'s signature over it (`sig` left out).
pub fn signed(id: &Identity, f: Frame) -> Frame {
    let v = id.signed(serde_json::to_value(&f).expect("a frame serializes"));
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
    let Frame::JoinRequest { candidate, .. } = &f else {
        conn.close(VarInt::from_u32(0), b"");
        return;
    };
    if *candidate != peer || !genuine {
        eprintln!(
            "join: dropped a forged join request from {}",
            net::short(&peer)
        );
        conn.close(VarInt::from_u32(0), b"");
        return;
    }
    let Some(req) = Request::from_frame(&f) else {
        eprintln!("join: refused {} ({BAD_TICKET})", net::short(&peer));
        return tell(&d, &conn, send, "", &peer, Err(BAD_TICKET)).await;
    };
    let ticket_id = req.ticket.ticket_id.clone();
    // Admitted already (its roster lost on the way): the roster again, nothing written.
    if d.team.lock().unwrap().roster.members.contains_key(&peer) {
        eprintln!(
            "join: {} is a member; resending the roster",
            net::short(&peer)
        );
        return tell(&d, &conn, send, &ticket_id, &peer, Ok(())).await;
    }
    let verdict = open(&d, &req).map_or_else(Err, |(_, v)| v);
    if let Err(r) = verdict {
        eprintln!("join: refused {} ({r})", net::short(&peer));
    }
    tell(&d, &conn, send, &ticket_id, &peer, verdict).await;
}

/// Checks `req` and stores it as an `open` row within the limits; an `open` link's request is
/// admitted at once, under the same locks. Its row id, and its verdict when decided already.
fn open(d: &Daemon, req: &Request) -> Result<(i64, Verdict), &'static str> {
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
    // An invite is the approval: nobody is asked. (If an agent minted the link, its person is
    // told afterwards and may remove the member.)
    let by = d
        .agent_invites
        .lock()
        .unwrap()
        .remove(&req.ticket.ticket_id);
    let n = store.put_join(&row(req, now)).map_err(fail)?;
    eprintln!("join: J{n} for {}", net::short(&req.candidate));
    let v = try_admit(d, &mut team, &store, n, req, once, by.as_deref(), now);
    close(&store, n, v);
    if v.is_ok() {
        if let Some(by) = &by {
            let who = crate::label::render(&team.roster, &d.identity.key, &session_of(&store, by));
            let text = format!("{who} invited {} into your network.", req.name);
            let args = dialog::args("Lightbulb", &text, &["Keep", "Remove"]);
            d.dialogs.show(&format!("AR:{}", req.candidate), args);
        }
    }
    Ok((n, v))
}

/// The session an agent invite was minted by, as `label::render` reads it: `by` is
/// `<member>/<handle>` (`claude-8`, kept that way in the roster's "admitted by"), and the
/// session row with that handle gives its `<member>/<provider>:<id>` key, so the notice says
/// "Your Claude Code" rather than "Your claude-8 mailbox". `by` itself when no row has it.
fn session_of(store: &crate::store::Store, by: &str) -> String {
    let handle = by.split_once('/').map_or(by, |(_, h)| h);
    store
        .sessions()
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|s| s.handle.as_deref() == Some(handle))
        })
        .map_or_else(|| by.to_string(), |s| s.key)
}

/// `req`'s `join_requests` row, made at `now`.
fn row(req: &Request, now: i64) -> Join {
    Join {
        id: 0,
        ticket_id: req.ticket.ticket_id.clone(),
        inviter: req.ticket.inviter.clone(),
        candidate: req.candidate.clone(),
        name: req.name.clone(),
        computer: req.computer.clone(),
        created_at: now,
    }
}

/// Whether this computer may admit `req` into `r` now: the ticket and its live link, the
/// secret, a free name and prefix, and our membership. Whether the link is a once link.
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
    if !fits(&req.name) || !fits(&req.computer) {
        return Err(BAD_NAME);
    }
    match r.collides(&req.name, &req.candidate) {
        Some(Collision::Name) => return Err(NAME_TAKEN),
        Some(Collision::Key) => return Err(KEY_COLLISION),
        None => {}
    }
    if !r.members.contains_key(me) {
        return Err(net_ops::NOT_ADMIN);
    }
    Ok(link.once)
}

/// A name or computer name: trimmed, 1-64 characters, all `label::printable` (so the dialog
/// shows it as is).
pub fn fits(s: &str) -> bool {
    !s.is_empty()
        && s.trim() == s
        && s.chars().count() <= 64
        && s.chars().all(crate::label::printable)
}

/// Writes request `n`'s checked `admit`; for a `once` link, marks it used here.
#[allow(clippy::too_many_arguments)]
fn try_admit(
    d: &Daemon,
    team: &mut TeamState,
    store: &Store,
    n: i64,
    req: &Request,
    once: bool,
    by: Option<&str>,
    now: i64,
) -> Verdict {
    let mut body = json!({"member": req.candidate, "name": req.name, "computer": req.computer,
        "role": "member", "ticket_id": req.ticket.ticket_id});
    // The agent session that approved it, in the signed record.
    if let Some(by) = by {
        body["by"] = by.into();
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
        if id == dialog::JOIN_CONFIRM {
            if let Some(waiter) = d.join_confirm.lock().unwrap().take() {
                let _ = waiter.send(button);
            }
            continue;
        }
        let Some(b) = button else { continue };
        if let Some(n) = dialog::number(&id, 'H') {
            crate::holds::on_dialog(&d, n, &b);
        } else if let Some(key) = id.strip_prefix("AR:") {
            if b == "Remove" {
                net_ops::remove_member(&d, key);
            }
        } else {
            eprintln!("dialog {id}: nothing waits for it");
        }
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

/// The person's own answer to an agent's join, before this computer dials anyone. Anything but
/// Join is no. `t`: the ticket, when the agent gave one (a code's is not fetched until the person
/// has said yes). A ticket names no team, only its inviter.
async fn person_confirms(d: &Daemon, t: Option<&Ticket>) -> Result<(), &'static str> {
    let by = t.map_or(String::new(), |t| {
        format!(", invited by computer {}", net::short(&t.inviter))
    });
    let text = format!(
        "An agent on this computer asked to join a team{by}. Teammates there will see your \
         sessions' names, folders and status."
    );
    let (waiter, answer) = tokio::sync::oneshot::channel();
    *d.join_confirm.lock().unwrap() = Some(waiter);
    let args = dialog::args("Lightbulb", &text, &["Don't join", "Join"]);
    d.dialogs.show_over_claim(dialog::JOIN_CONFIRM, args);
    let answer = tokio::time::timeout(CONFIRM_WAIT, answer).await;
    if answer.is_err() {
        d.dialogs.withdraw(dialog::JOIN_CONFIRM);
    }
    match answer {
        Ok(Ok(Some(b))) if b == "Join" => Ok(()),
        Ok(Ok(Some(_))) => Err(PERSON_SAID_NO),
        _ => Err(NEEDS_A_PERSON),
    }
}

/// `team.join {ticket | code, name?}` -> `{state: admitted|waiting|refused, reason?, team,
/// online}`. Only a solo computer joins; it asks the ticket's members in order, one at a time.
/// `agent`: an agent asked on macOS, so its person confirms first.
pub async fn join(id: u64, body: &Value, d: &Arc<Daemon>, agent: bool) -> Response {
    let (ticket_in, code_in) = (body["ticket"].as_str(), body["code"].as_str());
    if ticket_in.is_some() == code_in.is_some() {
        return Response::err(id, BAD_REQUEST);
    }
    let Ok(_joining) = d.joining.try_lock() else {
        return Response::err(id, BAD_REQUEST);
    };
    let name = body["name"]
        .as_str()
        .map_or_else(user_name, str::to_string)
        .trim()
        .to_string();
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
    // What every member would refuse, refused before any dial.
    if !fits(&name) || !fits(&computer) {
        return done("refused", Some(BAD_NAME), "", 0);
    }
    if agent {
        // A ticket that cannot be used asks no one; `signed_by_inviter`: the dialog names its signer.
        let given = ticket_in.map(|l| ticket::decode(l).ok().filter(ticket::signed_by_inviter));
        if given.as_ref().is_some_and(Option::is_none) {
            return done("refused", Some(BAD_TICKET), "", 0);
        }
        if let Err(why) = person_confirms(d, given.flatten().as_ref()).await {
            return done("refused", Some(why), "", 0);
        }
        // Someone may have joined this computer's network during the wait.
        let t = d.team.lock().unwrap();
        if !t.roster.is_solo(me) {
            return done("refused", Some(ALREADY_IN_TEAM), &t.roster.team_name, 0);
        }
    }
    let Some(ep) = d.peers.endpoint.get() else {
        return Response::err(id, INTERNAL);
    };
    // A code stands for the ticket its inviter serves once; wrong, used and offline look the same.
    let fetched;
    let link = match (ticket_in, code_in) {
        (Some(link), _) => link,
        (_, Some(code)) => {
            let ticket = match crate::code::normalize(code) {
                Some(code) => crate::code::fetch(d, ep, &code).await,
                None => None,
            };
            let Some(ticket) = ticket else {
                return done("refused", Some(NO_CODE), "", 0);
            };
            fetched = ticket;
            &fetched
        }
        _ => return Response::err(id, BAD_REQUEST),
    };
    let Ok(t) = ticket::decode(link) else {
        return done("refused", Some(BAD_TICKET), "", 0);
    };
    let req = request(&d.identity, &t, &name, &computer);
    // The last listed member's refusal, when every reachable one refused.
    let mut last = None;
    // The inviter's addresses on the ticket: what reaches it with no relay and no mDNS, kept
    // for the dials after an admission too. Only from a ticket its inviter signed; one with
    // bad addresses is refused outright.
    let addrs = net::parse_addrs(&t.addrs).filter(|_| ticket::signed_by_inviter(&t));
    let Some(inviter_addrs) = addrs else {
        return done("refused", Some(BAD_TICKET), "", 0);
    };
    {
        let team = d.team.lock().unwrap();
        d.peers
            .learn(&d.dir, &t.inviter, inviter_addrs.clone(), &team.roster);
    }
    // A workspace member's ticket carries its doorbell: when no member answers, ring it once
    // and keep dialing while the workspace wakes (`join_waking`, for the CLI). The ticket is
    // its inviter's (checked above); the wake is re-parsed so a ticket cannot hand curl
    // anything `Wake::parse` refuses. A workspace member never rings.
    let mut wake = t
        .wake
        .as_ref()
        .filter(|_| d.cloud.is_none())
        .and_then(|w| crate::roster::Wake::parse(&json!({"url": w.url, "token": w.token})));
    let mut until: Option<tokio::time::Instant> = None;
    // A computer that has just started may reach nobody on its first pass (its relay and paths
    // are still coming up): while no member has answered, keep dialing for `dial_wait`.
    let dial_until = tokio::time::Instant::now() + d.peers.knobs().dial_wait;
    loop {
        for m in t.members.iter().filter(|m| *m != me) {
            let Some(mut addr) = d.peers.addr(m) else {
                continue;
            };
            // The inviter's relay, before this computer knows its own list (an https one only).
            if let Some(u) = crate::relay::parse(&t.relay_hint) {
                addr = addr.with_relay_url(u);
            }
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
                    // Kept again now the inviter is a member: a roster change while the request
                    // waited pruned what the ticket gave (the inviter was no member then), and
                    // with no relay and no mDNS it is the one way to reach the team.
                    {
                        let team = d.team.lock().unwrap();
                        d.peers
                            .learn(&d.dir, &t.inviter, inviter_addrs.clone(), &team.roster);
                    }
                    d.peers.changed.notify_one();
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
                    // A workspace member's doorbell goes into the team it just joined.
                    if let Err(e) = crate::embed::publish(d) {
                        eprintln!("join: wake: {e}");
                    }
                    let team = d.team.lock().unwrap().roster.team_name.clone();
                    return done("admitted", None, &team, d.peers.online().len() + 1);
                }
            }
        }
        if last.is_none() {
            if let Some(w) = wake.take() {
                let _ = d.events.send(json!({"event": "join_waking"}));
                net::doorbell(w, net::short(&t.inviter).to_string()).await;
                until = Some(tokio::time::Instant::now() + d.peers.knobs().wake_wait);
            }
        }
        let now = tokio::time::Instant::now();
        let keep = match until {
            Some(u) => now < u,
            None => last.is_none() && now < dial_until,
        };
        if !keep {
            break;
        }
        tokio::time::sleep(WAKE_RETRY).await;
    }
    // `wake`: the ticket named a workspace, whose doorbell was rung (the CLI says so).
    let mut r = done("refused", Some(last.unwrap_or(UNREACHABLE)), "", 0);
    r.body["wake"] = t.wake.is_some().into();
    r
}

/// One member's answer to `req` on `conn`.
async fn attempt(d: &Daemon, conn: &Connection, req: &Frame, m: &str, t: &Ticket) -> Outcome {
    let Ok((mut send, mut recv)) = conn.open_bi().await else {
        return Outcome::Next(None);
    };
    if frames::write(&mut send, req).await.is_err() {
        return Outcome::Next(None);
    }
    // The member gives up at its wait; we a step later. No answer within a second: it took
    // the ticket and asks its person, which `join` then says (`join_asking`).
    let wait = d.peers.knobs().join_wait + STEP;
    let f = {
        let read = frames::read(&mut recv);
        tokio::pin!(read);
        match tokio::time::timeout(ASKING, &mut read).await {
            Ok(f) => Ok(f),
            Err(_) => {
                let _ = d.events.send(json!({"event": "join_asking"}));
                tokio::time::timeout(wait.saturating_sub(ASKING), &mut read).await
            }
        }
    };
    let f = match f {
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
