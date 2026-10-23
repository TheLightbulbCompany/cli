//! Newline-delimited JSON over the daemon's Unix socket.

use super::Daemon;
use crate::sessions::{self, Key};
use crate::store;
use crate::{admission, frame, label, mail, outbox, presence};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
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
        "subscribe" => {
            *subscribed = true;
            Response::ok(id, json!({}))
        }
        "inbox" => inbox(id, &req.body, daemon, peer).await,
        op => self::op(id, op, &req.body, daemon, Caller::Socket(peer_pid)).await,
    }
}

/// Who an op comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caller {
    /// A socket connection, by its peer's pid (`None` when the OS will not say).
    Socket(Option<u32>),
    /// An embedded instance's caller: the bridge.
    Instance,
}

impl Caller {
    /// Whether a person asked: a socket peer with no agent above it. Never an instance's
    /// caller, but for the ops it may run for the workspace admin who confirmed in the app
    /// (`team.join`, `team.leave`, `snapshot`), which also take `Caller::Instance`.
    pub fn is_person(self) -> bool {
        matches!(self, Caller::Socket(Some(pid)) if super::net_ops::person_only(pid))
    }
}

/// What an instance does not do: it mints no link, its settings are its `EmbedOpts`, it asks
/// no one (`ui.claim`), and it stops by `Instance::shutdown`.
const NOT_ON_AN_INSTANCE: [&str; 4] = ["team.invite", "reload", "ui.claim", "shutdown"];

/// The fields of an `inbox` record an agent (MCP `check_inbox`) or an instance's caller is
/// handed.
pub const INBOX_RECORD: [&str; 11] = [
    "id",
    "request_id",
    "thread",
    "from",
    "label",
    "person",
    "person_verified",
    "computer",
    "text",
    "framed",
    "sent_at",
];

/// `v` (an `inbox` record from `records`) cut to `INBOX_RECORD`.
pub fn inbox_record(v: &Value) -> Value {
    INBOX_RECORD
        .iter()
        .map(|k| (k.to_string(), v[k].clone()))
        .collect()
}

/// The fields of a receipt (`finish`) an app's answer carries (`app_send`).
pub const RECEIPT_FIELDS: [&str; 5] = ["state", "reason", "limit", "thread", "tier"];

/// One op, from `caller`: every person-only op refuses without a person.
pub async fn op(id: u64, op: &str, body: &Value, daemon: &Arc<Daemon>, caller: Caller) -> Response {
    let instance = caller == Caller::Instance;
    if instance && NOT_ON_AN_INSTANCE.contains(&op) {
        return Response::err(id, UNSUPPORTED);
    }
    // Admitted for an instance's caller; for a socket, a person only (asked only when needed:
    // it walks the peer's process tree).
    let for_a_person = || instance || caller.is_person();
    match op {
        "ping" => Response::ok(id, json!({})),
        // Events come from `Instance::events`.
        "subscribe" if instance => Response::ok(id, json!({})),
        "inbox" if instance => crate::embed::inbox(id, body, daemon),
        "cloud.sessions.set" => super::net_ops::cloud_sessions_set(id, body, daemon).await,
        "app.send" | "app.inbox" | "app.receipt" => crate::app_send::op(id, op, body, daemon).await,
        "status" => {
            let t = daemon.team.lock().unwrap();
            let me = &daemon.identity.key;
            let members = &t.roster.members;
            let total = members.keys().filter(|k| *k != me).count();
            let ours = daemon.peers.knobs().protocol;
            let (online, direct, outdated) = daemon.peers.status();
            let (relay, relay_error) = daemon.peers.relay_status();
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
                    "peers": { "online": online, "direct": direct, "total": total },
                    "relay": relay,
                    // Why our relay list was refused (or could not be fetched), when it was.
                    "relay_error": relay_error,
                    "port": daemon.peers.port.get(),
                    "needs_update": needs_update,
                    "claim_live": daemon.claim_live(now() as i64),
                    "dropped": dropped(daemon),
                }),
            )
        }
        "shutdown" => {
            daemon.shutdown.notify_one();
            Response::ok(id, json!({}))
        }
        "register" => register(id, body, daemon),
        // `sessions {handle?}`: with a handle, the live session of it (`not_found` when none
        // is; `bad_request` for a string that is no handle, which is a name).
        "sessions" => match body["handle"].as_str() {
            None => Response::ok(id, json!({ "sessions": live_sessions(daemon) })),
            Some(h) if !is_handle(h) => Response::err(id, BAD_REQUEST),
            Some(h) => match live_sessions(daemon)
                .into_iter()
                .find(|s| s.handle.as_deref() == Some(h))
            {
                Some(s) => Response::ok(id, json!({ "sessions": [s] })),
                None => Response::err(id, NOT_FOUND),
            },
        },
        "end" => match body["key"].as_str() {
            Some(key) => end(id, key, body, daemon),
            None => Response::err(id, BAD_REQUEST),
        },
        // Hops restart at a person: an agent may not speak as one.
        "send" if body["from"].as_str().is_some_and(mail::is_person) && !caller.is_person() => {
            refused(id, "not_a_person")
        }
        "send" => match send(id, body, daemon) {
            Ok((from, r)) => {
                let r = finish(daemon, &from, r, body["wait"] == true).await;
                Response::ok(id, json!({ "receipt": r }))
            }
            Err(r) => r,
        },
        "receipt" => match (body["from"].as_str(), body["request_id"].as_str()) {
            (Some(from), Some(rid)) => {
                let from = Key(from.into());
                let got = mail::receipt(&daemon.store.lock().unwrap(), &from, rid);
                match got {
                    Ok(Some(r)) => {
                        let r = finish(daemon, &from, r, false).await;
                        Response::ok(id, json!({ "receipt": r }))
                    }
                    Ok(None) => Response::err(id, NOT_FOUND),
                    Err(_) => Response::err(id, INTERNAL),
                }
            }
            _ => Response::err(id, BAD_REQUEST),
        },
        // Reads every session's transcript (never its folder): off the async threads.
        "directory" => {
            let (d, q) = (daemon.clone(), body["query"].as_str().map(String::from));
            match tokio::task::spawn_blocking(move || presence::directory(&d, q.as_deref())).await {
                Ok(v) => Response::ok(id, v),
                Err(_) => Response::err(id, INTERNAL),
            }
        }
        // An agent fed a hostile link must not move this computer into another network.
        "team.join" if !for_a_person() => refused(id, "not_a_person"),
        "team.join" => crate::admission::join(id, body, daemon).await,
        // `mark_read {key}` -> `{}`: `key`'s session took a step (an MCP tool call), so the mail
        // delivered to it before now has been seen.
        "mark_read" => match body["key"].as_str() {
            Some(key) => {
                match mail::mark_read_for(&daemon.store.lock().unwrap(), &Key(key.into()), now()) {
                    Ok(()) => Response::ok(id, json!({})),
                    Err(_) => Response::err(id, INTERNAL),
                }
            }
            None => Response::err(id, BAD_REQUEST),
        },
        // `unread {key}` -> `{unread}`: how many messages wait for `key`; takes nothing.
        "unread" => match body["key"].as_str() {
            Some(key) => match mail::unread(&daemon.store.lock().unwrap(), &Key(key.into()), now())
            {
                Ok(n) => Response::ok(id, json!({ "unread": n })),
                Err(_) => Response::err(id, INTERNAL),
            },
            None => Response::err(id, BAD_REQUEST),
        },
        "team.leave" => super::net_ops::leave(id, daemon, for_a_person()).await,
        "holds" | "hold.get" | "hold.answer" | "hold.file" | "policy.get" | "policy.set" => {
            crate::holds::op(id, op, body, daemon, caller)
        }
        "share.grant" | "stream.open" | "stream.send" | "stream.close" => {
            crate::streams::handle(id, op, body, daemon, caller).await
        }
        op => super::net_ops::handle(id, op, body, daemon, caller),
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
    repo: Option<String>,
    branch: Option<String>,
}

/// `register {session: {provider, conversation, pid, birth_micros, cwd, title?, status?, executable?, repo?, branch?}, caller?}`
/// upserts by key; the daemon supplies the member prefix. A `cwd` under `folders_off` is
/// answered `{key, ignored: true}` and nothing is written. A key whose row names another
/// live process is refused (`bad_request`, reason `owned_by_live_process`). `status` is `idle` (default) or
/// `busy` (stored as live); `conversation` is `[A-Za-z0-9._-]{1,128}`. `repo` and `branch` are
/// what the hook read from `cwd`'s `.git` (it has the folder access the person gave the agent).
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
    // A folder turned off is not shown to anyone: its sessions are not registered at all, and
    // their key is remembered so a discovery of it (Claude's own file, whose cwd may name the
    // folder another way) is left out too, until a register finds it shown again.
    let hidden = presence::hidden(&daemon.team.lock().unwrap().off, &r.cwd);
    let mut store = daemon.store.lock().unwrap();
    match store.set_hidden(&key.0, hidden, now() as i64) {
        // Its teammates learn at once (a discovery of it may be on their screens).
        Ok(true) => daemon.presence.notify_one(),
        Ok(false) => {}
        Err(_) => return Response::err(id, INTERNAL),
    }
    if hidden {
        return Response::ok(id, json!({ "key": key, "ignored": true }));
    }
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
    // Shown to teammates: plain text, cut to 256 characters, whatever the caller sent. A hook
    // reads them only at a session's start and each prompt: absent, the stored ones stay.
    let clamp = |s: Option<String>| {
        s.map(|s| crate::label::plain(&s, 256))
            .filter(|s| !s.is_empty())
    };
    let reported = ["repo", "branch"]
        .iter()
        .any(|f| body["session"].get(f).is_some());
    let (repo, branch) = if reported {
        (clamp(r.repo), clamp(r.branch))
    } else {
        let old = store.session(&key.0).ok().flatten();
        old.map_or((None, None), |o| (o.repo, o.branch))
    };
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
        repo,
        branch,
        handle: None,
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
            r.repo.clone(),
            r.branch.clone(),
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

/// `status`'s `dropped`: frames `from_peer_ok` dropped, by kind, and viewer `input` frames.
fn dropped(daemon: &Daemon) -> Value {
    let mut v = json!(*daemon.peers.dropped.lock().unwrap());
    v["input"] = daemon.streams.dropped_input.load(Ordering::Relaxed).into();
    v
}

/// Whether `s` is a `who` handle: `<provider>-<n>`.
fn is_handle(s: &str) -> bool {
    s.rsplit_once('-')
        .is_some_and(|(p, n)| sessions::PROVIDERS.contains(&p) && n.parse::<u32>().is_ok())
}

fn live_count(daemon: &Daemon) -> usize {
    live_sessions(daemon).len()
}

/// `m`'s sender as this computer's agents are told it (`label::sender`).
pub(crate) fn sender_label(daemon: &Daemon, m: &store::Message) -> String {
    let from = label::from(&daemon.identity.short, m);
    let declared = presence::declared_agents(daemon).remove(&from);
    let team = daemon.team.lock().unwrap();
    let me = (daemon.identity.key.as_str(), daemon.identity.short.as_str());
    label::sender(&team.roster, me, m, declared.as_deref())
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
pub(crate) fn send(
    id: u64,
    body: &Value,
    daemon: &Arc<Daemon>,
) -> Result<(Key, mail::Receipt), Response> {
    let (Some(from), Some(text), Some(rid)) = (
        body["from"].as_str(),
        body["text"].as_str(),
        body["request_id"].as_str(),
    ) else {
        return Err(Response::err(id, BAD_REQUEST));
    };
    match body.get("kind") {
        None | Some(Value::Null) => {}
        Some(k) if k == "message" => {}
        Some(_) => return Err(Response::err(id, UNSUPPORTED)),
    }
    let reply_to = body["reply_to"].as_str();
    let to = match body["to"].as_str() {
        Some(t) => match sessions::qualify(&daemon.identity.short, t) {
            Some(t) => Some(Key(t)),
            None => return Err(refused(id, "bad_address")),
        },
        None if reply_to.is_some() => None,
        None => return Err(Response::err(id, BAD_REQUEST)),
    };
    // A workspace member sends only as the agents it declared.
    if daemon.cloud.as_ref().is_some_and(|c| !c.declares(from)) {
        return Err(refused(id, "not_declared"));
    }
    let from = Key(from.into());
    let team = daemon.team.lock().unwrap();
    let store = daemon.store.lock().unwrap();
    let internal = |_| Response::err(id, INTERNAL);
    let prepared = mail::prepare(&store, &from, to.as_ref(), text, rid, reply_to, now());
    let p = match prepared.map_err(internal)? {
        mail::Prep::Go(p) => p,
        mail::Prep::Done(r) => return Ok((from, r)),
    };
    let prefix = sessions::member_of(&p.to.0).unwrap_or("");
    if prefix != daemon.identity.short {
        let Some(member) = team.roster.by_short(prefix) else {
            return Err(Response::err(id, super::net_ops::NOT_MEMBER));
        };
        let r = outbox::enqueue(&store, p, member).map_err(internal)?;
        daemon.peers.outbox.notify_one();
        return Ok((from, r));
    }
    drop(team);
    let sent = mail::store_local(&store, &p).map_err(internal)?;
    drop(store);
    if sent.state == mail::State::Stored {
        stored(daemon, &p.to, Some(&p.message_id));
    }
    Ok((from, sent))
}

/// Mail was stored for `to` here: a waiting `inbox` looks again, a Claude or Codex session is
/// pushed it (`super::deliver`) if its tier is `push`, and for new mail `id` (not a resend)
/// an embedded instance's caller hears `message {to_session, id}` (the declared name).
pub(crate) fn stored(daemon: &Arc<Daemon>, to: &Key, id: Option<&str>) {
    daemon.mail.notify_waiters();
    if let (Some(_), Some(id)) = (&daemon.cloud, id) {
        if let Some((_, name)) = sessions::split_addr(&to.0) {
            let e = json!({"event": "message", "to_session": name, "id": id});
            let _ = daemon.events.send(e);
        }
    }
    if matches!(
        frame::Sender::of("", &to.0),
        frame::Sender::Agent("claude" | "codex")
    ) {
        tokio::spawn(super::deliver(daemon.clone(), to.clone()));
    }
}

/// How long a `send` with `wait` waits for a teammate's online computer to answer.
const SETTLE: Duration = Duration::from_secs(5);

/// Receipt `r` of `from`'s message as its reader is told it: with `wait`, first `settle`d; then,
/// for mail under another member's prefix, with `where` (that member's computer), `person` (its
/// person's name, `you` for this computer's own), `online` (whether that computer is connected
/// now), `tier` (the session's, from that member's presence) and, while queued, `until` (when
/// the outbox gives up). Any receipt of a message (not a refusal) carries its `thread`.
pub(crate) async fn finish(daemon: &Daemon, from: &Key, mut r: mail::Receipt, wait: bool) -> Value {
    let thread = daemon
        .store
        .lock()
        .unwrap()
        .thread_of(&from.0, &r.request_id)
        .ok()
        .flatten();
    let with_thread = |r: &mail::Receipt| {
        let mut v = json!(r);
        if let Some(t) = &thread {
            v["thread"] = t.as_str().into();
        }
        v
    };
    let member = r.to.as_deref().and_then(sessions::member_of).and_then(|p| {
        let team = daemon.team.lock().unwrap();
        let m = team
            .roster
            .by_short(p)
            .filter(|m| m.key != daemon.identity.key)?;
        let mine = m.person_id == team.roster.person_of(&daemon.identity.key);
        let person = if mine { "you".into() } else { label::name(m) };
        Some((m.key.clone(), m.computer.clone(), person))
    });
    let Some((member, computer, person)) = member else {
        return with_thread(&r);
    };
    if wait {
        r = settle(daemon, from, r, &member).await;
    }
    let mut v = with_thread(&r);
    v["where"] = computer.into();
    v["person"] = person.into();
    v["online"] = daemon.peers.is_online(&member).into();
    let latest = daemon.latest.lock().unwrap();
    let to = r.to.as_deref().unwrap_or_default();
    if let Some(s) = latest
        .get(&member)
        .and_then(|l| l.sessions.iter().find(|s| s.session_id == to))
    {
        v["tier"] = s.tier.clone().into();
    }
    if let (mail::State::Queued, Some(at)) = (r.state, r.at) {
        v["until"] = (at + outbox::TTL_S as i64).into();
    }
    v
}

/// Waits up to `SETTLE` for queued receipt `r` to move while `member` is online, waking when a
/// member answers (`Peers::acked`) or one disconnects: a teammate's computer that is up stores
/// (or holds) the mail within a moment.
async fn settle(daemon: &Daemon, from: &Key, mut r: mail::Receipt, member: &str) -> mail::Receipt {
    let deadline = tokio::time::Instant::now() + SETTLE;
    while r.state == mail::State::Queued && daemon.peers.is_online(member) {
        // Interest first, so an answer between the look and the wait is not missed.
        let (acked, gone) = (
            daemon.peers.acked.notified(),
            daemon.peers.disconnected.notified(),
        );
        tokio::pin!(acked, gone);
        acked.as_mut().enable();
        gone.as_mut().enable();
        let now = mail::receipt(&daemon.store.lock().unwrap(), from, &r.request_id);
        if let Ok(Some(now)) = now {
            r = now;
        }
        if r.state != mail::State::Queued {
            break;
        }
        tokio::select! {
            _ = acked => {}
            _ = gone => {}
            _ = tokio::time::sleep_until(deadline) => break,
        }
    }
    r
}

/// Inbox records for `messages`, each with `framed`, the text as the agent is shown it
/// (`frame::framed`), and who sent it: `label` (`label::sender`) and its member's `person`,
/// `person_verified` (`label::verified`, as the label says it) and `computer`; this computer's
/// own name and user, verified, before it is in a team. One team lock, one label per sender.
pub(crate) fn records(daemon: &Daemon, messages: &[store::Message]) -> Vec<Value> {
    let (key, short) = (&daemon.identity.key, &daemon.identity.short);
    // Before `team`: the presences' own lock.
    let declared = presence::declared_agents(daemon);
    let team = daemon.team.lock().unwrap();
    let mut senders: HashMap<String, Value> = HashMap::new();
    messages
        .iter()
        .map(|m| {
            let from = label::from(short, m);
            let who = senders.entry(from.clone()).or_insert_with(|| {
                let agent = declared.get(&from).map(String::as_str);
                let label = label::render_as(&team.roster, key, &from, agent);
                match team
                    .roster
                    .by_short(sessions::member_of(&from).unwrap_or(""))
                {
                    Some(s) => json!({ "label": label, "person": label::name(s),
                        "person_verified": label::verified(s, key), "computer": s.computer }),
                    None => json!({ "label": label, "person": super::user_name(),
                        "person_verified": true, "computer": team.config.computer }),
                }
            });
            let label = who["label"].as_str().unwrap_or_default();
            json!({ "id": m.id, "request_id": m.request_id, "from": m.from_key, "to": m.to_key,
                "kind": m.kind, "thread": m.thread, "hops": m.hops, "text": m.body,
                "sent_at": m.sent_at, "label": who["label"], "person": who["person"],
                "person_verified": who["person_verified"], "computer": who["computer"],
                "framed": frame::framed(label, &m.request_id, &m.body) })
        })
        .collect()
}

/// `inbox {key, wait_secs?}` -> `{messages}` (`records`): takes (and so delivers) what waits for `key`;
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
                repo: None,
                branch: None,
                handle: None,
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
                let messages = records(daemon, &m);
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
