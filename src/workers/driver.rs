//! A worker's harness driver: the argv that launches it headless, and the task that speaks its
//! protocol over the child's stdio until it ends. Shared here: the types a driver hands back
//! and the child plumbing.

use crate::Result;
use std::fs::File;
use std::future::Future;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};

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

/// The driver for `provider`'s harness; none yet (the Claude Code driver is Task 5's).
pub fn driver_for(_provider: &str) -> Option<Box<dyn Driver>> {
    None
}

/// Flags that would let a harness act without its person: never in a launch argv.
const BYPASS: [&str; 4] = [
    "--dangerously-skip-permissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "bypassPermissions",
    "--approve-for-me",
];

/// Whether `argv` names a program and no permission bypass.
pub fn launchable(argv: &[String]) -> bool {
    !argv.is_empty() && !argv.iter().any(|a| BYPASS.iter().any(|b| a.contains(b)))
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
        .kill_on_drop(true)
        .process_group(0)
        .spawn()?)
}
