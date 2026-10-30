//! Claude's own session listing, `claude agents --json --all`: the supported way to read
//! session state from outside Claude Code (2.1.169+ for `--all`, `id` and `state`).

use crate::os;
use crate::sessions::{self, Job, Key, Session, Status, Tier};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
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

/// The listing's rows; `None` when `text` is not a JSON array, or is one with rows of which
/// none could be read (not an empty list: an answer we do not understand). Rows without a UUID
/// `sessionId` are skipped.
pub fn parse(text: &str) -> Option<Vec<Listed>> {
    let rows: Vec<Value> = serde_json::from_str(text).ok()?;
    let s = |v: &Value, f: &str| v[f].as_str().map(String::from);
    let listed: Vec<Listed> = rows
        .iter()
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
        .collect();
    (rows.is_empty() || !listed.is_empty()).then_some(listed)
}

/// `8-4-4-4-12` hex digits.
fn uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The `claude`s to ask, in order: the executables of live Claude processes (pid files under
/// `<home>/.claude/sessions`, checked as `scan_claude` checks them) that look like a Claude
/// binary, `<home>/.local/bin/claude`, then the first on `PATH` (a launchd daemon's `PATH`
/// rarely has it).
pub fn candidates(home: &Path) -> Vec<PathBuf> {
    let running = sessions::scan_claude(home, "")
        .into_iter()
        .filter_map(|s| os::process(s.pid).ok().map(|p| p.executable))
        .filter(|e| looks_like_claude(e));
    let installed = Some(home.join(".local/bin/claude")).filter(|p| p.is_file());
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("claude"))
            .find(|p| p.is_file())
    });
    let mut out: Vec<PathBuf> = vec![];
    for exe in running.chain(installed).chain(on_path) {
        if !out.contains(&exe) {
            out.push(exe);
        }
    }
    out
}

/// A process's executable worth asking: an npm Claude's is `node`, which cannot answer. Claude's
/// are named `claude` or sit in a `claude` folder (`.../claude/versions/2.1.312`,
/// `.../claude.app/Contents/MacOS/claude`).
fn looks_like_claude(exe: &Path) -> bool {
    let name = exe.file_name().map(|n| n.to_string_lossy().to_lowercase());
    if matches!(
        name.as_deref(),
        None | Some("node" | "nodejs" | "bun" | "deno")
    ) {
        return false;
    }
    exe.components().any(|c| {
        let c = c.as_os_str().to_string_lossy().to_lowercase();
        c == "claude" || c == "claude.app"
    }) || name.is_some_and(|n| n.starts_with("claude"))
}

/// The first answer to `<exe> agents --json --all` of `exes`, tried in turn within one 3 s
/// limit; `None` when each exits non-zero, cannot start, answers unreadably, or time runs out.
pub fn read(exes: &[PathBuf]) -> Option<Vec<Listed>> {
    let deadline = Instant::now() + Duration::from_secs(3);
    exes.iter()
        .take_while(|_| Instant::now() < deadline)
        .find_map(|exe| ask(exe, deadline))
}

/// One `exe`; killed with its process group if it has not finished by `deadline`.
fn ask(exe: &Path, deadline: Instant) -> Option<Vec<Listed>> {
    let mut child = Command::new(exe)
        .args(["agents", "--json", "--all"])
        .process_group(0)
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
    let kill_group = |pid: u32| {
        // SAFETY: a plain syscall; `process_group(0)` made the child's pid its group's id. A
        // reaped child's id is not reused while its group has members; with none this is ESRCH.
        unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) };
    };
    let mut status = None;
    let text = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left.min(Duration::from_millis(20))) {
            Ok(text) => break text.ok(),
            Err(mpsc::RecvTimeoutError::Timeout) if !left.is_zero() => {}
            Err(_) => break None,
        }
        if status.is_none() {
            status = child.try_wait().ok().flatten();
            // It has answered and gone; a helper it left holding the pipe must not cost the answer.
            if status.is_some() {
                kill_group(child.id());
            }
        }
    };
    while status.is_none() && text.is_some() && Instant::now() < deadline {
        status = child.try_wait().ok().flatten();
        if status.is_none() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if status.is_none() {
        kill_group(child.id());
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
                    Some("busy" | "shell") => (Status::Live, None),
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
        .filter(|l| job_over(l))
        .map(|l| l.conversation.clone())
        .collect()
}

/// The finished jobs with no process left: ended. One whose process lives is idle, not ended.
pub fn ended_jobs(listed: &[Listed]) -> impl Iterator<Item = &Listed> {
    listed.iter().filter(|l| job_over(l) && l.pid.is_none())
}

fn job_over(l: &Listed) -> bool {
    matches!(l.state.as_deref(), Some("done" | "failed" | "stopped"))
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
    fn an_array_with_no_row_we_read_is_none_but_an_empty_one_is_an_answer() {
        assert!(parse(r#"[{"foo":1}]"#).is_none());
        assert_eq!(parse("[]"), Some(vec![]));
    }

    /// An executable script in `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = dir.join(name);
        std::fs::write(&exe, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    #[test]
    fn a_hanging_claude_and_its_children_are_gone_within_three_seconds() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        let exe = script(
            dir.path(),
            "claude",
            &format!("sleep 10 &\necho $! > '{}'\nwait", child.display()),
        );
        let t = Instant::now();
        assert!(read(&[exe]).is_none());
        assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
        let pid: u32 = std::fs::read_to_string(child)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while os::process(pid).is_ok() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(os::process(pid).is_err(), "the group was killed");
    }

    /// Its output is complete, but a helper it left behind still holds the pipe open.
    #[test]
    fn a_claude_that_answered_and_exited_is_read_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let exe = script(dir.path(), "claude", "sleep 10 &\necho '[]'");
        let t = Instant::now();
        assert_eq!(read(&[exe]), Some(vec![]));
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }

    #[test]
    fn a_candidate_that_cannot_answer_falls_through_to_the_next() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let fails = script(dir.path(), "fails", "exit 1");
        let garbage = script(dir.path(), "garbage", "echo nonsense");
        let works = script(dir.path(), "works", "echo '[]'");
        assert_eq!(
            read(&[missing, fails.clone(), garbage, works]),
            Some(vec![])
        );
        assert_eq!(read(&[fails]), None);
    }

    #[test]
    fn only_a_claude_binary_counts_as_one() {
        for yes in [
            "/Users/p/.local/share/claude/versions/2.1.312",
            "/Applications/Claude.app/Contents/MacOS/Claude",
            "/opt/x/claude.app/Contents/MacOS/claude",
            "/usr/local/bin/claude",
        ] {
            assert!(looks_like_claude(Path::new(yes)), "{yes}");
        }
        for no in [
            "/usr/local/bin/node",
            "/opt/claude/node",
            "/Users/p/.bun/bin/bun",
            "/repo/target/debug/deps/mcp_tools-85428a",
        ] {
            assert!(!looks_like_claude(Path::new(no)), "{no}");
        }
    }

    /// A live process that is not Claude's binary is passed over for the installed one.
    #[test]
    fn candidates_skip_a_running_executable_that_is_no_claude() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join(".claude/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let me = std::process::id();
        std::fs::write(
            sessions.join("a.json"),
            format!(
                r#"{{"pid":{me},"sessionId":"s","cwd":"/","startedAt":{}}}"#,
                os::started_ms(me).unwrap()
            ),
        )
        .unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let installed = script(&bin, "claude", "exit 0");
        let found = candidates(home.path());
        assert_eq!(found.first(), Some(&installed));
        assert!(!found.contains(&std::env::current_exe().unwrap()));
    }
}
