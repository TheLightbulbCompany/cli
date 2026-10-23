//! Newline-delimited JSON over the daemon's Unix socket.

use super::Daemon;
use crate::sessions::{self, Key};
use crate::store;
use crate::{admission, frame, label, mail, outbox, presence};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::Lines;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::{error::RecvError, Receiver};
use tokio::task::JoinSet;

// The closed list of `error` codes.
pub const UNKNOWN_OP: &str = "unknown_op";
pub const BAD_REQUEST: &str = "bad_request";
pub const NOT_FOUND: &str = "not_found";
pub const UNSUPPORTED: &str = "unsupported";
pub const INTERNAL: &str = "internal";

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub op: String,
    #[serde(flatten)]
    pub body: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub ok: bool,
    #[serde(flatten)]
    pub body: Value,
}

impl Response {
    pub fn ok(id: u64, body: Value) -> Response {
        Response { id, ok: true, body }
    }

    pub fn err(id: u64, code: &str) -> Response {
        Response::err_with(id, code, json!({}))
    }

    /// An `error` response carrying `extra`'s fields beside the code.
    pub fn err_with(id: u64, code: &str, mut extra: Value) -> Response {
        extra["error"] = code.into();
        Response {
            id,
            ok: false,
            body: extra,
        }
    }
}

/// Accepts connections until a `shutdown` op, then tells every connection
/// (subscribers get `{"event":"shutdown"}`) and waits briefly for them to flush.
pub async fn serve(listener: UnixListener, daemon: Arc<Daemon>) {
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    // Subscribe here, not in the task, so a connection accepted
                    // just before shutdown cannot miss the shutdown event.
                    conns.spawn(connection(stream, daemon.events.subscribe(), daemon.clone()));
                }
                Err(e) => eprintln!("accept: {e}"),
            },
            _ = daemon.shutdown.notified() => break,
        }
    }
    drop(listener);
    let _ = daemon.events.send(json!({ "event": "shutdown" }));
    let _ = tokio::time::timeout(Duration::from_secs(2), conns.join_all()).await;
}

async fn connection(stream: UnixStream, mut events: Receiver<Value>, daemon: Arc<Daemon>) {
    // `None` when the OS will not say: person-only ops then refuse.
    let peer_pid = crate::os::peer_pid(stream.as_raw_fd()).ok();
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut subscribed = false;
    loop {
        let (frame, last) = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => (Some(json!(handle(&line, &daemon, &mut subscribed, &mut lines, peer_pid).await)), false),
                _ => return,
            },
            event = events.recv() => match event {
                Ok(event) => {
                    let last = event["event"] == "shutdown";
                    (subscribed.then_some(event), last)
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            },
        };
        if let Some(frame) = frame {
            if write
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
        }
        if last {
            return;
        }
    }
}

async fn handle(
    line: &str,
    daemon: &Arc<Daemon>,
    subscribed: &mut bool,
    peer: &mut Lines<BufReader<OwnedReadHalf>>,
    peer_pid: Option<u32>,
) -> Response {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Response::err(0, BAD_REQUEST);
    };
    let id = value["id"].as_u64().unwrap_or(0);
    let Ok(req) = serde_json::from_value::<Request>(value) else {
        return Response::err(id, BAD_REQUEST);
    };
    match req.op.as_str() {
        "ping" => Response::ok(id, json!({})),
        "status" => {
            let t = daemon.team.lock().unwrap();
            let me = &daemon.identity.key;
            let members = &t.roster.members;
            let total = members.keys().filter(|k| *k != me).count();
            let ours = daemon.peers.knobs().protocol;
            let (online, outdated) = daemon.peers.status();
            // `<computer> needs an update (protocol <theirs>, yours <ours>)`
            let needs_update: Vec<String> = outdated
                .into_iter()
                .filter_map(|(k, theirs)| {
                    let computer = &members.get(&k)?.computer;
                    Some(format!(
                        "{computer} needs an update (protocol {theirs}, yours {ours})"
                    ))
                })
                .collect();
            Response::ok(
                id,
                json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol": ours,
                    "member_key_short": daemon.identity.short,
                    "team": { "id": t.roster.team_id, "name": t.roster.team_name },
                    "sessions": live_count(daemon),
                    "peers": { "online": online, "total": total },
                    "relay": "off",
                    "port": daemon.peers.port.get(),
                    "needs_update": needs_update,
                    "claim_live": daemon.claim_live(now() as i64),
                    "dropped": {
                        "grants": daemon.streams.dropped_grants.load(Ordering::Relaxed),
                        "input": daemon.streams.dropped_input.load(Ordering::Relaxed),
                    },
                }),
            )
        }
        "subscribe" => {
            *subscribed = true;
            Response::ok(id, json!({}))
        }
        "shutdown" => {
            daemon.shutdown.notify_one();
            Response::ok(id, json!({}))
        }
        "register" => register(id, &req.body, daemon),
        "sessions" => Response::ok(id, json!({ "sessions": live_sessions(daemon) })),
        "end" => match req.body["key"].as_str() {
            Some(key) => end(id, key, &req.body, daemon),
            None => Response::err(id, BAD_REQUEST),
        },
        // Hops restart at a person: an agent may not speak as one.
        "send"
            if req.body["from"].as_str().is_some_and(mail::is_person)
                && !peer_pid.is_some_and(super::net_ops::person_only) =>
        {
            refused(id, "not_a_person")
        }
        "send" => {
            let mut r = send(id, &req.body, daemon);
            if let (true, Some(from)) = (r.ok, req.body["from"].as_str()) {
                let from = Key(from.into());
                if req.body["wait"] == true {
                    settle(daemon, &from, &mut r.body["receipt"]).await;
                }
                annotate(daemon, &from, &mut r.body["receipt"]);
            }
            r
        }
        "receipt" => match (req.body["from"].as_str(), req.body["request_id"].as_str()) {
            (Some(from), Some(rid)) => {
                let from = Key(from.into());
                let got = mail::receipt(&daemon.store.lock().unwrap(), &from, rid);
                match got {
                    Ok(Some(r)) => {
                        let mut r = json!(r);
                        annotate(daemon, &from, &mut r);
                        Response::ok(id, json!({ "receipt": r }))
                    }
                    Ok(None) => Response::err(id, NOT_FOUND),
                    Err(_) => Response::err(id, INTERNAL),
                }
            }
            _ => Response::err(id, BAD_REQUEST),
        },
        "inbox" => inbox(id, &req.body, daemon, peer).await,
        // Reads every session's transcript and git files: off the async threads.
        "directory" => {
            let (d, q) = (daemon.clone(), req.body["query"].as_str().map(String::from));
            match tokio::task::spawn_blocking(move || presence::directory(&d, q.as_deref())).await {
                Ok(v) => Response::ok(id, v),
                Err(_) => Response::err(id, INTERNAL),
            }
        }
        // An agent fed a hostile link must not move this computer into another network.
        "team.join" if !peer_pid.is_some_and(super::net_ops::person_only) => {
            refused(id, "not_a_person")
        }
        "team.join" => crate::admission::join(id, &req.body, daemon).await,
        // `mark_read {key}` -> `{}`: `key`'s session took a step (an MCP tool call), so the mail
        // delivered to it before now has been seen.
        "mark_read" => match req.body["key"].as_str() {
            Some(key) => {
                match mail::mark_read_for(&daemon.store.lock().unwrap(), &Key(key.into()), now()) {
                    Ok(()) => Response::ok(id, json!({})),
                    Err(_) => Response::err(id, INTERNAL),
                }
            }
            None => Response::err(id, BAD_REQUEST),
        },
        // `unread {key}` -> `{unread}`: how many messages wait for `key`; takes nothing.
        "unread" => match req.body["key"].as_str() {
            Some(key) => match mail::unread(&daemon.store.lock().unwrap(), &Key(key.into()), now())
            {
                Ok(n) => Response::ok(id, json!({ "unread": n })),
                Err(_) => Response::err(id, INTERNAL),
            },
            None => Response::err(id, BAD_REQUEST),
        },
        "team.leave" => super::net_ops::leave(id, daemon, peer_pid).await,
        "holds" | "hold.get" | "hold.answer" | "hold.file" | "policy.get" | "policy.set" => {
            crate::holds::op(id, &req.op, &req.body, daemon, peer_pid)
        }
        "share.grant" | "stream.open" | "stream.send" | "stream.close" => {
            crate::streams::handle(id, &req.op, &req.body, daemon, peer_pid).await
        }
        op => super::net_ops::handle(id, op, &req.body, daemon, peer_pid),
    }
}

/// A process as `(pid, birth_micros)`.
type Proc = (u32, u64);

/// The optional `caller: {pid, birth_micros}` a hook sends: the agent process it acts for.
/// `Err` when present but malformed.
fn caller(body: &Value) -> Result<Option<Proc>, ()> {
    match body.get("caller") {
        None | Some(Value::Null) => Ok(None),
        Some(c) => match (
            c["pid"].as_u64().and_then(|p| u32::try_from(p).ok()),
            c["birth_micros"].as_u64(),
        ) {
            (Some(p), Some(b)) => Ok(Some((p, b))),
            _ => Err(()),
        },
    }
}

/// The process a stored row names; `Ok(None)` for no row (or one without a process).
fn row_process(store: &store::Store, key: &str) -> rusqlite::Result<Option<Proc>> {
    Ok(store.session(key)?.and_then(|s| {
        Some((
            u32::try_from(s.pid?).ok()?,
            u64::try_from(s.birth_micros?).ok()?,
        ))
    }))
}

/// The process a key belongs to: its stored row's, else (never registered) the Claude
/// session discovered under that key.
fn owner_process(daemon: &Daemon, key: &str) -> rusqlite::Result<Option<Proc>> {
    if let Some(p) = row_process(&daemon.store.lock().unwrap(), key)? {
        return Ok(Some(p));
    }
    Ok(
        sessions::scan_claude(&daemon.claude_home, &daemon.identity.short)
            .into_iter()
            .find(|s| s.key.0 == key)
            .map(|s| (s.pid, s.birth_micros)),
    )
}

pub(crate) fn refused(id: u64, reason: &str) -> Response {
    Response::err_with(id, BAD_REQUEST, json!({ "reason": reason }))
}

/// `end {key, caller?, if_gone?}` -> `{ended}`: ends the session row (kept a day, ended);
/// its mail stays for the hold. With `caller`, only the row's own process may end it; with `if_gone`, also
/// anyone once the row's process is dead. `ended: false` when refused. Neither field (the
/// CLI) ends it outright.
fn end(id: u64, key: &str, body: &Value, daemon: &Daemon) -> Response {
    let Ok(caller) = caller(body) else {
        return Response::err(id, BAD_REQUEST);
    };
    let if_gone = body["if_gone"] == true;
    let store = daemon.store.lock().unwrap();
    if caller.is_some() || if_gone {
        let row = match row_process(&store, key) {
            Ok(r) => r,
            Err(_) => return Response::err(id, INTERNAL),
        };
        let owned = caller.is_some() && row == caller;
        let dead = !row.is_some_and(|(p, b)| sessions::alive(p, b));
        if !(owned || (if_gone && dead)) {
            return Response::ok(id, json!({ "ended": false }));
        }
    }
    // The row stays a day, ended: its address is kept (`presence`).
    match store.end_session(key, now() as i64) {
        Ok(true) => {
            daemon.presence.notify_one();
            Response::ok(id, json!({ "ended": true }))
        }
        Ok(false) => Response::err(id, NOT_FOUND),
        Err(_) => Response::err(id, INTERNAL),
    }
}

#[derive(Deserialize)]
struct Register {
    provider: String,
    conversation: String,
    pid: u32,
    birth_micros: u64,
    cwd: String,
    title: Option<String>,
    status: Option<String>,
    executable: Option<String>,
}

/// `register {session: {provider, conversation, pid, birth_micros, cwd, title?, status?, executable?}, caller?}`
/// upserts by key; the daemon supplies the member prefix. A `cwd` under `folders_off` is
/// answered `{key, ignored: true}` and nothing is written. A key whose row names another
/// live process is refused (`bad_request`, reason `owned_by_live_process`). `status` is `idle` (default) or
/// `busy` (stored as live); `conversation` is `[A-Za-z0-9._-]{1,128}`.
fn register(id: u64, body: &Value, daemon: &Daemon) -> Response {
    let Ok(r) = serde_json::from_value::<Register>(body["session"].clone()) else {
        return Response::err(id, BAD_REQUEST);
    };
    let status = match r.status.as_deref() {
        None | Some("idle") => "idle",
        Some("busy") => "live",
        Some(_) => return Response::err(id, BAD_REQUEST),
    };
    if !sessions::PROVIDERS.contains(&r.provider.as_str()) || !sessions::word(&r.conversation, 128)
    {
        return Response::err(id, BAD_REQUEST);
    }
    let Ok(caller) = caller(body) else {
        return Response::err(id, BAD_REQUEST);
    };
    let me = caller.unwrap_or((r.pid, r.birth_micros));
    let key = Key::new(&daemon.identity.short, &r.provider, &r.conversation);
    // A folder turned off is not shown to anyone: its sessions are not registered at all.
    let off: Vec<_> = {
        let folders = daemon.team.lock().unwrap().config.folders_off.clone();
        folders.iter().map(|o| presence::canonical(o)).collect()
    };
    if presence::hidden(&off, &r.cwd) {
        return Response::ok(id, json!({ "key": key, "ignored": true }));
    }
    let mut store = daemon.store.lock().unwrap();
    match row_process(&store, &key.0) {
        Ok(Some(owner)) if owner != me && sessions::alive(owner.0, owner.1) => {
            return refused(id, "owned_by_live_process")
        }
        // A new session may not take what the person's other computer keeps.
        Ok(None) => {
            drop(store);
            if !presence::claim(daemon, &key.0) {
                return Response::err(id, admission::NAME_TAKEN);
            }
            store = daemon.store.lock().unwrap();
        }
        Ok(_) => {}
        Err(_) => return Response::err(id, INTERNAL),
    }
    let row = store::Session {
        key: key.0.clone(),
        provider: r.provider,
        conversation: Some(r.conversation),
        pid: Some(r.pid.into()),
        birth_micros: i64::try_from(r.birth_micros).ok(),
        cwd: r.cwd,
        title: r.title,
        status: status.into(),
        seen_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
        executable: r.executable,
    };
    // Every hook event registers: mail delivered before it has been seen.
    match put_row(daemon, &store, &row).and_then(|()| mail::mark_read_for(&store, &key, now())) {
        Ok(()) => Response::ok(id, json!({ "key": key })),
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// Writes `row`, and wakes the presence sender when that changes what it publishes: a new
/// (or revived) row, or another folder, process, executable, status or title.
fn put_row(daemon: &Daemon, store: &store::Store, row: &store::Session) -> crate::Result<()> {
    let old = store.session(&row.key)?;
    store.put_session(row)?;
    let fields = |r: &store::Session| {
        (
            r.cwd.clone(),
            r.pid,
            r.executable.clone(),
            r.status.clone(),
            r.title.clone(),
        )
    };
    let same = |o: &store::Session| fields(o) == fields(row);
    if !old.as_ref().is_some_and(same) {
        daemon.presence.notify_one();
    }
    Ok(())
}

/// Registered and discovered sessions, `gone` ones left out (their rows stay until `end`).
/// A session whose last push failed reads `next_step`: its mail waits for the next step.
pub(crate) fn live_sessions(daemon: &Daemon) -> Vec<sessions::Session> {
    let rows = daemon.store.lock().unwrap().sessions().unwrap_or_default();
    let mut all = sessions::all(&rows, &daemon.claude_home, &daemon.identity.short);
    let failed = daemon.push_failed.lock().unwrap();
    for s in &mut all {
        if s.tier == sessions::Tier::Push && failed.contains(&s.key.0) {
            s.tier = sessions::Tier::NextStep;
        }
    }
    all
}

fn live_count(daemon: &Daemon) -> usize {
    live_sessions(daemon).len()
}

/// `m`'s sender as this computer's agents are told it (`label::sender`).
pub(crate) fn sender_label(daemon: &Daemon, m: &store::Message) -> String {
    let team = daemon.team.lock().unwrap();
    let me = (daemon.identity.key.as_str(), daemon.identity.short.as_str());
    label::sender(&team.roster, me, m)
}

/// `m`'s sender for an inbox record: `label`, and its member's `person`, `person_verified` and
/// `computer` (this computer's own name and user before it is in a team).
fn sender_fields(daemon: &Daemon, m: &store::Message) -> Value {
    let label = sender_label(daemon, m);
    let from = label::from(&daemon.identity.short, m);
    let team = daemon.team.lock().unwrap();
    let mut v = json!({ "label": label, "person": super::user_name(),
        "person_verified": false, "computer": team.config.computer });
    if let Some(s) = team
        .roster
        .by_short(sessions::member_of(&from).unwrap_or(""))
    {
        v["person"] = label::name(s).into();
        v["person_verified"] = s.proof.is_some().into();
        v["computer"] = s.computer.clone().into();
    }
    v
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `send {from, to?, text, request_id, reply_to?, kind?}` -> `{receipt}`; `to` may be left out
/// only with `reply_to` (the reply goes to the parent's sender). `to` follows
/// `sessions::qualify` (a bare name is this member's; anything else, groups included, is
/// refused with reason `bad_address`). `kind` is `message` (the default) or `unsupported`.
/// Mail for a session here is stored, then pushed into a Claude or Codex session
/// (`super::deliver`) if its tier is `push`; mail under another member's prefix goes to the
/// outbox (`queued`, `where` that member's computer), `not_member` for a prefix no member has.
fn send(id: u64, body: &Value, daemon: &Arc<Daemon>) -> Response {
    let (Some(from), Some(text), Some(rid)) = (
        body["from"].as_str(),
        body["text"].as_str(),
        body["request_id"].as_str(),
    ) else {
        return Response::err(id, BAD_REQUEST);
    };
    match body.get("kind") {
        None | Some(Value::Null) => {}
        Some(k) if k == "message" => {}
        Some(_) => return Response::err(id, UNSUPPORTED),
    }
    let reply_to = body["reply_to"].as_str();
    let to = match body["to"].as_str() {
        Some(t) => match sessions::qualify(&daemon.identity.short, t) {
            Some(t) => Some(Key(t)),
            None => return refused(id, "bad_address"),
        },
        None if reply_to.is_some() => None,
        None => return Response::err(id, BAD_REQUEST),
    };
    let from = Key(from.into());
    let team = daemon.team.lock().unwrap();
    let store = daemon.store.lock().unwrap();
    let prepared = match mail::prepare(&store, &from, to.as_ref(), text, rid, reply_to, now()) {
        Ok(p) => p,
        Err(_) => return Response::err(id, INTERNAL),
    };
    let p = match prepared {
        mail::Prep::Go(p) => p,
        mail::Prep::Done(r) => return Response::ok(id, json!({ "receipt": r })),
    };
    let prefix = sessions::member_of(&p.to.0).unwrap_or("");
    if prefix != daemon.identity.short {
        let Some(member) = team.roster.by_short(prefix) else {
            return Response::err(id, super::net_ops::NOT_MEMBER);
        };
        return match outbox::enqueue(&store, p, member) {
            Ok(r) => {
                daemon.peers.outbox.notify_one();
                Response::ok(id, json!({ "receipt": r }))
            }
            Err(_) => Response::err(id, INTERNAL),
        };
    }
    drop(team);
    let sent = mail::store_local(&store, &p);
    drop(store);
    let to = p.to;
    match sent {
        Ok(receipt) => {
            if receipt.state == mail::State::Stored {
                stored(daemon, &to);
            }
            Response::ok(id, json!({ "receipt": receipt }))
        }
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// Mail was stored for `to` here: a waiting `inbox` looks again, and a Claude or Codex
/// session is pushed it (`super::deliver`) if its tier is `push`.
pub(crate) fn stored(daemon: &Arc<Daemon>, to: &Key) {
    daemon.mail.notify_waiters();
    if matches!(
        frame::Sender::of("", &to.0),
        frame::Sender::Agent("claude" | "codex")
    ) {
        tokio::spawn(super::deliver(daemon.clone(), to.clone()));
    }
}

/// How long a `send` with `wait` waits for a teammate's online computer to answer.
const SETTLE: Duration = Duration::from_secs(5);

/// The member key mail `from`'s `request_id` went to, when that is another member.
fn member_for(daemon: &Daemon, from: &Key, rid: &str) -> Option<(String, String)> {
    let to = mail::recipient(&daemon.store.lock().unwrap(), from, rid).ok()??;
    let prefix = sessions::member_of(&to)?;
    if prefix == daemon.identity.short {
        return None;
    }
    let team = daemon.team.lock().unwrap();
    Some((team.roster.by_short(prefix)?.key.clone(), to))
}

/// Waits up to `SETTLE` for queued receipt `r` to move while its member is online: a teammate's
/// computer that is up stores (or holds) the mail within a moment.
async fn settle(daemon: &Daemon, from: &Key, r: &mut Value) {
    let Some(rid) = r["request_id"].as_str().map(String::from) else {
        return;
    };
    let Some((member, _)) = member_for(daemon, from, &rid) else {
        return;
    };
    let deadline = tokio::time::Instant::now() + SETTLE;
    while r["state"] == "queued"
        && tokio::time::Instant::now() < deadline
        && daemon.peers.online().contains(&member)
    {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = mail::receipt(&daemon.store.lock().unwrap(), from, &rid);
        if let Ok(Some(now)) = now {
            r["state"] = json!(now.state);
            r["reason"] = json!(now.reason);
        }
    }
}

/// Adds to receipt `r` (of `from`'s `request_id`) what its reader is told. For mail under another
/// member's prefix: `where` (that member's computer), `person` (its person's name, `you` for
/// this computer's own), `tier` (the session's, from that member's presence) and, while
/// queued, `until` (when the outbox gives up). For a `loop` refusal: `loop`, `hops` or `rate`
/// (`mail::loop_kind`).
fn annotate(daemon: &Daemon, from: &Key, r: &mut Value) {
    let Some(rid) = r["request_id"].as_str().map(String::from) else {
        return;
    };
    let (kind, at) = {
        let store = daemon.store.lock().unwrap();
        let looped = r["state"] == "refused" && r["reason"] == "loop";
        let kind = looped.then(|| mail::loop_kind(&store, from, &rid).ok());
        (
            kind.flatten(),
            mail::receipt_at(&store, from, &rid).ok().flatten(),
        )
    };
    if let Some(k) = kind {
        r["loop"] = k.into();
    }
    let Some((member, to)) = member_for(daemon, from, &rid) else {
        return;
    };
    {
        let team = daemon.team.lock().unwrap();
        let Some(m) = team.roster.members.get(&member) else {
            return;
        };
        let mine = m.person_id == team.roster.person_of(&daemon.identity.key);
        r["where"] = m.computer.clone().into();
        r["person"] = if mine { "you".into() } else { label::name(m) }.into();
    }
    let latest = daemon.latest.lock().unwrap();
    if let Some(s) = latest
        .get(&member)
        .and_then(|l| l.sessions.iter().find(|s| s.session_id == to))
    {
        r["tier"] = s.tier.clone().into();
    }
    if let (true, Some(at)) = (r["state"] == "queued", at) {
        r["until"] = (at + outbox::TTL_S as i64).into();
    }
}

/// `inbox {key, wait_secs?}` -> `{messages}`, each with `framed`, the text as the agent
/// is shown it (`frame::framed`), and who sent it (`sender_fields`): takes (and so delivers) what waits for `key`;
/// with `wait_secs` (at most 600) it first waits for something to arrive. With `caller`, only
/// the row's own process may take it (`bad_request`, reason `not_owner`, otherwise).
async fn inbox(
    id: u64,
    body: &Value,
    daemon: &Daemon,
    peer: &mut Lines<BufReader<OwnedReadHalf>>,
) -> Response {
    let (Some(key), Ok(caller)) = (body["key"].as_str().map(|k| Key(k.into())), caller(body))
    else {
        return Response::err(id, BAD_REQUEST);
    };
    if let Some(c) = caller {
        match owner_process(daemon, &key.0) {
            Ok(Some(owner)) if owner == c => {}
            Ok(_) => return refused(id, "not_owner"),
            Err(_) => return Response::err(id, INTERNAL),
        }
    }
    let wait = match body.get("wait_secs") {
        None | Some(Value::Null) => 0,
        Some(v) => match v.as_u64() {
            Some(w) if w <= 600 => w,
            _ => return Response::err(id, BAD_REQUEST),
        },
    };
    // A name (`listen --as`) holds a row (no process, no folder) while it is in use here, so
    // it is kept a day after; one the person's other computer keeps is refused.
    let short = &daemon.identity.short;
    if let frame::Sender::Name(_) = frame::Sender::of(short, &key.0) {
        if sessions::member_of(&key.0) == Some(short.as_str()) {
            if !presence::claim(daemon, &key.0) {
                return Response::err(id, admission::NAME_TAKEN);
            }
            let row = store::Session {
                key: key.0.clone(),
                provider: String::new(),
                conversation: None,
                pid: None,
                birth_micros: None,
                cwd: String::new(),
                title: None,
                status: "idle".into(),
                seen_at: now() as i64,
                executable: None,
            };
            if put_row(daemon, &daemon.store.lock().unwrap(), &row).is_err() {
                return Response::err(id, INTERNAL);
            }
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    let mut events = daemon.events.subscribe();
    // Watch the peer for EOF while waiting, so mail that arrives after it hung up stays in
    // the mailbox. Bytes it sent meanwhile stay buffered for the next request.
    let mut watch_peer = true;
    loop {
        // Register interest before looking, so a send between the look and the wait is not missed.
        let notified = daemon.mail.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        // Under the key's lock, so a push in flight is not delivered twice; a push holding
        // it past the deadline means nothing for this call.
        let lock = daemon.key_lock(&key.0);
        let Ok(held) = tokio::time::timeout_at(deadline, lock.lock()).await else {
            return Response::ok(id, json!({ "messages": [] }));
        };
        // Look at the peer once more, without waiting: one that hung up as the mail arrived
        // (both ready in the same wakeup) must not have mail taken for it.
        if watch_peer {
            tokio::select! {
                biased;
                buf = peer.get_mut().fill_buf() => match buf {
                    Ok(b) if !b.is_empty() => watch_peer = false,
                    _ => return Response::ok(id, json!({ "messages": [] })),
                },
                () = std::future::ready(()) => {}
            }
        }
        let taken = mail::take(&daemon.store.lock().unwrap(), &key, now());
        match taken {
            Err(_) => return Response::err(id, INTERNAL),
            Ok(m) if !m.is_empty() => {
                let messages: Vec<Value> = m
                    .iter()
                    .map(|m| {
                        let mut v = sender_fields(daemon, m);
                        let label = v["label"].as_str().unwrap_or_default();
                        let framed = frame::framed(label, &m.request_id, &m.body);
                        for (k, x) in [
                            ("id", json!(m.id)),
                            ("request_id", json!(m.request_id)),
                            ("from", json!(m.from_key)),
                            ("to", json!(m.to_key)),
                            ("kind", json!(m.kind)),
                            ("thread", json!(m.thread)),
                            ("hops", json!(m.hops)),
                            ("text", json!(m.body)),
                            ("sent_at", json!(m.sent_at)),
                            ("framed", json!(framed)),
                        ] {
                            v[k] = x;
                        }
                        v
                    })
                    .collect();
                return Response::ok(id, json!({ "messages": messages }));
            }
            Ok(_) => {}
        }
        drop(held);
        // Wake on mail, the deadline, shutdown (so this connection can say goodbye), or the
        // peer hanging up (then take nothing: there is no one to hand it to).
        let woke = tokio::select! {
            r = tokio::time::timeout_at(deadline, &mut notified) => r.is_ok(),
            _ = async { while !matches!(events.recv().await, Ok(e) if e["event"] == "shutdown") {} } => false,
            buf = peer.get_mut().fill_buf(), if watch_peer => match buf {
                Ok(b) if !b.is_empty() => {
                    watch_peer = false;
                    continue;
                }
                _ => false,
            },
        };
        if !woke {
            return Response::ok(id, json!({ "messages": [] }));
        }
    }
}
