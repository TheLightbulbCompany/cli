//! `lightbulb app check`: runs the app's own `pnpm run check`, turns what it prints (tsc, ESLint,
//! the design lint) into problems, and counts repeats so a builder that keeps failing the same
//! way is told to diagnose, then to undo (spec §5).
//!
//! The TypeScript diagnostic parser is derived from Dyad (https://github.com/dyad-sh/dyad), commit
//! 0ee4a524, src/ipc/processors/tsc.ts (`parseTypeScriptDiagnosticsDetailed`). Copyright (c) Dyad
//! contributors. Apache-2.0. Modified by The Lightbulb Company. (Rust port of the line parser;
//! Windows paths, snippets and the CLI worker are left out.)

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

impl CheckResult {
    fn to_json(&self) -> Value {
        let problems: Vec<Value> = self
            .problems
            .iter()
            .map(|p| {
                json!({"file": p.file, "line": p.line, "column": p.column,
                "code": p.code, "message": p.message})
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
    let design = regex::Regex::new(r"^([^\s:()]+):(\d+): (.+)$").unwrap();
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

/// Runs the check in `root`, updates `.lightbulb/progress.json` and returns what to do next.
/// Prints nothing.
pub(crate) fn run_check(root: &Path, publish: bool) -> std::result::Result<CheckResult, ApiError> {
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
    let ok = out.status.success();
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut problems = if ok { vec![] } else { parse(&text, root) };
    if !ok && problems.is_empty() {
        // It failed in a way no parser knows (an audit, a build): keep the tail as the problem.
        let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = tail[tail.len().saturating_sub(10)..].join("\n");
        problems.push(Problem {
            file: "package.json".into(),
            line: 1,
            column: None,
            code: Some(script.into()),
            message: tail,
        });
    }
    let next = update_progress(root, ok, publish, &problems).map_err(|e| {
        ApiError::new(
            "progress_failed",
            &e.to_string(),
            "check .lightbulb is writable",
        )
    })?;
    Ok(CheckResult { ok, problems, next })
}

/// Counts this run's problems in `progress.json` (other keys stay) and picks `next`.
fn update_progress(
    root: &Path,
    ok: bool,
    publish: bool,
    problems: &[Problem],
) -> Result<Option<&'static str>> {
    let path = root.join(".lightbulb/progress.json");
    let mut progress: Value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
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
    std::fs::create_dir_all(root.join(".lightbulb"))?;
    std::fs::write(&path, progress.to_string())?;
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
            let v = r.to_json();
            if json {
                println!("{v}");
            } else if r.ok {
                println!("Check passed.");
            } else {
                for p in &r.problems {
                    println!("{}:{}: {}", p.file, p.line, p.message);
                }
                println!("Next: {}", r.next.unwrap_or(""));
            }
            Ok(if r.ok { 0 } else { 1 })
        }
    }
}
