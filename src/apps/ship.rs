//! `up`, `publish`, `rollback`, `destroy`: send the app's source to the builder routes and print
//! what the backend says. Only contract fields are printed, never the backend's JSON as is.

use super::api::{self, ApiError, Auth};
use super::{check, evidence, fail, migrations, AppConfig};
use crate::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The backend refuses a bigger source archive.
const MAX_ARCHIVE: u64 = 9 * 1024 * 1024;

fn git(root: &Path, args: &[&str]) -> std::result::Result<Output, ApiError> {
    Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|e| {
            ApiError::new(
                "git_failed",
                &format!("Could not run git: {e}"),
                "install git",
            )
        })
}

pub(super) fn git_ok(root: &Path, args: &[&str]) -> std::result::Result<String, ApiError> {
    let o = git(root, args)?;
    if o.status.success() {
        return Ok(String::from_utf8_lossy(&o.stdout).trim().to_string());
    }
    Err(ApiError::new(
        "git_failed",
        &format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        "fix the repo and try again",
    ))
}

/// `-c` flags that give git an identity when this computer has none.
fn identity(root: &Path) -> Vec<&'static str> {
    let has = git_ok(root, &["config", "user.email"]).is_ok_and(|e| !e.is_empty());
    if has {
        vec![]
    } else {
        vec![
            "-c",
            "user.name=Lightbulb",
            "-c",
            "user.email=app@thelightbulb.company",
        ]
    }
}

fn commit(root: &Path, msg: &str) -> std::result::Result<(), ApiError> {
    let mut args = identity(root);
    args.extend(["commit", "-qm", msg]);
    git_ok(root, &args).map(drop)
}

/// Never shipped, even when tracked (the backend refuses them too).
fn shipped(path: &str) -> bool {
    !path.split('/').any(|c| {
        c.starts_with(".env") || matches!(c, "node_modules" | ".next" | ".git" | ".vercel")
    })
}

/// Commits a dirty tree (one commit per change), then packs the tracked files at HEAD into a
/// tar.gz. Returns its path and HEAD.
pub fn archive(root: &Path) -> std::result::Result<(PathBuf, String), ApiError> {
    // Raw stdout: git_ok trims, which would eat the first line's leading space.
    let dirty = String::from_utf8_lossy(&git(root, &["status", "--porcelain"])?.stdout).to_string();
    if !dirty.is_empty() {
        let paths: Vec<&str> = dirty
            .lines()
            .map(|l| l.get(3..).unwrap_or(l).trim())
            .collect();
        git_ok(root, &["add", "-A"])?;
        // Never commit a secret file; build output is just unstaged.
        let staged = git(root, &["diff", "--cached", "--name-only", "-z"])?.stdout;
        let staged: Vec<String> = staged
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect();
        let secrets: Vec<&String> = staged
            .iter()
            .filter(|p| p.split('/').any(|c| c.starts_with(".env")))
            .collect();
        let build: Vec<&String> = staged.iter().filter(|p| !shipped(p)).collect();
        for list in [&secrets, &build] {
            if !list.is_empty() {
                let mut args = vec!["reset", "-q", "--"];
                args.extend(list.iter().map(|p| p.as_str()));
                git_ok(root, &args)?;
            }
        }
        if !secrets.is_empty() {
            return Err(ApiError::new(
                "secret_file",
                &format!(
                    "These files hold secrets and would be committed: {}",
                    secrets
                        .iter()
                        .map(|p| p.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "add them to .gitignore",
            ));
        }
        if !git(root, &["diff", "--cached", "--quiet"])?
            .status
            .success()
        {
            let more = paths.len().saturating_sub(3);
            let mut msg = format!(
                "lightbulb: changed {}",
                paths[..paths.len().min(3)].join(", ")
            );
            if more > 0 {
                msg.push_str(&format!(" and {more} more"));
            }
            commit(root, &msg)?;
        }
    }
    let head = git_ok(root, &["rev-parse", "HEAD"])?;
    let files = git(root, &["ls-files", "-z"])?.stdout;
    let list: Vec<u8> = files
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty() && shipped(&String::from_utf8_lossy(p)))
        .flat_map(|p| p.iter().copied().chain([0]))
        .collect();
    let tgz = std::env::temp_dir().join(format!(
        "lightbulb-app-{}-{}.tar.gz",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    let pack = || -> std::io::Result<bool> {
        use std::io::Write;
        let mut tar = Command::new("tar")
            .current_dir(root)
            .arg("-czf")
            .arg(&tgz)
            .args(["--null", "-T", "-"])
            .stdin(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        tar.stdin.take().unwrap().write_all(&list)?;
        Ok(tar.wait()?.success())
    };
    if !pack().unwrap_or(false) {
        let _ = std::fs::remove_file(&tgz);
        return Err(ApiError::new(
            "archive_failed",
            "Could not pack the app's files.",
            "check that tar works",
        ));
    }
    let size = std::fs::metadata(&tgz).map_or(0, |m| m.len());
    if size > MAX_ARCHIVE {
        let _ = std::fs::remove_file(&tgz);
        return Err(ApiError::new(
            "source_too_large",
            &format!(
                "The app's source is {:.1} MiB packed; Lightbulb takes at most 9 MiB.",
                size as f64 / (1024.0 * 1024.0)
            ),
            "keep large media and assets out of git (add them to .gitignore, or use external storage) and do not commit generated files, then try again",
        ));
    }
    Ok((tgz, head))
}

fn app_json(root: &Path) -> Value {
    std::fs::read(root.join(".lightbulb/app.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn save_app_json(root: &Path, v: &Value) -> Result<()> {
    let path = root.join(".lightbulb/app.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::create_dir_all(root.join(".lightbulb"))?;
    std::fs::write(&tmp, v.to_string())?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// A review record for `head` with verdict pass (R10), in `.lightbulb/progress.json`.
pub fn has_passing_review(root: &Path, head: &str) -> bool {
    std::fs::read(root.join(".lightbulb/progress.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["evidence"].as_array().cloned())
        .is_some_and(|e| {
            e.iter()
                .any(|r| r["kind"] == "review" && r["commit"] == head && r["verdict"] == "pass")
        })
}

/// The contract's fields of an `UpResult`, nothing else.
fn contract(v: &Value) -> Value {
    let mut out = json!({});
    for k in [
        "app_id",
        "state",
        "preview_url",
        "live_url",
        "canvas_url",
        "version",
        "sample_data",
        "cost_cents",
    ] {
        if !v[k].is_null() {
            out[k] = v[k].clone();
        }
    }
    let cards: Vec<Value> = v["cards"]
        .as_array()
        .map(|c| {
            c.iter()
                .map(|c| {
                    json!({"workflow_id": c["workflow_id"], "provider": c["provider"],
                    "service": c["service"], "state": c["state"]})
                })
                .collect()
        })
        .unwrap_or_default();
    out["cards"] = json!(cards);
    if v["error"].is_object() {
        out["error"] = json!({"code": v["error"]["code"], "message": v["error"]["message"],
            "next": v["error"]["next"]});
    }
    out
}

/// The result of `cmd` (`up`, `publish`, `rollback`): JSON, or lines for a person.
fn print(v: &Value, cmd: &str, json: bool) {
    if json {
        println!("{v}");
    } else {
        for l in human(v, cmd) {
            println!("{l}");
        }
    }
}

/// An `UpResult` in the words preview / live / done; never a JSON key or enum.
pub(crate) fn human(v: &Value, cmd: &str) -> Vec<String> {
    let mut out = vec![];
    let url = |k: &str| v[k].as_str().filter(|u| !u.is_empty());
    match v["state"].as_str() {
        Some("awaiting_approval") => {
            out.push("Preview: waiting for the owner to approve the cards in Inbox".into())
        }
        Some("provisioning" | "building") => out.push("Preview: building".into()),
        _ if cmd == "up" => {
            let sample = if v["sample_data"] == true {
                " (sample data)"
            } else {
                ""
            };
            out.push(format!(
                "Preview: {}{sample}",
                url("preview_url").unwrap_or("built")
            ));
        }
        _ => {
            let version = v["version"]
                .as_u64()
                .map_or(String::new(), |n| format!(" (version {n})"));
            out.push(format!(
                "Live: {}{version}",
                url("live_url").unwrap_or("yes")
            ));
        }
    }
    for c in v["cards"].as_array().into_iter().flatten() {
        out.push(format!(
            "Approval card: {} {} ({})",
            c["provider"].as_str().unwrap_or(""),
            c["service"].as_str().unwrap_or(""),
            c["state"].as_str().unwrap_or("")
        ));
    }
    if let Some(m) = v["error"]["message"].as_str().filter(|m| !m.is_empty()) {
        out.push(format!("Note: {m}"));
    }
    if let Some(n) = v["note"].as_str() {
        out.push(format!("Note: {n}"));
    }
    if let Some(u) = url("canvas_url") {
        out.push(format!("Canvas: {u}"));
    }
    out.extend(done_lines(v));
    out
}

/// A missing piece of the done gate, for a person.
fn missing_words(m: &str) -> &str {
    match m {
        "ready" => "a finished preview",
        "preview" => "the preview's address from Lightbulb",
        "preview_commit" => "a preview of this commit",
        "health" => "a passing health check",
        "browser_evidence" => "a person checking the preview in the browser",
        other => other,
    }
}

/// `Done:`, `Missing:` (when not done) and `Next:` (when there is one).
pub(crate) fn done_lines(v: &Value) -> Vec<String> {
    let mut out = vec![];
    if let Some(d) = v["done"].as_bool() {
        out.push(format!("Done: {}", if d { "yes" } else { "no" }));
    }
    let missing: Vec<&str> = v["missing"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(missing_words)
        .collect();
    if !missing.is_empty() {
        out.push(format!("Missing: {}", missing.join(", ")));
    }
    if let Some(n) = v["next"].as_str().filter(|n| !n.trim().is_empty()) {
        out.push(format!("Next: {n}"));
    }
    out
}

fn failure(json: bool, e: ApiError) -> i32 {
    fail(json, &e.code, &e.message, &e.next)
}

/// `ready`, or `live` after a publish (R21): the backend is done with it.
pub(crate) fn finished(state: &Value) -> bool {
    state == "ready" || state == "live"
}

/// What to do after `cmd` (`up`, `publish`, `rollback`), by state, unless the backend said.
/// While it builds the answer is `status`, never the command again: a second upload of the
/// same commit would move the preview past its evidence (R15).
fn next_for(cmd: &str, r: &Value) -> String {
    if let Some(n) = r["error"]["next"].as_str().filter(|n| !n.is_empty()) {
        return n.into();
    }
    let live = r["live_url"]
        .as_str()
        .map_or(String::new(), |u| format!(" at {u}"));
    match (cmd, r["state"].as_str()) {
        (_, Some("awaiting_approval")) => {
            format!("the owner approves the cards in Inbox, then run lightbulb app {cmd} again")
        }
        (_, Some("provisioning" | "building")) => {
            "it is still building; run lightbulb app status to see when it is done".into()
        }
        ("publish", Some("ready" | "live")) => {
            format!("it is live{live}; tell the room what changed")
        }
        ("rollback", Some("ready" | "live")) => {
            format!("the earlier version is live{live}; the data is unchanged")
        }
        (_, Some("ready" | "live")) => {
            "open the preview and check it works, then record it with lightbulb app evidence".into()
        }
        _ => format!("fix what the error says, then run lightbulb app {cmd} again"),
    }
}

/// Still building when the wait gave up.
fn building(r: &Value) -> bool {
    matches!(r["state"].as_str(), Some("building" | "provisioning"))
}

/// The result's printed form and exit code. A `failed` result is a failure (1):
/// `{code, message, next}` from the backend's error, else what to do about a failed build.
/// `up` or `publish` still building after the wait is a `timeout` (2): not done, not failed (R25).
fn show(r: &Value, cmd: &str, extra: Value, json: bool) -> i32 {
    if cmd != "rollback" && building(r) {
        let (what, end) = if cmd == "up" {
            ("The preview", "the preview")
        } else {
            ("The new version", "the publish")
        };
        fail(
            json,
            "timeout",
            &format!("{what} is still building after 10 minutes."),
            &format!("run `lightbulb app status`; {end} may still finish"),
        );
        return 2;
    }
    if r["state"] == "failed" {
        let e = &r["error"];
        let said = |k: &str| e[k].as_str().filter(|s| !s.is_empty());
        return fail(
            json,
            said("code").unwrap_or("build_failed"),
            said("message").unwrap_or("The build failed."),
            said("next").unwrap_or("run lightbulb app status --errors to see why the build failed"),
        );
    }
    let mut out = contract(r);
    for (k, v) in extra.as_object().into_iter().flatten() {
        out[k] = v.clone();
    }
    print(&out, cmd, json);
    0
}

/// Packs HEAD and uploads it; returns `(source_id, head)`.
fn upload(
    root: &Path,
    auth: &Auth,
    config: &AppConfig,
    room: Option<&str>,
    expect: Option<&str>,
) -> std::result::Result<String, ApiError> {
    let (tgz, head) = archive(root)?;
    let sent = (|| {
        if expect.is_some_and(|e| e != head) {
            return Err(review_required(root));
        }
        let mut fields = vec![("name", config.name.as_str()), ("commit", head.as_str())];
        fields.extend(room.map(|r| ("room_id", r)));
        api::form(auth, "/source", &fields, Some(("archive", &tgz)))
    })();
    let _ = std::fs::remove_file(&tgz);
    sent?["source_id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| {
            ApiError::new(
                "bad_answer",
                "Lightbulb did not return a source id.",
                "try again",
            )
        })
}

/// The preview address `up` last saw (kept in app.json), if any.
fn last_preview(root: &Path) -> Option<String> {
    app_json(root)["preview_url"].as_str().map(String::from)
}

fn review_required(root: &Path) -> ApiError {
    let at = last_preview(root).map_or(String::new(), |u| format!(" at {u}"));
    ApiError::new(
        "review_required",
        "Publish needs a passing review recorded for this commit.",
        &format!("run the reviewer or ask your workspace agent to review the preview{at} (lightbulb app test-link --json gets past its door), then record it: lightbulb app evidence --kind review --verdict pass --file <screenshot> --note \"<what was checked>\""),
    )
}

/// Sets `--room` in app.json when given; returns the room to send.
fn room_for(root: &Path, room: Option<String>) -> Result<Option<String>> {
    let mut a = app_json(root);
    if let Some(r) = room {
        a["room_id"] = r.into();
        save_app_json(root, &a)?;
    }
    Ok(a["room_id"].as_str().map(String::from))
}

fn check_first(root: &Path, publish: bool, json: bool) -> std::result::Result<(), i32> {
    let r = check::run_check(root, publish).map_err(|e| failure(json, e))?;
    if r.ok {
        return Ok(());
    }
    let problems: Vec<String> = r
        .problems
        .iter()
        .map(|p| {
            let h = check::hint(root, p).map_or(String::new(), |h| format!("\nHint: {h}"));
            format!("{}:{}: {}{h}", p.file, p.line, p.message)
        })
        .collect();
    Err(fail(
        json,
        "check_failed",
        &problems.join("\n"),
        r.next.unwrap_or("fix"),
    ))
}

pub(crate) fn up(
    root: &Path,
    config: &AppConfig,
    auth: &Auth,
    approved: Option<String>,
    requested_by: Option<String>,
    room: Option<String>,
    json: bool,
) -> Result<i32> {
    let room = room_for(root, room)?;
    let existed = app_json(root)["app_id"].is_string();
    // One report of a broken tool is allowed per cycle; each `up` starts a new one.
    let counted = check::read_progress(root).and_then(|mut p| {
        p["up_cycle"] = (p["up_cycle"].as_u64().unwrap_or(0) + 1).into();
        check::write_progress(root, &p)
    });
    if let Err(e) = counted {
        return Ok(failure(json, e));
    }
    if let Err(code) = check_first(root, false, json) {
        return Ok(code);
    }
    let result = upload(root, auth, config, room.as_deref(), None).and_then(|source_id| {
        let mut body = json!({"name": config.name, "room_id": room, "services": config.services,
            "open_to": super::word(&config.open_to), "budget_usd": config.budget_usd,
            "source_id": source_id,
            "origin": {"kind": config.origin.kind, "ref": config.origin.r#ref}});
        if let Some(a) = approved {
            body["approved_destructive"] = a.into();
        }
        if let Some(p) = requested_by {
            body["requested_by"] = p.into();
        }
        api::call(auth, "POST", "/up", Some(&body))
    });
    let r = match result {
        Ok(r) => r,
        Err(e) => return Ok(failure(json, e)),
    };
    if let Some(id) = r["app_id"].as_str() {
        let mut a = app_json(root);
        a["app_id"] = id.into();
        save_app_json(root, &a)?;
    }
    let mut r = r;
    let id = r["app_id"].as_str().unwrap_or_default().to_string();
    if let Err(e) = wait(auth, &id, &mut r) {
        return Ok(failure(json, e));
    }
    if r["state"] == "failed" || finished(&r["state"]) {
        match check::count_build(root, r["state"] == "failed") {
            Ok(Some(next)) if r["state"] == "failed" => r["error"]["next"] = next.into(),
            Ok(_) => {}
            Err(e) => return Ok(failure(json, e)),
        }
    }
    if r["state"] == "failed" || building(&r) {
        return Ok(show(&r, "up", json!({}), json));
    }
    if let Some(u) = r["preview_url"].as_str() {
        let mut a = app_json(root);
        a["preview_url"] = u.into();
        save_app_json(root, &a)?;
    }
    let mut extra = if finished(&r["state"]) {
        let state = git_ok(root, &["rev-parse", "HEAD"])
            .and_then(|head| evidence::done(root, auth, &id, &head));
        match state {
            Ok(d) => json!({"done": d.done, "missing": d.missing, "next": d.next}),
            Err(e) => return Ok(failure(json, e)),
        }
    } else {
        json!({"done": false, "next": next_for("up", &r)})
    };
    if existed {
        if let Some(n) = settings_note(auth, &id, config) {
            extra["note"] = n.into();
        }
    }
    Ok(show(&r, "up", extra, json))
}

/// After the first `up` the backend's record of who can open the app and its budget wins
/// (they change in Canvas); says so when app.yaml has drifted from it.
fn settings_note(auth: &Auth, id: &str, config: &AppConfig) -> Option<&'static str> {
    let s = api::call(auth, "GET", &format!("/{id}/status"), None).ok()?;
    let open_to = s["open_to"].as_str()?;
    let budget = s["budget_usd"].as_f64()?;
    (open_to != super::word(&config.open_to) || budget != f64::from(config.budget_usd)).then_some(
        "Sharing and budget changes go through Canvas; app.yaml changes to them are ignored after the first up.",
    )
}

/// While `r` is building, polls the status (2, 4, 8, 15, 15... s, up to 10 minutes) until it no
/// longer is; `LIGHTBULB_APP_POLL_SCALE_MS` is the unit of those seconds (1000).
/// The backend never waits inside a request, so the waiting is here.
fn wait(auth: &Auth, id: &str, r: &mut Value) -> std::result::Result<(), ApiError> {
    if !matches!(r["state"].as_str(), Some("building" | "provisioning")) {
        return Ok(());
    }
    let unit: u64 = std::env::var("LIGHTBULB_APP_POLL_SCALE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let mut waited = 0;
    for secs in [2, 4, 8].into_iter().chain(std::iter::repeat(15)) {
        if waited + secs > 600 {
            break;
        }
        eprintln!(
            "The app is {}; checking again in {secs}s.",
            r["state"].as_str().unwrap_or("building")
        );
        std::thread::sleep(std::time::Duration::from_millis(secs * unit));
        waited += secs;
        let s = api::call(auth, "GET", &format!("/{id}/status"), None)?;
        for k in ["state", "preview_url", "live_url"] {
            if !s[k].is_null() {
                r[k] = s[k].clone();
            }
        }
        if !matches!(r["state"].as_str(), Some("building" | "provisioning")) {
            break;
        }
    }
    Ok(())
}

fn app_id(root: &Path, json: bool) -> std::result::Result<String, i32> {
    app_json(root)["app_id"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| {
            fail(
                json,
                "not_up_yet",
                "This app has not been built yet.",
                "run lightbulb app up",
            )
        })
}

pub(crate) fn publish(
    root: &Path,
    config: &AppConfig,
    auth: &Auth,
    reason: Option<String>,
    approved: Option<String>,
    room: Option<String>,
    json: bool,
) -> Result<i32> {
    let room = room_for(root, room)?;
    let id = match app_id(root, json) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    if let Err(code) = check_first(root, true, json) {
        return Ok(code);
    }
    let head = match git_ok(root, &["rev-parse", "HEAD"]) {
        Ok(h) if has_passing_review(root, &h) => h,
        Ok(_) => return Ok(failure(json, review_required(root))),
        Err(e) => return Ok(failure(json, e)),
    };
    let result = migrations::gate(root).and_then(|g| {
        // Changes the archive commits were never reviewed: the review must name the commit sent.
        let source_id = upload(root, auth, config, room.as_deref(), Some(&head))?;
        let mut body = json!({"source_id": source_id});
        if let Some(r) = reason {
            body["reason"] = r.into();
        }
        if let Some(a) = approved {
            body["approved_destructive"] = a.into();
        }
        let mut r = api::call(auth, "POST", &format!("/{id}/publish"), Some(&body))?;
        wait(auth, &id, &mut r)?;
        // Only a finished publish applied the migrations; one still building records nothing.
        if finished(&r["state"]) {
            migrations::record(root, &g.unrecorded)?;
        }
        Ok(r)
    });
    match result {
        Ok(r) => Ok(show(
            &r,
            "publish",
            json!({"next": next_for("publish", &r)}),
            json,
        )),
        Err(e) => Ok(failure(json, e)),
    }
}

pub(crate) fn rollback(root: &Path, auth: &Auth, version: Option<u32>, json: bool) -> Result<i32> {
    let id = match app_id(root, json) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    // Restoring the files would overwrite unsaved work: stop before anything changes.
    match git_ok(root, &["status", "--porcelain"]) {
        Ok(d) if d.is_empty() => {}
        Ok(_) => {
            return Ok(fail(
                json,
                "dirty_tree",
                "There are changes that are not committed.",
                "commit or discard your changes, then roll back",
            ))
        }
        Err(e) => return Ok(failure(json, e)),
    }
    // The version and its commit come from the version list, before anything changes.
    let target = api::call(auth, "GET", &format!("/{id}/status"), None).and_then(|s| {
        let n = version
            .map(u64::from)
            .or_else(|| s["live_version"].as_u64().and_then(|l| l.checked_sub(1)));
        n.and_then(|n| {
            let commit = s["versions"]
                .as_array()?
                .iter()
                .find(|v| v["n"].as_u64() == Some(n))?["commit"]
                .as_str()?;
            Some((n, commit.to_string()))
        })
        .ok_or_else(|| {
            ApiError::new(
                "unknown_version",
                "That version does not exist.",
                "run lightbulb app status to see the versions",
            )
        })
    });
    let (n, target) = match target {
        Ok(t) => t,
        Err(e) => return Ok(failure(json, e)),
    };
    let r = match api::call(
        auth,
        "POST",
        &format!("/{id}/rollback"),
        Some(&json!({"version": n})),
    ) {
        Ok(r) => r,
        Err(e) => return Ok(failure(json, e)),
    };
    if r["state"] == "failed" {
        return Ok(show(&r, "rollback", json!({}), json));
    }
    if let Err(e) = restore(root, &target, n) {
        return Ok(failure(json, e));
    }
    Ok(show(
        &r,
        "rollback",
        json!({"next": next_for("rollback", &r)}),
        json,
    ))
}

/// One new commit on top whose tree is `target`'s. HEAD never moves backwards.
fn restore(root: &Path, target: &str, n: u64) -> std::result::Result<(), ApiError> {
    git_ok(root, &["read-tree", "-u", "--reset", target]).map_err(|e| {
        ApiError::new(
            "restore_failed",
            &format!(
                "The live app was rolled back, but git could not restore the files: {}",
                e.message
            ),
            "run lightbulb app status",
        )
    })?;
    if git(root, &["diff", "--cached", "--quiet"])?
        .status
        .success()
    {
        return Ok(());
    }
    commit(root, &format!("lightbulb: roll back to version {n}"))
}

pub(crate) fn destroy(root: &Path, auth: &Auth, json: bool) -> Result<i32> {
    let id = match app_id(root, json) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    match api::call(auth, "POST", &format!("/{id}/destroy"), Some(&json!({}))) {
        Ok(r) => {
            let v = json!({"state": r["state"], "next": r["next"]});
            if json {
                println!("{v}");
            } else {
                match r["state"].as_str() {
                    Some("awaiting_owner") => println!(
                        "Removal: waiting for the owner to confirm in Canvas. Nothing is removed until they do."
                    ),
                    s => println!("Removal: {}", s.unwrap_or("asked")),
                }
                if let Some(n) = r["next"].as_str().filter(|n| !n.trim().is_empty()) {
                    println!("Next: {n}");
                }
            }
            Ok(0)
        }
        Err(e) => Ok(failure(json, e)),
    }
}

/// `lightbulb app test-link`: prints the preview's builder link. It holds a pass, so it is only
/// printed: never written to `.lightbulb/`, never logged.
pub(crate) fn test_link(root: &Path, auth: &Auth, json: bool) -> Result<i32> {
    let id = match app_id(root, json) {
        Ok(id) => id,
        Err(code) => return Ok(code),
    };
    let r = match api::call(auth, "POST", &format!("/{id}/test-link"), Some(&json!({}))) {
        Ok(r) => r,
        Err(e) => return Ok(failure(json, e)),
    };
    let Some(url) = r["url"].as_str() else {
        return Ok(fail(
            json,
            "no_url",
            "Lightbulb did not return a link.",
            "try again in a minute",
        ));
    };
    if json {
        println!("{}", json!({ "url": url }));
    } else {
        println!("{url}");
    }
    Ok(0)
}

fn pick(v: &Value, keys: &[&str]) -> Value {
    let mut out = json!({});
    for k in keys {
        if !v[*k].is_null() {
            out[*k] = v[*k].clone();
        }
    }
    out
}

fn pick_each(v: &Value, keys: &[&str]) -> Value {
    json!(v
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| pick(x, keys))
        .collect::<Vec<_>>())
}

/// The contract's fields of an `AppStatus`, nothing else.
pub(crate) fn status_contract(v: &Value) -> Value {
    let mut out = pick(
        v,
        &[
            "app_id",
            "name",
            "room_id",
            "state",
            "open_to",
            "budget_usd",
            "preview_url",
            "live_url",
            "canvas_url",
            "live_version",
            "last_opened_at",
            "spend_month_cents",
        ],
    );
    if v["origin"].is_object() {
        out["origin"] = pick(&v["origin"], &["kind", "ref"]);
    }
    if v["preview"].is_object() {
        out["preview"] = pick(&v["preview"], &["deployment_id", "commit", "ready_at"]);
    }
    out["versions"] = pick_each(
        &v["versions"],
        &["n", "commit", "deployment_id", "at", "by"],
    );
    out["resources"] = pick_each(&v["resources"], &["provider", "service", "state"]);
    out
}

/// The last errors: only when and what.
pub(crate) fn errors_contract(v: &Value) -> Value {
    json!({"client": pick_each(&v["client"], &["at", "message"]),
        "server": pick_each(&v["server"], &["at", "message"])})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_in_words_says_preview_sample_data_and_what_is_missing() {
        let v = json!({"state": "ready", "preview_url": "https://p.example", "sample_data": true,
            "canvas_url": "https://c.example", "cards": [], "done": false,
            "missing": ["browser_evidence"], "next": "open it"});
        assert_eq!(
            human(&v, "up"),
            [
                "Preview: https://p.example (sample data)",
                "Canvas: https://c.example",
                "Done: no",
                "Missing: a person checking the preview in the browser",
                "Next: open it",
            ]
        );
    }

    #[test]
    fn publish_in_words_says_live_and_never_an_empty_next() {
        let v = json!({"state": "ready", "live_url": "https://l.example", "version": 3,
            "error": {"code": "code_only", "message": "Data is unchanged."}, "next": ""});
        assert_eq!(
            human(&v, "publish"),
            [
                "Live: https://l.example (version 3)",
                "Note: Data is unchanged."
            ]
        );
        let waiting = json!({"state": "awaiting_approval",
            "cards": [{"provider": "stripe", "service": "payments", "state": "pending"}]});
        assert_eq!(
            human(&waiting, "up"),
            [
                "Preview: waiting for the owner to approve the cards in Inbox",
                "Approval card: stripe payments (pending)",
            ]
        );
        for l in human(&v, "publish").iter().chain(&human(&waiting, "up")) {
            assert!(!l.contains("ready") && !l.contains('_'), "{l}");
        }
    }
}
