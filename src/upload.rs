//! Uploading captured sessions, by the person's choice (`capture_upload`: `off`, `metadata` or
//! `full`). The daemon sends the closed sessions since the last upload every few minutes, signed
//! with this computer's identity (`mcp::rest`), and moves the watermark (`upload.mark`, unix
//! seconds) only after a 2xx. `off` makes no request at all. Nothing here logs a row or a summary.

use crate::capture::{self, SessionRow, ToolCallRow};
use crate::daemon::Daemon;
use crate::{config::Config, login, mcp, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const MARK: &str = "upload.mark";
const MAX_SESSIONS: usize = 200;
const MAX_CALLS: usize = 5000;
/// (tests) Milliseconds between uploads; debug builds only.
const EVERY_ENV: &str = "LIGHTBULB_CONNECT_DEBUG_UPLOAD_MS";

pub const MODES: [&str; 3] = ["off", "metadata", "full"];

/// What each mode sends, said once to the person.
pub fn describe(mode: &str) -> &'static str {
    match mode {
        "metadata" => "From now on, finished sessions are sent to Lightbulb signed in as you: the agent, repository, times and token counts, and which tools ran, without what the tools did.",
        "full" => "From now on, finished sessions are sent to Lightbulb signed in as you: the agent, repository, times and token counts, and for each tool call its name and a short summary (never file contents, diffs, command output or prompts).",
        _ => "Nothing leaves this computer. Sessions stay captured here only.",
    }
}

/// Sets `capture_upload`. Turning uploads on starts the watermark at now (only sessions that
/// finish from here on go); turning them off forgets it.
pub fn set_mode(dir: &Path, mode: &str) -> Result<()> {
    Config::set(dir, "capture_upload", mode)?;
    let mark = dir.join(MARK);
    if mode == "off" {
        let _ = std::fs::remove_file(mark);
    } else if !mark.exists() {
        std::fs::write(mark, crate::daemon::ipc::now().to_string())?;
    }
    Ok(())
}

/// Runs for the daemon's life.
pub async fn run(d: Arc<Daemon>) {
    let every = crate::cli::test_env(EVERY_ENV)
        .and_then(|v| v.to_str()?.parse().ok())
        .map_or(Duration::from_secs(300), Duration::from_millis);
    let mut failing = false;
    loop {
        tokio::time::sleep(every).await;
        let d2 = d.clone();
        let r = tokio::task::spawn_blocking(move || tick(&d2)).await;
        match r {
            Ok(Ok(())) => failing = false,
            Ok(Err(e)) => {
                if !failing {
                    eprintln!("upload: {e}"); // once; the next tick retries
                }
                failing = true;
            }
            Err(_) => {}
        }
    }
}

/// The wire form of an id: the server keeps it as a key, `[A-Za-z0-9._-]` only.
fn wire_id(id: &str) -> String {
    let ok = |c: char| c.is_ascii_alphanumeric() || "._-".contains(c);
    id.chars()
        .map(|c| if ok(c) { c } else { '_' })
        .take(128)
        .collect()
}

fn iso(secs: i64) -> String {
    format!(
        "{}:{:02}Z",
        crate::date::utc(secs)[..16].replace(' ', "T"),
        secs.rem_euclid(60)
    )
}

fn session_json(s: &SessionRow) -> Value {
    json!({
        "id": wire_id(&s.id), "harness": s.harness, "computer": s.computer,
        "repo": s.repo.clone().unwrap_or_default(), "branch": s.branch.clone().unwrap_or_default(),
        "started": iso(s.started), "ended": s.ended.map(iso),
        "prompt_tokens": s.prompt_tokens.unwrap_or(0), "completion_tokens": s.completion_tokens.unwrap_or(0),
    })
}

fn call_json(c: &ToolCallRow, full: bool) -> Value {
    let mut v = json!({
        "session_id": wire_id(&c.session_id), "seq": c.seq, "at": iso(c.at), "tool": c.tool,
        "duration_ms": c.duration_ms.unwrap_or(0), "ok": c.ok,
    });
    if full {
        v["summary"] = c.summary.clone().into();
    }
    v
}

/// One upload round: nothing when signed out, ended or `off`; else one batch, and the watermark
/// moves after its 2xx. ponytail: a batch is at most 200 sessions and 5000 calls; the rest go on
/// the following ticks, and a single session over 5000 calls is cut at 5000.
fn tick(d: &Daemon) -> Result<()> {
    let mode = Config::load(&d.dir)?.capture_upload;
    if mode == "off" || !MODES.contains(&mode.as_str()) {
        return Ok(());
    }
    let now = crate::daemon::ipc::now() as i64;
    if login::stored_proof(&d.dir).is_none() || login::ended(&d.dir, now) {
        return Ok(());
    }
    let mark = d.dir.join(MARK);
    let Some(after) = std::fs::read_to_string(&mark)
        .ok()
        .and_then(|s| s.trim().parse().ok())
    else {
        // Turned on without `sessions upload` (config edited): from now on.
        std::fs::write(&mark, now.to_string())?;
        return Ok(());
    };
    let full = mode == "full";
    let (sessions, calls, next) = {
        let store = d.store.lock().unwrap();
        let all: Vec<SessionRow> = capture::sessions_since(&store, after)?
            .into_iter()
            .filter(|s| s.ended.is_some())
            .collect();
        let (mut sessions, mut calls) = (vec![], vec![]);
        for s in &all {
            let c = capture::tool_calls(&store, &s.id, full)?;
            if !sessions.is_empty()
                && (sessions.len() == MAX_SESSIONS || calls.len() + c.len() > MAX_CALLS)
            {
                break;
            }
            calls.extend(c);
            sessions.push(s.clone());
        }
        calls.truncate(MAX_CALLS);
        let cut = sessions.len() < all.len();
        let last = sessions.last().map(|s| s.started);
        let newest = sessions
            .iter()
            .map(|s| s.ended.unwrap_or(s.started).max(s.started))
            .max();
        // Cut short: resume at the last sent session's start; else past the newest end.
        let next = if cut { last.map(|t| t - 1) } else { newest };
        (sessions, calls, next)
    };
    let Some(next) = next else { return Ok(()) };
    let body = json!({
        "mode": mode,
        "sessions": sessions.iter().map(session_json).collect::<Vec<_>>(),
        "tool_calls": calls.iter().map(|c| call_json(c, full)).collect::<Vec<_>>(),
    });
    mcp::rest("POST", "/sessions", Some(&body), "upload").map_err(|e| {
        format!(
            "could not upload ({}); retrying",
            e["error"].as_str().unwrap_or("failed")
        )
    })?;
    std::fs::write(mark, next.max(after).to_string())?;
    Ok(())
}
