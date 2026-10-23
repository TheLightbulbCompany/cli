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
//! or `{app, error}`. An inbox answer holds the newest records (of the newest `INBOX_ROWS`)
//! within `INBOX_BUDGET` bytes; they are marked read only once the answer was written to the
//! stream, the rest stay unread and `more` says so. Logs carry ids, apps and shorts, never text.
//!
//! Fallback ceiling: a computer that sent but whose answer is lost is asked once more
//! (`app_receipt_query`, `PROBE`) before the next computer sends; if that probe is lost too,
//! the next computer sends the same text again, from its own `<app>-app` session (another
//! sender, so `request_id` dedupe cannot catch it). ponytail: two Macs, both silent past
//! 13 s, is the window; add an id across the person's computers if it shows up.

use crate::daemon::ipc::{self, Response, BAD_REQUEST, INTERNAL, NOT_FOUND, UNSUPPORTED};
use crate::daemon::net_ops::NOT_MEMBER;
use crate::daemon::Daemon;
use crate::net::frames::{self, Frame};
use crate::net::short;
use crate::sessions::Key;
use crate::{mail, policy};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::sync::Arc;
use std::time::Duration;

/// How long the member waits for one computer before it asks the next.
const WAIT: Duration = Duration::from_secs(10);
/// The probe of a computer that did not answer an `app_send`: did it send?
const PROBE: Duration = Duration::from_secs(3);
pub const NO_COMPUTER_ONLINE: &str = "no_computer_online";
const NO_COMPUTER_TEXT: &str = "none of your computers is online right now";
/// Each request kind, and the kind that answers it.
const KINDS: [(&str, &str); 3] = [
    ("app_send", "app_receipt"),
    ("app_inbox", "app_inbox_result"),
    ("app_receipt_query", "app_receipt"),
];
/// An inbox answer's records, at most (escaped once more into the frame: well under
/// `frames::MAX_FRAME`), and the newest waiting rows rendered for it.
const INBOX_BUDGET: usize = 256 * 1024;
const INBOX_ROWS: usize = 200;
/// The error codes a computer's answer may carry; any other reads `internal`.
const ERRORS: [&str; 4] = [BAD_REQUEST, NOT_FOUND, NOT_MEMBER, INTERNAL];

/// A request out (`Cloud::apps`): the computer asked, the answer kind expected, and where the
/// answer goes. Kept apart from admission's `Pending::Forwarded` (another key and check).
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

/// The answer kind of request `kind`, when it is one.
fn answer_kind(kind: &str) -> Option<&'static str> {
    KINDS.iter().find(|(r, _)| *r == kind).map(|(_, a)| *a)
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

/// `app_receipt_query`'s body.
fn receipt_query(app: &str, request_id: &Value, person: &str) -> Value {
    json!({"app": app, "request_id": request_id, "for_person": person})
}

/// Member `key`'s `app_*` envelope, its `from` one of the member's own addresses: on a
/// computer a request, served when it comes from this computer's own workspace's member (in
/// force) for this computer's person (`policy::my_workspace`); on an instance an answer of the
/// kind asked, from the computer asked. Anything else is dropped unanswered (counted under
/// `status.dropped.app`).
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
    let (own, trusted) = {
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        let own = r
            .members
            .get(key)
            .is_some_and(|p| frames::own(&p.short, &from));
        let person = body["for_person"].as_str().unwrap_or("\0");
        (
            own,
            policy::my_workspace(r, &d.identity.key, key, Some(person)),
        )
    };
    let served = own
        && match (&d.cloud, answer_kind(&kind)) {
            (Some(cloud), None) => {
                let mut apps = cloud.apps.lock().unwrap();
                let r = reply_to.unwrap_or_default();
                match apps.remove(&r) {
                    Some((asked, want, tx)) if asked == key && want == kind => {
                        tx.send(body).is_ok()
                    }
                    Some(w) => {
                        apps.insert(r, w);
                        false
                    }
                    None => false,
                }
            }
            (None, Some(answer)) if trusted => {
                let (d, key, id) = (d.clone(), key.to_string(), id.clone());
                tokio::spawn(serve(d, key, id, from, kind, answer, body));
                true
            }
            _ => false,
        };
    if !served {
        d.peers.dropped("app");
        // Not `id` or `kind`: the peer chose them.
        eprintln!("app: dropped an app envelope from {}", short(key));
    }
}

/// One trusted request `kind` (answered by `answer`), `id` from `from`: answered on the
/// connection it came over.
async fn serve(
    d: Arc<Daemon>,
    key: String,
    id: String,
    from: String,
    kind: String,
    answer: &'static str,
    body: Value,
) {
    let app = body["app"].as_str().unwrap_or_default();
    let mine = Key(format!("{}/{app}-app", d.identity.short));
    let mut out = json!({ "app": app });
    if kind != "app_inbox" {
        out["request_id"] = body["request_id"].clone();
    }
    // Held while an inbox answer is out: a second one would read the same rows.
    let _held = match kind.as_str() {
        "app_inbox" => Some(d.key_lock(&mine.0).lock_owned().await),
        _ => None,
    };
    let mut written = vec![];
    let out = match kind.as_str() {
        _ if !valid_slug(app) => error(out, BAD_REQUEST),
        "app_inbox" => match inbox(&d, &mine) {
            Ok((records, more, ids)) => {
                written = ids;
                json!({ "app": app, "records": records, "more": more })
            }
            Err(_) => error(out, INTERNAL),
        },
        "app_send" => {
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
        }
        _ => {
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
        }
    };
    let f = envelope(answer, mine.0.clone(), from, Some(id), out);
    let Some(done) = d.peers.send_written(&key, f) else {
        return eprintln!("app: no connection to {} for the answer", short(&key));
    };
    if written.is_empty() || tokio::time::timeout(WAIT, done).await.is_err() {
        return;
    }
    let store = d.store.lock().unwrap();
    for m in &written {
        let taken = mail::take_one(&store, &mine, m, ipc::now());
        if let Err(e) = taken.and_then(|_| mail::mark_read(&store, m)) {
            eprintln!("app: marking {m} read: {e}");
        }
    }
}

/// `mine`'s newest waiting records (of the newest `INBOX_ROWS`) within `INBOX_BUDGET`, newest
/// first: the records, whether others are left, and their message ids (to mark read once
/// written).
fn inbox(d: &Daemon, mine: &Key) -> crate::Result<(Vec<Value>, bool, Vec<String>)> {
    let waiting = mail::waiting(&d.store.lock().unwrap(), mine, ipc::now())?;
    let mut more = waiting.len() > INBOX_ROWS;
    let newest: Vec<_> = waiting.into_iter().rev().take(INBOX_ROWS).collect();
    let (mut records, mut ids, mut size) = (vec![], vec![], 0);
    for (m, v) in newest.iter().zip(ipc::records(d, &newest)) {
        let r = ipc::inbox_record(&v);
        let n = r.to_string().len();
        // ponytail: a record alone over the budget (control characters escape six times) is
        // never handed over this way; it stays in the mailbox for its session, unflagged.
        if n > INBOX_BUDGET {
            continue;
        }
        if size + n > INBOX_BUDGET {
            more = true;
            break;
        }
        size += n;
        records.push(r);
        ids.push(m.id.clone());
    }
    Ok((records, more, ids))
}

fn error(mut out: Value, code: &str) -> Value {
    out["error"] = code.into();
    out
}

/// `out` with receipt `r`'s fields (as `ipc::finish` gives it) and the message's `id`.
fn receipt(mut out: Value, r: &Value, id: Option<String>) -> Value {
    for k in ipc::RECEIPT_FIELDS {
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
/// reason, limit, thread, tier, computer}`, asked of one computer at a time, each given
/// `WAIT`, a silent one probed (`PROBE`) before the next sends. `app.inbox {for_person, app}`
/// -> `{records, more}` (every online computer's, by `sent_at`; one that answers an error is
/// skipped like a silent one) and `app.receipt {for_person, app, request_id}` -> the receipt
/// (the first computer that knows it) ask them all at once. The person's online computers in
/// this workspace, newest presence first; none -> `no_computer_online`, also for a
/// `for_person` outside the workspace (no membership oracle). An instance's ops only.
pub(crate) async fn op(id: u64, op: &str, body: &Value, d: &Arc<Daemon>) -> Response {
    if d.cloud.is_none() {
        return Response::err(id, UNSUPPORTED);
    }
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
            receipt_query(app, &body["request_id"], person),
        ),
        "app.inbox" => ("app_inbox", json!({"app": app, "for_person": person})),
        _ => return Response::err(id, BAD_REQUEST),
    };
    let request = |short: &str, kind: &str, req: &Value| {
        let (from, to) = (
            format!("{}/{app}-app", d.identity.short),
            format!("{short}/{app}-app"),
        );
        envelope(kind, from, to, None, req.clone())
    };
    let none = || {
        let text = json!({ "message": NO_COMPUTER_TEXT });
        Response::err_with(id, NO_COMPUTER_ONLINE, text)
    };
    let computers = online_computers(d, person);
    if kind == "app_send" {
        for (key, short, computer) in computers {
            let mut got = ask(d, &key, request(&short, kind, &req), WAIT).await;
            if got.is_none() {
                // It may have sent and lost the answer: only `not_found` lets the next send.
                let probe = receipt_query(app, &req["request_id"], person);
                let f = request(&short, "app_receipt_query", &probe);
                got = ask(d, &key, f, PROBE)
                    .await
                    .filter(|v| v["error"] != NOT_FOUND);
            }
            if let Some(v) = got {
                return answer(id, v, computer);
            }
        }
        return none();
    }
    // Reads: every computer at once.
    let mut asks = tokio::task::JoinSet::new();
    for (key, short, computer) in computers {
        let (d, f) = (d.clone(), request(&short, kind, &req));
        asks.spawn(async move { (ask(&d, &key, f, WAIT).await, computer) });
    }
    let (mut records, mut more, mut answered) = (vec![], false, false);
    while let Some(Ok((got, computer))) = asks.join_next().await {
        let Some(v) = got else {
            continue;
        };
        let err = v["error"].as_str();
        if kind == "app_inbox" {
            if err.is_none() {
                answered = true;
                records.extend(v["records"].as_array().cloned().unwrap_or_default());
                more |= v["more"] == true;
            }
        } else if err != Some(NOT_FOUND) {
            // The rest end within `WAIT` and clear their own waiters.
            asks.detach_all();
            return answer(id, v, computer);
        } else {
            answered = true;
        }
    }
    match (answered, kind) {
        (false, _) => none(),
        (true, "app_inbox") => {
            records.sort_by_key(|r| r["sent_at"].as_i64());
            Response::ok(id, json!({ "records": records, "more": more }))
        }
        (true, _) => Response::err(id, NOT_FOUND),
    }
}

/// A computer's receipt answer `v` as the op answers it: its error (one of `ERRORS`, else
/// `internal`), or the receipt with the `computer` that sent it.
fn answer(id: u64, mut v: Value, computer: String) -> Response {
    if let Some(e) = v["error"].as_str() {
        let e = if ERRORS.contains(&e) { e } else { INTERNAL };
        return Response::err_with(id, e, json!({ "reason": v["reason"] }));
    }
    for k in ["app", "id"] {
        v.as_object_mut().map(|o| o.remove(k));
    }
    v["computer"] = computer.into();
    Response::ok(id, v)
}

/// `(key, short, computer)` of `person`'s computers in this instance's workspace that are
/// online, newest presence first.
fn online_computers(d: &Daemon, person: &str) -> Vec<(String, String, String)> {
    let mut out: Vec<_> = {
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        let ws = r
            .members
            .get(&d.identity.key)
            .and_then(|m| m.workspace.as_ref());
        (r.computers_of(person).into_iter())
            .filter(|m| ws.is_some() && m.workspace.as_ref() == ws && d.peers.is_online(&m.key))
            .map(|m| (m.key.clone(), m.short.clone(), m.computer.clone()))
            .collect()
    };
    // After `team`: the presences' own lock.
    let latest = d.latest.lock().unwrap();
    out.sort_by_key(|(k, ..)| Reverse(latest.get(k).map_or(0, |l| l.seq)));
    out
}

/// Sends request `f` to computer `key` and waits up to `wait` for its answer.
async fn ask(d: &Daemon, key: &str, f: Frame, wait: Duration) -> Option<Value> {
    let (cloud, Frame::Envelope { id, kind, .. }) = (d.cloud.as_ref()?, &f) else {
        return None;
    };
    let (id, kind) = (id.clone(), kind.clone());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let waiter = (key.to_string(), answer_kind(&kind)?, tx);
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
