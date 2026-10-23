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
//! tier}` or `{app, request_id, error, reason?}`, and `app_inbox_result {app, records, more}`
//! or `{app, error}`. An inbox answer holds the newest records within `INBOX_BUDGET` bytes;
//! they are marked read only once the answer was written to the stream, the rest stay unread
//! and `more` says so. Logs carry ids, apps and shorts, never text.
//!
//! Fallback ceiling: a computer that sent but whose answer is lost is asked once more
//! (`app_receipt_query`, `PROBE`) before the next computer sends; if that probe is lost too,
//! the next computer sends the same text again, from its own `<app>-app` session (another
//! sender, so `request_id` dedupe cannot catch it). ponytail: two Macs, both silent past
//! 13 s, is the window; add an id across the person's computers if it shows up.

use crate::daemon::ipc::{self, Response, BAD_REQUEST, INTERNAL, NOT_FOUND, UNSUPPORTED};
use crate::daemon::net_ops::NOT_MEMBER;
use crate::daemon::{Cloud, Daemon};
use crate::mail;
use crate::net::frames::{self, Frame, MAX_FRAME};
use crate::net::short;
use crate::roster::MemberKind;
use crate::sessions::Key;
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
/// The probe of a computer that did not answer an `app_send`: did it send?
const PROBE: Duration = Duration::from_secs(3);
/// An inbox answer's records, at most (well under `frames::MAX_FRAME`).
const INBOX_BUDGET: usize = 256 * 1024;
/// The error codes a computer's answer may carry; any other reads `internal`.
const ERRORS: [&str; 4] = [BAD_REQUEST, NOT_FOUND, NOT_MEMBER, INTERNAL];

/// A request out (`Cloud::apps`): the computer asked, the answer kind expected, and where the
/// answer goes.
pub type Waiter = (String, &'static str, tokio::sync::oneshot::Sender<Value>);

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

/// The answer kind to request `kind`.
fn answer_kind(kind: &str) -> &'static str {
    if kind == "app_inbox" {
        "app_inbox_result"
    } else {
        "app_receipt"
    }
}

/// Member `key`'s `app_*` envelope: on a computer a request, served when it comes from this
/// computer's own workspace's member (in force) for this computer's person; on an instance an
/// answer of the kind asked, from the computer asked. Its `from` must be one of the member's
/// own addresses. Anything else is dropped unanswered (counted under `status.dropped.app`).
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
    let own = {
        let team = d.team.lock().unwrap();
        let peer = team.roster.members.get(key);
        peer.is_some_and(|p| frames::own(&p.short, &from))
    };
    let served = own
        && match &d.cloud {
            Some(cloud) => answered(cloud, key, &kind, reply_to.as_deref(), body),
            None if REQUESTS.contains(&kind.as_str()) && trusted(d, key, &body) => {
                let (d, key, id) = (d.clone(), key.to_string(), id.clone());
                tokio::spawn(serve(d, key, id, from, kind, body));
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

/// The waiting request `reply_to` (asked of `key`, answered by `kind`) takes `body`.
fn answered(cloud: &Cloud, key: &str, kind: &str, reply_to: Option<&str>, body: Value) -> bool {
    let Some(r) = reply_to else {
        return false;
    };
    let mut apps = cloud.apps.lock().unwrap();
    if apps
        .get(r)
        .is_none_or(|(asked, want, _)| asked != key || *want != kind)
    {
        return false;
    }
    apps.remove(r)
        .is_some_and(|(_, _, tx)| tx.send(body).is_ok())
}

/// One trusted request, `id` from `from`: answered on the connection it came over.
async fn serve(d: Arc<Daemon>, key: String, id: String, from: String, kind: String, body: Value) {
    let app = body["app"].as_str().unwrap_or_default();
    let mine = Key(format!("{}/{app}-app", d.identity.short));
    let mut out = json!({ "app": app });
    if kind != "app_inbox" {
        out["request_id"] = body["request_id"].clone();
    }
    let reply =
        |answer: Value| envelope(answer_kind(&kind), mine.0.clone(), from, Some(id), answer);
    if kind == "app_inbox" && valid_slug(app) {
        return inbox(&d, &key, &mine, out, reply).await;
    }
    let answer = if !valid_slug(app) {
        error(out, BAD_REQUEST)
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
    if d.peers.send(&key, reply(answer)).is_err() {
        eprintln!("app: no connection to {} for the answer", short(&key));
    }
}

/// `app_inbox`: the newest records of `mine`'s mailbox within `INBOX_BUDGET`, each taken and
/// marked read once the answer was written; `more` when any are left.
async fn inbox(
    d: &Daemon,
    key: &str,
    mine: &Key,
    mut out: Value,
    reply: impl FnOnce(Value) -> Frame,
) {
    let waiting = mail::waiting(&d.store.lock().unwrap(), mine, ipc::now());
    let Ok(mut waiting) = waiting else {
        let _ = d.peers.send(key, reply(error(out, INTERNAL)));
        return;
    };
    waiting.reverse();
    let all = ipc::records(d, &waiting);
    let mut records = vec![];
    let mut size = 0;
    for v in &all {
        let r: serde_json::Map<String, Value> = (ipc::INBOX_RECORD.iter())
            .map(|k| (k.to_string(), v[*k].clone()))
            .collect();
        size += Value::Object(r.clone()).to_string().len();
        // The newest always goes, so one large record cannot stall the rest behind it.
        if size > INBOX_BUDGET && !records.is_empty() {
            break;
        }
        records.push(Value::Object(r));
    }
    let mut n = records.len();
    out["records"] = records.into();
    out["more"] = (n < all.len()).into();
    let mut f = reply(out.clone());
    // ponytail: a record whose JSON alone passes the frame cap (control characters escape six
    // times) is never handed over this way; it stays in the mailbox for its session.
    if serde_json::to_vec(&f).map_or(true, |b| b.len() > MAX_FRAME) {
        n = 0;
        out["records"] = json!([]);
        out["more"] = true.into();
        f = reply_again(&f, &out);
    }
    let Some(rx) = d.peers.send_written(key, f.clone()) else {
        return;
    };
    let Frame::Envelope { id, .. } = &f else {
        return;
    };
    if tokio::time::timeout(WAIT, rx).await.is_err() {
        d.peers.unwritten(id);
        return;
    }
    let store = d.store.lock().unwrap();
    for m in &waiting[..n] {
        let taken = mail::take_one(&store, mine, &m.id, ipc::now());
        if let Err(e) = taken.and_then(|_| mail::mark_read(&store, &m.id)) {
            eprintln!("app: marking {} read: {e}", m.id);
        }
    }
}

/// Envelope `f` with body `out` instead.
fn reply_again(f: &Frame, out: &Value) -> Frame {
    let mut f = f.clone();
    if let Frame::Envelope { body, .. } = &mut f {
        *body = out.to_string();
    }
    f
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
/// reason, limit, thread, tier, computer}`; `app.inbox {for_person, app}` -> `{records, more}`
/// (every online computer's, by `sent_at`; one that answers an error is skipped like a silent
/// one); `app.receipt {for_person, app, request_id}` -> the receipt (the first computer that
/// knows it). Asked of the person's online computers in this workspace, newest presence first,
/// each given `WAIT`; a computer silent on an `app_send` is probed (`PROBE`) before the next
/// one sends. None -> `no_computer_online`, also for a `for_person` outside the workspace (no
/// membership oracle). An instance's ops only.
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
    let (mut records, mut more) = (vec![], false);
    let mut asked_any = false;
    for (key, short, computer) in online_computers(d, person) {
        let (from, to) = (
            format!("{}/{app}-app", d.identity.short),
            format!("{short}/{app}-app"),
        );
        let ask_as = |kind, req: &Value, wait| {
            let f = envelope(kind, from.clone(), to.clone(), None, req.clone());
            ask(d, cloud, &key, f, wait)
        };
        let mut got = ask_as(kind, &req, WAIT).await;
        if got.is_none() && kind == "app_send" {
            // It may have sent and lost the answer: only a `not_found` lets the next one send.
            let probe = json!({"app": app, "request_id": req["request_id"], "for_person": person});
            got = ask_as("app_receipt_query", &probe, PROBE)
                .await
                .filter(|v| v["error"] != NOT_FOUND);
        }
        let Some(mut v) = got else {
            continue;
        };
        let err = v["error"]
            .as_str()
            .map(|e| *ERRORS.iter().find(|k| **k == e).unwrap_or(&INTERNAL));
        if op == "app.inbox" {
            if err.is_none() {
                asked_any = true;
                records.extend(v["records"].as_array().cloned().unwrap_or_default());
                more |= v["more"] == true;
            }
            continue;
        }
        asked_any = true;
        if op == "app.receipt" && err == Some(NOT_FOUND) {
            continue;
        }
        if let Some(e) = err {
            return Response::err_with(id, e, json!({ "reason": v["reason"] }));
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
            Response::ok(id, json!({ "records": records, "more": more }))
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

/// Sends request `f` to computer `key` and waits up to `wait` for its answer.
async fn ask(d: &Daemon, cloud: &Cloud, key: &str, f: Frame, wait: Duration) -> Option<Value> {
    let Frame::Envelope { id, kind, .. } = &f else {
        return None;
    };
    let (id, kind) = (id.clone(), kind.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let waiter = (key.to_string(), answer_kind(&kind), tx);
    cloud.apps.lock().unwrap().insert(id.clone(), waiter);
    let got = match d.peers.send(key, f) {
        Ok(()) => tokio::time::timeout(wait, rx)
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
