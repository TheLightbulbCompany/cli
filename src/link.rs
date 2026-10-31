//! The route link: while this computer is signed in (`lightbulb login`), the daemon keeps ONE
//! outbound WebSocket to Lightbulb, so its person's hosted apps (the Claude app, ChatGPT, a bot
//! on Lightbulb's hosted MCP) can use the network through this computer when no workspace of
//! theirs is on one.
//!
//! Lightbulb opens with `{type: challenge, challenge}`; this computer answers `{type: hello,
//! key, sig, proof, computer}` (its member key's signature over the nonce, purpose
//! `lightbulb-route`, its sign-in proof and its name); Lightbulb answers `{type: ready, person}`, the opaque handle of the
//! signed-in person. From then on Lightbulb asks `{id, op, ...}` and this computer answers
//! `{id, ok, result}` or `{id, ok: false, error, reason?}`. Only these ops run, and only for the
//! app sessions `<me>/<app>-<person>` of that person: `app.send` (which may carry a kind), `app.inbox` (`peek`), `app.ack`,
//! `app.unread`, `app.receipt` (`app_send`) and `directory`. Each app op carries the app
//! session's `title`, which declares it here (published in presence, like a workspace's). New
//! mail for one of them sends `{type: mail, session}`, so Lightbulb can wake a bot that asked.
//!
//! Reconnects with backoff (1 s doubling to 60 s; 5 min after a refused sign-in). Logout (the
//! sign-in file gone or changed) drops the link within seconds.

use crate::daemon::{ipc, Daemon};
use crate::net::frames::Remote;
use crate::{login, sessions};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_websockets::{ClientBuilder, Message};

/// The ops Lightbulb may ask over the link. Nothing else on this computer is reachable.
pub const OPS: [&str; 6] = [
    "app.send",
    "app.inbox",
    "app.ack",
    "app.unread",
    "app.receipt",
    "directory",
];
/// At most this many hosted apps' sessions at once (the oldest goes first).
const MAX_APPS: usize = 20;
const MAX_FRAME: usize = 1 << 20;
/// The backend's close for a missing, expired or foreign proof (`network_route`).
const NOT_SIGNED_IN: u16 = 4403;

/// The link's state, shared with the ops and the presence.
#[derive(Default)]
pub struct State {
    /// The signed-in person's handle (`ready`): the only `person` the app ops take.
    pub person: Option<String>,
    /// The hosted apps' sessions declared over the link, newest last.
    pub apps: Vec<Remote>,
    /// The live link's mail notices.
    mail: Option<mpsc::UnboundedSender<String>>,
}

/// Whether app session `name` (`<app>-<person>`) of `person` is declared over the link.
pub fn declares(d: &Daemon, name: &str, person: &str) -> bool {
    let s = d.link.lock().unwrap();
    let id = format!("{}/{name}", d.identity.short);
    s.person.as_deref() == Some(person) && s.apps.iter().any(|r| r.session_id == id)
}

/// The declared title of app session `addr` (`ChatGPT via Aden`), while the link knows it.
pub fn title_of(d: &Daemon, addr: &str) -> Option<String> {
    let s = d.link.lock().unwrap();
    s.apps
        .iter()
        .find(|r| r.session_id == addr)
        .and_then(|r| r.title.clone())
}

/// The app sessions to publish with this computer's own (`presence`).
pub fn apps(d: &Daemon) -> Vec<Remote> {
    d.link.lock().unwrap().apps.clone()
}

/// Mail was stored for `to` here: if it is a linked app's session, tell Lightbulb (its name
/// only, never the mail).
pub fn stored(d: &Daemon, to: &str) {
    let Some((_, name)) = sessions::split_addr(to) else {
        return;
    };
    let s = d.link.lock().unwrap();
    if s.apps.iter().any(|r| r.session_id == to) {
        if let Some(tx) = &s.mail {
            let _ = tx.send(name.to_string());
        }
    }
}

/// Runs for the daemon's life (a computer only).
pub async fn run(d: Arc<Daemon>) {
    let mut backoff = 1u64;
    let mut refreshed: Option<Instant> = None;
    loop {
        let Some(signed) = login::stored(&d.dir) else {
            tokio::time::sleep(Duration::from_secs(15)).await;
            continue;
        };
        if refreshed.is_none_or(|t| t.elapsed() > Duration::from_secs(86_400)) {
            refreshed = Some(Instant::now());
            let dir = d.dir.clone();
            match tokio::task::spawn_blocking(move || login::refresh(&dir)).await {
                Ok(Ok(true)) => {
                    eprintln!("link: sign-in renewed");
                    d.peers.changed.notify_one(); // members hear the new proof
                }
                Ok(Err(e)) => eprintln!("link: sign-in renewal failed: {e}"),
                _ => {}
            }
            continue; // read the file again: the proof may be new
        }
        let (Some(url), Some(proof)) = (signed["route_url"].as_str(), signed["proof"].as_str())
        else {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        };
        if login::ended(&d.dir, ipc::now() as i64) {
            eprintln!("link: {}", login::SIGN_IN_AGAIN);
            tokio::time::sleep(Duration::from_secs(300)).await;
            continue;
        }
        let started = Instant::now();
        let refused = match session(&d, url, proof).await {
            Ok(code) => {
                eprintln!("link: closed ({code})");
                if code == NOT_SIGNED_IN {
                    eprintln!("link: {}", login::SIGN_IN_AGAIN);
                }
                code == NOT_SIGNED_IN
            }
            Err(e) => {
                eprintln!("link: {e}");
                false
            }
        };
        {
            let mut s = d.link.lock().unwrap();
            s.mail = None;
            s.person = None;
        }
        backoff = if started.elapsed() > Duration::from_secs(60) {
            1
        } else {
            (backoff * 2).min(60)
        };
        let wait = if refused { 300 } else { backoff };
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

fn plain_ok(url: &str) -> bool {
    url.starts_with("wss://")
        || ["ws://127.0.0.1:", "ws://localhost:", "ws://[::1]:"]
            .iter()
            .any(|p| url.starts_with(p))
}

/// One connection, until it closes: its close code (0 when the stream just ended).
async fn session(d: &Arc<Daemon>, url: &str, proof: &str) -> crate::Result<u16> {
    if !plain_ok(url) {
        return Err("the route URL must be wss:// (ws:// only to this computer)".into());
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let builder = ClientBuilder::new()
        .uri(url)
        .map_err(|e| format!("route URL: {e}"))?;
    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(20), builder.connect())
        .await
        .map_err(|_| "connect timed out")?
        .map_err(|e| format!("connect: {e}"))?;
    let next =
        |m: Option<Result<Message, tokio_websockets::Error>>| -> crate::Result<Option<Value>> {
            match m {
                None => Ok(None),
                Some(Err(e)) => Err(format!("read: {e}").into()),
                Some(Ok(m)) => Ok(m
                    .as_text()
                    .filter(|t| t.len() <= MAX_FRAME)
                    .and_then(|t| serde_json::from_str(t).ok())),
            }
        };
    let first = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .map_err(|_| "no challenge")?;
    let challenge = next(first)?.ok_or("closed before the challenge")?;
    let Some(nonce) = challenge["challenge"].as_str().map(String::from) else {
        return Err("no challenge".into());
    };
    let p = login::proof(&d.identity, "route", &nonce, None);
    // `computer`: what the person sees when they pin an app to this computer.
    let computer = d.team.lock().unwrap().config.computer.clone();
    let hello = json!({"type": "hello", "key": d.identity.key, "sig": p["sig"], "proof": proof,
        "computer": computer});
    ws.send(Message::text(hello.to_string())).await?;

    let (tx, mut mail) = mpsc::unbounded_channel::<String>();
    let (answers, mut answered) = mpsc::unbounded_channel::<Value>();
    let mut check = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            m = ws.next() => {
                let Some(m) = m else { return Ok(0) };
                let m = m.map_err(|e| format!("read: {e}"))?;
                if let Some((code, _)) = m.as_close() {
                    return Ok(code.into());
                }
                let Some(frame) = next(Some(Ok(m)))? else { continue };
                if frame["type"] == "ready" {
                    let person = frame["person"].as_str().filter(|p| sessions::valid_name(p));
                    let mut s = d.link.lock().unwrap();
                    s.person = person.map(String::from);
                    s.mail = Some(tx.clone());
                    eprintln!("link: up");
                    continue;
                }
                let (d, answers) = (d.clone(), answers.clone());
                tokio::spawn(async move {
                    let _ = answers.send(answer(&d, frame).await);
                });
            }
            Some(a) = answered.recv() => ws.send(Message::text(a.to_string())).await?,
            Some(name) = mail.recv() => {
                let notice = json!({"type": "mail", "session": name});
                ws.send(Message::text(notice.to_string())).await?;
            }
            _ = check.tick() => {
                // A logout (or a new login) ends this link.
                if login::stored_proof(&d.dir).as_deref() != Some(proof) {
                    let _ = ws.send(Message::close(None, "signed out")).await;
                    return Ok(1000);
                }
            }
        }
    }
}

/// One op's answer, enveloped as the member socket's (`{id, ok, result}`).
async fn answer(d: &Arc<Daemon>, frame: Value) -> Value {
    let id = frame["id"].as_u64().unwrap_or(0);
    let refused = |code: &str| json!({"id": id, "ok": false, "error": code});
    let op = frame["op"].as_str().unwrap_or_default();
    if !OPS.contains(&op) {
        return refused("unsupported");
    }
    let r = if op == "directory" {
        let q = frame["query"].as_str().map(String::from);
        let d2 = d.clone();
        match tokio::task::spawn_blocking(move || crate::presence::directory(&d2, q.as_deref()))
            .await
        {
            Ok(v) => ipc::Response::ok(id, this_computers_own_kept(v)),
            Err(_) => ipc::Response::err(id, ipc::INTERNAL),
        }
    } else {
        if let Err(code) = declare(d, &frame) {
            return refused(code);
        }
        crate::app_send::op(id, op, &frame, d).await
    };
    if r.ok {
        json!({"id": id, "ok": true, "result": r.body})
    } else {
        let mut e = r.body;
        e["id"] = id.into();
        e["ok"] = false.into();
        e
    }
}

/// `directory`'s answer without what is only of use on this computer, and without what is kept
/// from teammates (its folder is hidden): neither leaves it.
fn this_computers_own_kept(mut v: Value) -> Value {
    if let Some(rows) = v["sessions"].as_array_mut() {
        rows.retain(|r| r["hidden"] != true);
    }
    for r in v["sessions"].as_array_mut().into_iter().flatten() {
        if let Some(o) = r.as_object_mut() {
            for f in ["resume", "pid", "birth_micros", "entrypoint", "kind", "job"] {
                o.remove(f);
            }
        }
    }
    v
}

/// An app op's session `<app>-<person>` (titled `title`) declared here, for the link's person
/// only. `Err` with the refusal code otherwise.
fn declare(d: &Daemon, frame: &Value) -> Result<(), &'static str> {
    let (Some(app), Some(person)) = (frame["app"].as_str(), frame["person"].as_str()) else {
        return Err(ipc::BAD_REQUEST);
    };
    let name = format!("{app}-{person}");
    let title = frame["title"]
        .as_str()
        .filter(|t| t.chars().all(crate::label::printable))
        .map(String::from);
    if !sessions::valid_name(&name) || !sessions::valid_name(app) {
        return Err(ipc::BAD_REQUEST);
    }
    let r = Remote {
        session_id: format!("{}/{name}", d.identity.short),
        agent: app.into(),
        repo: None,
        branch: None,
        folder: String::new(),
        status: "idle".into(),
        doing: None,
        tier: sessions::Tier::Pull.as_str().into(),
        title,
        free: None,
        waiting_for: None,
    }
    .clamp();
    if !r.valid(&d.identity.short) {
        return Err(ipc::BAD_REQUEST);
    }
    let changed = {
        let mut s = d.link.lock().unwrap();
        if s.person.as_deref() != Some(person) {
            return Err("not_your_person");
        }
        let before = s.apps.iter().position(|a| a.session_id == r.session_id);
        let same = before.is_some_and(|i| s.apps[i] == r);
        if !same {
            if let Some(i) = before {
                s.apps.remove(i);
            }
            s.apps.push(r);
            if s.apps.len() > MAX_APPS {
                s.apps.remove(0);
            }
        }
        !same
    };
    if changed {
        d.presence.notify_one();
    }
    Ok(())
}
