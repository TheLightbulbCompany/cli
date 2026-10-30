//! Newline-delimited JSON over the daemon's Unix socket.

use super::Daemon;
use crate::agents::listing::{self, Listed};
use crate::sessions::{self, Key};
use crate::store;
use crate::{doing, frame, label, mail, outbox, presence};
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

    /// Who is asking, for the ops an agent may also run: `local` for a person, else the agent
    /// session above the socket peer (`<member>/<handle>`, `agent` before it registers). `None`
    /// for a caller that cannot be resolved (no pid, a failed lookup) and for an instance's.
    pub fn actor(self, store: &crate::store::Store) -> Option<String> {
        let Caller::Socket(Some(pid)) = self else {
            return None;
        };
        if super::net_ops::person_only(pid) {
            return Some("local".into());
        }
        super::net_ops::agent_session(store, pid)
    }
}

/// What an instance does not do: it mints no link but a person one (`net_ops`), its settings
/// are its `EmbedOpts`, it asks no one (`ui.claim`), and it stops by `Instance::shutdown`.
const NOT_ON_AN_INSTANCE: [&str; 3] = ["reload", "ui.claim", "shutdown"];

/// What only an instance does: the workspace's memory ops on its cloud member. `memory.list`
/// is one of them when it carries `since_seq` (without, it is the person's `memory list`).
const ONLY_ON_AN_INSTANCE: [&str; 2] = ["memory.import", "memory.forget_author"];

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

/// (tests) Milliseconds the daemon holds each `capture.*` op before doing it; debug builds only.
const CAPTURE_DELAY_ENV: &str = "LIGHTBULB_CONNECT_DEBUG_CAPTURE_DELAY_MS";

/// One op, from `caller`: every person-only op refuses without a person.
pub async fn op(id: u64, op: &str, body: &Value, daemon: &Arc<Daemon>, caller: Caller) -> Response {
    let instance = caller == Caller::Instance;
    if instance && NOT_ON_AN_INSTANCE.contains(&op) {
        return Response::err(id, UNSUPPORTED);
    }
    let cloud_list = op == "memory.list" && body.get("since_seq").is_some();
    if !instance && (ONLY_ON_AN_INSTANCE.contains(&op) || cloud_list) {
        return Response::err(id, UNSUPPORTED);
    }
    // Admitted for an instance's caller; for a socket, a person only (asked only when needed:
    // it walks the peer's process tree).
    let for_a_person = || instance || caller.is_person();
    match op {
        "ping" => Response::ok(id, json!({})),
        // `login.changed` -> `{}`: `login`/`logout` rewrote this computer's sign-in; every
        // connected member hears a fresh `hello` with the proof now on disk (or none).
        "login.changed" => {
            daemon.peers.changed.notify_one();
            Response::ok(id, json!({}))
        }
        // Events come from `Instance::events`.
        "subscribe" if instance => Response::ok(id, json!({})),
        "inbox" if instance => crate::embed::inbox(id, body, daemon),
        "inbox.peek" if instance => crate::embed::peek(id, daemon),
        "inbox.ack" if instance => crate::embed::ack(id, body, daemon),
        "inbox.peek" | "inbox.ack" => peek_or_ack(id, op, body, daemon).await,
        "cloud.sessions.set" => super::net_ops::cloud_sessions_set(id, body, daemon).await,
        "app.send" | "app.inbox" | "app.ack" | "app.unread" | "app.receipt" if instance => {
            crate::app_send::op(id, op, body, daemon).await
        }
        "status" => {
            // Before the team lock: `live_sessions` takes it.
            let sessions = live_count(daemon);
            let t = daemon.team.lock().unwrap();
            let me = &daemon.identity.key;
            let members = &t.roster.members;
            let total = members.keys().filter(|k| *k != me).count();
            let ours = daemon.peers.knobs().protocol;
            let (online, direct, outdated) = daemon.peers.status();
            let relay = daemon.peers.relay_status();
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
                    "sessions": sessions,
                    "peers": { "online": online, "direct": direct, "total": total },
                    "relay": relay,
                    "port": daemon.peers.port.get(),
                    // Local discovery: "on", "off" (`config.lan`) or "blocked" (macOS).
                    "lan": daemon.peers.lan_status(),
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
            None => {
                let live = live_sessions(daemon);
                // A row that ended yet is seen live again (no hook said so) is shown live.
                let ended: Vec<Value> = ended_sessions(daemon)
                    .iter()
                    .filter(|r| live.iter().all(|s| s.key.0 != r.key))
                    .map(ended_json)
                    .collect();
                Response::ok(id, json!({ "sessions": live, "ended": ended }))
            }
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
        "team.join" if !instance && caller.actor(&daemon.store.lock().unwrap()).is_none() => {
            refused(id, "not_a_person")
        }
        "team.join" => crate::admission::join(id, body, daemon).await,
        // `mark_read {key}` -> `{}`: `key`'s session took a step (an MCP tool call), so the mail
        // delivered to it before now has been seen.
        "mark_read" => match body["key"].as_str() {
            Some(key) => {
                match mail::mark_read_for(&daemon.store.lock().unwrap(), &Key(key.into()), now()) {
                    Ok(()) => {
                        daemon.peers.outbox.notify_one();
                        Response::ok(id, json!({}))
                    }
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
        "sessions.upload" => sessions_upload(id, body, daemon, for_a_person()),
        "sessions.turns" => sessions_turns(id, body, daemon, for_a_person()),
        "capture.session_start"
        | "capture.tool_call"
        | "capture.turn"
        | "capture.stop"
        | "capture.session_end"
        | "sessions.list"
        | "sessions.show"
        | "sessions.forget"
        | "sessions.export" => {
            // Debug builds only: a daemon slow to answer capture writes (tests).
            let slow =
                crate::cli::test_env(CAPTURE_DELAY_ENV).and_then(|v| v.to_str()?.parse().ok());
            if let Some(ms) = slow.filter(|_| op.starts_with("capture.")) {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            // Asked only for `sessions.*`: the walk of the peer's process tree is slower than
            // the hook waits for a capture write.
            let person = op.starts_with("sessions.") && for_a_person();
            let r = capture_op(id, op, body, daemon, person);
            // A Stop kicks it only when it carried a reply to embed.
            if op == "capture.turn" || (op == "capture.stop" && body["text"].is_string()) {
                embed_missing(daemon);
            }
            r
        }
        // `person` -> `{}` for a person's terminal, else refused: a command that settles
        // something elsewhere first (`memory approve`) asks before it acts.
        "person" if for_a_person() => Response::ok(id, json!({})),
        "person" => refused(id, "not_a_person"),
        "memory.remember" => {
            memory_remember(id, body, daemon, instance, !instance && for_a_person())
        }
        "memory.recall" => memory_recall(id, body, daemon),
        #[cfg(feature = "bench")]
        "bench.ingest_turns" => bench_ingest_turns(id, body, daemon),
        #[cfg(feature = "bench")]
        "bench.settle" => bench_settle(id, daemon),
        // Takes the team lock and no store lock; `memory_manage` reads the team first.
        "memory.team" => memory_team(id, body, daemon, for_a_person()),
        "memory.import" => memory_import(id, body, daemon),
        "memory.forget_author" => memory_forget_author(id, body, daemon),
        "memory.list" if cloud_list => memory_list_since(id, body, daemon),
        "memory.list" | "memory.show" | "memory.forget" | "memory.share" | "memory.last"
        | "memory.export" => memory_manage(id, op, body, daemon, for_a_person()),
        "team.leave" => super::net_ops::leave(id, daemon, for_a_person()).await,
        "holds" | "hold.get" | "hold.answer" | "hold.file" => {
            crate::holds::op(id, op, body, daemon, caller)
        }
        "share.grant" | "stream.open" | "stream.send" | "stream.close" => {
            crate::streams::handle(id, op, body, daemon, caller).await
        }
        "team.invite" => {
            let mut r = super::net_ops::handle(id, op, body, daemon, caller);
            // `code: false` (`invite --agents`): the link alone, no second link behind a code.
            if r.ok && body["code"] != false {
                crate::code::offer(daemon, &mut r.body).await;
            }
            r
        }
        op => super::net_ops::handle(id, op, body, daemon, caller),
    }
}

fn fact_json(f: &crate::memory::Fact) -> Value {
    json!({
        "id": f.id, "subject": f.subject, "value": f.value, "kind": f.kind, "scope": f.scope,
        "author": f.author, "when": f.when,
        "status": format!("{:?}", f.status).to_lowercase(),
        "history": f.history.iter().map(|(id, v, w)| json!({"id": id, "value": v, "when": w})).collect::<Vec<_>>(),
    })
}

/// `memory.remember {subject, fact, kind, scope?, replaces?, when?, session?}` ->
/// `{stored, entry_id?, neighbours, refused?}`: stores a signed entry, with the harness and repo
/// of `session` (an MCP caller's key) as its source. `neighbours` are the current facts on the
/// subject from before this one. A secret or an oversize entry is `stored: false` with `refused`.
/// On an instance (the workspace) the source is `workspace`, on behalf of `on_behalf_of`.
fn memory_remember(
    id: u64,
    body: &Value,
    daemon: &Arc<Daemon>,
    instance: bool,
    person: bool,
) -> Response {
    use crate::memory::{self, Kind, Op, Scope, Source, Status};
    let (Some(subject), Some(fact), Ok(kind)) = (
        body["subject"].as_str(),
        body["fact"].as_str(),
        serde_json::from_value::<Kind>(body["kind"].clone()),
    ) else {
        return Response::err(id, BAD_REQUEST);
    };
    let scope = match body.get("scope") {
        None | Some(Value::Null) => Ok(Scope::Me),
        Some(v) => serde_json::from_value(v.clone()),
    };
    let replaces: Result<Vec<String>, _> = match body.get("replaces") {
        None | Some(Value::Null) => Ok(vec![]),
        Some(v) => serde_json::from_value(v.clone()),
    };
    let (Ok(scope), Ok(replaces)) = (scope, replaces) else {
        return Response::err(id, BAD_REQUEST);
    };
    if subject.trim().is_empty() {
        return refused(id, "no_subject");
    }
    if scope == Scope::Team && !daemon.team.lock().unwrap().config.memory_team {
        return Response::ok(id, json!({ "stored": false, "refused": "team_off" }));
    }
    let session = body["session"].as_str().map(String::from);
    let found = session
        .as_deref()
        .and_then(|k| live_sessions(daemon).into_iter().find(|s| s.key.0 == k));
    // A person's terminal names the source itself (`memory approve`: the cloud evaluator).
    let named = person
        .then(|| serde_json::from_value::<Source>(body["source"].clone()).ok())
        .flatten();
    let source = named.unwrap_or_else(|| Source {
        harness: if instance {
            "workspace".into()
        } else {
            found
                .as_ref()
                .map_or("unknown".into(), |s| crate::frame::agent_name(&s.provider))
        },
        repo: found.and_then(|s| s.repo),
        session,
        on_behalf_of: body["on_behalf_of"]
            .as_str()
            .filter(|_| instance)
            .map(String::from),
        shared_from: None,
    });
    let op = Op::Remember {
        subject: subject.into(),
        fact: fact.into(),
        kind,
        scope,
        when: body["when"].as_i64(),
        source,
        replaces,
    };
    let store = daemon.store.lock().unwrap();
    let neighbours: Vec<Value> = memory::search(&store, subject, 5)
        .unwrap_or_default()
        .iter()
        .filter(|f| f.status != Status::Replaced)
        .map(fact_json)
        .collect();
    match memory::remember(&store, &daemon.identity, op, now() as i64) {
        Ok(e) => Response::ok(
            id,
            json!({ "stored": true, "entry_id": e.id, "neighbours": neighbours }),
        ),
        Err(e) => {
            let reason = e.to_string();
            if reason.starts_with("secret:") || reason == "too_long" {
                Response::ok(
                    id,
                    json!({ "stored": false, "refused": reason, "neighbours": neighbours }),
                )
            } else {
                Response::err(id, INTERNAL)
            }
        }
    }
}

/// `{tool, args, duration_ms?, ok?}` recorded for session `sid`; whether it was.
fn tool_call(store: &crate::store::Store, sid: &str, b: &Value) -> crate::Result<bool> {
    let Some(tool) = b["tool"].as_str() else {
        return Err("bad".into());
    };
    crate::capture::record_tool_call(
        store,
        sid,
        tool,
        b["args"].as_str().unwrap_or_default(),
        b["duration_ms"].as_i64(),
        b["ok"].as_bool().unwrap_or(true),
    )
}

/// Local capture. The hooks send `capture.session_start {harness, conversation, cwd, repo?,
/// branch?}`, `capture.tool_call {harness, conversation, tool, args, duration_ms?, ok?, cwd?,
/// repo?, branch?}` (which opens a session that has no row),
/// `capture.turn {harness, conversation, role, text}` -> `{stored}`,
/// `capture.stop {harness, conversation, prompt_tokens?, completion_tokens?, text?, key?}` ->
/// `{nudge}` (the reply `text` kept as the assistant's turn; `Store::nudge`, asked only with
/// `key`) and
/// `capture.session_end {harness, conversation, prompt_tokens?, completion_tokens?}` -> `{stored}`. A person reads and deletes:
/// `sessions.list` -> `{sessions}`, `sessions.show {session}` -> `{session, tool_calls}`,
/// `sessions.forget {session}` -> `{}`, `sessions.export` -> `{sessions: [{..., tool_calls}]}`.
fn capture_op(id: u64, op: &str, body: &Value, daemon: &Arc<Daemon>, person: bool) -> Response {
    use crate::capture;
    let bad = Response::err(id, BAD_REQUEST);
    let who = |b: &Value| match (b["harness"].as_str(), b["conversation"].as_str()) {
        (Some(h), Some(c)) if !h.is_empty() && !c.is_empty() => Some(format!("{h}:{c}")),
        _ => None,
    };
    // Team before store, never the other way round.
    let (computer, hidden) = {
        let t = daemon.team.lock().unwrap();
        let cwd = body["cwd"].as_str();
        (
            t.config.computer.clone(),
            cwd.is_some_and(|c| presence::hidden(&t.off, c)),
        )
    };
    let store = daemon.store.lock().unwrap();
    // From the file each time: `sessions turns off` reaches a running service.
    let turn = |store: &crate::store::Store, sid: &str, role: &str, text: &str| {
        let on = crate::config::Config::load(&daemon.dir).is_ok_and(|c| c.capture_turns);
        crate::Result::Ok(on && capture::record_turn(store, sid, role, text, now() as i64)?)
    };
    // Opens session `sid` from `body`'s cwd, repo and branch; whether it was (a cwd, not in
    // a hidden folder, and new).
    let start = |sid: &str| -> crate::Result<bool> {
        let Some(cwd) = body["cwd"].as_str() else {
            return Ok(false);
        };
        let row = capture::SessionRow {
            id: sid.into(),
            harness: body["harness"].as_str().unwrap_or_default().into(),
            computer: computer.clone(),
            repo: body["repo"].as_str().map(String::from),
            branch: body["branch"].as_str().map(String::from),
            started: now() as i64,
            ended: None,
            prompt_tokens: None,
            completion_tokens: None,
        };
        Ok(!hidden && capture::start_session(&store, &row, cwd)?)
    };
    let r: crate::Result<Value> = (|| match op {
        "capture.session_start" => {
            let (Some(sid), Some(_)) = (who(body), body["cwd"].as_str()) else {
                return Err("bad".into());
            };
            Ok(json!({ "stored": start(&sid)? }))
        }
        "capture.tool_call" => {
            let Some(sid) = who(body) else {
                return Err("bad".into());
            };
            // A session running before capture existed has no row: this call (which carries
            // `cwd`, `repo` and `branch`) opens it, in the one request, so a hook that has
            // stopped waiting never loses the call.
            let stored =
                tool_call(&store, &sid, body)? || (start(&sid)? && tool_call(&store, &sid, body)?);
            Ok(json!({ "stored": stored }))
        }
        "capture.turn" => {
            let (Some(sid), Some(role), Some(text)) =
                (who(body), body["role"].as_str(), body["text"].as_str())
            else {
                return Err("bad".into());
            };
            Ok(json!({ "stored": turn(&store, &sid, role, text)? }))
        }
        "capture.stop" => {
            let Some(sid) = who(body) else {
                return Err("bad".into());
            };
            capture::set_tokens(
                &store,
                &sid,
                body["prompt_tokens"].as_i64(),
                body["completion_tokens"].as_i64(),
            )?;
            // The reply, if any: a turn that fails never costs the Stop its tokens or nudge.
            if let Some(text) = body["text"].as_str() {
                let _ = turn(&store, &sid, "assistant", text);
            }
            // `key`: the hook can hand the agent a nudge on this Stop.
            let nudge = match body["key"].as_str() {
                Some(key) => store.nudge(&sid, key, now() as i64)?,
                None => false,
            };
            Ok(json!({ "nudge": nudge }))
        }
        "capture.session_end" => {
            let Some(sid) = who(body) else {
                return Err("bad".into());
            };
            // The hook's one call at the end carries the transcript's totals, when it had any.
            let tokens = (
                body["prompt_tokens"].as_i64(),
                body["completion_tokens"].as_i64(),
            );
            capture::set_tokens(&store, &sid, tokens.0, tokens.1)?;
            capture::end_session(&store, &sid, now() as i64)?;
            Ok(json!({}))
        }
        _ if !person => Err("not_a_person".into()),
        "sessions.list" => Ok(json!({ "sessions": capture::sessions_since(&store, -1)? })),
        "sessions.show" | "sessions.forget" => {
            let Some(s) = capture::find(&store, body["session"].as_str().unwrap_or_default())?
            else {
                return Err("not_found".into());
            };
            if op == "sessions.forget" {
                capture::forget(&store, &s.id)?;
                return Ok(json!({}));
            }
            Ok(
                json!({ "session": s, "tool_calls": capture::tool_calls(&store, &s.id, true)?,
                "turns": capture::turns(&store, &s.id)? }),
            )
        }
        _ => {
            let mut all = vec![];
            for s in capture::sessions_since(&store, -1)? {
                let mut j = serde_json::to_value(&s)?;
                j["tool_calls"] = serde_json::to_value(capture::tool_calls(&store, &s.id, true)?)?;
                all.push(j);
            }
            Ok(json!({ "sessions": all }))
        }
    })();
    match r {
        Ok(v) => Response::ok(id, v),
        Err(e) if e.to_string() == "bad" => bad,
        Err(e) if e.to_string() == "not_a_person" => refused(id, "not_a_person"),
        Err(e) if e.to_string() == "not_found" => Response::err(id, NOT_FOUND),
        Err(e) if e.to_string() == "ambiguous" => Response::err(id, "ambiguous"),
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// `memory.recall {question, budget_chars?, repo?, session?}` -> `{page, facts, turns}`: what the
/// team holds that matches `question` (all of it when empty, or only `repo`'s, team facts first),
/// as the page an agent reads and as facts, and up to 5 of this computer's own turns that match a
/// question (never for an empty one), only `repo`'s when it is given. With `repo` and `session` (a session-start hook) the page is kept
/// for `memory last`.
fn memory_recall(id: u64, body: &Value, daemon: &Arc<Daemon>) -> Response {
    use crate::memory::{self, Status};
    let Some(q) = body["question"].as_str() else {
        return Response::err(id, BAD_REQUEST);
    };
    // The start block keeps its 1,600 characters. A question's page is the benchmark's:
    // `memory::trim_evidence` is its only cutter, and the page holds every result it kept, each
    // with its line's author and date (so `counts` are the page's lines).
    let asked = !q.trim().is_empty();
    let budget = body["budget_chars"]
        .as_u64()
        .map_or(if asked { usize::MAX } else { 1_600 }, |b| b as usize);
    let as_of = bench_as_of(body);
    let names = member_names(daemon);
    // From the file, before the lock: `sessions turns off` also stops recall using the turns
    // already stored (they stay for `sessions show` and `sessions forget`).
    let turns_on = crate::config::Config::load(&daemon.dir).is_ok_and(|c| c.capture_turns);
    let store = daemon.store.lock().unwrap();
    let found = if q.trim().is_empty() {
        let repo = body["repo"].as_str();
        memory::facts(&store, None, repo).map(|mut f| {
            if repo.is_some() {
                f.sort_by_key(|f| f.scope != memory::Scope::Team);
            }
            f
        })
    } else {
        memory::search_as_of(&store, q, 50, as_of)
    };
    let Ok(mut facts) = found else {
        return Response::err(id, INTERNAL);
    };
    facts.retain(|f| f.status != Status::Replaced);
    // The session-start block (an empty question) never carries turns.
    let turns = if q.trim().is_empty() || !turns_on {
        Ok(Vec::new())
    } else {
        crate::capture::search_turns(&store, q, body["repo"].as_str(), 50, as_of)
    };
    let Ok(mut turns) = turns else {
        return Response::err(id, INTERNAL);
    };
    if asked {
        memory::trim_evidence(&mut facts, &mut turns);
    }
    // The start block caps one subject's lines; a question's page is governed by its trim.
    let per_subject = if asked {
        usize::MAX
    } else {
        memory::START_LINES_PER_SUBJECT
    };
    let page = memory::page(&store, &facts, &turns, &names, budget, per_subject);
    let block = memory::block(&page);
    // Only the start block is kept: an agent's `recall` carries its repo and session too.
    if let (Some(_), Some(session), false, true) = (
        body["repo"].as_str(),
        body["session"].as_str(),
        block.is_empty(),
        q.trim().is_empty(),
    ) {
        let _ = store.set_memory_last(session, &block, now() as i64);
    }
    let mut out = json!({ "page": page, "block": block, "facts": facts.iter().map(fact_json).collect::<Vec<_>>(), "turns": turns });
    if let Some(t) = as_of {
        out["as_of"] = t.into();
    }
    Response::ok(id, out)
}

/// `memory.recall`'s benchmark knob, `as_of` (unix seconds): read only in a `bench` build;
/// a release recalls as of now.
#[cfg(feature = "bench")]
fn bench_as_of(body: &Value) -> Option<i64> {
    body["as_of"].as_i64()
}

#[cfg(not(feature = "bench"))]
fn bench_as_of(_: &Value) -> Option<i64> {
    None
}

/// `bench.ingest_turns {session, turns: [{role, text, at}]}` -> `{stored}`: a benchmark's turns,
/// in order, at the times the dataset gives (unix seconds), through `capture::record_turn`.
#[cfg(feature = "bench")]
fn bench_ingest_turns(id: u64, body: &Value, daemon: &Arc<Daemon>) -> Response {
    use crate::capture::{record_turn, start_session, SessionRow};
    let (Some(session), Some(turns)) = (body["session"].as_str(), body["turns"].as_array()) else {
        return Response::err(id, BAD_REQUEST);
    };
    let store = daemon.store.lock().unwrap();
    let row = SessionRow {
        id: session.into(),
        harness: "bench".into(),
        computer: "bench".into(),
        repo: None,
        branch: None,
        started: turns.first().and_then(|t| t["at"].as_i64()).unwrap_or(0),
        ended: None,
        prompt_tokens: None,
        completion_tokens: None,
    };
    if start_session(&store, &row, "/bench").is_err() {
        return Response::err(id, INTERNAL);
    }
    let mut stored = 0;
    for t in turns {
        let (Some(role), Some(text), Some(at)) =
            (t["role"].as_str(), t["text"].as_str(), t["at"].as_i64())
        else {
            return Response::err(id, BAD_REQUEST);
        };
        match record_turn(&store, session, role, text, at) {
            Ok(true) => stored += 1,
            Ok(false) => {}
            Err(_) => return Response::err(id, INTERNAL),
        }
    }
    Response::ok(id, json!({ "stored": stored }))
}

/// `bench.settle` -> `{embedded}`: embeds every entry and turn still without a vector, before
/// answering, so a benchmark's questions see all of it.
#[cfg(feature = "bench")]
fn bench_settle(id: u64, daemon: &Arc<Daemon>) -> Response {
    let mut embedded = 0;
    loop {
        match crate::memory::embed_missing(&daemon.store.lock().unwrap(), 200) {
            Ok(0) => break,
            Ok(n) => embedded += n,
            Err(_) => return Response::err(id, INTERNAL),
        }
    }
    Response::ok(id, json!({ "embedded": embedded }))
}

/// Member key -> the name shown for it.
fn member_names(daemon: &Daemon) -> std::collections::HashMap<String, String> {
    daemon
        .team
        .lock()
        .unwrap()
        .roster
        .members
        .values()
        .map(|m| (m.key.clone(), m.name.clone()))
        .collect()
}

/// `sessions.upload {mode}` -> `{}`: a person chooses what is uploaded (`upload::MODES`).
fn sessions_upload(id: u64, body: &Value, daemon: &Arc<Daemon>, person: bool) -> Response {
    if !person {
        return refused(id, "not_a_person");
    }
    let Some(mode) = body["mode"]
        .as_str()
        .filter(|m| crate::upload::MODES.contains(m))
    else {
        return Response::err(id, BAD_REQUEST);
    };
    match crate::upload::set_mode(&daemon.dir, mode) {
        Ok(()) => Response::ok(id, json!({})),
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// Embeds what has no vector yet (turns, and memory entries, received ones too) in the
/// background, taking the store lock only to read and to write. Run after each turn and by the
/// housekeeping loop every minute; one pass at a time, a kick during a pass is dropped.
pub(crate) fn embed_missing(daemon: &Arc<Daemon>) {
    static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    use std::sync::atomic::Ordering::SeqCst;
    if RUNNING.swap(true, SeqCst) {
        return; // what a pass misses, the next minute's picks up
    }
    /// Clears `RUNNING` however the pass ends, a panic included.
    struct Done;
    impl Drop for Done {
        fn drop(&mut self) {
            RUNNING.store(false, SeqCst);
        }
    }
    let d = daemon.clone();
    tokio::task::spawn_blocking(move || {
        let _done = Done;
        let lock = || d.store.lock().unwrap();
        if let Err(e) = crate::memory::embed_missing_with(lock, 200) {
            eprintln!("embed: {e}");
        }
    });
}

/// `sessions.turns {on}` -> `{}`: a person keeps or stops keeping their prompts and replies.
fn sessions_turns(id: u64, body: &Value, daemon: &Arc<Daemon>, person: bool) -> Response {
    if !person {
        return refused(id, "not_a_person");
    }
    let Some(on) = body["on"].as_bool() else {
        return Response::err(id, BAD_REQUEST);
    };
    match crate::config::Config::set(&daemon.dir, "capture_turns", &on.to_string()) {
        Ok(()) => Response::ok(id, json!({})),
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// `memory.team {on}` -> `{}`: a person turns team memory on or off.
fn memory_team(id: u64, body: &Value, daemon: &Arc<Daemon>, person: bool) -> Response {
    if !person {
        return refused(id, "not_a_person");
    }
    let Some(on) = body["on"].as_bool() else {
        return Response::err(id, BAD_REQUEST);
    };
    match crate::config::Config::set(&daemon.dir, "memory_team", &on.to_string())
        .and_then(|_| super::net_ops::reload(daemon))
    {
        Ok(_) => Response::ok(id, json!({})),
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// `memory.import {entries: [{subject, fact, kind, scope, when?, source, replaces?}]}` ->
/// `{ids, refused}`: the cloud member signs each as its own, in order. All are parsed before any
/// is stored; a refused one (`secret:..`, `too_long`, `no_subject`) is `null` in `ids` and
/// `{index, reason}` in `refused`, and the rest go on. An entry's `key` is written on the entry as
/// its `source.session`, and also kept in the `memory_import_keys` side table, because a
/// forgotten entry loses its body (and the key with it) while a held key must still answer
/// with its id (idempotent).
fn memory_import(id: u64, body: &Value, daemon: &Arc<Daemon>) -> Response {
    use crate::memory::{self, Op, Source};
    let Some(items) = body["entries"].as_array() else {
        return Response::err(id, BAD_REQUEST);
    };
    let ops: Option<Vec<(Op, Option<String>)>> = items
        .iter()
        .map(|e| {
            let s = &e["source"];
            let key = e["key"].as_str().map(String::from);
            let op = Op::Remember {
                subject: e["subject"].as_str()?.into(),
                fact: e["fact"].as_str()?.into(),
                kind: serde_json::from_value(e["kind"].clone()).ok()?,
                scope: serde_json::from_value(e["scope"].clone()).ok()?,
                when: e["when"].as_i64(),
                source: Source {
                    harness: s["harness"].as_str()?.into(),
                    repo: s["repo"].as_str().map(String::from),
                    session: key
                        .clone()
                        .or_else(|| s["session"].as_str().map(String::from)),
                    on_behalf_of: s["on_behalf_of"].as_str().map(String::from),
                    shared_from: None,
                },
                replaces: match e.get("replaces") {
                    None | Some(Value::Null) => vec![],
                    Some(v) => serde_json::from_value(v.clone()).ok()?,
                },
            };
            Some((op, key))
        })
        .collect();
    let Some(ops) = ops else {
        return Response::err(id, BAD_REQUEST);
    };
    let store = daemon.store.lock().unwrap();
    // key -> entry id, kept apart from the entries: a forgotten entry loses its body (and its
    // key with it), but a replay of its key must still answer with its id.
    let conn = store.conn();
    if conn
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS memory_import_keys (key TEXT PRIMARY KEY, id TEXT NOT NULL)",
        )
        .is_err()
    {
        return Response::err(id, INTERNAL);
    }
    let (mut ids, mut refusals) = (Vec::new(), Vec::new());
    for (index, (op, key)) in ops.into_iter().enumerate() {
        let held: Option<String> = key.as_ref().and_then(|k| {
            conn.query_row(
                "SELECT id FROM memory_import_keys WHERE key = ?1",
                [k],
                |r| r.get(0),
            )
            .ok()
        });
        if held.is_some() {
            ids.push(json!(held));
            continue;
        }
        match memory::remember(&store, &daemon.identity, op, now() as i64) {
            Ok(e) => {
                if let Some(k) = key {
                    if conn
                        .execute(
                            "INSERT INTO memory_import_keys (key, id) VALUES (?1, ?2)",
                            [&k, &e.id],
                        )
                        .is_err()
                    {
                        return Response::err(id, INTERNAL);
                    }
                }
                ids.push(json!(e.id));
            }
            Err(e) => {
                let reason = e.to_string();
                if !(reason.starts_with("secret:")
                    || reason == "too_long"
                    || reason == "no_subject")
                {
                    return Response::err(id, INTERNAL);
                }
                ids.push(Value::Null);
                refusals.push(json!({ "index": index, "reason": reason }));
            }
        }
    }
    daemon.peers.outbox.notify_one();
    Response::ok(id, json!({ "ids": ids, "refused": refusals }))
}

/// `memory.list {since_seq}` -> `{entries, next}`: up to 200 rows after `since_seq` in insertion
/// order, each stored entry as `memory.export` shows it with `seq` the row's number; a tombstoned
/// row is `{seq, id, author, tombstone: true}`.
fn memory_list_since(id: u64, body: &Value, daemon: &Arc<Daemon>) -> Response {
    let Some(since) = body["since_seq"].as_i64() else {
        return Response::err(id, BAD_REQUEST);
    };
    match crate::memory::page_since(&daemon.store.lock().unwrap(), since, 200) {
        Ok(rows) => {
            let next = rows.last().map_or(since, |(seq, _)| *seq);
            let entries: Vec<Value> = rows.into_iter().map(|(_, v)| v).collect();
            Response::ok(id, json!({ "entries": entries, "next": next }))
        }
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// `memory.forget_author {author}` -> `{forgotten}`: a `Forget` for every entry held from
/// `author` (a member wipe).
fn memory_forget_author(id: u64, body: &Value, daemon: &Arc<Daemon>) -> Response {
    use crate::memory::{self, Op};
    let Some(author) = body["author"].as_str() else {
        return Response::err(id, BAD_REQUEST);
    };
    let store = daemon.store.lock().unwrap();
    let Ok(held) = memory::entries(&store) else {
        return Response::err(id, INTERNAL);
    };
    let mut forgotten = 0;
    for e in held
        .into_iter()
        .filter(|e| e.author == author && matches!(e.op, Op::Remember { .. }))
    {
        let op = Op::Forget { target: e.id };
        if memory::remember(&store, &daemon.identity, op, now() as i64).is_err() {
            return Response::err(id, INTERNAL);
        }
        forgotten += 1;
    }
    daemon.peers.outbox.notify_one();
    Response::ok(id, json!({ "forgotten": forgotten }))
}

/// The `memory.*` ops a person manages memory with: `list {scope?, repo?}` -> `{facts}`;
/// `show {entry}` -> `{fact, source}`; `forget {entry}` -> `{}`; `share {entry}` -> `{entry_id}`;
/// `last {session?}` -> `{block}`; `export` -> `{entries}`. `entry` is an entry id or its first
/// six or more digits. Authors are member names where the roster knows them.
fn memory_manage(id: u64, op: &str, body: &Value, daemon: &Arc<Daemon>, person: bool) -> Response {
    use crate::memory::{self, Op, Scope};
    let names = member_names(daemon);
    let who = |k: &str| {
        names
            .get(k)
            .cloned()
            .unwrap_or_else(|| k.chars().take(12).collect())
    };
    let shown = |f: &memory::Fact| {
        let mut j = fact_json(f);
        j["author"] = who(&f.author).into();
        j
    };
    let bad = |c: &str| Response::err(id, c);
    // Before the store lock: team then store is the order everywhere else.
    let memory_team = daemon.team.lock().unwrap().config.memory_team;
    let store = daemon.store.lock().unwrap();
    let entry = || match body["entry"].as_str() {
        Some(i) => memory::entry(&store, i)
            .map_err(|_| bad(INTERNAL))
            .and_then(|e| e.ok_or_else(|| bad(NOT_FOUND))),
        None => Err(bad(BAD_REQUEST)),
    };
    if !person && matches!(op, "memory.forget" | "memory.share") {
        return refused(id, "not_a_person");
    }
    let r: Result<Value, Response> = (|| match op {
        "memory.list" => {
            let scope = match body["scope"].as_str() {
                None => None,
                Some("me") => Some(Scope::Me),
                Some("team") => Some(Scope::Team),
                Some(_) => return Err(bad(BAD_REQUEST)),
            };
            let f =
                memory::facts(&store, scope, body["repo"].as_str()).map_err(|_| bad(INTERNAL))?;
            Ok(json!({ "facts": f.iter().map(&shown).collect::<Vec<_>>() }))
        }
        "memory.show" => {
            let e = entry()?;
            let f = memory::fact(&store, &e.id)
                .map_err(|_| bad(INTERNAL))?
                .ok_or_else(|| bad(NOT_FOUND))?;
            let Op::Remember { source, .. } = e.op else {
                return Err(bad(NOT_FOUND));
            };
            let mut fact = shown(&f);
            fact["history"] = f
                .history
                .iter()
                .map(|(i, v, w)| json!({"id": i, "value": v, "when": w}))
                .collect();
            Ok(json!({ "fact": fact, "source": source }))
        }
        "memory.forget" => {
            let e = entry()?;
            let op = Op::Forget { target: e.id };
            memory::remember(&store, &daemon.identity, op, now() as i64)
                .map_err(|_| bad(INTERNAL))?;
            daemon.peers.outbox.notify_one();
            Ok(json!({}))
        }
        "memory.share" => {
            if !memory_team {
                return Err(bad("team_off"));
            }
            let e = entry()?;
            let Op::Remember {
                subject,
                fact,
                kind,
                scope: Scope::Me,
                mut source,
                ..
            } = e.op
            else {
                return Err(bad("not_mine"));
            };
            if e.author != daemon.identity.key {
                return Err(bad("not_mine"));
            }
            let current = memory::fact(&store, &e.id)
                .map_err(|_| bad(INTERNAL))?
                .ok_or_else(|| bad(NOT_FOUND))?;
            if current.status == memory::Status::Replaced {
                let newer = memory::facts(&store, None, None)
                    .map_err(|_| bad(INTERNAL))?
                    .into_iter()
                    .find(|f| f.history.iter().any(|(h, _, _)| *h == e.id));
                return Err(Response::err_with(
                    id,
                    "replaced",
                    json!({ "current": newer.map(|f| f.id) }),
                ));
            }
            let shared = memory::entries(&store).map_err(|_| bad(INTERNAL))?;
            if shared.iter().any(|x| {
                matches!(&x.op, Op::Remember { source, .. } if source.shared_from.as_deref() == Some(&e.id))
            }) {
                return Err(bad("already_shared"));
            }
            source.shared_from = Some(e.id);
            let op = Op::Remember {
                subject,
                fact,
                kind,
                scope: Scope::Team,
                when: None,
                source,
                replaces: vec![],
            };
            let n = memory::remember(&store, &daemon.identity, op, now() as i64)
                .map_err(|_| bad(INTERNAL))?;
            Ok(json!({ "entry_id": n.id }))
        }
        "memory.last" => {
            let b = store
                .memory_last(body["session"].as_str())
                .map_err(|_| bad(INTERNAL))?;
            Ok(json!({ "block": b.unwrap_or_default() }))
        }
        _ => Ok(json!({ "entries": memory::entries(&store).map_err(|_| bad(INTERNAL))? })),
    })();
    match r {
        Ok(b) => Response::ok(id, b),
        Err(r) => r,
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

/// `end {key, caller?, if_gone?, reason?}` -> `{ended}`: ends the session row (kept, ended);
/// `reason` is `process-exited` (default), `moved-on`, `job-done` or `session-end`;
/// its mail stays for the hold. With `caller`, only the row's own process may end it; with `if_gone`, also
/// anyone once the row's process is dead. `ended: false` when refused. Neither field (the
/// CLI) ends it outright.
fn end(id: u64, key: &str, body: &Value, daemon: &Daemon) -> Response {
    let Ok(caller) = caller(body) else {
        return Response::err(id, BAD_REQUEST);
    };
    let reason = match body["reason"].as_str() {
        None => "process-exited",
        Some(r @ ("process-exited" | "moved-on" | "job-done" | "session-end")) => r,
        Some(_) => return Response::err(id, BAD_REQUEST),
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
    // The row stays, ended, with its reason: its address is kept (`presence`).
    match store.end_session(key, now() as i64, reason, None) {
        Ok(true) => {
            daemon.presence.notify_one();
            daemon.listing.lock().unwrap().dirty = true;
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
    waiting_for: Option<String>,
}

/// `register {session: {provider, conversation, pid, birth_micros, cwd, title?, status?, waiting_for?, executable?, repo?, branch?}, caller?}`
/// upserts by key; the daemon supplies the member prefix. A `cwd` under `folders_off` is
/// answered `{key, ignored: true}` and nothing is written. A key whose row names another
/// live process is refused (`bad_request`, reason `owned_by_live_process`). `status` is `idle` (default),
/// `busy` (stored as live) or `waiting` (with `waiting_for`, at most 64 characters, what for); `conversation` is `[A-Za-z0-9._-]{1,128}`. `repo` and `branch` are
/// what the hook read from `cwd`'s `.git` (it has the folder access the person gave the agent).
fn register(id: u64, body: &Value, daemon: &Daemon) -> Response {
    let Ok(r) = serde_json::from_value::<Register>(body["session"].clone()) else {
        return Response::err(id, BAD_REQUEST);
    };
    let status = match r.status.as_deref() {
        None | Some("idle") => "idle",
        Some("busy") => "live",
        Some("waiting") => "waiting",
        Some(_) => return Response::err(id, BAD_REQUEST),
    };
    if r.waiting_for
        .as_ref()
        .is_some_and(|w| w.chars().count() > 64)
    {
        return Response::err(id, BAD_REQUEST);
    }
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
    let store = daemon.store.lock().unwrap();
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
        reason: None,
        ended_at: None,
        waiting_for: r.waiting_for.filter(|_| status == "waiting"),
    };
    // Every hook event registers: mail delivered before it has been seen.
    match put_row(daemon, &store, &row).and_then(|()| mail::mark_read_for(&store, &key, now())) {
        Ok(()) => {
            // A hook event: Claude's listing has news too.
            daemon.listing.lock().unwrap().dirty = true;
            // A `read` owed to a sender goes now (`Net::flush_loop`).
            daemon.peers.outbox.notify_one();
            Response::ok(id, json!({ "key": key }))
        }
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

/// Registered and discovered sessions; a discovered one gains a row and a row no longer live
/// ends (`record`). A session whose last push failed reads `next_step`: its mail waits for the next step. One
/// under a hidden folder, or marked by a register a hidden folder refused, reads `hidden`.
pub(crate) fn live_sessions(daemon: &Daemon) -> Vec<sessions::Session> {
    let (rows, marked) = {
        let store = daemon.store.lock().unwrap();
        let rows = store.sessions().unwrap_or_default();
        (rows, store.hidden_keys().unwrap_or_default())
    };
    let off = daemon.team.lock().unwrap().off.clone();
    let listed = super::current_at(daemon);
    let mut all = sessions::all(
        &rows,
        listed.as_ref().map(|(l, at)| (l.as_slice(), *at)),
        &daemon.claude_home,
        &daemon.identity.short,
    );
    let failed = daemon.push_failed.lock().unwrap();
    for s in &mut all {
        if s.tier == sessions::Tier::Push && failed.contains(&s.key.0) {
            s.tier = sessions::Tier::NextStep;
        }
        s.hidden = marked.contains(&s.key.0) || presence::hidden(&off, &s.cwd);
    }
    drop(failed);
    let hidden = |key: &str, cwd: &str| marked.contains(key) || presence::hidden(&off, cwd);
    let listed = listed.as_ref().map(|(l, _)| l.as_slice());
    record(daemon, &rows, &mut all, listed, &hidden);
    all
}

/// The most rows of finished jobs written per read of the listing: hundreds of old jobs arrive
/// over a minute or so, not at once.
const JOBS_PER_READ: usize = 200;

/// The one writer of rows from what was seen: a live Claude session with no row gets one, a row
/// `all` no longer finds live ends with its reason and title, and a finished job with no process
/// and no row gets one, ended. Reads transcripts only for rows it ends or creates ended, and
/// outside the store lock.
fn record(
    daemon: &Daemon,
    rows: &[store::Session],
    live: &mut [sessions::Session],
    listed: Option<&[Listed]>,
    hidden: &dyn Fn(&str, &str) -> bool,
) {
    let (home, now) = (&daemon.claude_home, now() as i64);
    let is_live = |key: &str| live.iter().any(|s| s.key.0 == key);
    let title = |provider: &str, cwd: &str, conversation: &str| {
        (provider == "claude")
            .then(|| doing::title(home, cwd, conversation))
            .flatten()
    };
    let gone: Vec<(&store::Session, &str, Option<String>)> = rows
        .iter()
        .filter(|r| !r.provider.is_empty() && !is_live(&r.key))
        .map(|r| {
            let conversation = r.conversation.as_deref().unwrap_or_default();
            (
                r,
                ended_reason(r, listed),
                title(&r.provider, &r.cwd, conversation),
            )
        })
        .collect();
    let jobs = new_ended_jobs(daemon, listed, &is_live, hidden);
    let jobs: Vec<(String, &Listed, i64, Option<String>)> = jobs
        .into_iter()
        .map(|(key, l)| {
            // Over long before this computer looked: never "just ended".
            let started = (l.started_ms / 1000) as i64;
            if started + (presence::WEEK as i64) < now {
                (key, l, started, None)
            } else {
                let found = title("claude", &l.cwd, &l.conversation);
                (key, l, now, found.or(l.name.clone()))
            }
        })
        .collect();
    let store = daemon.store.lock().unwrap();
    let mut changed = false;
    for s in live.iter_mut() {
        if s.provider != "claude" || s.hidden || store.holds(&s.key.0).unwrap_or(true) {
            continue;
        }
        if store.put_session(&discovered_row(s, now)).is_ok() {
            changed = true;
            s.handle = store
                .session(&s.key.0)
                .ok()
                .flatten()
                .and_then(|r| r.handle);
        }
    }
    for (r, reason, title) in &gone {
        // A hook wrote it after `rows` was read: it is the newer word.
        if store.session(&r.key).ok().flatten().as_ref() == Some(*r) {
            changed |= store
                .end_session(&r.key, now, reason, title.as_deref())
                .unwrap_or(false);
        }
    }
    for (key, l, at, title) in &jobs {
        changed |= store
            .put_ended_job(key, &l.conversation, &l.cwd, title.as_deref(), *at)
            .unwrap_or(false);
    }
    drop(store);
    if changed {
        daemon.presence.notify_one();
    }
}

/// Why `r`, no longer live, ended.
fn ended_reason(r: &store::Session, listed: Option<&[Listed]>) -> &'static str {
    let job_done = r.provider == "claude"
        && listed.is_some_and(|l| {
            listing::ended_jobs(l)
                .any(|j| Some(j.conversation.as_str()) == r.conversation.as_deref())
        });
    let alive = || {
        r.pid
            .zip(r.birth_micros)
            .is_some_and(|(p, b)| sessions::alive(p as u32, b as u64))
    };
    if job_done {
        "job-done"
    } else if alive() {
        // The process lives but the harness left the conversation (`/clear`, `/resume`, the
        // agent view).
        "moved-on"
    } else {
        "process-exited"
    }
}

/// The first `JOBS_PER_READ` finished jobs of a read not yet looked at, with their keys, that
/// have no row and may have one. Each read is looked at once.
fn new_ended_jobs<'a>(
    daemon: &Daemon,
    listed: Option<&'a [Listed]>,
    is_live: &dyn Fn(&str) -> bool,
    hidden: &dyn Fn(&str, &str) -> bool,
) -> Vec<(String, &'a Listed)> {
    let Some(listed) = listed else {
        return vec![];
    };
    {
        let mut c = daemon.listing.lock().unwrap();
        if c.recorded == c.at {
            return vec![];
        }
        c.recorded = c.at;
    }
    let store = daemon.store.lock().unwrap();
    listing::ended_jobs(listed)
        .map(|l| {
            (
                Key::new(&daemon.identity.short, "claude", &l.conversation).0,
                l,
            )
        })
        .filter(|(key, l)| {
            !is_live(key) && !hidden(key, &l.cwd) && !store.holds(key).unwrap_or(true)
        })
        .take(JOBS_PER_READ)
        .collect()
}

/// A discovered session's first row. `waiting` is left to the hooks, its one writer: the
/// discovery's own status is laid over the row on every read.
fn discovered_row(s: &sessions::Session, now: i64) -> store::Session {
    store::Session {
        key: s.key.0.clone(),
        provider: s.provider.clone(),
        conversation: Some(s.conversation.clone()),
        pid: Some(s.pid.into()),
        birth_micros: i64::try_from(s.birth_micros).ok(),
        cwd: s.cwd.clone(),
        title: s.title.clone(),
        status: if s.status == sessions::Status::Live {
            "live".into()
        } else {
            "idle".into()
        },
        seen_at: now,
        executable: None,
        repo: None,
        branch: None,
        handle: None,
        reason: None,
        ended_at: None,
        waiting_for: None,
    }
}

/// The sessions that ended within the week, newest first, hidden folders left out. Rows past it
/// go, except the finished jobs Claude still lists: they stay, unseen, so that they are not
/// born again as just ended.
pub(crate) fn ended_sessions(daemon: &Daemon) -> Vec<store::Session> {
    let since = now().saturating_sub(presence::WEEK) as i64;
    let keep: Vec<String> = super::current(daemon)
        .iter()
        .flat_map(|l| listing::finished(l))
        .map(|c| Key::new(&daemon.identity.short, "claude", &c).0)
        .collect();
    let keep: Vec<&str> = keep.iter().map(String::as_str).collect();
    let off = daemon.team.lock().unwrap().off.clone();
    let store = daemon.store.lock().unwrap();
    let marked = store.hidden_keys().unwrap_or_default();
    let mut ended = store.ended_since(since, &keep).unwrap_or_default();
    ended.retain(|r| {
        r.ended_at.is_some_and(|at| at >= since)
            && !marked.contains(&r.key)
            && !presence::hidden(&off, &r.cwd)
    });
    ended
}

/// An ended row as `sessions` answers it, with the owner's resume command.
fn ended_json(r: &store::Session) -> Value {
    json!({
        "key": r.key,
        "provider": r.provider,
        "conversation": r.conversation,
        "cwd": r.cwd,
        "title": r.title,
        "handle": r.handle,
        "repo": r.repo,
        "branch": r.branch,
        "reason": r.reason,
        "ended_at": r.ended_at,
        "resume": r.conversation.as_deref().and_then(|c| sessions::resume(&r.provider, c)),
    })
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
    if m.origin.as_deref() == Some(mail::APP_ORIGIN) {
        let title = crate::link::title_of(daemon, &m.from_key);
        return label::hosted_app(&m.from_key, title.as_deref());
    }
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

/// `send {from, to?, text, request_id, reply_to?, kind?}` -> `{receipt}`; a bare `from` name is
/// this member's (`<member>/<name>`), and `to` may be left out
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
    send_as(id, body, daemon, false)
}

/// `send`; with `app`, from a hosted app's session on this computer (`link`): never hop 0 (the
/// agents' loop limits apply), and its mail for a session here is stored as from that app
/// (origin `app`: framed as "<App> (you)", never as typed at the terminal). Not held: the person
/// connected the app themselves.
pub(crate) fn send_as(
    id: u64,
    body: &Value,
    daemon: &Arc<Daemon>,
    app: bool,
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
    // A bare name is this member's own, as for `to` (the workspace bridge sends `friday`).
    let from = match sessions::valid_name(from) {
        true => format!("{}/{from}", daemon.identity.short),
        false => from.to_string(),
    };
    // A workspace member sends only as the agents it declared.
    if daemon.cloud.as_ref().is_some_and(|c| !c.declares(&from)) {
        return Err(refused(id, "not_declared"));
    }
    let from = Key(from);
    let team = daemon.team.lock().unwrap();
    let store = daemon.store.lock().unwrap();
    let internal = |_| Response::err(id, INTERNAL);
    let prepared = mail::prepare(&store, &from, to.as_ref(), text, rid, reply_to, now());
    let mut p = match prepared.map_err(internal)? {
        mail::Prep::Go(p) => p,
        mail::Prep::Done(r) => return Ok((from, r)),
    };
    if app {
        p.hops = p.hops.max(1);
    }
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
    let sent = if app {
        mail::store_local_from(&store, &p, mail::Origin::App)
    } else {
        mail::store_local(&store, &p)
    }
    .map_err(internal)?;
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
    if daemon.cloud.is_none() && id.is_some() {
        crate::link::stored(daemon, &to.0); // a hosted app's session: wake its bot, if it asked
    }
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
        let bell = team.roster.wakes.contains_key(&m.key);
        Some((m.key.clone(), m.computer.clone(), person, bell))
    });
    let Some((member, computer, person, bell)) = member else {
        return with_thread(&r);
    };
    if wait {
        r = settle(daemon, from, r, &member).await;
    }
    let mut v = with_thread(&r);
    v["where"] = computer.into();
    v["person"] = person.into();
    let online = daemon.peers.is_online(&member);
    v["online"] = online.into();
    // A parked workspace with a doorbell: the outbox rang it, so it is starting, not gone.
    v["waking"] = (bell && !online).into();
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
    // A hosted app's session here (`link`): named as the app, never as this computer's person.
    let apps = crate::link::apps(daemon);
    let team = daemon.team.lock().unwrap();
    let mut senders: HashMap<String, Value> = HashMap::new();
    messages
        .iter()
        .map(|m| {
            let from = label::from(short, m);
            let app = m.origin.as_deref() == Some(mail::APP_ORIGIN);
            let entry = if app {
                format!("app:{from}")
            } else {
                from.clone()
            };
            let who = senders.entry(entry).or_insert_with(|| {
                if app {
                    let title = apps.iter().find(|r| r.session_id == m.from_key);
                    let title = title.and_then(|r| r.title.as_deref());
                    return json!({ "label": label::hosted_app(&m.from_key, title),
                        "person": super::user_name(), "person_verified": false,
                        "computer": team.config.computer });
                }
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

/// For a hook that hands mail over only once it is in the session (OpenCode's plugin):
/// `inbox.peek {key, caller}` -> `{messages}` (`records`), left waiting; `inbox.ack {key,
/// caller, ids}` -> `{acked}` takes those messages from `key`'s mailbox, so delivers them (an
/// id not waiting for `key`, another session's or one acked before, is skipped). Only the
/// row's own process may do either (`bad_request`, reason `not_owner`, otherwise).
async fn peek_or_ack(id: u64, op: &str, body: &Value, daemon: &Daemon) -> Response {
    let (Some(key), Ok(Some(c))) = (body["key"].as_str().map(|k| Key(k.into())), caller(body))
    else {
        return Response::err(id, BAD_REQUEST);
    };
    match owner_process(daemon, &key.0) {
        Ok(Some(owner)) if owner == c => {}
        Ok(_) => return refused(id, "not_owner"),
        Err(_) => return Response::err(id, INTERNAL),
    }
    // Under the key's lock, like `inbox`: nothing else takes this mail meanwhile.
    let lock = daemon.key_lock(&key.0);
    let _held = lock.lock().await;
    if op == "inbox.peek" {
        let Ok(waiting) = mail::waiting(&daemon.store.lock().unwrap(), &key, now()) else {
            return Response::err(id, INTERNAL);
        };
        return Response::ok(id, json!({ "messages": records(daemon, &waiting) }));
    }
    let Some(ids) = body["ids"]
        .as_array()
        .and_then(|a| a.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
    else {
        return Response::err(id, BAD_REQUEST);
    };
    let acked = {
        let store = daemon.store.lock().unwrap();
        let mut n = 0;
        for m in ids {
            match mail::take_one(&store, &key, m, now()) {
                Ok(taken) => n += usize::from(taken.is_some()),
                Err(_) => return Response::err(id, INTERNAL),
            }
        }
        n
    };
    if acked > 0 {
        // Their `delivered` goes to the senders now (`Net::flush_loop`).
        daemon.peers.outbox.notify_one();
    }
    Response::ok(id, json!({ "acked": acked }))
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
    // it is kept a day after.
    let short = &daemon.identity.short;
    if let frame::Sender::Name(_) = frame::Sender::of(short, &key.0) {
        if sessions::member_of(&key.0) == Some(short.as_str()) {
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
                reason: None,
                ended_at: None,
                waiting_for: None,
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
                // Their `delivered` goes to the senders now (`Net::flush_loop`).
                daemon.peers.outbox.notify_one();
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
