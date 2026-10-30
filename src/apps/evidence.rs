//! `lightbulb app evidence` and the done gate: a preview is done only when it is for HEAD, ready,
//! healthy, and someone has looked at it since it came up (spec §5).

use super::api::{self, ApiError, Auth};
use super::ship::git_ok;
use super::{check, fail, EvidenceKind, Verdict};
use crate::{date, login, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(crate) struct DoneState {
    pub done: bool,
    /// What is still missing: `ready`, `preview`, `preview_commit`, `health`, `browser_evidence`.
    pub missing: Vec<&'static str>,
    pub next: String,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `secs` as `YYYY-MM-DDTHH:MM:SSZ`.
fn rfc3339(secs: i64) -> String {
    let d = date::utc(secs);
    format!("{}T{}:{:02}Z", &d[..10], &d[11..16], secs.rem_euclid(60))
}

/// `YYYY-MM-DDTHH:MM:SS[.fff](Z|+hh:mm)` as unix seconds.
fn parse(t: &str) -> Option<i64> {
    let n = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, m, d, h, mi, s) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    let mut rest = t.get(19..)?;
    if rest.starts_with('.') {
        rest = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    }
    let off = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            let (oh, om) = (
                rest.get(1..3)?.parse::<i64>().ok()?,
                rest.get(4..6)?.parse::<i64>().ok()?,
            );
            sign * (oh * 3600 + om * 60)
        }
    };
    // Days from a civil date (Howard Hinnant's `days_from_civil`).
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + mi * 60 + s - off)
}

fn status_of(auth: &Auth, id: &str) -> std::result::Result<Value, ApiError> {
    api::call(auth, "GET", &format!("/{id}/status"), None)
}

/// Whether the preview is done for `head`; `next` names the first missing piece.
pub(crate) fn done(
    root: &Path,
    auth: &Auth,
    id: &str,
    head: &str,
) -> std::result::Result<DoneState, ApiError> {
    done_with(root, &status_of(auth, id)?, head)
}

/// `done` for a status already in hand.
pub(crate) fn done_with(
    root: &Path,
    s: &Value,
    head: &str,
) -> std::result::Result<DoneState, ApiError> {
    let not = |missing: &'static str, next: String| {
        Ok(DoneState {
            done: false,
            missing: vec![missing],
            next,
        })
    };
    if !super::ship::finished(&s["state"]) {
        return not(
            "ready",
            "wait for the build to finish, then run lightbulb app status".into(),
        );
    }
    let p = &s["preview"];
    if !p.is_object() {
        return not(
            "preview",
            "the backend did not report the preview deployment".into(),
        );
    }
    if p["commit"] != head {
        return not(
            "preview_commit",
            "the preview is for an older version of the app; run lightbulb app up".into(),
        );
    }
    let Some(url) = p["url"].as_str().or(s["preview_url"].as_str()) else {
        return not(
            "preview",
            "the backend did not report the preview address".into(),
        );
    };
    // The address comes from the backend and goes to curl: http(s) and no control characters.
    if !(url.starts_with("http://") || url.starts_with("https://"))
        || url.chars().any(char::is_control)
    {
        return not(
            "preview",
            "the backend reported a preview address that is not a web address".into(),
        );
    }
    let url = url.trim_end_matches('/');
    let health = format!("{url}/api/_lightbulb/health");
    if !matches!(login::request_at(url, "GET", "/api/_lightbulb/health", None, &[]),
        Ok((200, v)) if v["commit"] == head)
    {
        return not(
            "health",
            format!("{health} did not answer 200 for this version; run lightbulb app status --errors to see why"),
        );
    }
    // The deployment's ready time; else when `up` first saw it ready for this commit.
    let mut progress = check::read_progress(root)?;
    let ready = match p["ready_at"].as_str().and_then(parse) {
        Some(t) => t,
        None => {
            if progress["preview_seen"]["commit"] != head
                || progress["preview_seen"]["deployment_id"] != p["deployment_id"]
            {
                progress["preview_seen"] = json!({"commit": head,
                    "deployment_id": p["deployment_id"], "at": rfc3339(now())});
                check::write_progress(root, &progress)?;
            }
            progress["preview_seen"]["at"]
                .as_str()
                .and_then(parse)
                .unwrap_or(0)
        }
    };
    let seen = progress["evidence"].as_array().is_some_and(|e| {
        e.iter().any(|r| {
            r["kind"] == "browser"
                && r["commit"] == head
                && r["verdict"] == "pass"
                && r["at"].as_str().and_then(parse).is_some_and(|t| t >= ready)
        })
    });
    if !seen {
        return not(
            "browser_evidence",
            format!("open {url} in the browser, check the main flow, then lightbulb app evidence --kind browser --file …"),
        );
    }
    Ok(DoneState {
        done: true,
        missing: vec![],
        next: "tell the room the preview is done and send them the link".into(),
    })
}

/// `lightbulb app evidence`: appends the record to `.lightbulb/progress.json`. The file stays
/// here; only its path and sha256 are kept.
pub(crate) fn record(
    root: &Path,
    auth: &Auth,
    kind: EvidenceKind,
    verdict: Option<Verdict>,
    file: &Path,
    note: &str,
    json: bool,
) -> Result<i32> {
    let verdict = match (kind, verdict) {
        (EvidenceKind::Review, None) => {
            return Ok(fail(
                json,
                "verdict_required",
                "A review needs a verdict.",
                "add --verdict pass or --verdict fail",
            ))
        }
        (_, Some(Verdict::Fail)) => "fail",
        _ => "pass",
    };
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(e) => {
            return Ok(fail(
                json,
                "bad_file",
                &format!("Could not read {}: {e}", file.display()),
                "check the path",
            ))
        }
    };
    let head = match git_ok(root, &["rev-parse", "HEAD"]) {
        Ok(h) => h,
        Err(e) => return Ok(fail(json, &e.code, &e.message, &e.next)),
    };
    if let Some(e) = in_repo(root, file) {
        return Ok(fail(json, &e.code, &e.message, &e.next));
    }
    // Offline or not up yet is fine: the deployment is just not named. A preview for another
    // commit is not the one that was looked at.
    let deployment = std::fs::read(root.join(".lightbulb/app.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|a| a["app_id"].as_str().map(String::from))
        .and_then(|id| status_of(auth, &id).ok())
        .filter(|s| s["preview"]["commit"] == head)
        .map(|s| s["preview"]["deployment_id"].clone())
        .unwrap_or(Value::Null);
    let sha: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let rec = json!({"kind": match kind { EvidenceKind::Browser => "browser", EvidenceKind::Review => "review" },
        "verdict": verdict, "commit": head, "deployment_id": deployment,
        "file": file.to_string_lossy(), "sha256": sha, "note": note, "at": rfc3339(now())});
    let saved = check::read_progress(root).and_then(|mut p| {
        let mut all = p["evidence"].as_array().cloned().unwrap_or_default();
        all.push(rec.clone());
        p["evidence"] = json!(all);
        check::write_progress(root, &p)
    });
    if let Err(e) = saved {
        return Ok(fail(json, &e.code, &e.message, &e.next));
    }
    if json {
        println!("{rec}");
    } else {
        println!(
            "Recorded a {} for commit {}.",
            match kind {
                EvidenceKind::Browser => "browser check",
                EvidenceKind::Review => "review",
            },
            &head[..head.len().min(8)]
        );
        println!("Next: run lightbulb app status to see whether the preview is done");
    }
    Ok(0)
}

/// A file inside the app that git would commit would move HEAD on the next `up`.
fn in_repo(root: &Path, file: &Path) -> Option<ApiError> {
    let (root, file) = (root.canonicalize().ok()?, file.canonicalize().ok()?);
    if !file.starts_with(&root) {
        return None;
    }
    let ignored = std::process::Command::new("git")
        .current_dir(&root)
        .args(["check-ignore", "-q", "--"])
        .arg(&file)
        .status()
        .is_ok_and(|s| s.success());
    (!ignored).then(|| {
        ApiError::new(
            "file_in_repo",
            "That file is inside the app and git would commit it, which changes the version.",
            "save it under .lightbulb/evidence/ or outside the app",
        )
    })
}
