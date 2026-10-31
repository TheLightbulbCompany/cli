//! `lightbulb login` / `logout` and `invite --agents`: the one tie between this computer and a
//! person's Lightbulb account. A device flow: the backend hands out a one-time challenge, this
//! computer signs it with its member key (so the backend learns the key is really this
//! computer's), and the signed-in person confirms the code in their browser. The answer is a
//! sign-in proof (`proof`), kept in `login.json` (0600) with the route link's URL: this
//! computer shows it in its `hello` (a workspace answers only its own people's computers) and
//! on its route link (`link`). The CLI never holds an account token; the poll `code` is single
//! use and lives only in memory here. Logout tells Lightbulb (signed), then deletes the file.
//! `login` then joins the workspace's network when this computer is alone on its own.
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
/// (tests, debug builds) Set: no browser is opened.
const NO_BROWSER: &str = "LIGHTBULB_CONNECT_NO_BROWSER";
/// The sign-in, `{proof, route_url, name}`, in the state dir.
pub const FILE: &str = "login.json";

/// This computer's sign-in, when it has one.
pub fn stored(dir: &std::path::Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(dir.join(FILE)).ok()?).ok()
}

/// What this computer says once its sign-in can no longer be renewed.
pub const SIGN_IN_AGAIN: &str = "This computer's sign-in has ended. Sign in again: lightbulb login";
/// Whether this computer's sign-in has ended at `now` (its proof expired): only
/// `lightbulb login` brings it back. (A renewal asks the account server, which can refuse.) `false` when not signed in.
pub fn ended(dir: &std::path::Path, now: i64) -> bool {
    let Some(p) = stored_proof(dir).and_then(|t| crate::proof::peek(&t)) else {
        return false;
    };
    now > p.exp
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

pub(crate) fn api() -> String {
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
    request("POST", path, Some(&body.to_string()), &[])
}

/// `method {api}{path}` with JSON `body` and `headers`: the status and the answer's JSON
/// (`null` when none). All of it goes to curl as its config on stdin (`-K -`), so neither a
/// body nor a credential header is ever in argv.
pub(crate) fn request(
    method: &str,
    path: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> Result<(u16, Value)> {
    request_at(&api(), method, path, body, headers)
}

/// A curl config string. Compact JSON holds no raw line break, so `\` and `"` are all.
/// A value with a control character (a line break would start a new curl option) is refused.
fn quoted(s: &str) -> Result<String> {
    if s.chars().any(char::is_control) {
        return Err("A value for the request has a control character in it.".into());
    }
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// `request` against `base` (an API base URL) instead of the public one.
pub(crate) fn request_at(
    base: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> Result<(u16, Value)> {
    let mut config = head(base, method, path, headers)?;
    if let Some(b) = body {
        config.push_str("header = \"Content-Type: application/json\"\n");
        config.push_str(&format!("data-raw = {}\n", quoted(b)?));
    }
    curl(&config)
}

fn head(base: &str, method: &str, path: &str, headers: &[(&str, &str)]) -> Result<String> {
    let mut config = format!(
        "url = {}\nrequest = {}\n",
        quoted(&format!("{base}{path}"))?,
        quoted(method)?
    );
    for (k, v) in headers {
        config.push_str(&format!("header = {}\n", quoted(&format!("{k}: {v}"))?));
    }
    Ok(config)
}

/// `POST {base}{path}` as multipart form data: `fields`, then `file` as `(field, path)`.
/// The body is built here (random boundary) into a 0600 temp file and sent with
/// `data-binary @file`, so `headers` can sign the sha256 of exactly the bytes sent: it is
/// called with the body and returns the headers to add (the backend hashes the raw body).
pub(crate) fn request_form(
    base: &str,
    path: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &std::path::Path)>,
    headers: impl FnOnce(&[u8]) -> Result<Vec<(String, String)>>,
) -> Result<(u16, Value)> {
    use std::os::unix::fs::OpenOptionsExt;
    let boundary = format!("lightbulb{}", ulid::Ulid::generate());
    let mut body = Vec::new();
    for (k, v) in fields {
        let k = k.replace(['"', '\r', '\n'], "_");
        body.extend(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    if let Some((k, p)) = file {
        let name = p.file_name().map_or(String::new(), |n| {
            n.to_string_lossy().replace(['"', '\r', '\n'], "_")
        });
        body.extend(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes());
        body.extend(std::fs::read(p)?);
        body.extend(b"\r\n");
    }
    body.extend(format!("--{boundary}--\r\n").as_bytes());
    let headers = headers(&body)?;
    let tmp = std::env::temp_dir().join(format!("lightbulb-form-{}", ulid::Ulid::generate()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?
        .write_all(&body)?;
    let hs: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let config = (|| -> Result<String> {
        let mut config = head(base, "POST", path, &hs)?;
        config.push_str(&format!(
            "header = {}\n",
            quoted(&format!(
                "Content-Type: multipart/form-data; boundary={boundary}"
            ))?
        ));
        config.push_str(&format!(
            "data-binary = {}\n",
            quoted(&format!("@{}", tmp.display()))?
        ));
        // Up to 100 MB on a slow uplink: overrides curl()'s 30 s for this call only.
        config.push_str("max-time = 600\n");
        Ok(config)
    })();
    let out = config.and_then(|c| curl(&c));
    let _ = std::fs::remove_file(&tmp);
    out
}

fn curl(config: &str) -> Result<(u16, Value)> {
    curl_after(config).map(|(code, v, _)| (code, v))
}

/// `curl`, with the answer's `Retry-After` in seconds, when it has one. (A curl older than 7.84
/// prints the `%header` as it is, which parses as none.)
fn curl_after(config: &str) -> Result<(u16, Value, Option<u64>)> {
    let mut c = Command::new("curl");
    c.args(["-q", "-sS", "--connect-timeout", "10", "--max-time", "30"])
        .args(["-K", "-", "-w", "\n%header{retry-after}\n%{http_code}"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = c.spawn().map_err(|e| format!("curl: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("curl: no stdin")?
        .write_all(config.as_bytes())?;
    let out = child.wait_with_output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (rest, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    let (answer, after) = rest.rsplit_once('\n').unwrap_or((rest, ""));
    match code.trim().parse::<u16>() {
        Ok(code) if code > 0 => Ok((
            code,
            serde_json::from_str(answer).unwrap_or(Value::Null),
            after.trim().parse().ok(),
        )),
        _ => Err("Could not reach Lightbulb. Check your connection and try again.".into()),
    }
}

/// The backend's sentence for a refusal (`detail`, or `detail.message`), else a generic one.
pub(crate) fn said(code: u16, v: &Value) -> String {
    let detail = &v["detail"];
    detail
        .as_str()
        .or(detail["message"].as_str())
        .filter(|s| !s.is_empty() && s.chars().all(crate::label::printable))
        .map_or_else(|| format!("Lightbulb refused that ({code})."), String::from)
}

/// The answer to a 2xx, else the backend's sentence (`said`) as the error.
fn ok(path: &str, body: &Value) -> Result<Value> {
    let (code, v) = post(path, body)?;
    if (200..300).contains(&code) {
        return Ok(v);
    }
    Err(said(code, &v).into())
}

/// This computer's identity, read only: one is made by `setup` (or the daemon's first start).
pub(crate) fn identity() -> Result<Identity> {
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
    if !url.starts_with("https://") || crate::cli::test_env(NO_BROWSER).is_some() {
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
/// person confirms. The approved poll's answer. `app`: the code alone, as one line of JSON for
/// the desktop app, which confirms it itself; no browser.
fn device(purpose: &str, ticket: Option<&str>, app: bool) -> Result<Value> {
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
    if app {
        println!("{}", json!({ "user_code": user_code }));
    } else {
        println!("Confirm in your browser: {url}");
        println!("Type this code there: {user_code}");
        open(url);
    }
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

/// `login`: ties this computer to the person who confirms in the browser, then puts it on their
/// workspace's network. `app` (`login --app`, the desktop app): the sign-in alone, in silence.
pub fn login(app: bool) -> Result<i32> {
    let r = device("login", None, app)?;
    let Some(proof) = r["proof"].as_str() else {
        return Err(
            "Lightbulb sent an answer this version cannot read. Run lightbulb update.".into(),
        );
    };
    save(
        &paths::state_dir(),
        &json!({"proof": proof, "route_url": r["routeUrl"], "name": r["name"]}),
    )?;
    tell_daemon();
    if app {
        return Ok(0);
    }
    match r["name"]
        .as_str()
        .filter(|n| n.chars().all(crate::label::printable))
    {
        Some(n) if !n.is_empty() => {
            println!("This computer is signed in as {n}. Not you? Run lightbulb logout.")
        }
        _ => println!("This computer is signed in."),
    }
    // The proof is saved: whatever happens here, the login stands.
    match join_workspace() {
        Ok(Some(said)) => println!("{said}"),
        Ok(None) => {}
        Err(e) => println!("This computer did not join your workspace's network: {e}"),
    }
    crate::cli::ask_upload_mode()?;
    Ok(0)
}

/// One call signed as this computer's person (`mcp::proof_headers`), with no body: the status,
/// the answer's JSON and its `Retry-After`. Signed afresh each time: a signed call is good once.
fn signed_call(method: &str, path: &str) -> Result<(u16, Value, Option<u64>)> {
    let headers = crate::mcp::proof_headers(method, path, b"", "lightbulb login").map_err(|e| {
        let why = e["hint"].as_str().or(e["error"].as_str());
        why.unwrap_or("not signed in").to_string()
    })?;
    let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut config = head(&api(), method, path, &headers)?;
    // Past API Gateway's own 30 s cut, so its answer is what arrives. Overrides curl()'s 30 s.
    config.push_str("max-time = 40\n");
    curl_after(&config)
}

/// `POST /network/join-line`, waiting out a cold member: the gateway answers a 5xx at once while
/// it wakes (about 55 s), with `Retry-After` (else 15 s). Up to 3 minutes.
// ponytail: only a 5xx is retried; a dropped connection fails this login's join with one line and
// the person runs login again. Retry those here too if they turn up during cold starts.
fn join_line() -> Result<(u16, Value)> {
    let wait = crate::cli::test_env("LIGHTBULB_CONNECT_JOIN_WAIT_S")
        .and_then(|s| s.to_str()?.parse().ok())
        .unwrap_or(180);
    let until = Instant::now() + Duration::from_secs(wait);
    loop {
        let (code, v, after) = signed_call("POST", "/network/join-line")?;
        if code < 500 {
            return Ok((code, v));
        }
        // At least a second: the next call's signature must differ from this one's.
        let pause = Duration::from_secs(after.unwrap_or(15).clamp(1, 60));
        if Instant::now() + pause > until {
            return Err("your workspace did not wake in time. Run lightbulb login again.".into());
        }
        std::thread::sleep(pause);
    }
}

/// After the sign-in: puts a computer that is alone on its signed-in person's workspace's
/// network, through a once link its cloud member minted, and leaves one on another network with
/// members where it is. What to tell the person, if anything: nothing for a person with no
/// workspace, or a backend without these routes (404: older than this engine).
fn join_workspace() -> Result<Option<String>> {
    let (code, me, _) = signed_call("GET", "/network/team")?;
    if code == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&code) {
        return Err(said(code, &me).into());
    }
    if !me["workspace"].is_string() {
        return Ok(None);
    }
    let workspace = shown(&me["workspace"]).unwrap_or("your workspace");
    let mut conn = crate::cli::connect()?;
    let team = crate::cli::ask(&mut conn, "team.get", json!({}))?;
    if me["teamId"].is_string() && me["teamId"] == team["team_id"] {
        return Ok(Some(format!("On {workspace}'s network.")));
    }
    if team["members"].as_array().map_or(0, Vec::len) != 1 {
        let name = shown(&team["name"]).unwrap_or("another network");
        return Ok(Some(format!(
            "You are on {name}. To join {workspace}'s network: lightbulb leave, then lightbulb login."
        )));
    }
    println!("Connecting your computer to {workspace}…");
    let (code, line) = join_line()?;
    if code == 404 {
        return Ok(None);
    }
    if !(200..300).contains(&code) {
        return Err(said(code, &line).into());
    }
    let ticket = line["ticket"].as_str().ok_or("Lightbulb sent no invite.")?;
    let name = shown(&line["name"]).map_or_else(crate::daemon::user_name, String::from);
    // Dialing, and ringing a parked member's doorbell, as `cli::join` waits for it.
    conn.wait_up_to(Duration::from_secs(660))?;
    let body = json!({ "ticket": ticket, "name": name });
    let mut r = crate::cli::ask(&mut conn, "team.join", body)?;
    // A person's second computer: every computer is its own member, each name unique.
    if r["reason"] == crate::admission::NAME_TAKEN {
        let computer = crate::config::Config::load(&paths::state_dir())?.computer;
        let body = json!({ "ticket": ticket, "name": format!("{name} ({computer})") });
        r = crate::cli::ask(&mut conn, "team.join", body)?;
    }
    if r["state"] != "admitted" {
        let why = r["reason"].as_str().unwrap_or("refused").replace('_', " ");
        return Err(format!("the network refused it ({why}). Run lightbulb login again.").into());
    }
    crate::telemetry::send(
        &paths::state_dir(),
        crate::telemetry::Event::NetworkJoined(crate::telemetry::Via::Line),
    );
    Ok(Some(format!(
        "Your computer joined {workspace}'s network. Teammates on it see the names, folders and \
         status of your sessions, never your files or conversations. Hide a folder: lightbulb \
         folder <path> off."
    )))
}

/// `v`, when it is text this terminal can safely show.
fn shown(v: &Value) -> Option<&str> {
    (v.as_str()).filter(|s| !s.is_empty() && s.chars().all(crate::label::printable))
}

/// `logout`: signs this computer out everywhere first (a signed call: the backend adds its key
/// to every workspace's `revoked_keys`, so their agents stop answering it at once), then deletes
/// its sign-in; its route link drops within seconds (`link`). Offline, it signs out here only.
pub fn logout() -> Result<i32> {
    let dir = paths::state_dir();
    let told = stored_proof(&dir).map(|proof| -> Result<Value> {
        let mut body = signed(&identity()?, "logout", None)?;
        body["proof"] = proof.into();
        ok("/network/proof/revoke", &body)
    });
    let removed = std::fs::remove_file(dir.join(FILE));
    tell_daemon();
    match removed {
        Ok(()) => match told {
            Some(Err(e)) => println!(
                "This computer is signed out here, but Lightbulb was not told ({e}). Your \
                 workspaces answer it until its sign-in ends (at most 30 days)."
            ),
            _ => println!("This computer is signed out."),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("This computer was not signed in.")
        }
        Err(e) => return Err(e.into()),
    }
    Ok(0)
}

/// Tells a running daemon its sign-in changed, so connected members get the new proof (or
/// none) in a fresh `hello` now, not at their next reconnect. No daemon: nothing to tell.
fn tell_daemon() {
    if let Ok(mut c) = crate::hook::Conn::open() {
        let _ = c.call("login.changed", json!({}));
    }
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
/// link minted here goes to the workspace, which joins with it (the invite is the approval).
pub fn invite_agents(ticket: &str) -> Result<i32> {
    let r = device("agents", Some(ticket), false)?;
    let ws = r["workspaceName"]
        .as_str()
        .filter(|n| !n.is_empty() && n.chars().all(crate::label::printable))
        .unwrap_or("the workspace");
    println!("Confirmed. {ws} is joining this network.");
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
