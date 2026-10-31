//! A hosted assistant acting as a person (ChatGPT, Claude.ai, Grok… through Lightbulb's hosted
//! MCP, signed in as that person) sends through the workspace member, from the app session
//! `{app}-{person}` the bridge declared for it (`cloud.sessions.set`, titled `"<App> via
//! <First name>"`): an ordinary send of the member's, which its recipient reads as that title
//! (`label`). `person` is the opaque key the bridge passes. No computer is asked.
//!
//! `app.send {app, person, to, text, request_id, reply_to?, kind?, done_when?, deadline_s?, ..}` (a
//! send may be an assignment, a result or a cancel like a session's) -> the receipt with the message's
//! `id` (a `request_id` sent before answers its first receipt and sends nothing);
//! `app.inbox {app, person}` -> `{records}`, taken and read; with `peek: true` they are left
//! as they are (delivered-unread, then waiting) until `app.ack {app, person, ids}` -> `{acked}`
//! takes and reads those of them that are this session's (at most 100 ids), so an answer lost on its way to the
//! app loses no mail (it is handed out again); `app.unread {app, person}` -> `{unread}`, what
//! waits, taking nothing; `app.receipt {app, person, request_id}` -> the receipt. A pair not declared is `refused`, reason `not_declared` (the
//! bridge declares it and asks once more). An instance's ops, and a signed-in computer's over
//! its route link (`link`), for its own person's app sessions.

use crate::daemon::ipc::{self, Response, BAD_REQUEST, INTERNAL, NOT_FOUND};
use crate::daemon::Daemon;
use crate::mail;
use crate::sessions::{self, Key};
use serde_json::{json, Value};
use std::sync::Arc;

/// The most ids one `app.ack` takes: it holds the store's lock for the list.
const MAX_ACK: usize = 100;

pub(crate) async fn op(id: u64, op: &str, body: &Value, d: &Arc<Daemon>) -> Response {
    let (Some(app), Some(person)) = (body["app"].as_str(), body["person"].as_str()) else {
        return Response::err(id, BAD_REQUEST);
    };
    let name = format!("{app}-{person}");
    if !sessions::valid_name(&name) {
        return Response::err(id, BAD_REQUEST);
    }
    let from = Key(format!("{}/{name}", d.identity.short));
    // An instance's declared sessions; a computer's, the ones its route link declared for its
    // own signed-in person (`link`).
    let declared = match &d.cloud {
        Some(cloud) => cloud.declares(&from.0),
        None => crate::link::declares(d, &name, person),
    };
    if !declared {
        return Response::err_with(id, "refused", json!({ "reason": "not_declared" }));
    }
    // A send's receipt waits for the recipient's word (`ipc::finish`), as `send {wait}` does.
    let (receipt, wait) = match op {
        "app.send" => {
            let mut req = body.clone();
            req["from"] = from.0.clone().into();
            // On a computer the app is no local session of its person's (`ipc::send_as`).
            match ipc::send_as(id, &req, d, d.cloud.is_none()) {
                Ok((_, r)) => (r, true),
                Err(e) => return e,
            }
        }
        "app.inbox" if body["peek"] == true => {
            return match peek(d, &from) {
                Ok(records) => Response::ok(id, json!({ "records": records })),
                Err(_) => Response::err(id, INTERNAL),
            }
        }
        "app.ack" => {
            let Some(ids) = body["ids"]
                .as_array()
                .filter(|a| a.len() <= MAX_ACK)
                .and_then(|a| a.iter().map(Value::as_str).collect::<Option<Vec<_>>>())
            else {
                return Response::err(id, BAD_REQUEST);
            };
            return match ack(d, &from, &ids) {
                Ok(n) => Response::ok(id, json!({ "acked": n })),
                Err(_) => Response::err(id, INTERNAL),
            };
        }
        "app.unread" => {
            return match mail::unread(&d.store.lock().unwrap(), &from, ipc::now()) {
                Ok(n) => Response::ok(id, json!({ "unread": n })),
                Err(_) => Response::err(id, INTERNAL),
            }
        }
        "app.inbox" => {
            return match crate::embed::take_read(d, &from) {
                Ok(records) => Response::ok(id, json!({ "records": records })),
                Err(_) => Response::err(id, INTERNAL),
            }
        }
        _ => {
            let Some(rid) = body["request_id"].as_str() else {
                return Response::err(id, BAD_REQUEST);
            };
            match mail::receipt(&d.store.lock().unwrap(), &from, rid) {
                Ok(Some(r)) => (r, false),
                Ok(None) => return Response::err(id, NOT_FOUND),
                Err(_) => return Response::err(id, INTERNAL),
            }
        }
    };
    let mut r = ipc::finish(d, &from, receipt, wait).await;
    let store = d.store.lock().unwrap();
    let q = "SELECT id FROM messages WHERE from_key = ?1 AND request_id = ?2";
    let rid = r["request_id"].as_str().unwrap_or_default();
    let msg: Option<String> = store.conn().query_row(q, [&from.0, rid], |x| x.get(0)).ok();
    r["id"] = msg.into();
    Response::ok(id, r)
}

/// What `key` has not read: delivered-unread, then waiting, each oldest first; nothing changes.
fn peek(d: &Daemon, key: &Key) -> crate::Result<Vec<Value>> {
    let listed = {
        let store = d.store.lock().unwrap();
        let now = ipc::now();
        let mut m: Vec<_> = mail::delivered_unread(&store, &d.identity.short, now)?
            .into_iter()
            .filter(|m| m.to_key == key.0)
            .collect();
        m.extend(mail::waiting(&store, key, now)?);
        m
    };
    // After the store lock: `records` takes the team's.
    Ok(ipc::records(d, &listed)
        .iter()
        .map(ipc::inbox_record)
        .collect())
}

/// Takes and reads each of `ids` that is mail to `key`; how many were. Another session's id,
/// or one unknown, is skipped; one acked before counts again and changes nothing.
fn ack(d: &Daemon, key: &Key, ids: &[&str]) -> crate::Result<usize> {
    let n = {
        let store = d.store.lock().unwrap();
        let mut n = 0;
        for m in ids {
            mail::take_one(&store, key, m, ipc::now())?;
            if store.message(m)?.is_some_and(|x| x.to_key == key.0) {
                mail::mark_read(&store, m)?;
                n += 1;
            }
        }
        mail::forget(&store)?;
        n
    };
    // Their `delivered` and `read` go to the senders now (`Net::flush_loop`).
    d.peers.outbox.notify_one();
    Ok(n)
}
