//! Push into Codex: `<daemon codex> queue --thread <id> --message <text>` queues a turn into the
//! thread. Only threads served by Codex's shared app-server daemon are pushed, through that
//! daemon's own binary: queueing into a `--no-daemon` or ChatGPT-app thread would add a second
//! writer.

use super::PushError;
use crate::sessions::Session;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// `queue` output past this is refused unread.
const MAX_OUTPUT: u64 = 1024 * 1024;
/// The longest `--message` passed on a command line.
const MAX_MESSAGE: usize = 64 * 1024;

/// `exe` (resolved) when it is a release of Codex's app-server daemon under `home`; that
/// process serves its threads, so its binary can queue into them.
pub fn daemon_program(home: &Path, exe: &Path) -> Option<PathBuf> {
    let root = home
        .join(".codex/packages/app-server-daemon/releases")
        .canonicalize()
        .ok()?;
    let exe = exe.canonicalize().ok()?;
    (exe.starts_with(&root) && exe.is_file()).then_some(exe)
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

/// Queues `framed` as the thread's next turn (starting one if it is idle), with the daemon
/// binary in `session.push_via`. Argv only, no shell; 10 s at most.
pub async fn push(session: &Session, framed: &str) -> Result<(), PushError> {
    let program = session
        .push_via
        .as_deref()
        .ok_or(PushError("not a daemon-served thread"))?;
    if framed.len() > MAX_MESSAGE {
        return Err(PushError("message too long for codex queue"));
    }
    let run = async {
        let mut child = tokio::process::Command::new(program)
            .args([
                "queue",
                "--thread",
                &session.conversation,
                "--message",
                framed,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| PushError("codex unavailable"))?;
        let mut out = vec![];
        child
            .stdout
            .take()
            .ok_or(PushError("codex unavailable"))?
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut out)
            .await
            .map_err(|_| PushError("codex unavailable"))?;
        let status = child
            .wait()
            .await
            .map_err(|_| PushError("codex unavailable"))?;
        Ok::<_, PushError>((status, out))
    };
    let (status, out) = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .map_err(|_| PushError("codex queue timed out"))??;
    if status.success()
        && out.len() as u64 <= MAX_OUTPUT
        && queued(&String::from_utf8_lossy(&out), &session.conversation)
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

    #[test]
    fn only_a_daemon_release_serves_threads() {
        let home = tempfile::tempdir().unwrap();
        let bin = home
            .path()
            .join(".codex/packages/app-server-daemon/releases/x/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("codex"), "").unwrap();
        std::fs::write(home.path().join("codex"), "").unwrap();
        assert!(daemon_program(home.path(), &bin.join("codex")).is_some());
        assert!(daemon_program(home.path(), &home.path().join("codex")).is_none());
        let sneaky = bin.join("../../../../../../codex");
        assert!(daemon_program(home.path(), &sneaky).is_none());
    }
}
