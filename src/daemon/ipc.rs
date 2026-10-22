//! Newline-delimited JSON over the daemon's Unix socket.

use super::Daemon;
use crate::sessions::{self, Key};
use crate::store;
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
                Ok(Some(line)) => (Some(json!(handle(&line, &daemon, &mut subscribed))), false),
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

fn handle(line: &str, daemon: &Daemon, subscribed: &mut bool) -> Response {
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
            Some(key) => match daemon.store.lock().unwrap().delete_session(key) {
                Ok(true) => Response::ok(id, json!({})),
                Ok(false) => Response::err(id, NOT_FOUND),
                Err(_) => Response::err(id, INTERNAL),
            },
            None => Response::err(id, BAD_REQUEST),
        },
        "send" | "receipt" | "inbox" | "hook" | "tool" => Response::err(id, UNSUPPORTED),
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
}

/// `register {session}` upserts by key as `idle`; the daemon supplies the member prefix.
fn register(id: u64, body: &Value, daemon: &Daemon) -> Response {
    let Ok(r) = serde_json::from_value::<Register>(body["session"].clone()) else {
        return Response::err(id, BAD_REQUEST);
    };
    let key = Key::new(&daemon.member_key_short, &r.provider, &r.conversation);
    let row = store::Session {
        key: key.0.clone(),
        provider: r.provider,
        conversation: Some(r.conversation),
        pid: Some(r.pid.into()),
        birth_micros: i64::try_from(r.birth_micros).ok(),
        cwd: r.cwd,
        title: r.title,
        status: "idle".into(),
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
