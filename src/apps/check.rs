//! `lightbulb app check`: runs the app's own `pnpm run check`, turns what it prints (tsc, ESLint,
//! the design lint) into problems, and counts repeats so a builder that keeps failing the same
//! way is told to diagnose, then to undo (spec §5).
//!

// Derived from Dyad (https://github.com/dyad-sh/dyad), commit 0ee4a524, src/ipc/processors/tsc.ts. Copyright (c) Dyad contributors. Apache-2.0. Modified by The Lightbulb Company.
// (Rust port of the tsc line parser only; Windows paths, snippets and the CLI worker are left out.)

use super::api::ApiError;
use super::{fail, preflight};
use crate::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

const DIAGNOSE: &str =
    "diagnose: list what you tried, find the root cause, consider a stronger model for a fresh plan";
const UNDO: &str = "undo: run lightbulb app rollback or git revert, then tell the room what failed";

/// One thing to fix. ESLint and design-lint lines have no TS code; the design lint has no column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub file: String,
    pub line: u32,
    pub column: Option<u32>,
    pub code: Option<String>,
    pub message: String,
}

#[derive(Debug)]
pub struct CheckResult {
    pub ok: bool,
    pub problems: Vec<Problem>,
    /// What the builder does about it; none when the check passed.
    pub next: Option<&'static str>,
}

/// Entries of a lessons file: a `## `/`### ` heading with the lines under it, or a `- **Title.**`
/// bullet with its indented continuation lines. Returns (title, body).
fn entries(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for line in text.lines() {
        let heading = line
            .strip_prefix("### ")
            .or_else(|| line.strip_prefix("## "));
        let bullet = line
            .strip_prefix("- **")
            .and_then(|r| r.split_once("**"))
            .map(|(t, _)| t);
        if let Some(t) = heading.or(bullet) {
            let t = t.trim_matches(|c: char| c == '.' || c == '"' || c == '`' || c.is_whitespace());
            let body = if bullet.is_some() { line } else { "" };
            out.push((t.to_lowercase(), body.to_string()));
        } else if let Some((_, body)) = out.last_mut() {
            body.push('\n');
            body.push_str(line);
        }
    }
    out.into_iter()
        .map(|(t, b)| (t, b.trim().to_string()))
        .collect()
}

/// The entry of `.lightbulb/known-fixes.md` or `lessons.md` that matches `p`, if any. A title
/// matches when it is a substring of the problem's code or message (case-insensitive), or contains
/// the code (`TS2307 Cannot find module`). Missing files give no hint.
pub(crate) fn hint(root: &Path, p: &Problem) -> Option<String> {
    let code = p.code.as_deref().unwrap_or("").to_lowercase();
    let message = p.message.to_lowercase();
    ["known-fixes.md", "lessons.md"].iter().find_map(|f| {
        let text = std::fs::read_to_string(root.join(".lightbulb").join(f)).ok()?;
        entries(&text).into_iter().find_map(|(t, body)| {
            // A short or generic title would match every problem.
            let hit = t.len() >= 4
                && t != "broken tools"
                && (code.contains(&t)
                    || message.contains(&t)
                    || (!code.is_empty() && t.contains(&code)));
            (hit && !body.is_empty()).then_some(body)
        })
    })
}

impl CheckResult {
    fn to_json(&self, root: &Path) -> Value {
        let problems: Vec<Value> = self
            .problems
            .iter()
            .map(|p| {
                let mut v = json!({"file": p.file, "line": p.line, "column": p.column,
                "code": p.code, "message": p.message});
                if let Some(h) = hint(root, p) {
                    v["hint"] = h.into();
                }
                v
            })
            .collect();
        json!({"ok": self.ok, "problems": problems, "next": self.next})
    }
}

fn relative(root: &Path, file: &str) -> String {
    Path::new(file)
        .strip_prefix(root)
        .map_or(file.to_string(), |p| p.to_string_lossy().into_owned())
}

/// The problems in the output of `pnpm run check`.
pub fn parse(output: &str, root: &Path) -> Vec<Problem> {
    let tsc = regex::Regex::new(r"^(.*)\((\d+),(\d+)\): error TS(\d+):\s*(.*)$").unwrap();
    let tsc_global = regex::Regex::new(r"^error TS(\d+):\s*(.*)$").unwrap();
    let eslint = regex::Regex::new(r"^\s+(\d+):(\d+)\s+error\s+(.*?)(?:\s{2,}(\S+))?\s*$").unwrap();
    let design = regex::Regex::new(r"^(.+?):(\d+): (.+)$").unwrap();
    let mut problems: Vec<Problem> = vec![];
    // The last tsc problem (continuation lines extend it) and the last ESLint file header.
    let mut tsc_open = false;
    let mut eslint_file = String::new();
    for raw in output.replace("\r\n", "\n").lines() {
        if raw.trim().is_empty() {
            continue;
        }
        let at = |c: &regex::Captures, i: usize| c[i].parse().unwrap();
        if let Some(c) = tsc.captures(raw) {
            problems.push(Problem {
                file: relative(root, &c[1]),
                line: at(&c, 2),
                column: Some(at(&c, 3)),
                code: Some(format!("TS{}", &c[4])),
                message: c[5].to_string(),
            });
            tsc_open = true;
        } else if let Some(c) = tsc_global.captures(raw) {
            problems.push(Problem {
                file: "tsconfig.json".into(),
                line: 1,
                column: Some(1),
                code: Some(format!("TS{}", &c[1])),
                message: c[2].to_string(),
            });
            tsc_open = true;
        } else if let Some(c) = eslint.captures(raw).filter(|_| !eslint_file.is_empty()) {
            problems.push(Problem {
                file: eslint_file.clone(),
                line: at(&c, 1),
                column: Some(at(&c, 2)),
                code: c.get(4).map(|m| m.as_str().to_string()),
                message: c[3].to_string(),
            });
            tsc_open = false;
        } else if raw.starts_with(char::is_whitespace) {
            // Indented text after a tsc error continues its message.
            if let (true, Some(p)) = (tsc_open, problems.last_mut()) {
                p.message.push('\n');
                p.message.push_str(raw.trim_end());
            }
        } else if let Some(c) = design.captures(raw) {
            problems.push(Problem {
                file: relative(root, &c[1]),
                line: at(&c, 2),
                column: None,
                code: None,
                message: c[3].to_string(),
            });
            tsc_open = false;
            eslint_file.clear();
        } else {
            // A non-indented line is the next ESLint file header (or noise nothing follows).
            tsc_open = false;
            eslint_file = relative(root, raw.trim());
        }
    }
    problems
}

fn signature(p: &Problem) -> String {
    let head: String = p.message.chars().take(80).collect();
    let text = format!("{}{}{head}", p.code.as_deref().unwrap_or(""), p.file);
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const DOOR_FILES: [&str; 2] = ["src/proxy.ts", "src/lib/door.ts"];
const DOOR_HASH: &str = ".lightbulb/door.sha256";

/// Hash of the door files as they are now; none when one is missing.
pub(crate) fn door_hash(root: &Path) -> Option<String> {
    let mut h = Sha256::new();
    for f in DOOR_FILES {
        h.update(std::fs::read(root.join(f)).ok()?);
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Records the door files as the starter shipped them (`app new`).
pub(crate) fn record_door(root: &Path) -> std::io::Result<()> {
    match door_hash(root) {
        Some(h) => std::fs::write(root.join(DOOR_HASH), h),
        None => Ok(()),
    }
}

/// A problem when the door files differ from the recorded hash. Apps made before the hash was
/// recorded have none, and are never failed.
fn door_problem(root: &Path) -> Option<Problem> {
    let want = std::fs::read_to_string(root.join(DOOR_HASH)).ok()?;
    (door_hash(root).as_deref() != Some(want.trim())).then(|| Problem {
        file: DOOR_FILES[0].into(),
        line: 1,
        column: None,
        code: Some("door".into()),
        message: "src/proxy.ts or src/lib/door.ts was changed. They decide who can open the app; \
            restore them with: git checkout $(git rev-list --max-parents=0 HEAD) -- src/proxy.ts src/lib/door.ts"
            .into(),
    })
}

/// Counts a failed build of `up` in `progress.json` (`build_failures`), apart from the local
/// problems so a passing check does not clear it; a ready build clears it. Returns the step to
/// tell the builder once it has failed twice or more.
pub(crate) fn count_build(
    root: &Path,
    failed: bool,
) -> std::result::Result<Option<&'static str>, ApiError> {
    let mut progress = read_progress(root)?;
    let n = if failed {
        progress["build_failures"].as_u64().unwrap_or(0) + 1
    } else {
        0
    };
    progress["build_failures"] = n.into();
    write_progress(root, &progress)?;
    Ok(match n {
        0 | 1 => None,
        2 => Some(DIAGNOSE),
        _ => Some(UNDO),
    })
}

/// Runs the check in `root`, updates `.lightbulb/progress.json` and returns what to do next.
/// Prints nothing.
pub(crate) fn run_check(root: &Path, publish: bool) -> std::result::Result<CheckResult, ApiError> {
    // Not the app's fault, so not counted toward diagnose/undo.
    if !root.join("node_modules").is_dir() {
        return Err(ApiError::new(
            "not_installed",
            "The app's packages are not installed.",
            "run pnpm install, then try again",
        ));
    }
    let script = if publish { "check:publish" } else { "check" };
    let out = std::process::Command::new("pnpm")
        .args(["run", script])
        .current_dir(root)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ApiError::new(
                    "tool_missing",
                    "pnpm is not installed on this computer.",
                    "install pnpm (corepack enable, or npm install -g pnpm), then run the check again",
                )
            } else {
                ApiError::new("check_failed", &format!("Could not run pnpm: {e}"), "try again")
            }
        })?;
    let mut ok = out.status.success();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut problems = if ok { vec![] } else { parse(&text, root) };
    if !ok && problems.is_empty() {
        // It failed in a way no parser knows (an audit, a build): the last line is the problem,
        // so the same failure signs the same and reaches `undo`.
        let last = text.lines().rev().find(|l| !l.trim().is_empty());
        problems.push(Problem {
            file: "package.json".into(),
            line: 1,
            column: None,
            code: Some(script.into()),
            message: last
                .unwrap_or("the check failed with no output")
                .trim()
                .into(),
        });
    }
    if let Some(p) = door_problem(root) {
        ok = false;
        problems.push(p);
    }
    let next = update_progress(root, ok, publish, &problems)?;
    Ok(CheckResult { ok, problems, next })
}

fn bad_progress(why: String) -> ApiError {
    ApiError::new(
        "bad_progress",
        &format!("Could not use .lightbulb/progress.json: {why}"),
        "fix or delete .lightbulb/progress.json, then run the check again",
    )
}

/// `.lightbulb/progress.json` (`{}` when there is none). Evidence lives in this file: an
/// unreadable one is an error, never overwritten.
pub(crate) fn read_progress(root: &Path) -> std::result::Result<Value, ApiError> {
    match std::fs::read(root.join(".lightbulb/progress.json")) {
        Ok(b) => serde_json::from_slice(&b)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| bad_progress("it is not a JSON object".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(bad_progress(e.to_string())),
    }
}

pub(crate) fn write_progress(root: &Path, v: &Value) -> std::result::Result<(), ApiError> {
    let path = root.join(".lightbulb/progress.json");
    let tmp = path.with_extension("json.tmp");
    std::fs::create_dir_all(root.join(".lightbulb"))
        .and_then(|_| std::fs::write(&tmp, v.to_string()))
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| bad_progress(e.to_string()))
}

/// Counts this run's problems in `progress.json` (other keys stay) and picks `next`.
fn update_progress(
    root: &Path,
    ok: bool,
    publish: bool,
    problems: &[Problem],
) -> std::result::Result<Option<&'static str>, ApiError> {
    let mut progress = read_progress(root)?;
    let mut attempts = progress["attempts"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut worst = 0;
    if ok {
        attempts.clear();
    }
    let mut seen = std::collections::HashSet::new();
    for p in problems {
        let sig = signature(p);
        if !seen.insert(sig.clone()) {
            continue;
        }
        let n = attempts.get(&sig).and_then(Value::as_u64).unwrap_or(0) + 1;
        attempts.insert(sig, n.into());
        worst = worst.max(n);
    }
    progress["attempts"] = Value::Object(attempts);
    progress["last_check"] = json!({"ok": ok, "publish": publish, "problems": problems.len()});
    write_progress(root, &progress)?;
    Ok(match worst {
        0 => None,
        1 => Some("fix"),
        2 => Some(DIAGNOSE),
        _ => Some(UNDO),
    })
}

/// `lightbulb app check [--publish]`.
pub fn check(publish: bool, json: bool) -> Result<i32> {
    let root = match preflight(json) {
        Ok((root, _, _)) => root,
        Err(code) => return Ok(code),
    };
    match run_check(&root, publish) {
        Err(e) => Ok(fail(json, &e.code, &e.message, &e.next)),
        Ok(r) => {
            let v = r.to_json(&root);
            if json {
                println!("{v}");
            } else if r.ok {
                println!("Check passed.");
            } else {
                for p in &r.problems {
                    println!("{}:{}: {}", p.file, p.line, p.message);
                    if let Some(h) = hint(&root, p) {
                        println!("  Hint: {h}");
                    }
                }
                println!("Next: {}", r.next.unwrap_or(""));
            }
            Ok(if r.ok { 0 } else { 1 })
        }
    }
}
