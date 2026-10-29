//! A worker's harness driver: the argv that launches it headless, and the task that speaks its
//! protocol over the child's stdio until it ends. Shared here: the types a driver hands back
//! and the child plumbing.

use crate::net::Knobs;
use crate::store::Store;
use crate::Result;
use serde_json::Value;
use std::ffi::OsStr;
use std::fs::File;
use std::future::Future;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

/// What a worker that asks its person a question is told: nobody is at its terminal.
pub const NO_PERSON: &str =
    "no person is attached to this worker; decide yourself or report what you need";
/// How long a harness has to answer each handshake request.
pub const INIT_WAIT: Duration = Duration::from_secs(30);
/// The longest stdout line read; a longer one ends the run.
const MAX_LINE: usize = 1 << 20;

/// The person's answer to a worker's permission prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// Refused, with the reason the harness is told.
    Deny(String),
}

/// A prompt the harness raised: `tool` and what it would act on (`summary`), answered on `reply`.
pub struct PermissionRequest {
    pub tool: String,
    pub summary: String,
    pub reply: oneshot::Sender<Decision>,
}

/// What a running driver reports to the daemon.
pub struct Relay {
    pub permission: mpsc::Sender<PermissionRequest>,
    /// The harness's conversation id, when the harness names its own (Codex).
    pub session: oneshot::Sender<String>,
}

/// How the harness ended: `status` `done` or `failed` (`reason` `error`), and its final output.
pub struct Outcome {
    pub status: &'static str,
    pub reason: Option<&'static str>,
    pub body: String,
}

/// A driver's run, to its `Outcome`.
pub type Run = Pin<Box<dyn Future<Output = Outcome> + Send>>;

/// One harness's headless protocol.
pub trait Driver: Send {
    /// The launch argv (program first) for a worker in `folder` with conversation `session_id`.
    fn argv(&self, folder: &Path, session_id: &str) -> Vec<String>;
    /// Hands the harness `first_prompt` and speaks its protocol until it ends.
    fn run(self: Box<Self>, child: Child, first_prompt: String, relay: Relay) -> Run;
}

/// The driver for `provider`'s harness on this computer (`home` holds `.claude/`): none for a
/// harness without one; `Err(why)` when it cannot run here (Claude Code: `Claude::here`).
pub fn driver_for(
    provider: &str,
    knobs: &Knobs,
    home: &Path,
    store: &Store,
) -> Option<std::result::Result<Box<dyn Driver>, &'static str>> {
    match provider {
        "claude" => {
            let knob = knobs.claude_program.as_deref();
            let c = super::claude::Claude::here(knob, home, store);
            Some(c.map(|c| Box::new(c) as Box<dyn Driver>))
        }
        "codex" => {
            let knob = knobs.codex_program.as_deref();
            let c = super::codex::Codex::here(knob, home, store);
            Some(c.map(|c| Box::new(c) as Box<dyn Driver>))
        }
        _ => None,
    }
}

pub fn executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The executables registered `provider` sessions ran, newest first.
pub fn registered(store: &Store, provider: &str) -> Vec<PathBuf> {
    let rows = (|| -> rusqlite::Result<Vec<String>> {
        let mut stmt = store.conn().prepare(
            "SELECT executable FROM sessions WHERE provider = ?1 \
             AND executable IS NOT NULL ORDER BY seen_at DESC",
        )?;
        let rows = stmt.query_map([provider], |r| r.get(0))?;
        rows.collect()
    })();
    rows.unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// `name` in each directory of `path`.
pub fn on_path(path: Option<&OsStr>, name: &str) -> Vec<PathBuf> {
    path.into_iter()
        .flat_map(std::env::split_paths)
        .map(|d| d.join(name))
        .collect()
}

/// Writes `v` as one line to the harness.
pub async fn send(stdin: &mut ChildStdin, v: &Value) -> std::io::Result<()> {
    stdin.write_all(format!("{v}\n").as_bytes()).await?;
    stdin.flush().await
}

pub fn failed(body: impl Into<String>) -> Outcome {
    Outcome {
        status: "failed",
        reason: Some("error"),
        body: body.into(),
    }
}

/// A line of the harness's stdout, or why reading stopped.
pub type Line = std::result::Result<String, &'static str>;

/// Reads `stdout` line by line into `lines` (on its own task, so the reader loses nothing when
/// a driver's select picks another branch) until EOF, an error, or a line over `MAX_LINE`.
async fn read_lines(stdout: ChildStdout, lines: mpsc::Sender<Line>) {
    let mut r = BufReader::new(stdout);
    loop {
        let mut buf = vec![];
        let read = (&mut r)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await;
        let line = match read {
            Ok(0) => return,
            Ok(_) if buf.len() > MAX_LINE => Err("the worker's harness wrote a line over 1 MiB"),
            Ok(_) => Ok(String::from_utf8_lossy(&buf).trim_end().to_string()),
            Err(_) => Err("the worker's harness output could not be read"),
        };
        let stop = line.is_err();
        if lines.send(line).await.is_err() || stop {
            return;
        }
    }
}

/// Waits for the response to request `id`: its result, or why there is none.
pub async fn response(
    lines: &mut mpsc::Receiver<Line>,
    id: u64,
) -> std::result::Result<Value, String> {
    let wait = async {
        while let Some(line) = lines.recv().await {
            let v: Value = serde_json::from_str(&line?).unwrap_or_default();
            if v["id"] == id && v.get("method").is_none() {
                return match v.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("refused").to_string()),
                    None => Ok(v["result"].clone()),
                };
            }
        }
        Err("the worker's harness ended before it started".to_string())
    };
    match tokio::time::timeout(INIT_WAIT, wait).await {
        Ok(r) => r,
        Err(_) => Err("the worker's harness did not start within 30 s".into()),
    }
}

/// Runs `talk` over `child`'s stdin and its stdout's lines to an `Outcome`; then stdin is
/// closed (`talk` owns it), the child has a moment to exit, and is killed (`kill_on_drop`).
pub async fn converse<F, Fut>(mut child: Child, talk: F) -> Outcome
where
    F: FnOnce(ChildStdin, mpsc::Receiver<Line>) -> Fut,
    Fut: Future<Output = Outcome>,
{
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return failed("the worker's harness has no stdio");
    };
    let (tx, lines) = mpsc::channel(64);
    let reader = tokio::spawn(read_lines(stdout, tx));
    let outcome = talk(stdin, lines).await;
    reader.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    outcome
}

/// Flags (and `--permission-mode` values) that would let a harness act without its person:
/// never in a launch argv.
const BYPASS: [&str; 12] = [
    "--dangerously-skip-permissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "bypassPermissions",
    "--approve-for-me",
    "--force",
    "--yolo",
    "--always-approve",
    "--auto-review",
    "--approve-mcps",
    "--trust",
    "--auto",
    "dontAsk",
];

/// Whether `argv` names a program and no permission bypass: a `BYPASS` word as a whole element,
/// either side of one `=` (`--force=true`, `--permission-mode=dontAsk`); `--automation` is not
/// `--auto`.
pub fn launchable(argv: &[String]) -> bool {
    let bypass = |a: &str| {
        let (flag, value) = a.split_once('=').unwrap_or((a, ""));
        [a, flag, value].iter().any(|w| BYPASS.contains(w))
    };
    !argv.is_empty() && !argv.iter().any(|a| bypass(a))
}

/// Starts `argv` in `folder`: its own process group (a daemon restart leaves it running),
/// killed if its handle drops, stdin and stdout piped, stderr appended to `log`.
pub fn spawn(argv: &[String], folder: &Path, log: &Path) -> Result<Child> {
    if !launchable(argv) {
        return Err("refused: the launch argv has no program or names a bypass".into());
    }
    let log = File::options()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)?;
    Ok(tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(folder)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(log)
        .env("PATH", child_path(&argv[0]))
        .kill_on_drop(true)
        .process_group(0)
        .spawn()?)
}

/// The `PATH` a harness runs with: its own folder, the Homebrew ones and `~/.local/bin` before
/// the daemon's (a launchd agent's is the bare system one; an npm-installed Claude Code needs
/// `node`).
fn child_path(program: &str) -> std::ffi::OsString {
    let own = Path::new(program).parent().map(Path::to_path_buf);
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let dirs = own
        .into_iter()
        .chain(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from))
        .chain(dirs::home_dir().map(|h| h.join(".local/bin")))
        .chain(std::env::split_paths(&inherited))
        .filter(|d| !d.as_os_str().is_empty());
    std::env::join_paths(dirs).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::launchable;

    fn argv(args: &[&str]) -> Vec<String> {
        ["x"].iter().chain(args).map(|a| a.to_string()).collect()
    }

    #[test]
    fn launchable_matches_whole_elements() {
        for bad in [
            &["--auto-review"][..],
            &["--auto"],
            &["--force=true"],
            &["--permission-mode", "bypassPermissions"],
            &["--permission-mode=bypassPermissions"],
            &["--permission-mode=dontAsk"],
            &["--yolo"],
        ] {
            assert!(!launchable(&argv(bad)), "{bad:?}");
        }
        for ok in [
            &["--automation"][..],
            &["--auto-reviewer"],
            &["--forced"],
            &["acp"],
            &["--permission-mode", "default"],
            &["--note=trust"],
        ] {
            assert!(launchable(&argv(ok)), "{ok:?}");
        }
        assert!(!launchable(&[]));
    }

    #[test]
    fn a_harness_finds_node_without_a_login_path() {
        let p = super::child_path("/x/bin/claude");
        let dirs: Vec<_> = std::env::split_paths(&p).collect();
        let home = dirs::home_dir().unwrap().join(".local/bin");
        assert_eq!(
            dirs[..4],
            [
                "/x/bin".into(),
                "/opt/homebrew/bin".into(),
                "/usr/local/bin".into(),
                home
            ]
        );
        // A bare name adds no empty entry.
        assert_eq!(
            std::env::split_paths(&super::child_path("claude"))
                .next()
                .unwrap(),
            std::path::PathBuf::from("/opt/homebrew/bin")
        );
    }
}
