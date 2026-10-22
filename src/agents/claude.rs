//! Push into Claude Code: a user frame written to the session's messaging socket (the
//! `messagingSocketPath` of `~/.claude/sessions/<pid>.json`). Adapted from the monorepo's
//! `apps/desktop/src-tauri/src/local_sessions/claude.rs` (`connect`, `send`, `sender_handle`).

use super::PushError;
use crate::os;
use crate::sessions::{self, Session};
use serde_json::json;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// Claude renders this as `Message from @lightbulb-teammate`; the frame names the real sender.
const FROM: &str = "lightbulb-teammate";

/// Writes `framed` into the session as a user turn (starting one if it is idle).
pub async fn push(session: &Session, framed: &str) -> Result<(), PushError> {
    let path = session
        .push_via
        .as_deref()
        .ok_or(PushError("no messaging socket"))?;
    // The socket must sit in a directory this user owns, and be served by the session itself.
    let dir = path.parent().ok_or(PushError("endpoint unavailable"))?;
    let owner = std::fs::metadata(dir)
        .map_err(|_| PushError("endpoint unavailable"))?
        .uid();
    // SAFETY: geteuid has no preconditions.
    if owner != unsafe { libc::geteuid() } {
        return Err(PushError("endpoint owner changed"));
    }
    let mut socket = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(path))
        .await
        .map_err(|_| PushError("endpoint unavailable"))?
        .map_err(|_| PushError("endpoint unavailable"))?;
    if os::peer_pid(socket.as_raw_fd()).ok() != Some(session.pid)
        || !sessions::alive(session.pid, session.birth_micros)
    {
        return Err(PushError("endpoint generation mismatch"));
    }
    let wire = format!(
        "{}\n",
        json!({"type": "user", "from": FROM, "message": {"role": "user", "content": framed}})
    );
    tokio::time::timeout(Duration::from_secs(3), socket.write_all(wire.as_bytes()))
        .await
        .map_err(|_| PushError("native admission unknown"))?
        .map_err(|_| PushError("native admission unknown"))
}
