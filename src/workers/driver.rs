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
    /// Why the harness may not run in `folder` (a config there it reads would skip its
    /// person's approvals), checked once the folder is made and before the spawn.
    fn refuses(&self, _folder: &Path) -> Option<String> {
        None
    }
    /// How the harness's environment differs from the daemon's, in its child only: a variable
    /// set (OpenCode's server password), or removed (`None`).
    fn env(&self) -> Vec<(&'static str, Option<String>)> {
        vec![]
    }
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
        "cursor" => {
            let c = super::cursor::here(knobs.cursor_program.as_deref(), home, store);
            Some(c.map(|c| Box::new(c) as Box<dyn Driver>))
        }
        "grok" => {
            let g = super::grok::here(knobs.grok_program.as_deref(), home, store);
            Some(g.map(|g| Box::new(g) as Box<dyn Driver>))
        }
        "opencode" => {
            let o = super::opencode::here(knobs.opencode_program.as_deref(), home, store);
            Some(o.map(|o| Box::new(o) as Box<dyn Driver>))
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

/// The first executable file of: `knob` (when set, the only one: a test never reaches a real
/// harness); `known`; the newest registered `provider` session's executable when its file name
/// is one of `names`; `names[0]` on `path`.
pub fn find(
    knob: Option<&Path>,
    known: Vec<PathBuf>,
    store: &Store,
    provider: &str,
    names: &[&str],
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(k) = knob {
        return executable(k).then(|| k.into());
    }
    let registered = registered(store, provider)
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| names.iter().any(|w| n == *w)));
    known
        .into_iter()
        .chain(registered)
        .chain(on_path(path, names[0]))
        .find(|p| executable(p))
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
/// closed (`talk` owns it), the child has a moment to exit, and is killed (`kill_on_drop`)
/// with whatever is left of its process group (`end_group`).
pub async fn converse<F, Fut>(mut child: Child, talk: F) -> Outcome
where
    F: FnOnce(ChildStdin, mpsc::Receiver<Line>) -> Fut,
    Fut: Future<Output = Outcome>,
{
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return failed("the worker's harness has no stdio");
    };
    let group = child.id();
    let (tx, lines) = mpsc::channel(64);
    let reader = tokio::spawn(read_lines(stdout, tx));
    let outcome = talk(stdin, lines).await;
    reader.abort();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    end_group(group);
    outcome
}

/// Kills what is left of process group `pgid` (the harness's own, `spawn` made it leader: its
/// tools' processes), when the group still exists. A child that is not a group leader names
/// no group, and nothing is sent.
// ponytail: once the leader is reaped its id could in principle be reused before this runs;
// the window is the few instructions between `wait` and here.
pub fn end_group(pgid: Option<u32>) {
    let Some(pgid) = pgid.and_then(|p| i32::try_from(p).ok()).filter(|&p| p > 0) else {
        return;
    };
    // SAFETY: plain syscalls on a process group id, no memory involved.
    unsafe {
        if libc::killpg(pgid, 0) == 0 {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

/// Flags (and `--permission-mode` values) that would let a harness act without its person:
/// never in a launch argv.
const BYPASS: [&str; 14] = [
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
    // OpenCode's server open beyond this computer's loopback or to a browser origin.
    "--cors",
    "--mdns",
];

/// Words that mean a bypass wherever they appear in an argv element.
const DISTINCTIVE: [&str; 4] = ["dangerously", "bypassPermissions", "always-approve", "yolo"];

/// Whether `argv` names a program and no permission bypass: a `DISTINCTIVE` word anywhere in an
/// argument (not the program's path, which a person's folder names may contain), or a `BYPASS`
/// word as a whole element or either side of its first `=` (`--force=true`,
/// `--permission-mode=dontAsk`); `--automation` is not `--auto`.
pub fn launchable(argv: &[String]) -> bool {
    let bypass = |(i, a): (usize, &String)| {
        let (flag, value) = a.split_once('=').unwrap_or((a, ""));
        [a.as_str(), flag, value].iter().any(|w| BYPASS.contains(w))
            || (i > 0 && DISTINCTIVE.iter().any(|d| a.contains(d)))
    };
    !argv.is_empty() && !argv.iter().enumerate().any(bypass)
}

/// Starts `argv` in `folder` with `env` set or removed (`Driver::env`): its own process group (a
/// daemon restart leaves it running), killed if its handle drops, stdin and stdout piped, stderr
/// appended to `log`.
pub fn spawn(
    argv: &[String],
    env: &[(&'static str, Option<String>)],
    folder: &Path,
    log: &Path,
) -> Result<Child> {
    if !launchable(argv) {
        return Err("refused: the launch argv has no program or names a bypass".into());
    }
    let log = File::options()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)?;
    let mut command = tokio::process::Command::new(&argv[0]);
    for (k, v) in env {
        match v {
            Some(v) => command.env(k, v),
            None => command.env_remove(k),
        };
    }
    Ok(command
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

/// JSONC `text` as JSON: without its `//` line and `/* */` block comments outside strings (a
/// URL's `//` stays), each comment replaced by a space, nor a trailing comma before `}` or `]`.
pub fn uncommented(text: &str) -> String {
    let (mut out, mut chars) = (String::with_capacity(text.len()), text.chars().peekable());
    let (mut string, mut escaped) = (false, false);
    while let Some(c) = chars.next() {
        if string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                while chars.next_if(|&n| n != '\n').is_some() {}
                out.push(' ');
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            ('}' | ']', _) => {
                let end = out.trim_end().len();
                if out[..end].ends_with(',') {
                    out.remove(end - 1);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::launchable;
    use crate::store::Store;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// An executable script at `p`.
    pub fn exe(p: &Path) -> PathBuf {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.into()
    }

    /// Registers a `provider` session `key` that ran `exe`, seen at `seen_at`.
    pub fn session(s: &Store, provider: &str, key: &str, exe: &Path, seen_at: i64) {
        s.put_session(&crate::store::Session {
            key: key.into(),
            provider: provider.into(),
            conversation: None,
            pid: None,
            birth_micros: None,
            cwd: "/".into(),
            title: None,
            status: "idle".into(),
            seen_at,
            executable: Some(exe.to_string_lossy().into()),
            repo: None,
            branch: None,
            handle: None,
        })
        .unwrap();
    }

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
            &["--settings={\"defaultMode\":\"bypassPermissions\"}"],
            &["--dangerously-skip-permissions=1"],
            &["x--yolo"],
            &["--cors", "https://x"],
            &["--cors=https://x"],
            &["--mdns"],
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
        // The program's path is the person's: a folder named `yolo` is not a flag.
        let program = |p: &str| [p, "acp"].map(String::from);
        assert!(launchable(&program("/Users/yolo-fan/.local/bin/cursor-agent")));
        assert!(launchable(&program("/opt/dangerously/grok")));
        assert!(!launchable(&program("--yolo")));
    }


    #[tokio::test]
    async fn a_natural_end_takes_the_whole_process_group() {
        let d = tempfile::tempdir().unwrap();
        let pid = d.path().join("pid");
        // The harness leaves a sleeping grandchild behind and exits when its stdin closes.
        let script = format!(
            "sleep 300 >/dev/null 2>&1 & echo $! > {}; cat >/dev/null",
            pid.display()
        );
        let argv = ["/bin/sh", "-c", &script].map(String::from);
        let child = super::spawn(&argv, &[], d.path(), &d.path().join("log")).unwrap();
        let wait = pid.clone();
        let outcome = super::converse(child, |stdin, _lines| async move {
            while std::fs::read_to_string(&wait).map_or(true, |p| !p.ends_with('\n')) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            drop(stdin);
            super::failed("done")
        })
        .await;
        assert_eq!(outcome.body, "done");
        let grandchild: i32 = std::fs::read_to_string(&pid).unwrap().trim().parse().unwrap();
        // Gone (once reaped by init).
        let alive = || unsafe { libc::kill(grandchild, 0) } == 0;
        for _ in 0..100 {
            if !alive() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        panic!("the grandchild {grandchild} outlived the worker");
    }

    #[test]
    fn comments_go_but_strings_stay() {

        let text = "{\"u\": \"https://x/*y*/\\\"//z\", // a\n/* b\n */ \"v\": [1, \",]\",],}";
        let v: serde_json::Value = serde_json::from_str(&super::uncommented(text)).unwrap();
        assert_eq!(v["u"], "https://x/*y*/\"//z");
        assert_eq!(v["v"], serde_json::json!([1, ",]"]));
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
