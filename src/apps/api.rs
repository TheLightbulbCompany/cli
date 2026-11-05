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
        /// `LIGHTBULB_RUN_SECRET`, kept only when `LIGHTBULB_RUN_ID` is set too. Sent as
        /// `X-Lightbulb-Run-Secret`; `X-Lightbulb-Run` already rides on every request.
        run_secret: Option<RunSecret>,
    },
    /// This computer's sign-in proof.
    Proof,
}

/// The Worker's run secret. Its `Debug` never prints the value.
#[derive(PartialEq, Eq)]
pub struct RunSecret(pub String);

impl std::fmt::Debug for RunSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunSecret([redacted])")
    }
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
                let run_secret = get("LIGHTBULB_RUN_ID")
                    .and(get("LIGHTBULB_RUN_SECRET"))
                    .map(RunSecret);
                Auth::HookToken {
                    api,
                    owner,
                    token,
                    run_secret,
                }
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

/// The headers a hook-token call carries.
fn hook_headers(owner: &str, token: &str, secret: &Option<RunSecret>) -> Vec<(String, String)> {
    let mut h = vec![
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("X-Lightbulb-Owner".to_string(), owner.to_string()),
    ];
    if let Some(RunSecret(s)) = secret {
        h.push(("X-Lightbulb-Run-Secret".to_string(), s.clone()));
    }
    h
}

fn as_strs(h: &[(String, String)]) -> Vec<(&str, &str)> {
    h.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

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
        Auth::HookToken {
            api,
            owner,
            token,
            run_secret,
        } => {
            let h = hook_headers(owner, token, run_secret);
            let h = as_strs(&h);
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

/// `GET {api}/apps/build{path}` into the file `to`: the answer's `X-Lightbulb-Commit`.
pub fn download(auth: &Auth, path: &str, to: &Path) -> Result<String, ApiError> {
    let path = format!("{BUILD}{path}");
    const COMMIT: &str = "x-lightbulb-commit";
    let answer = match auth {
        Auth::HookToken {
            api,
            owner,
            token,
            run_secret,
        } => {
            let h = hook_headers(owner, token, run_secret);
            let h = as_strs(&h);
            login::download_at(api, &path, &h, COMMIT, to)
        }
        Auth::Proof => {
            let h = proof("GET", &path, b"")?;
            let h: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
            login::download_at(&login::api(), &path, &h, COMMIT, to)
        }
    };
    let (status, commit) = answer.map_err(|_| ApiError::unreachable())?;
    // On a failure the body in the file is the backend's JSON error.
    let error = std::fs::read(to)
        .ok()
        .filter(|_| !(200..300).contains(&status))
        .and_then(|b| serde_json::from_slice(&b).ok());
    answered(Ok((status, error.unwrap_or(Value::Null))))?;
    if commit.is_empty() || commit.chars().any(|c| !c.is_ascii_alphanumeric()) {
        return Err(ApiError::new(
            "bad_answer",
            "Lightbulb did not say which commit the source is. Reading it needs curl 7.84 or newer.",
            "check curl --version; update curl if it is older than 7.84, then try again",
        ));
    }
    Ok(commit)
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
            Auth::HookToken {
                api,
                owner,
                token,
                run_secret,
            } => {
                let h = hook_headers(owner, token, run_secret);
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
    // Every failure says what to do: the backend's next, else one by the HTTP status.
    let next = e["next"]
        .as_str()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(match status {
            401 => "run lightbulb login",
            404 => "check the app with lightbulb app status",
            500.. => "try again in a minute",
            _ => "fix that and try again",
        });
    if let (Some(code), Some(message)) = (e["code"].as_str(), e["message"].as_str()) {
        return Err(ApiError::new(code, message, next));
    }
    // A 404 the backend did not explain is a route it does not have (an older backend).
    let code = if status == 404 { "no_route" } else { "refused" };
    Err(ApiError::new(code, &login::said(status, &v), next))
}
