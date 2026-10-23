//! A hosted app acting as a person (Claude.ai, ChatGPT, Muse through Lightbulb's hosted MCP)
//! never sends with the workspace member's authority (ruling §5): the member carries the call
//! to one of that person's online computers, which sends it as its own session
//! `<short>/<app>-app`, an ordinary agent send, and answers the same way.
//!
//! The `app_*` envelopes (body JSON) ride the live connection only, `hops` 0: never a mailbox,
//! a hold, the hop count, an `ack` or the outbox. Requests, member to computer: `app_send
//! {app, to, text, request_id, reply_to?, for_person}`, `app_inbox {app, for_person}`,
//! `app_receipt_query {app, request_id, for_person}`. Answers, computer to member, `reply_to`
//! the request's envelope id: `app_receipt {app, request_id, id, state, reason, limit, thread,
//! tier}` or `{app, request_id, error, reason?}`, and `app_inbox_result {app, records}` or `{app,
//! error}`. Logs carry ids, apps and shorts, never text.

use crate::daemon::ipc::{self, Response, BAD_REQUEST, INTERNAL, NOT_FOUND, UNSUPPORTED};
use crate::daemon::{Cloud, Daemon};
use crate::net::frames::Frame;
use crate::net::short;
use crate::roster::MemberKind;
use crate::sessions::Key;
use crate::{embed, mail};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How long the member waits for one computer before it asks the next.
const WAIT: Duration = Duration::from_secs(10);
pub const NO_COMPUTER_ONLINE: &str = "no_computer_online";
const NO_COMPUTER_TEXT: &str = "none of your computers is online right now";
const REQUESTS: [&str; 3] = ["app_send", "app_inbox", "app_receipt_query"];
const ANSWERS: [&str; 2] = ["app_receipt", "app_inbox_result"];

/// Whether an envelope of `kind` is this module's (every `app_*`, known or not).
pub fn is_app(kind: &str) -> bool {
    kind.starts_with("app_")
}

/// An app slug: `[a-z0-9-]{1,24}`.
pub fn valid_slug(app: &str) -> bool {
    (1..=24).contains(&app.len())
        && app
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The app whose session name is `name` (`claude` for `claude-app`).
pub fn app_of(name: &str) -> Option<&str> {
    name.strip_suffix("-app").filter(|a| valid_slug(a))
}

/// `app` as a label says it: `Claude`, `ChatGPT`, `Muse`, else the slug.
pub fn app_name(app: &str) -> &str {
    match app {
        "claude" => "Claude",
        "chatgpt" => "ChatGPT",
        "muse" => "Muse",
        a => a,
    }
}

fn envelope(kind: &str, from: String, to: String, reply_to: Option<String>, body: Value) -> Frame {
    let id = ulid::Ulid::generate().to_string();
    Frame::Envelope {
        request_id: id.clone(),
        id,
        kind: kind.into(),
        from,
        to,
        reply_to,
        thread: None,
        hops: 0,
        body: body.to_string(),
        files: vec![],
        sent_at: ipc::now() as i64,
    }
}

/// Member `key`'s `app_*` envelope: on a computer a request, served when it comes from this
/// computer's own workspace's member for this computer's person; on an instance an answer to
/// one of its requests, from the computer asked. Anything else is dropped unanswered (counted
/// under `status.dropped.app`).
pub(crate) fn on_envelope(d: &Arc<Daemon>, key: &str, env: Frame) {
    let Frame::Envelope {
        id,
        kind,
        from,
        reply_to,
        body,
        ..
    } = env
    else {
        return;
    };
    let body: Value = serde_json::from_str(&body).unwrap_or_default();
    let served = match &d.cloud {
        Some(cloud) => answered(cloud, key, &kind, reply_to.as_deref(), body),
        None if REQUESTS.contains(&kind.as_str()) && trusted(d, key, &body) => {
            tokio::spawn(serve(
                d.clone(),
                key.to_string(),
                id.clone(),
                from,
                kind,
                body,
            ));
            true
        }
        None => false,
    };
    if !served {
        d.peers.dropped("app");
        // Not `id` or `kind`: the peer chose them.
        eprintln!("app: dropped an app envelope from {}", short(key));
    }
}

/// Whether `key` is this computer's own workspace's member and `body.for_person` its person.
fn trusted(d: &Daemon, key: &str, body: &Value) -> bool {
    let team = d.team.lock().unwrap();
    let Some(me) = team.roster.members.get(&d.identity.key) else {
        return false;
    };
    team.roster
        .my_workspace_member(me)
        .is_some_and(|w| w.key == key)
        && body["for_person"].as_str() == Some(me.person_id.as_str())
}

/// The waiting request `reply_to` (asked of `key`) takes `body`.
fn answered(cloud: &Cloud, key: &str, kind: &str, reply_to: Option<&str>, body: Value) -> bool {
    let Some(r) = reply_to.filter(|_| ANSWERS.contains(&kind)) else {
        return false;
    };
    let mut apps = cloud.apps.lock().unwrap();
    if apps.get(r).is_none_or(|(asked, _)| asked != key) {
        return false;
    }
    apps.remove(r).is_some_and(|(_, tx)| tx.send(body).is_ok())
}

/// One trusted request, `id` from `from`: answered on the connection it came over.
async fn serve(d: Arc<Daemon>, key: String, id: String, from: String, kind: String, body: Value) {
    let app = body["app"].as_str().unwrap_or_default();
    let mine = Key(format!("{}/{app}-app", d.identity.short));
    let mut out = json!({ "app": app });
    if kind != "app_inbox" {
        out["request_id"] = body["request_id"].clone();
    }
    let answer = if !valid_slug(app) {
        error(out, BAD_REQUEST)
    } else if kind == "app_inbox" {
        match embed::take_read(&d, &mine) {
            Ok(records) => json!({ "app": app, "records": records }),
            Err(_) => error(out, INTERNAL),
        }
    } else if kind == "app_send" {
        let req = json!({"from": mine.0, "to": body["to"], "text": body["text"],
            "request_id": body["request_id"], "reply_to": body["reply_to"]});
        match ipc::send(0, &req, &d) {
            Ok((from, r)) => {
                let to = r.to.clone();
                let r = ipc::finish(&d, &from, r, true).await;
                let msg = message_id(&d, &from, &r);
                if let (Some(id), Some(to)) = (&msg, to) {
                    eprintln!("app: {app} sent {id}");
                    let e = json!({"event": "app_sent", "id": id, "app": app, "to": to});
                    let _ = d.events.send(e);
                }
                receipt(out, &r, msg)
            }
            Err(e) => {
                out["reason"] = e.body["reason"].clone();
                error(out, e.body["error"].as_str().unwrap_or(INTERNAL))
            }
        }
    } else {
        let rid = body["request_id"].as_str().unwrap_or_default();
        let got = mail::receipt(&d.store.lock().unwrap(), &mine, rid);
        match got {
            Ok(Some(r)) => {
                let r = ipc::finish(&d, &mine, r, false).await;
                let msg = message_id(&d, &mine, &r);
                receipt(out, &r, msg)
            }
            Ok(None) => error(out, NOT_FOUND),
            Err(_) => error(out, INTERNAL),
        }
    };
    let kind = if kind == "app_inbox" {
        "app_inbox_result"
    } else {
        "app_receipt"
    };
    let f = envelope(kind, mine.0.clone(), from, Some(id), answer);
    if d.peers.send(&key, f).is_err() {
        eprintln!("app: no connection to {} for the answer", short(&key));
    }
}

fn error(mut out: Value, code: &str) -> Value {
    out["error"] = code.into();
    out
}

/// `out` with receipt `r`'s fields (as `ipc::finish` gives it) and the message's `id`.
fn receipt(mut out: Value, r: &Value, id: Option<String>) -> Value {
    for k in ["state", "reason", "limit", "thread", "tier"] {
        out[k] = r[k].clone();
    }
    out["id"] = id.into();
    out
}

/// The id of `from`'s message of receipt `r`, if one was sent.
fn message_id(d: &Daemon, from: &Key, r: &Value) -> Option<String> {
    let rid = r["request_id"].as_str()?;
    let store = d.store.lock().unwrap();
    let q = "SELECT id FROM messages WHERE from_key = ?1 AND request_id = ?2";
    store.conn().query_row(q, [&from.0, rid], |x| x.get(0)).ok()
}

/// `app.send {for_person, app, to, text, request_id, reply_to?}` -> `{request_id, state,
/// reason, limit, thread, tier, computer}`; `app.inbox {for_person, app}` -> `{records}` (every
/// online computer's, by `sent_at`); `app.receipt {for_person, app, request_id}` -> the receipt
/// (the first computer that knows it). Asked of the person's online computers in this
/// workspace, newest presence first, each given `WAIT`; none -> `no_computer_online`, also for
/// a `for_person` outside the workspace (no membership oracle). An instance's ops only.
pub(crate) async fn op(id: u64, op: &str, body: &Value, d: &Arc<Daemon>) -> Response {
    let Some(cloud) = &d.cloud else {
        return Response::err(id, UNSUPPORTED);
    };
    let (Some(person), Some(app)) = (
        body["for_person"].as_str(),
        body["app"].as_str().filter(|a| valid_slug(a)),
    ) else {
        return Response::err(id, BAD_REQUEST);
    };
    let str_of = |k: &str| body[k].as_str().is_some();
    let (kind, req) = match op {
        "app.send" if ["to", "text", "request_id"].iter().all(|k| str_of(k)) => (
            "app_send",
            json!({"app": app, "to": body["to"], "text": body["text"],
                "request_id": body["request_id"], "reply_to": body["reply_to"],
                "for_person": person}),
        ),
        "app.receipt" if str_of("request_id") => (
            "app_receipt_query",
            json!({"app": app, "request_id": body["request_id"], "for_person": person}),
        ),
        "app.inbox" => ("app_inbox", json!({"app": app, "for_person": person})),
        _ => return Response::err(id, BAD_REQUEST),
    };
    let mut records = vec![];
    let mut asked_any = false;
    for (key, short, computer) in online_computers(d, person) {
        let to = format!("{short}/{app}-app");
        let from = format!("{}/{app}-app", d.identity.short);
        let Some(mut v) = ask(d, cloud, &key, envelope(kind, from, to, None, req.clone())).await
        else {
            continue;
        };
        asked_any = true;
        let err = v["error"].as_str();
        if op == "app.receipt" && err == Some(NOT_FOUND) {
            continue;
        }
        if let Some(e) = err {
            return Response::err_with(id, e, json!({ "reason": v["reason"] }));
        }
        if op == "app.inbox" {
            records.extend(v["records"].as_array().cloned().unwrap_or_default());
            continue;
        }
        for k in ["app", "id"] {
            v.as_object_mut().map(|o| o.remove(k));
        }
        v["computer"] = computer.into();
        return Response::ok(id, v);
    }
    match (asked_any, op) {
        (false, _) => Response::err_with(
            id,
            NO_COMPUTER_ONLINE,
            json!({ "message": NO_COMPUTER_TEXT }),
        ),
        (true, "app.inbox") => {
            records.sort_by_key(|r| r["sent_at"].as_i64());
            Response::ok(id, json!({ "records": records }))
        }
        (true, _) => Response::err(id, NOT_FOUND),
    }
}

/// `(key, short, computer)` of `person`'s computers in this instance's workspace that are
/// online, newest presence first.
fn online_computers(d: &Daemon, person: &str) -> Vec<(String, String, String)> {
    // Before `team`: the presences' own lock.
    let seqs: HashMap<String, u64> = (d.latest.lock().unwrap().iter())
        .map(|(k, l)| (k.clone(), l.seq))
        .collect();
    let team = d.team.lock().unwrap();
    let members = &team.roster.members;
    let Some(ws) = members
        .get(&d.identity.key)
        .and_then(|m| m.workspace.clone())
    else {
        return vec![];
    };
    let mut out: Vec<_> = members
        .values()
        .filter(|m| {
            m.kind == MemberKind::Computer
                && m.person_id == person
                && m.workspace.as_deref() == Some(ws.as_str())
                && d.peers.is_online(&m.key)
        })
        .map(|m| (m.key.clone(), m.short.clone(), m.computer.clone()))
        .collect();
    out.sort_by_key(|(k, ..)| Reverse(seqs.get(k).copied().unwrap_or(0)));
    out
}

/// Sends request `f` to computer `key` and waits `WAIT` for its answer.
async fn ask(d: &Daemon, cloud: &Cloud, key: &str, f: Frame) -> Option<Value> {
    let Frame::Envelope { id, kind, .. } = &f else {
        return None;
    };
    let (id, kind) = (id.clone(), kind.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    cloud
        .apps
        .lock()
        .unwrap()
        .insert(id.clone(), (key.to_string(), tx));
    let got = match d.peers.send(key, f) {
        Ok(()) => tokio::time::timeout(WAIT, rx)
            .await
            .ok()
            .and_then(|r| r.ok()),
        Err(_) => None,
    };
    cloud.apps.lock().unwrap().remove(&id);
    if got.is_none() {
        eprintln!("app: no answer to {kind} {id} from {}", short(key));
    }
    got
}
