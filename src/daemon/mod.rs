pub mod ipc;

use crate::{paths, store::Store, Result};
use data_encoding::BASE32_NOPAD;
use ed25519_dalek::SigningKey;
use serde_json::Value;
use std::fs::{self, File, OpenOptions, Permissions, TryLockError};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Notify};

/// What every connection handler shares.
pub struct Daemon {
    pub store: Mutex<Store>,
    pub member_key_short: String,
    pub team_id: String,
    pub team_name: String,
    /// Pushed to every `subscribe`d connection as `{"event": ...}` frames.
    pub events: broadcast::Sender<Value>,
    pub shutdown: Notify,
}

/// Runs the daemon for `dir` until a `shutdown` op. Returns `Ok` at once,
/// without touching anything, when another daemon already holds `daemon.lock`.
pub async fn run(dir: &Path) -> Result<()> {
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("daemon.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(e)) => return Err(e.into()),
    }
    lock.set_len(0)?;
    writeln!(lock, "{}", std::process::id())?;

    let public = identity(dir)?;
    let team_key = BASE32_NOPAD.encode(&public).to_lowercase();
    let store = Store::open(&dir.join("state.db"))?;
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "me".into());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let (team_id, team_name) = store.ensure_team(&team_key, &user, now)?;

    // We hold the lock, so any socket file left behind is stale.
    let sock = dir.join(paths::SOCKET);
    match fs::remove_file(&sock) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let listener = UnixListener::bind(&sock)?;
    fs::set_permissions(&sock, Permissions::from_mode(0o600))?;

    let daemon = Arc::new(Daemon {
        store: Mutex::new(store),
        member_key_short: team_key[..12].to_string(),
        team_id,
        team_name,
        events: broadcast::channel(256).0,
        shutdown: Notify::new(),
    });
    ipc::serve(listener, daemon).await;
    let _ = fs::remove_file(&sock);
    Ok(()) // `lock` drops here, after the socket is gone.
}

/// Loads `identity.key` (a raw 32-byte Ed25519 secret), creating it 0600 on
/// first run. Returns the public key.
fn identity(dir: &Path) -> Result<[u8; 32]> {
    let path = dir.join("identity.key");
    let key = match fs::read(&path) {
        Ok(bytes) => SigningKey::from_bytes(
            &bytes
                .try_into()
                .map_err(|_| "identity.key is not 32 bytes")?,
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let key = SigningKey::generate(&mut rand::rngs::OsRng);
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)?
                .write_all(&key.to_bytes())?;
            key
        }
        Err(e) => return Err(e.into()),
    };
    Ok(key.verifying_key().to_bytes())
}

/// Makes sure a daemon is listening on this user's socket, starting one if not.
pub fn ensure_running() -> Result<()> {
    ensure_running_at(&std::env::current_exe()?, &paths::state_dir())
}

/// `ensure_running` for an explicit binary and state dir (tests).
pub fn ensure_running_at(exe: &Path, dir: &Path) -> Result<()> {
    let sock = dir.join(paths::SOCKET);
    if UnixStream::connect(&sock).is_ok() {
        return Ok(());
    }
    let log = File::options()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    let mut child = Command::new(exe)
        .arg("daemon")
        .env(paths::HOME_ENV, dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0) // detached from the caller's terminal signals
        .spawn()?;
    // Reap it whenever it exits (a lock-race loser exits at once).
    std::thread::spawn(move || child.wait());
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        if UnixStream::connect(&sock).is_ok() {
            return Ok(());
        }
    }
    Err("daemon did not start within 5 s".into())
}
