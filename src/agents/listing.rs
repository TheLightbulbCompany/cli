//! Claude's own session listing, `claude agents --json --all`: the supported way to read
//! session state from outside Claude Code (2.1.169+ for `--all`, `id` and `state`).

use crate::os;
use crate::sessions::{self, Job, Key, Session, Status, Tier};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// One listed row: a conversation, its process while it lives, and its state in Claude's words.
#[derive(Debug, Clone, PartialEq)]
pub struct Listed {
    pub conversation: String,
    pub pid: Option<u32>,
    pub kind: String,
    pub status: Option<String>,
    pub waiting_for: Option<String>,
    pub state: Option<String>,
    pub job_id: Option<String>,
    pub name: Option<String>,
    pub cwd: String,
    pub started_ms: u64,
}

/// The listing's rows; `None` when `text` is not a JSON array. Rows without a UUID
/// `sessionId` are skipped.
pub fn parse(text: &str) -> Option<Vec<Listed>> {
    let rows: Vec<Value> = serde_json::from_str(text).ok()?;
    let s = |v: &Value, f: &str| v[f].as_str().map(String::from);
    Some(
        rows.iter()
            .filter_map(|v| {
                let conversation = s(v, "sessionId").filter(|c| uuid(c))?;
                Some(Listed {
                    conversation,
                    pid: v["pid"].as_u64().and_then(|p| u32::try_from(p).ok()),
                    kind: s(v, "kind").unwrap_or_default(),
                    status: s(v, "status"),
                    waiting_for: s(v, "waitingFor"),
                    state: s(v, "state"),
                    job_id: s(v, "id"),
                    name: s(v, "name"),
                    cwd: s(v, "cwd").unwrap_or_default(),
                    started_ms: v["startedAt"].as_u64().unwrap_or_default(),
                })
            })
            .collect(),
    )
}

/// `8-4-4-4-12` hex digits.
fn uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The `claude` to ask: the executable of a live Claude process (its pid file under
/// `<home>/.claude/sessions`, checked as `scan_claude` checks it), else
/// `<home>/.local/bin/claude`, else the first on `PATH` (a launchd daemon's `PATH` rarely has it).
pub fn claude_exe(home: &Path) -> Option<PathBuf> {
    let running = sessions::scan_claude(home, "")
        .into_iter()
        .find_map(|s| os::process(s.pid).ok().map(|p| p.executable));
    let on_path = || {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("claude"))
            .find(|p| p.is_file())
    };
    running
        .or_else(|| Some(home.join(".local/bin/claude")).filter(|p| p.is_file()))
        .or_else(on_path)
}

/// Runs `exe agents --json --all`; `None` on a non-zero exit, unparsable output, or no
/// answer within 3 s (then it is killed).
pub fn read(exe: &Path) -> Option<Vec<Listed>> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut child = Command::new(exe)
        .args(["agents", "--json", "--all"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read on a thread: a long listing fills the pipe before the process exits.
    let mut out = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = tx.send(out.read_to_string(&mut text).map(|_| text));
    });
    let text = rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .and_then(Result::ok);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) if text.is_some() => break Some(status),
            Ok(None) if text.is_some() && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => break None,
        }
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    parse(&text?).filter(|_| status.is_some_and(|s| s.success()))
}

/// A live `Session` per row with a pid; rows without one are over (Task 6 ends them). Push
/// tier when `sockets` (conversation -> the pid file's `messagingSocketPath`) has its socket.
/// A background row's status is its job's state: `working` live, `blocked` waiting, the rest idle.
pub fn to_sessions(
    listed: &[Listed],
    member_short: &str,
    sockets: &HashMap<String, PathBuf>,
) -> Vec<Session> {
    listed
        .iter()
        .filter_map(|l| {
            let p = os::process(l.pid?).ok()?;
            let waiting = |w: Option<&str>| {
                (
                    Status::Waiting,
                    l.waiting_for.as_deref().or(w).map(String::from),
                )
            };
            let (status, waiting_for) = match (l.kind.as_str(), l.state.as_deref()) {
                ("background", Some("working")) => (Status::Live, None),
                ("background", Some("blocked")) => waiting(Some("blocked")),
                ("background", Some(_)) => (Status::Idle, None),
                _ => match l.status.as_deref() {
                    Some("busy") => (Status::Live, None),
                    Some("waiting") => waiting(None),
                    _ => (Status::Idle, None),
                },
            };
            let push_via = sockets.get(&l.conversation).cloned();
            Some(Session {
                key: Key::new(member_short, "claude", &l.conversation),
                provider: "claude".into(),
                conversation: l.conversation.clone(),
                pid: p.pid,
                birth_micros: p.birth_micros,
                cwd: l.cwd.clone(),
                title: l.name.clone(),
                doing: None,
                status,
                tier: if push_via.is_some() {
                    Tier::Push
                } else {
                    Tier::NextStep
                },
                push_via,
                handle: None,
                repo: None,
                branch: None,
                hidden: false,
                waiting_for,
                kind: Some(l.kind.clone()).filter(|k| !k.is_empty()),
                entrypoint: None,
                job: l
                    .job_id
                    .clone()
                    .zip(l.state.clone())
                    .map(|(id, state)| Job { id, state }),
            })
        })
        .collect()
}

/// The conversations whose job is over (`done`, `failed`, `stopped`), its process gone or not.
pub fn finished(listed: &[Listed]) -> Vec<String> {
    listed
        .iter()
        .filter(|l| matches!(l.state.as_deref(), Some("done" | "failed" | "stopped")))
        .map(|l| l.conversation.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PID: &str = "\"pid\": 4242";

    /// The fixture with its live rows' pid made this test process (a pid that lives).
    fn five() -> Vec<Listed> {
        let me = format!("\"pid\": {}", std::process::id());
        let text = include_str!("../../tests/fixtures/claude-agents/five.json").replace(PID, &me);
        parse(&text).unwrap()
    }

    const BUSY: &str = "4374580d-5ef8-454f-9896-a36e07a3108a";
    const BLOCKED: &str = "b1b2c3d4-0000-4000-8000-000000000002";
    const DONE: &str = "c1b2c3d4-0000-4000-8000-000000000003";
    const STOPPED: &str = "d1b2c3d4-0000-4000-8000-000000000004";

    #[test]
    fn parse_maps_claudes_words_and_skips_what_has_no_pid() {
        let rows = five();
        assert_eq!(rows.len(), 9, "the row without a UUID is skipped");
        let live = to_sessions(&rows, "k3f9ab7cd2ef", &HashMap::new());
        let by = |c: &str| live.iter().find(|s| s.conversation == c).unwrap();
        assert_eq!(by(BUSY).status, Status::Live);
        assert_eq!(
            (by(BLOCKED).status, by(BLOCKED).waiting_for.as_deref()),
            (Status::Waiting, Some("permission prompt"))
        );
        assert_eq!(
            by(DONE).job.as_ref().map(|j| j.state.as_str()),
            Some("done")
        );
        assert!(live.iter().all(|s| s.conversation != STOPPED));
        assert_eq!(finished(&rows), vec![DONE.to_string(), STOPPED.to_string()]);
    }

    #[test]
    fn garbage_and_non_arrays_are_none() {
        assert!(parse("not json").is_none());
        assert!(parse("{}").is_none());
    }

    #[test]
    fn a_hanging_claude_is_none_within_three_seconds() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("claude");
        std::fs::write(&exe, "#!/bin/sh\nexec sleep 10\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = std::time::Instant::now();
        assert!(read(&exe).is_none());
        assert!(
            t.elapsed() < std::time::Duration::from_secs(4),
            "{:?}",
            t.elapsed()
        );
    }
}
