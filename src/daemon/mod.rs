pub mod ipc;
pub mod net_ops;

use crate::agents::{claude, codex};
use crate::config::Config;
use crate::identity::Identity;
use crate::sessions::{self, Key, Tier};
use crate::{cli, dialog, frame, mail, net, paths, roster, store::Store, Result};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Notify};

/// The team this computer is in, and its settings: replaced at runtime (join, leave, reload).
pub struct TeamState {
    /// The replayed roster, replaced by every write (`net_ops`), never replayed per op.
    pub roster: roster::Roster,
    pub config: Config,
}

/// What every connection handler shares.
pub struct Daemon {
    pub store: Mutex<Store>,
    pub identity: Identity,
    pub team: Mutex<TeamState>,
    /// Where Claude Code's `.claude/` lives: `$LIGHTBULB_NETWORK_CLAUDE_HOME`, else the home dir.
    pub claude_home: PathBuf,
    /// Pushed to every `subscribe`d connection as `{"event": ...}` frames.
    pub events: broadcast::Sender<Value>,
    pub shutdown: Notify,
    /// Poked after every accepted `send`, so a waiting `inbox` looks again.
    pub mail: Notify,
    /// One lock per recipient key, held while its mail is pushed or taken.
    /// ponytail: never pruned (one entry per key ever written to); prune when keys churn.
    pub mailbox_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Keys whose last push failed (more recently than one succeeded).
    pub push_failed: Mutex<HashSet<String>>,
    /// The member connections (`net`).
    pub peers: net::Peers,
    /// The person's OS dialogs, one at a time, none while `ui.claim` is live.
    pub dialogs: dialog::Queue,
    /// `dialogs`' answers, for the task that routes them (admission, holds) to take.
    pub dialog_answers: Mutex<Option<dialog::Answers>>,
    /// The join requests waiting for their person's answer (`admission`).
    pub admissions: crate::admission::Waiting,
}

impl Daemon {
    pub fn key_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.mailbox_locks
            .lock()
            .unwrap()
            .entry(key.into())
            .or_default()
            .clone()
    }

    /// Whether the Lightbulb app holds a `ui.claim` at `now`.
    pub fn claim_live(&self, now: i64) -> bool {
        self.dialogs.claim_live(now)
    }
}

/// Pushes the mail waiting for `to` into its session, oldest first, when the session is push
/// tier. Each message is taken (`delivered`) only after its push succeeds; at the first failure
/// it and everything after it stay in the mailbox for the next hook or `check_inbox`, and the
/// session reads `next_step` until a push succeeds again. At-least-once: a push whose result is
/// unknown (a timeout after the agent took it) counts as failed and is pushed again on the next
/// send, or handed over by the next hook.
pub async fn deliver(daemon: Arc<Daemon>, to: Key) {
    let rows = daemon.store.lock().unwrap().sessions().unwrap_or_default();
    let Some(session) = sessions::all(&rows, &daemon.claude_home, &daemon.identity.short)
        .into_iter()
        .find(|s| s.key == to && s.tier == Tier::Push)
    else {
        return;
    };
    let lock = daemon.key_lock(&to.0);
    loop {
        // One message per hold, so an `inbox` waits for at most one push.
        let _held = lock.lock().await;
        let next = mail::waiting(&daemon.store.lock().unwrap(), &to, ipc::now());
        let Some(m) = next.ok().and_then(|w| w.into_iter().next()) else {
            return;
        };
        let sender = frame::Sender::of(&daemon.identity.short, &m.from_key);
        let framed = frame::framed(sender, None, &m.request_id, &m.body);
        let pushed = match session.provider.as_str() {
            "claude" => claude::push(&session, &framed).await,
            _ => codex::push(&session, &framed).await,
        };
        if let Err(e) = pushed {
            eprintln!("push to {}: {}", to.0, e.0);
            daemon.push_failed.lock().unwrap().insert(to.0.clone());
            return;
        }
        daemon.push_failed.lock().unwrap().remove(&to.0);
        match mail::take_one(&daemon.store.lock().unwrap(), &to, &m.id, ipc::now()) {
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(e) => {
                eprintln!("push to {}: {e}", to.0);
                return;
            }
        }
    }
}

/// Runs the daemon for `dir` until a `shutdown` op. While another daemon holds `daemon.lock`
/// it waits, touching nothing, and takes over when that one exits: so launchd's copy, started
/// while a spawned daemon serves, neither exits into a respawn loop nor serves twice.
pub async fn run(dir: &Path) -> Result<()> {
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("daemon.lock"))?;
    // Blocks this (otherwise idle) thread; nothing else runs until the lock is ours.
    lock.lock()?;
    lock.set_len(0)?;
    writeln!(lock, "{}", std::process::id())?;

    let identity = Identity::load_or_create(dir)?;
    let config = Config::load(dir)?;
    let store = Store::open(&dir.join("state.db"))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    // The solo network's genesis, written once: a computer that has a roster keeps it.
    let mut entries = roster::entries(&store)?;
    if entries.is_empty() {
        let g = net_ops::genesis(&identity, &user_name(), &config.computer, &user_name(), now);
        roster::accept(&store, &g, now)?;
        entries.push(g);
    }
    let team = TeamState {
        roster: roster::replay(&entries),
        config,
    };
    net_ops::sync_team_row(&store, &roster::Roster::default(), &team.roster)?;

    // We hold the lock, so any socket file left behind is stale.
    let sock = dir.join(paths::SOCKET);
    match fs::remove_file(&sock) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let listener = UnixListener::bind(&sock)?;
    // Briefly wider than 0600 after bind; the 0700 state dir closes that window.
    fs::set_permissions(&sock, Permissions::from_mode(0o600))?;

    let peers = net::Peers::default();
    let (dialogs, answers) = dialog::Queue::new("/usr/bin/osascript".into());
    dialogs.set_knob(peers.knobs().dialog.clone());
    let daemon = Arc::new(Daemon {
        store: Mutex::new(store),
        identity,
        team: Mutex::new(team),
        claude_home: std::env::var_os("LIGHTBULB_NETWORK_CLAUDE_HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .ok_or("no home directory")?,
        events: broadcast::channel(256).0,
        shutdown: Notify::new(),
        mail: Notify::new(),
        mailbox_locks: Mutex::default(),
        push_failed: Mutex::default(),
        peers,
        dialogs,
        dialog_answers: Mutex::new(Some(answers)),
        admissions: Mutex::default(),
    });
    tokio::spawn(crate::admission::route(daemon.clone()));
    let net = net::Net::start(daemon.clone()).await?;
    ipc::serve(listener, daemon).await;
    net.close().await;
    let _ = fs::remove_file(&sock);
    Ok(()) // `lock` drops here, after the socket is gone.
}

/// This person's name: `$USER`, else `me`.
pub fn user_name() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "me".into())
}

/// Makes sure a daemon is listening on this user's socket, starting one if not. When the
/// launchd job is installed (macOS, the default state dir), launchd starts it, so there is
/// never a second owner; otherwise this binary spawns one.
pub fn ensure_running() -> Result<()> {
    let dir = paths::state_dir();
    let sock = dir.join(paths::SOCKET);
    if answers_ping(&sock) {
        return Ok(());
    }
    let launchd = cfg!(target_os = "macos")
        && cli::use_launchctl()
        && std::env::var_os(paths::HOME_ENV).is_none()
        && dirs::home_dir().is_some_and(|h| cli::plist_path(&h).exists());
    // A job launchd does not know (booted out) cannot be kickstarted: spawn instead.
    if launchd && cli::kickstart() {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if answers_ping(&sock) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        return Err("launchd's daemon did not answer within 5 s".into());
    }
    ensure_running_at(&std::env::current_exe()?, &dir)
}

/// `ensure_running` for an explicit binary and state dir (tests).
pub fn ensure_running_at(exe: &Path, dir: &Path) -> Result<()> {
    let sock = dir.join(paths::SOCKET);
    if answers_ping(&sock) {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut child = spawn_daemon(exe, dir)?;
    let mut up = false;
    while !up && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        up = answers_ping(&sock);
        // It lost the lock to a daemon that has since gone (one shutting
        // down when we looked): start another. try_wait reaps the loser.
        if !up && child.try_wait()?.is_some() {
            child = spawn_daemon(exe, dir)?;
        }
    }
    // A child that lost the lock to another daemon would wait for it forever: stop it, so
    // bursts of callers do not pile up waiting daemons.
    let holder = fs::read_to_string(dir.join("daemon.lock")).ok();
    if up && holder.is_some_and(|h| h.trim() != child.id().to_string()) {
        let _ = child.kill();
    }
    // Reap the last child whenever it exits, so a long-lived caller is not
    // left holding a zombie daemon.
    std::thread::spawn(move || child.wait());
    if up {
        Ok(())
    } else {
        Err("daemon did not start within 5 s".into())
    }
}

fn spawn_daemon(exe: &Path, dir: &Path) -> Result<Child> {
    let log = File::options()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    Ok(Command::new(exe)
        .arg("daemon")
        .env(paths::HOME_ENV, dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0) // detached from the caller's terminal signals
        .spawn()?)
}

/// A bare connect can land in the backlog of a daemon that is shutting down,
/// so liveness is a ping answered.
fn answers_ping(sock: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(sock) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut line = String::new();
    stream.write_all(b"{\"id\":0,\"op\":\"ping\"}\n").is_ok()
        && BufReader::new(stream).read_line(&mut line).is_ok()
        && line.contains("\"ok\":true")
}
