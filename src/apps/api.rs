//! Calls to the builder routes, `/apps/build/*` under the API base. One path for every
//! builder: a workspace agent's hook token, else this computer's `lightbulb login` proof.

use crate::{login, mcp};
use serde_json::Value;
use std::path::Path;

/// Which credential a call carries.
#[derive(Debug, PartialEq, Eq)]
pub enum Auth {
    /// A workspace agent: `api` is the API base (with `/api/v1`).
    HookToken {
        api: String,
        owner: String,
        token: String,
    },
    /// This computer's sign-in proof.
    Proof,
}

impl Auth {
    /// From the process env.
    pub fn detect() -> Auth {
        Auth::detect_with(|k| std::env::var(k).ok())
    }

    /// The hook token only when `OPENCLAW_HOOK_TOKEN`, `ISOL8_API_URL` and `ISOL8_OWNER_ID`
    /// are all set. `ISOL8_API_URL` is the origin; `/api/v1` is added unless it is there.
    pub fn detect_with(get: impl Fn(&str) -> Option<String>) -> Auth {
        let get = |k: &str| get(k).filter(|v| !v.is_empty());
        match (
            get("OPENCLAW_HOOK_TOKEN"),
            get("ISOL8_API_URL"),
            get("ISOL8_OWNER_ID"),
        ) {
            (Some(token), Some(url), Some(owner)) => {
                let url = url.trim_end_matches('/');
                let api = if url.ends_with("/api/v1") {
                    url.to_string()
                } else {
                    format!("{url}/api/v1")
                };
                Auth::HookToken { api, owner, token }
            }
            _ => Auth::Proof,
        }
    }
}

/// A failure, in the shape every command prints.
#[derive(Debug, PartialEq, Eq)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub next: String,
}

impl ApiError {
    pub fn new(code: &str, message: &str, next: &str) -> ApiError {
        ApiError {
            code: code.into(),
            message: message.into(),
            next: next.into(),
        }
    }

    fn unreachable() -> ApiError {
        ApiError::new(
            "unreachable",
            "Could not reach Lightbulb.",
            "check your connection and try again",
        )
    }
}

const BUILD: &str = "/apps/build";

/// `method {api}/apps/build{path}` with JSON `body`.
pub fn call(
    auth: &Auth,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Value, ApiError> {
    let path = format!("{BUILD}{path}");
    let text = body.map(Value::to_string);
    let answer = match auth {
        Auth::HookToken { api, owner, token } => {
            let bearer = format!("Bearer {token}");
            let h = [
                ("Authorization", bearer.as_str()),
                ("X-Lightbulb-Owner", owner),
            ];
            login::request_at(api, method, &path, text.as_deref(), &h)
        }
        Auth::Proof => {
            let h = proof(method, &path, text.as_deref().unwrap_or("").as_bytes())?;
            let h: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
            login::request(method, &path, text.as_deref(), &h)
        }
    };
    answered(answer)
}

/// `POST {api}/apps/build{path}` as multipart: `fields`, then `file` as `(field, path)`.
pub fn form(
    auth: &Auth,
    path: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &Path)>,
) -> Result<Value, ApiError> {
    let path = format!("{BUILD}{path}");
    if let Some((_, f)) = file {
        if let Err(e) = std::fs::File::open(f) {
            return Err(ApiError::new(
                "bad_file",
                &format!("Could not read {}: {e}", f.display()),
                "check the path",
            ));
        }
    }
    // A proof that cannot be made is kept here, so it is not reported as unreachable.
    let refused = std::cell::RefCell::new(None);
    let answer =
        match auth {
            Auth::HookToken { api, owner, token } => {
                let h = vec![
                    ("Authorization".to_string(), format!("Bearer {token}")),
                    ("X-Lightbulb-Owner".to_string(), owner.clone()),
                ];
                login::request_form(api, &path, fields, file, |_| Ok(h))
            }
            Auth::Proof => login::request_form(&login::api(), &path, fields, file, |body| {
                match proof("POST", &path, body) {
                    Ok(h) => Ok(h.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
                    Err(e) => {
                        let msg = e.message.clone();
                        *refused.borrow_mut() = Some(e);
                        Err(msg.into())
                    }
                }
            }),
        };
    if let Some(e) = refused.into_inner() {
        return Err(e);
    }
    answered(answer)
}

fn proof(method: &str, path: &str, body: &[u8]) -> Result<Vec<(&'static str, String)>, ApiError> {
    mcp::proof_headers(method, path, body, "cli").map_err(|e| {
        let hint = e["hint"].as_str().unwrap_or("run lightbulb doctor");
        match e["error"].as_str() {
            Some("not_signed_in") => ApiError::new(
                "not_signed_in",
                "This computer is not signed in to Lightbulb.",
                hint,
            ),
            _ => ApiError::new("identity_unavailable", hint, "run lightbulb doctor"),
        }
    })
}

/// A 2xx's JSON, else the backend's `{code, message, next}` (or its `detail`) as the error.
fn answered(r: crate::Result<(u16, Value)>) -> Result<Value, ApiError> {
    let (status, v) = r.map_err(|_| ApiError::unreachable())?;
    if (200..300).contains(&status) {
        return Ok(v);
    }
    let e = if v["code"].is_string() {
        &v
    } else {
        &v["error"]
    };
    if let (Some(code), Some(message)) = (e["code"].as_str(), e["message"].as_str()) {
        return Err(ApiError::new(
            code,
            message,
            e["next"].as_str().unwrap_or(""),
        ));
    }
    Err(ApiError::new(
        "refused",
        &login::said(status, &v),
        "fix that and try again",
    ))
}
