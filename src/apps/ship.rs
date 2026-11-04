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

pub(crate) fn app_json(root: &Path) -> Value {
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

/// A passing review or `verify` for `head` (R10), in `.lightbulb/progress.json`.
pub fn has_passing_review(root: &Path, head: &str) -> bool {
    std::fs::read(root.join(".lightbulb/progress.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["evidence"].as_array().cloned())
        .is_some_and(|e| {
            e.iter().any(|r| {
                (r["kind"] == "review" || r["kind"] == "verify")
                    && r["commit"] == head
                    && r["verdict"] == "pass"
            })
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

/// What to do when only the starter's placeholder page has gone up.
pub(crate) const PLACEHOLDER_NEXT: &str = "the app is still the starter's placeholder page; build \
the app, then run lightbulb app up again";

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
    let base = base_commit(root);
    let sent = (|| {
        if expect.is_some_and(|e| e != head) {
            return Err(review_required(root));
        }
        let mut fields = vec![("name", config.name.as_str()), ("commit", head.as_str())];
        fields.extend(room.map(|r| ("room_id", r)));
        // Another writer's newer source makes the backend refuse this one (stale_base). A
        // folder that never sent or pulled says `new`: refused if the app already has source.
        fields.push(("base_commit", base.as_deref().unwrap_or("new")));
        // An untouched starter page is not the app: the backend must not call its preview ready.
        if super::placeholder(root) {
            fields.push(("placeholder", "true"));
        }
        api::form(auth, "/source", &fields, Some(("archive", &tgz)))
    })();
    let _ = std::fs::remove_file(&tgz);
    let source_id = sent?["source_id"].as_str().map(String::from);
    // What was sent is now the app's latest source: the next upload's base.
    if source_id.is_some() {
        let mut a = app_json(root);
        a["base_commit"] = head.into();
        save_app_json(root, &a)
            .map_err(|e| ApiError::new("write_failed", &e.to_string(), "check the folder"))?;
    }
    source_id.ok_or_else(|| {
        ApiError::new(
            "bad_answer",
            "Lightbulb did not return a source id.",
            "try again",
        )
    })
}

/// The app's latest source as this folder last saw it: what it last uploaded or pulled
/// (`up_commit` for an app sent before `base_commit` was recorded).
fn base_commit(root: &Path) -> Option<String> {
    let a = app_json(root);
    a["base_commit"]
        .as_str()
        .or(a["up_commit"].as_str())
        .map(String::from)
}

/// A new path in the temp folder.
fn temp(what: &str) -> PathBuf {
    std::env::temp_dir().join(format!("lightbulb-{what}-{}", ulid::Ulid::generate()))
}

/// `tar -xzf tgz -C dir`.
fn untar(tgz: &Path, dir: &Path) -> std::result::Result<(), ApiError> {
    std::fs::create_dir_all(dir)
        .ok()
        .and_then(|_| {
            Command::new("tar")
                .arg("-xzf")
                .arg(tgz)
                .arg("-C")
                .arg(dir)
                .status()
                .ok()
        })
        .filter(|s| s.success())
        .map(drop)
        .ok_or_else(|| {
            ApiError::new(
                "archive_failed",
                "Could not unpack the app's source.",
                "check that tar works, then try again",
            )
        })
}

/// `lightbulb app pull`: the app's latest source into `dir`. A new or empty folder becomes a
/// git repo with it as one commit. The app's own folder (clean) gets it as a commit on top of
/// the one it last sent or pulled (`up_commit`), merged into HEAD: a fast-forward when
/// nothing changed here since, else a merge of both. Its commit is recorded as the base.
pub(crate) fn pull(app: &str, dir: Option<PathBuf>, json: bool) -> Result<i32> {
    let (id, found) = match super::app_id_for(app, json) {
        Ok(f) => f,
        Err(code) => return Ok(code),
    };
    let dir = dir.or(found).unwrap_or_else(|| PathBuf::from(app));
    let fresh = !dir.exists() || (dir.is_dir() && std::fs::read_dir(&dir)?.next().is_none());
    let own = dir.join("app.yaml").is_file() && dir.join(".git").exists();
    if !fresh && !own {
        return Ok(fail(
            json,
            "dir_not_empty",
            &format!("{} is not empty and is not an app's folder.", dir.display()),
            "pick an empty folder, or the app's own folder",
        ));
    }
    if !fresh {
        match git_ok(&dir, &["status", "--porcelain"]) {
            Ok(d) if d.is_empty() => {}
            Ok(_) => {
                return Ok(fail(
                    json,
                    "dirty_tree",
                    "There are changes that are not committed.",
                    "commit your changes, then run lightbulb app pull again",
                ))
            }
            Err(e) => return Ok(failure(json, e)),
        }
    }
    // A name is resolved to the app's real id first, so a fresh folder records the id, not the name.
    let id = if fresh && !id.starts_with("app_") {
        match api::call(&Auth::detect(), "GET", &format!("/{id}/status"), None) {
            Ok(v) => v["app_id"].as_str().map(String::from).unwrap_or(id),
            Err(e) => return Ok(fail(json, &e.code, &e.message, &e.next)),
        }
    } else {
        id
    };
    let tgz = temp("pull.tar.gz");
    let result = api::download(&Auth::detect(), &format!("/{id}/source"), &tgz).and_then(|c| {
        if fresh {
            pull_fresh(&dir, &tgz, &id, &c).map(|_| (c, true))
        } else {
            pull_merge(&dir, &tgz, &c).map(|changed| (c, changed))
        }
    });
    let _ = std::fs::remove_file(&tgz);
    let (commit, changed) = match result {
        Ok(r) => r,
        Err(e) => return Ok(failure(json, e)),
    };
    let next = if fresh {
        format!(
            "cd {} && pnpm install, then change the app and run lightbulb app up",
            dir.display()
        )
    } else {
        "change the app, then run lightbulb app up".to_string()
    };
    if json {
        println!(
            "{}",
            json!({"app_id": id, "dir": dir, "commit": commit, "changed": changed, "next": next})
        );
    } else {
        let short = commit.get(..7).unwrap_or(&commit);
        if changed {
            println!("Pulled {app} (version {short}) into {}.", dir.display());
        } else {
            println!(
                "{} already has the latest version of {app} ({short}).",
                dir.display()
            );
        }
        println!("Next: {next}");
    }
    Ok(0)
}

fn pull_fresh(dir: &Path, tgz: &Path, id: &str, commit: &str) -> std::result::Result<(), ApiError> {
    untar(tgz, dir)?;
    git_ok(dir, &["init", "-q"])?;
    git_ok(dir, &["add", "-A"])?;
    self::commit(dir, &format!("lightbulb app pull: version {commit}"))?;
    let head = git_ok(dir, &["rev-parse", "HEAD"])?;
    // The backend took `id` for this app's source, so its other routes take it too.
    let a = json!({"app_id": id, "base_commit": commit, "up_commit": head});
    save_app_json(dir, &a)
        .map_err(|e| ApiError::new("write_failed", &e.to_string(), "check the folder"))
}

/// Whether anything changed.
fn pull_merge(dir: &Path, tgz: &Path, commit: &str) -> std::result::Result<bool, ApiError> {
    if base_commit(dir).as_deref() == Some(commit) {
        return Ok(false);
    }
    let a = app_json(dir);
    let parent = a["up_commit"]
        .as_str()
        .filter(|p| git_ok(dir, &["cat-file", "-e", &format!("{p}^{{commit}}")]).is_ok())
        .ok_or_else(|| {
            ApiError::new(
                "no_base",
                "This folder has no record of the version it last sent or pulled, so the latest version cannot be merged into it.",
                "pull into an empty folder: lightbulb app pull <app> <new folder>",
            )
        })?
        .to_string();
    // The latest source's tree, from a scratch index over the unpacked files.
    let files = temp("pull");
    let index = temp("pull-index");
    let tree = untar(tgz, &files).and_then(|_| {
        let indexed = |args: &[&str]| {
            Command::new("git")
                .current_dir(dir)
                .env("GIT_INDEX_FILE", &index)
                .arg(format!("--work-tree={}", files.display()))
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        };
        // Tracked files the archive leaves out by design (`.env.example`) are kept as they
        // were at `parent`; any other file missing from the archive was deleted there.
        let kept = |_: String| {
            let listed = indexed(&["ls-tree", "-r", "-z", &parent])?;
            listed
                .split('\0')
                .filter_map(|l| l.split_once('\t'))
                .filter(|(_, path)| !shipped(path))
                .try_for_each(|(meta, path)| {
                    let mut m = meta.split(' ');
                    let (mode, _, sha) = (m.next()?, m.next()?, m.next()?);
                    let info = format!("{mode},{sha},{path}");
                    indexed(&["update-index", "--add", "--cacheinfo", &info]).map(drop)
                })
        };
        indexed(&["add", "-A", "-f"])
            .and_then(kept)
            .and_then(|_| indexed(&["write-tree"]))
            .ok_or_else(|| {
                ApiError::new(
                    "git_failed",
                    "git could not read the latest source.",
                    "try again",
                )
            })
    });
    let _ = std::fs::remove_dir_all(&files);
    let _ = std::fs::remove_file(&index);
    let (tree, msg) = (tree?, format!("lightbulb app pull: version {commit}"));
    let mut args = identity(dir);
    args.extend(["commit-tree", tree.as_str(), "-p", &parent, "-m", &msg]);
    let latest = git_ok(dir, &args)?;
    let mut args = identity(dir);
    args.extend(["merge", "-q", "--no-edit", &latest]);
    if let Err(e) = git_ok(dir, &args) {
        let conflicts =
            git_ok(dir, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
        if conflicts.is_empty() {
            return Err(e);
        }
        return Err(ApiError::new(
            "merge_conflict",
            &format!(
                "The latest version and your changes conflict in: {}",
                conflicts.lines().collect::<Vec<_>>().join(", ")
            ),
            "fix the conflicts and commit, then run lightbulb app up",
        ));
    }
    // Merged: only now is the latest version this folder's base.
    let mut a = a;
    a["base_commit"] = commit.into();
    a["up_commit"] = latest.as_str().into();
    save_app_json(dir, &a)
        .map_err(|e| ApiError::new("write_failed", &e.to_string(), "check the folder"))?;
    Ok(true)
}

fn review_required(_root: &Path) -> ApiError {
    ApiError::new(
        "review_required",
        "Publish needs a passing check recorded for this commit.",
        "run lightbulb app verify, then publish again",
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
        // The commit this up sent (the archive committed everything first): status tells a build
        // still on its way from a change never sent.
        if let Ok(h) = git_ok(root, &["rev-parse", "HEAD"]) {
            a["up_commit"] = h.into();
        }
        save_app_json(root, &a)?;
    }
    let mut r = r;
    let id = r["app_id"].as_str().unwrap_or_default().to_string();
    // Only the starter's placeholder went up (the hosting card is filed early): the backend keeps
    // it building until the real app arrives, so waiting here would only time out.
    if building(&r) && super::placeholder(root) {
        let mut out = contract(&r);
        // No preview_url: nothing should send the agent to look at the placeholder.
        out.as_object_mut().map(|o| o.remove("preview_url"));
        out["done"] = false.into();
        out["next"] = PLACEHOLDER_NEXT.into();
        print(&out, "up", json);
        return Ok(0);
    }
    if let Err(e) = wait(auth, &id, &mut r) {
        return Ok(failure(json, e));
    }
    if r["state"] == "failed" || finished(&r["state"]) {
        match check::count_build(root, r["state"] == "failed") {
            Ok(Some(next)) if r["state"] == "failed" => {
                // Keep the backend's own next step; the diagnose or undo step follows it.
                r["error"]["next"] = match r["error"]["next"].as_str().filter(|s| !s.is_empty()) {
                    Some(own) => format!("{own} {next}"),
                    None => next.to_string(),
                }
                .into();
            }
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

pub(crate) fn app_id(root: &Path, json: bool) -> std::result::Result<String, i32> {
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
