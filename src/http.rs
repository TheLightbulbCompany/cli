//! The engine's HTTP calls (the issuer's sign-in and our signed relay list): one policy.
//! https only in release builds (debug: the tests' loopback servers), no redirects, one 10 s
//! deadline for the whole call (connect, send, answer), answers at most 64 KiB. Blocking: callers run off the async threads.

use crate::Result;
use serde_json::Value;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY: u64 = 64 * 1024;

/// An agent with the policy; a non-2xx status is an answer, not an error.
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .https_only(!cfg!(debug_assertions))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

/// `GET url`: the body, when the status is 2xx.
pub fn get(url: &str) -> Result<Vec<u8>> {
    let mut r = agent().get(url).call().map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("{url} answered {}", r.status().as_u16()).into());
    }
    let body = r.body_mut().with_config().limit(MAX_BODY).read_to_vec();
    Ok(body.map_err(|e| e.to_string())?)
}

/// `POST url` with JSON `body`: the status and the JSON answer (`null` when it is none).
pub fn post_json(url: &str, body: &Value) -> Result<(u16, Value)> {
    let mut r = agent()
        .post(url)
        .header("content-type", "application/json")
        .send(body.to_string())
        .map_err(|e| e.to_string())?;
    let status = r.status().as_u16();
    let text = r.body_mut().with_config().limit(MAX_BODY).read_to_string();
    let text = text.map_err(|e| e.to_string())?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}
