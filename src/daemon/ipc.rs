//! Newline-delimited JSON over the daemon's Unix socket.

use super::Daemon;
use crate::sessions::{self, Key};
use crate::store;
use crate::{frame, mail};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::Lines;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::{error::RecvError, Receiver};
use tokio::task::JoinSet;

pub const PROTOCOL: u32 = 1;

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
        Response {
            id,
            ok: false,
            body: json!({ "error": code }),
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
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut subscribed = false;
    loop {
        let (frame, last) = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => (Some(json!(handle(&line, &daemon, &mut subscribed, &mut lines).await)), false),
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
        "status" => Response::ok(
            id,
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": PROTOCOL,
                "member_key_short": daemon.member_key_short,
                "team": { "id": daemon.team_id, "name": daemon.team_name },
                "sessions": live_count(daemon),
            }),
        ),
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
        "send" => send(id, &req.body, daemon),
        "receipt" => match (req.body["from"].as_str(), req.body["request_id"].as_str()) {
            (Some(from), Some(rid)) => {
                match mail::receipt(&daemon.store.lock().unwrap(), &Key(from.into()), rid) {
                    Ok(Some(r)) => Response::ok(id, json!({ "receipt": r })),
                    Ok(None) => Response::err(id, NOT_FOUND),
                    Err(_) => Response::err(id, INTERNAL),
                }
            }
            _ => Response::err(id, BAD_REQUEST),
        },
        "inbox" => inbox(id, &req.body, daemon, peer).await,
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
        _ => Response::err(id, UNKNOWN_OP),
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
        sessions::scan_claude(&daemon.claude_home, &daemon.member_key_short)
            .into_iter()
            .find(|s| s.key.0 == key)
            .map(|s| (s.pid, s.birth_micros)),
    )
}

fn refused(id: u64, reason: &str) -> Response {
    Response {
        id,
        ok: false,
        body: json!({ "error": BAD_REQUEST, "reason": reason }),
    }
}

/// `end {key, caller?, if_gone?}` -> `{ended}`: deletes the session row; its mail stays for
/// the hold. With `caller`, only the row's own process may end it; with `if_gone`, also
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
    match store.delete_session(key) {
        Ok(true) => Response::ok(id, json!({ "ended": true })),
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
/// upserts by key; the daemon supplies the member prefix. A key whose row names another
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
    let key = Key::new(&daemon.member_key_short, &r.provider, &r.conversation);
    let store = daemon.store.lock().unwrap();
    match row_process(&store, &key.0) {
        Ok(Some(owner)) if owner != me && sessions::alive(owner.0, owner.1) => {
            return refused(id, "owned_by_live_process")
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
    match store
        .put_session(&row)
        .map_err(Into::into)
        .and_then(|()| mail::mark_read_for(&store, &key, now()))
    {
        Ok(()) => Response::ok(id, json!({ "key": key })),
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// Registered and discovered sessions, `gone` ones left out (their rows stay until `end`).
/// A session whose last push failed reads `next_step`: its mail waits for the next step.
fn live_sessions(daemon: &Daemon) -> Vec<sessions::Session> {
    let rows = daemon.store.lock().unwrap().sessions().unwrap_or_default();
    let mut all = sessions::all(&rows, &daemon.claude_home, &daemon.member_key_short);
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

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `send {from, to?, text, request_id, reply_to?}` -> `{receipt}`; `to` may be left out only
/// with `reply_to` (the reply goes to the parent's sender). `to` follows
/// `sessions::qualify` (a bare name is this member's; anything else, groups included, is
/// refused with reason `bad_address`). Mail for a Claude or Codex session is then pushed into
/// it (`super::deliver`), if its tier is `push`.
fn send(id: u64, body: &Value, daemon: &Arc<Daemon>) -> Response {
    let (Some(from), Some(text), Some(rid)) = (
        body["from"].as_str(),
        body["text"].as_str(),
        body["request_id"].as_str(),
    ) else {
        return Response::err(id, BAD_REQUEST);
    };
    let reply_to = body["reply_to"].as_str();
    let to = match body["to"].as_str() {
        Some(t) => match sessions::qualify(&daemon.member_key_short, t) {
            Some(t) => Some(Key(t)),
            None => return refused(id, "bad_address"),
        },
        None if reply_to.is_some() => None,
        None => return Response::err(id, BAD_REQUEST),
    };
    let from = Key(from.into());
    let (sent, to) = {
        let store = daemon.store.lock().unwrap();
        let sent = mail::send(&store, &from, to.as_ref(), text, rid, reply_to, now());
        (sent, mail::recipient(&store, &from, rid).ok().flatten())
    };
    match sent {
        Ok(receipt) => {
            if receipt.state == mail::State::Stored {
                daemon.mail.notify_waiters();
                let push = |t: &String| {
                    matches!(
                        frame::Sender::of("", t),
                        frame::Sender::Agent("claude" | "codex")
                    )
                };
                if let Some(to) = to.filter(push) {
                    tokio::spawn(super::deliver(daemon.clone(), Key(to)));
                }
            }
            Response::ok(id, json!({ "receipt": receipt }))
        }
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// `inbox {key, wait_secs?}` -> `{messages}`, each with `framed`, the text as the agent
/// is shown it (`frame::framed`): takes (and so delivers) what waits for `key`;
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
                        json!({ "id": m.id, "request_id": m.request_id, "from": m.from_key,
                            "to": m.to_key, "kind": m.kind, "thread": m.thread, "hops": m.hops,
                            "text": m.body, "sent_at": m.sent_at,
                            "framed": frame::framed(frame::Sender::of(&daemon.member_key_short, &m.from_key), None, &m.request_id, &m.body) })
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
