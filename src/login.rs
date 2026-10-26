//! `lightbulb login` / `logout` and `invite --agents`: the one tie between this computer and a
//! person's Lightbulb account. A device flow: the backend hands out a one-time challenge, this
//! computer signs it with its member key (so the backend learns the key is really this
//! computer's), and the signed-in person confirms the code in their browser. The answer is a
//! sign-in proof (`proof`), kept in `login.json` (0600) with the route link's URL: this
//! computer shows it in its `hello` (a workspace answers only its own people's computers) and
//! on its route link (`link`). The CLI never holds an account token; the poll `code` is single
//! use and lives only in memory here. Logout deletes the file; nothing is asked of Lightbulb.
//!
//! The wire is `curl`, as `lightbulb update` and the usage counts use; bodies go in on stdin
//! (never in argv, which other processes can read).

use crate::identity::Identity;
use crate::{paths, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The public Lightbulb API. `LIGHTBULB_API` points a test or another environment elsewhere.
const API: &str = "https://api.thelightbulb.company/api/v1";
/// (tests) Set: no browser is opened.
const NO_BROWSER: &str = "LIGHTBULB_CONNECT_NO_BROWSER";
/// The sign-in, `{proof, route_url, name}`, in the state dir.
pub const FILE: &str = "login.json";

/// This computer's sign-in, when it has one.
pub fn stored(dir: &std::path::Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(dir.join(FILE)).ok()?).ok()
}

/// What this computer says once its sign-in can no longer be renewed.
pub const SIGN_IN_AGAIN: &str = "This computer's sign-in has ended. Sign in again: lightbulb login";
/// A renewal keeps the browser login's `auth_time`; Lightbulb renews for this long after it.
const MAX_AUTH_AGE_S: i64 = 90 * 86_400;

/// Whether this computer's sign-in has ended at `now` (expired, or its browser login is past
/// `MAX_AUTH_AGE_S`): only `lightbulb login` brings it back. `false` when not signed in.
pub fn ended(dir: &std::path::Path, now: i64) -> bool {
    let Some(p) = stored_proof(dir).and_then(|t| crate::proof::peek(&t)) else {
        return false;
    };
    now > p.exp || p.auth_time.is_some_and(|t| now - t > MAX_AUTH_AGE_S)
}

/// This computer's sign-in proof, when it has one.
pub fn stored_proof(dir: &std::path::Path) -> Option<String> {
    stored(dir)?["proof"].as_str().map(String::from)
}

/// Writes the sign-in, 0600 (it is a credential), replacing any.
pub fn save(dir: &std::path::Path, v: &Value) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = dir.join(format!("{FILE}.tmp"));
    // A leftover `.tmp` would keep its own (maybe wider) mode: `mode` applies only on create.
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(v.to_string().as_bytes())?;
    std::fs::rename(tmp, dir.join(FILE))?;
    Ok(())
}

fn api() -> String {
    std::env::var("LIGHTBULB_API")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| API.into())
}

/// What this computer signs for `purpose` (`login`, `logout` or `agents`): the backend's
/// `challenge`, this computer's `key`, and for `agents` the once link the workspace joins with,
/// so a captured proof cannot carry another link. Canonical JSON, Ed25519 (`Identity::sign`).
pub fn proof(id: &Identity, purpose: &str, challenge: &str, ticket: Option<&str>) -> Value {
    let mut v = json!({
        "purpose": format!("lightbulb-{purpose}"),
        "challenge": challenge,
        "key": id.key,
    });
    if let Some(t) = ticket {
        v["ticket"] = t.into();
    }
    id.signed(v)
}

/// `POST {api}{path}` with JSON `body`: the status and the answer's JSON (`null` when none).
pub(crate) fn post(path: &str, body: &Value) -> Result<(u16, Value)> {
    let mut c = Command::new("curl");
    c.args(["-q", "-sS", "--connect-timeout", "10", "--max-time", "30"])
        .args(["-X", "POST", "-H", "Content-Type: application/json"])
        .args(["--data-binary", "@-", "-w", "\n%{http_code}"])
        .arg(format!("{}{path}", api()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = c.spawn().map_err(|e| format!("curl: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("curl: no stdin")?
        .write_all(body.to_string().as_bytes())?;
    let out = child.wait_with_output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (answer, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    match code.trim().parse::<u16>() {
        Ok(code) if code > 0 => Ok((code, serde_json::from_str(answer).unwrap_or(Value::Null))),
        _ => Err("Could not reach Lightbulb. Check your connection and try again.".into()),
    }
}

/// The answer to a 2xx, else the backend's sentence (`detail`, or `detail.message`) as the error.
fn ok(path: &str, body: &Value) -> Result<Value> {
    let (code, v) = post(path, body)?;
    if (200..300).contains(&code) {
        return Ok(v);
    }
    let detail = &v["detail"];
    let said = detail
        .as_str()
        .or(detail["message"].as_str())
        .filter(|s| !s.is_empty() && s.chars().all(crate::label::printable));
    Err(said
        .map_or_else(|| format!("Lightbulb refused that ({code})."), String::from)
        .into())
}

/// This computer's identity, read only: one is made by `setup` (or the daemon's first start).
fn identity() -> Result<Identity> {
    let dir = paths::state_dir();
    if !crate::identity::exists(&dir)? {
        return Err("This computer is not set up yet. Run lightbulb setup first.".into());
    }
    Identity::load_or_create(&dir)
}

/// A fresh challenge, signed for `purpose`: the body every signed call starts with.
pub(crate) fn signed(id: &Identity, purpose: &str, ticket: Option<&str>) -> Result<Value> {
    let ch = ok("/network/device/challenge", &json!({}))?;
    let ch = ch["challenge"]
        .as_str()
        .ok_or("Lightbulb sent no challenge.")?;
    let p = proof(id, purpose, ch, ticket);
    Ok(json!({"key": id.key, "challenge": ch, "sig": p["sig"], "purpose": purpose}))
}

/// Opens `url` in the browser (an `https` one only); a failure is fine, the URL is printed.
fn open(url: &str) {
    if !url.starts_with("https://") || std::env::var_os(NO_BROWSER).is_some() {
        return;
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Command::new(opener)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The device flow for `purpose`: start, show the code, open the browser, poll until the
/// person confirms. The approved poll's answer.
fn device(purpose: &str, ticket: Option<&str>) -> Result<Value> {
    let id = identity()?;
    let mut body = signed(&id, purpose, ticket)?;
    let computer = crate::config::Config::load(&paths::state_dir())?.computer;
    body["computer"] = computer.into();
    if let Some(t) = ticket {
        body["ticket"] = t.into();
        // Whose computer this is: an admin adds a workspace only from their own.
        body["proof"] = stored_proof(&paths::state_dir())
            .ok_or("Sign this computer in first: run lightbulb login.")?
            .into();
    }
    let start = ok("/network/device/start", &body)?;
    let (Some(code), Some(user_code), Some(url)) = (
        start["code"].as_str(),
        start["userCode"].as_str(),
        start["verifyUrl"].as_str(),
    ) else {
        return Err(
            "Lightbulb sent an answer this version cannot read. Run lightbulb update.".into(),
        );
    };
    println!("Confirm in your browser: {url}");
    println!("Type this code there: {user_code}");
    open(url);
    let every = Duration::from_secs(start["interval"].as_u64().unwrap_or(2).clamp(1, 10));
    let until =
        Instant::now() + Duration::from_secs(start["expiresIn"].as_u64().unwrap_or(600).min(900));
    let poll = json!({ "code": code });
    loop {
        let r = ok("/network/device/poll", &poll)?;
        match r["status"].as_str() {
            Some("approved") => return Ok(r),
            Some("pending") if Instant::now() < until => std::thread::sleep(every),
            _ => {
                return Err(
                    format!("The code expired. Run lightbulb {} again.", again(purpose)).into(),
                )
            }
        }
    }
}

fn again(purpose: &str) -> &'static str {
    if purpose == "agents" {
        "invite --agents"
    } else {
        "login"
    }
}

/// `login`: ties this computer to the person who confirms in the browser.
pub fn login() -> Result<i32> {
    let r = device("login", None)?;
    let Some(proof) = r["proof"].as_str() else {
        return Err(
            "Lightbulb sent an answer this version cannot read. Run lightbulb update.".into(),
        );
    };
    save(
        &paths::state_dir(),
        &json!({"proof": proof, "route_url": r["routeUrl"], "name": r["name"]}),
    )?;
    match r["name"]
        .as_str()
        .filter(|n| n.chars().all(crate::label::printable))
    {
        Some(n) if !n.is_empty() => {
            println!("This computer is signed in as {n}. Not you? Run lightbulb logout.")
        }
        _ => println!("This computer is signed in."),
    }
    println!(
        "Your workspace's agents answer it once an admin adds them: lightbulb invite --agents"
    );
    Ok(0)
}

/// `logout`: deletes this computer's sign-in; its route link drops within seconds (`link`)
/// and workspaces stop answering it once its connections to them are made again.
pub fn logout() -> Result<i32> {
    match std::fs::remove_file(paths::state_dir().join(FILE)) {
        Ok(()) => println!("This computer is signed out."),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("This computer was not signed in.")
        }
        Err(e) => return Err(e.into()),
    }
    Ok(0)
}

/// Renews this computer's proof in its last week (the daemon's link calls it daily): a signed
/// call, no browser. `Ok(false)` when there is nothing to renew.
pub fn refresh(dir: &std::path::Path) -> Result<bool> {
    let Some(mut v) = stored(dir) else {
        return Ok(false);
    };
    let Some(old) = v["proof"].as_str().map(String::from) else {
        return Ok(false);
    };
    let id = Identity::load_or_create(dir)?;
    let now = crate::daemon::ipc::now() as i64;
    let Some(p) = crate::proof::peek(&old) else {
        return Ok(false);
    };
    if now > p.exp || p.exp - now > 7 * 86_400 {
        // Expired: only a login renews it. Else not yet its last week.
        return Ok(false);
    }
    let mut body = signed(&id, "refresh", None)?;
    body["proof"] = old.clone().into();
    let r = ok("/network/proof/refresh", &body)?;
    match r["proof"].as_str() {
        Some(new) if new != old => {
            v["proof"] = new.into();
            save(dir, &v)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// `invite --agents`: a workspace admin adds their workspace's agents to this network. A once
/// link minted here goes to the workspace, which joins with it; this computer then asks the
/// usual Allow.
pub fn invite_agents(ticket: &str) -> Result<i32> {
    let r = device("agents", Some(ticket))?;
    let ws = r["workspaceName"]
        .as_str()
        .filter(|n| !n.is_empty() && n.chars().all(crate::label::printable))
        .unwrap_or("the workspace");
    println!("Confirmed. {ws} is joining: allow it when this computer asks.");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proof_is_the_keys_signature_over_purpose_challenge_key_and_link() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::load_or_create(dir.path()).unwrap();
        let p = proof(&id, "agents", "n0nce", Some("t1"));
        let sig = p["sig"].as_str().unwrap();
        assert!(crate::identity::verify(&id.key, &p, sig));
        // What the backend rebuilds and checks: sorted keys, no spaces.
        let signed = format!(
            r#"{{"challenge":"n0nce","key":"{}","purpose":"lightbulb-agents","ticket":"t1"}}"#,
            id.key
        );
        assert_eq!(crate::identity::unsigned(&p), signed.as_bytes());
        // Another purpose, challenge or link is another message.
        for other in [
            proof(&id, "login", "n0nce", Some("t1")),
            proof(&id, "agents", "other", Some("t1")),
            proof(&id, "agents", "n0nce", Some("t2")),
        ] {
            assert!(!crate::identity::verify(&id.key, &other, sig));
        }
        assert!(proof(&id, "login", "n0nce", None).get("ticket").is_none());
    }
}
