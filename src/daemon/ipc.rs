//! Newline-delimited JSON over the daemon's Unix socket.

use super::Daemon;
use crate::sessions::{self, Key};
use crate::store;
use crate::{frame, mail};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
                Ok(Some(line)) => (Some(json!(handle(&line, &daemon, &mut subscribed).await)), false),
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

async fn handle(line: &str, daemon: &Daemon, subscribed: &mut bool) -> Response {
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
            Some(key) => {
                let store = daemon.store.lock().unwrap();
                match (
                    mail::discard(&store, &Key(key.into()), now()),
                    store.delete_session(key),
                ) {
                    (Ok(()), Ok(true)) => Response::ok(id, json!({})),
                    (Ok(()), Ok(false)) => Response::err(id, NOT_FOUND),
                    _ => Response::err(id, INTERNAL),
                }
            }
            None => Response::err(id, BAD_REQUEST),
        },
        "send" => send(id, &req.body, daemon),
        "receipt" => match req.body["request_id"].as_str() {
            Some(rid) => match mail::receipt(&daemon.store.lock().unwrap(), rid) {
                Ok(Some(r)) => Response::ok(id, json!({ "receipt": r })),
                Ok(None) => Response::err(id, NOT_FOUND),
                Err(_) => Response::err(id, INTERNAL),
            },
            None => Response::err(id, BAD_REQUEST),
        },
        "inbox" => inbox(id, &req.body, daemon).await,
        "tool" => Response::err(id, UNSUPPORTED),
        _ => Response::err(id, UNKNOWN_OP),
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
}

const PROVIDERS: [&str; 4] = ["claude", "codex", "cursor", "grok"];

/// `register {session: {provider, conversation, pid, birth_micros, cwd, title?, status?}}`
/// upserts by key; the daemon supplies the member prefix. `status` is `idle` (default) or
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
    if !PROVIDERS.contains(&r.provider.as_str())
        || !(1..=128).contains(&r.conversation.len())
        || !r
            .conversation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Response::err(id, BAD_REQUEST);
    }
    let key = Key::new(&daemon.member_key_short, &r.provider, &r.conversation);
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
    };
    match daemon.store.lock().unwrap().put_session(&row) {
        Ok(()) => Response::ok(id, json!({ "key": key })),
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// Registered and discovered sessions, `gone` ones left out (their rows stay until `end`).
fn live_sessions(daemon: &Daemon) -> Vec<sessions::Session> {
    let rows = daemon.store.lock().unwrap().sessions().unwrap_or_default();
    sessions::all(&rows, &daemon.claude_home, &daemon.member_key_short)
}

fn live_count(daemon: &Daemon) -> usize {
    live_sessions(daemon).len()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `send {from, to, text, request_id, reply_to?}` -> `{receipt}`.
fn send(id: u64, body: &Value, daemon: &Daemon) -> Response {
    let (Some(from), Some(to), Some(text), Some(rid)) = (
        body["from"].as_str(),
        body["to"].as_str(),
        body["text"].as_str(),
        body["request_id"].as_str(),
    ) else {
        return Response::err(id, BAD_REQUEST);
    };
    let reply_to = body["reply_to"].as_str();
    let sent = mail::send(
        &daemon.store.lock().unwrap(),
        &Key(from.into()),
        &Key(to.into()),
        text,
        rid,
        reply_to,
        now(),
    );
    match sent {
        Ok(receipt) => {
            if receipt.state == mail::State::Stored {
                daemon.mail.notify_waiters();
            }
            Response::ok(id, json!({ "receipt": receipt }))
        }
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// Who a message is from, for its frame. The frame reads only the provider, which the key
/// carries (`<member>/<provider>:<conversation>`), so the sender need not be live.
fn sender(key: &str) -> sessions::Session {
    let provider = key
        .split_once('/')
        .and_then(|(_, rest)| rest.split_once(':'))
        .map_or("", |(p, _)| p);
    sessions::Session {
        key: Key(key.into()),
        provider: provider.into(),
        conversation: String::new(),
        pid: 0,
        birth_micros: 0,
        cwd: String::new(),
        title: None,
        doing: None,
        status: sessions::Status::Gone,
        tier: sessions::Tier::Pull,
    }
}

/// `inbox {key, wait_secs?}` -> `{messages}`, each with `framed`, the text as the agent
/// is shown it (`frame::peer`): takes (and so delivers) what waits for `key`;
/// with `wait_secs` (at most 600) it first waits for something to arrive.
async fn inbox(id: u64, body: &Value, daemon: &Daemon) -> Response {
    let Some(key) = body["key"].as_str().map(|k| Key(k.into())) else {
        return Response::err(id, BAD_REQUEST);
    };
    let wait = match body.get("wait_secs") {
        None | Some(Value::Null) => 0,
        Some(v) => match v.as_u64() {
            Some(w) if w <= 600 => w,
            _ => return Response::err(id, BAD_REQUEST),
        },
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    let mut events = daemon.events.subscribe();
    loop {
        // Register interest before looking, so a send between the look and the wait is not missed.
        let notified = daemon.mail.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
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
                            "framed": frame::peer(&sender(&m.from_key), None, &m.request_id, &m.body) })
                    })
                    .collect();
                return Response::ok(id, json!({ "messages": messages }));
            }
            Ok(_) => {}
        }
        // Wake on mail, the deadline, or shutdown (so this connection can say goodbye).
        let woke = tokio::select! {
            r = tokio::time::timeout_at(deadline, notified) => r.is_ok(),
            _ = async { while !matches!(events.recv().await, Ok(e) if e["event"] == "shutdown") {} } => false,
        };
        if !woke {
            return Response::ok(id, json!({ "messages": [] }));
        }
    }
}
