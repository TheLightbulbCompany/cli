//! A hosted assistant acting as a person (ChatGPT, Claude.ai, Grok… through Lightbulb's hosted
//! MCP, signed in as that person) sends through the workspace member, from the app session
//! `{app}-{person}` the bridge declared for it (`cloud.sessions.set`, titled `"<App> via
//! <First name>"`): an ordinary send of the member's, which its recipient reads as that title
//! (`label`). `person` is the opaque key the bridge passes. No computer is asked.
//!
//! `app.send {app, person, to, text, request_id, reply_to?}` -> the receipt with the message's
//! `id` (a `request_id` sent before answers its first receipt and sends nothing);
//! `app.inbox {app, person}` -> `{records}`, taken and read; `app.receipt {app, person,
//! request_id}` -> the receipt. A pair not declared is `refused`, reason `not_declared` (the
//! bridge declares it and asks once more). An instance's ops only.

use crate::daemon::ipc::{self, Response, BAD_REQUEST, INTERNAL, NOT_FOUND, UNSUPPORTED};
use crate::daemon::Daemon;
use crate::mail;
use crate::sessions::{self, Key};
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) async fn op(id: u64, op: &str, body: &Value, d: &Arc<Daemon>) -> Response {
    let Some(cloud) = &d.cloud else {
        return Response::err(id, UNSUPPORTED);
    };
    let (Some(app), Some(person)) = (body["app"].as_str(), body["person"].as_str()) else {
        return Response::err(id, BAD_REQUEST);
    };
    let name = format!("{app}-{person}");
    if !sessions::valid_name(&name) {
        return Response::err(id, BAD_REQUEST);
    }
    let from = Key(format!("{}/{name}", d.identity.short));
    if !cloud.declares(&from.0) {
        return Response::err_with(id, "refused", json!({ "reason": "not_declared" }));
    }
    // A send's receipt waits for the recipient's word (`ipc::finish`), as `send {wait}` does.
    let (receipt, wait) = match op {
        "app.send" => {
            let mut req = body.clone();
            req["from"] = from.0.clone().into();
            match ipc::send(id, &req, d) {
                Ok((_, r)) => (r, true),
                Err(e) => return e,
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
