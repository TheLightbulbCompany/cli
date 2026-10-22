//! Push into Codex: `codex queue --thread <id> --message <text>` queues a turn into the thread.
//! Adapted from the monorepo's `apps/desktop/src-tauri/src/local_sessions/codex.rs` (`send`,
//! `queued_id`) and `local_sessions.rs` (`native_output`).

use super::PushError;
use crate::sessions::Session;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

/// The `codex` executable on `PATH`, looked up once per process (the daemon).
pub fn program() -> Option<&'static Path> {
    static PROGRAM: OnceLock<Option<PathBuf>> = OnceLock::new();
    PROGRAM
        .get_or_init(|| {
            std::env::split_paths(&std::env::var_os("PATH")?)
                .map(|dir| dir.join("codex"))
                .find(|p| {
                    p.metadata()
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                })
        })
        .as_deref()
}

/// `codex queue` answers `Queued message <id> for thread <thread>.`; anything else is a refusal.
fn queued(output: &str, thread: &str) -> bool {
    output
        .trim()
        .strip_prefix("Queued message ")
        .and_then(|r| r.strip_suffix('.'))
        .and_then(|r| r.split_once(" for thread "))
        .is_some_and(|(id, target)| {
            target == thread
                && !id.is_empty()
                && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        })
}

/// Queues `framed` as the thread's next turn (starting one if it is idle). Argv only, no shell.
pub async fn push(session: &Session, framed: &str) -> Result<(), PushError> {
    let program = program().ok_or(PushError("codex not on PATH"))?;
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(program)
            .args([
                "queue",
                "--thread",
                &session.conversation,
                "--message",
                framed,
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| PushError("codex queue timed out"))?
    .map_err(|_| PushError("codex unavailable"))?;
    if output.status.success()
        && queued(
            &String::from_utf8_lossy(&output.stdout),
            &session.conversation,
        )
    {
        Ok(())
    } else {
        Err(PushError("codex queue refused"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_is_exact_and_thread_bound() {
        let out = "Queued message 01a08ec5-b961-77c0-962d-25097fe0f5f4 for thread t1.\n";
        assert!(queued(out, "t1"));
        assert!(!queued(out, "other"));
        assert!(!queued(&format!("unknown {out}"), "t1"));
    }
}
