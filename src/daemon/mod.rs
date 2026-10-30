pub mod ipc;
pub mod net_ops;

use crate::agents::listing::{self, Listed};
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
use std::time::{Duration, Instant};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, Notify};

/// The team this computer is in, and its settings: replaced at runtime (join, leave, reload).
pub struct TeamState {
    /// The replayed roster, replaced by every write (`net_ops`), never replayed per op.
    pub roster: roster::Roster,
    pub config: Config,
    /// `config.folders_off`, as `folder` stored them (canonical): what `register` and the presence
    /// compare a session's folder with. Set with `config` (`set_config`).
    pub off: Vec<PathBuf>,
}

impl TeamState {
    /// Takes `config` and its canonical `off` (computed before the lock: `canonical` is a
    /// syscall); whether either changed (a folder may resolve anew with the file unchanged).
    pub fn set_config(&mut self, config: Config, off: Vec<PathBuf>) -> bool {
        let changed = self.config != config || self.off != off;
        self.off = off;
        self.config = config;
        changed
    }
}

/// `config.folders_off` as `folder` stored them (already canonical; lowercased again, a string
/// change), so the daemon makes no filesystem call on them: `TeamState::set_config`'s `off`.
pub fn canonical_off(config: &Config) -> Vec<PathBuf> {
    config
        .folders_off
        .iter()
        .map(|o| crate::presence::lower(o.clone()))
        .collect()
}

/// What every connection handler shares.
pub struct Daemon {
    /// The state dir.
    pub dir: PathBuf,
    pub store: Mutex<Store>,
    pub identity: Identity,
    pub team: Mutex<TeamState>,
    /// Where Claude Code's `.claude/` lives: `$LIGHTBULB_CONNECT_CLAUDE_HOME`, else the home dir.
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
    /// The person's OS dialogs, one at a time, none while `ui.claim` is live. An embedded
    /// instance's answers every dialog "not shown" at once: it asks no one.
    pub dialogs: dialog::Queue,
    /// `dialogs`' answers, for the task that routes them (admission, holds) to take.
    pub dialog_answers: Mutex<Option<dialog::Answers>>,
    /// Ticket id -> the agent session that minted that link: when it is used, its person is
    /// told (Keep/Remove). In memory: a restart forgets the notice, never the invite.
    pub agent_invites: Mutex<HashMap<String, String>>,
    /// Which surface answered each join request and hold (`J<n>`/`H<n>` -> `dialog`/`cli`),
    /// for the log and for `approve`'s "already answered" (in memory: what a person asks
    /// right after).
    pub answered_by: Mutex<HashMap<String, &'static str>>,
    /// Poked on every roster install (`net_ops::install`): waiters look again.
    pub roster_changed: Notify,
    /// Held while a `team.join` runs: one at a time.
    pub joining: tokio::sync::Mutex<()>,
    /// The live invite-code listeners (`code`), at most a few.
    pub codes: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Poked on a local session change: the presence sender (`presence::run`) looks again.
    pub presence: Notify,
    /// The presence last built (`presence::run`), sent stamped after each `hello`.
    pub built: Mutex<Option<net::frames::Frame>>,
    /// Each member's latest presence (`peers_sessions` is its copy across restarts).
    pub latest: Mutex<HashMap<String, crate::presence::Latest>>,
    /// The open streams (`streams`).
    pub streams: crate::streams::Streams,
    /// What an embedded (cloud) instance is; `None` on a computer.
    pub cloud: Option<Cloud>,
    /// Each member's sign-in proof from its latest `hello` (`proof`): what a workspace member's
    /// `members` gate reads.
    pub proofs: Mutex<HashMap<String, String>>,
    /// A computer's route link to Lightbulb, for its person's hosted apps (`link`).
    pub link: Mutex<crate::link::State>,
    /// An instance's `daemon.lock` (`lock_dir`): released only when the last task holding the
    /// daemon ends, so no second engine starts on this home while one still runs. The socket
    /// daemon holds its own across `run`.
    pub home_lock: Option<File>,
    /// Claude's own session listing, last read (`current`).
    pub listing: Arc<Mutex<ListingCache>>,
}

/// `claude agents --json --all` as last read; `rows` is `None` before the first read ends and
/// after one fails.
#[derive(Default)]
pub struct ListingCache {
    pub at: Option<Instant>,
    pub rows: Option<Vec<Listed>>,
    /// The unix second the last read began: what `rows` can know about.
    pub read_at: i64,
    /// A hook event that changes what the listing says since the last read began: read again
    /// sooner.
    pub dirty: bool,
    /// A read is running (at most one, on its own thread).
    pub refreshing: bool,
    /// The last read failed and has been logged: one line per streak.
    pub failing: bool,
    /// The read whose finished jobs have been written (`ipc::live_sessions`).
    pub recorded: Option<Instant>,
}

/// How often Claude's listing is read while a pid file says Claude is running and no hook has news.
const IDLE_READ: u64 = 30;

/// Claude's listing as last read, at once: never runs `claude` here, and never holds the lock
/// across it. Starts a read when the last is older than 30 s, or 2 s after a hook event that
/// changes what the listing says (`dirty`), or 60 s while no pid file says Claude is running (each
/// read costs about 0.2 s of CPU and a 155 MB spike); until the first ends, `None` (the pid
/// files). A failed read counts as a read: a hung `claude` is asked once per 30 s, or 60 s with
/// no pid file, so what this returns may be up to 60 s old. Ends of live rows do not wait for
/// it: they come from pid liveness on the 5 s tick. `None` when the Claude home is not `HOME` (an embedded
/// instance's, a test's): `claude` lists `HOME`'s sessions, not those.
pub(crate) fn current(daemon: &Daemon) -> Option<Vec<Listed>> {
    current_at(daemon).map(|(rows, _)| rows)
}

/// `current`, with the unix second that read began.
pub(crate) fn current_at(daemon: &Daemon) -> Option<(Vec<Listed>, i64)> {
    if dirs::home_dir().as_ref() != Some(&daemon.claude_home) {
        return None;
    }
    // A debug build (a test) never runs a `claude` it was not told to: never the person's real one.
    if cfg!(debug_assertions) && std::env::var_os("LIGHTBULB_CONNECT_DEBUG_LISTING").is_none() {
        return None;
    }
    let older = |at: Option<Instant>, s| at.is_none_or(|t| t.elapsed() > Duration::from_secs(s));
    // Reads each pid file and checks its process: only when it can matter, and off the lock
    // every session read takes.
    let at = daemon.listing.lock().unwrap().at;
    let running = older(at, IDLE_READ) && sessions::any_pid_file(&daemon.claude_home);
    let mut c = daemon.listing.lock().unwrap();
    let due = (c.dirty && older(c.at, 2)) || older(c.at, 60) || (older(c.at, IDLE_READ) && running);
    if !c.refreshing && due {
        let (cache, home) = (daemon.listing.clone(), daemon.claude_home.clone());
        let started = std::thread::Builder::new()
            .name("claude-listing".into())
            .spawn(move || refresh(&cache, &home));
        // Cleared at the start: a hook event during the read is news for the next one.
        c.refreshing = started.is_ok();
        c.dirty = c.dirty && started.is_err();
    }
    c.rows.clone().map(|rows| (rows, c.read_at))
}

/// Ends a refresh however it ends: a read that panics counts as a failed one, so the listing is
/// neither left stale nor stuck "refreshing".
struct Refreshed<'a> {
    cache: &'a Mutex<ListingCache>,
    rows: Option<Vec<Listed>>,
    began: i64,
}

impl Drop for Refreshed<'_> {
    fn drop(&mut self) {
        let mut c = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        c.rows = self.rows.take();
        c.read_at = self.began;
        c.at = Some(Instant::now());
        c.refreshing = false;
    }
}

/// One read of the listing, its result swapped into `cache`.
fn refresh(cache: &Mutex<ListingCache>, home: &Path) {
    let mut done = Refreshed {
        cache,
        rows: None,
        began: ipc::now() as i64,
    };
    let exes = listing::candidates(home);
    done.rows = listing::read(&exes);
    let first_failure = {
        let mut c = cache.lock().unwrap();
        let first = done.rows.is_none() && !exes.is_empty() && !c.failing;
        c.failing = done.rows.is_none() && (c.failing || first);
        first
    };
    if first_failure {
        eprintln!("claude agents: no answer from {exes:?}; using the pid files");
    }
}

/// An embedded instance: a Lightbulb workspace's member.
pub struct Cloud {
    /// The workspace's display name: its solo network's member and team name.
    pub name: String,
    declared: Mutex<Declared>,
    /// The last `Declared::version` the presence sender (`presence::run`) handed on.
    pub sent: tokio::sync::watch::Sender<u64>,
    /// This member's own doorbell (the sessions file's `wake`), republished after a join.
    pub wake: Mutex<Option<crate::roster::Wake>>,
}

/// The sessions an instance declared (`cloud.sessions.set`), as published; each set is a new
/// `version`.
#[derive(Clone, Default)]
pub struct Declared {
    pub version: u64,
    pub sessions: Arc<Vec<net::frames::Remote>>,
    pub ids: Arc<HashSet<String>>,
    /// The people (Lightbulb user ids) whose computers may message these sessions, by their
    /// sign-in proof (`proof::admitted`); `None`: anyone the policy admits.
    pub members: Option<Arc<HashSet<String>>>,
    /// Computers refused even with a valid proof (the backend's remote sign-out).
    pub revoked: Arc<HashSet<String>>,
}

impl Cloud {
    pub fn new(name: String) -> Cloud {
        Cloud {
            name,
            declared: Mutex::default(),
            sent: tokio::sync::watch::Sender::new(0),
            wake: Mutex::default(),
        }
    }

    pub fn declared(&self) -> Declared {
        self.declared.lock().unwrap().clone()
    }

    /// Whether `addr` is one of the declared sessions.
    pub fn declares(&self, addr: &str) -> bool {
        self.declared.lock().unwrap().ids.contains(addr)
    }

    /// Makes `sessions` the declared list: its version.
    pub fn set(
        &self,
        sessions: Vec<net::frames::Remote>,
        members: Option<HashSet<String>>,
        revoked: HashSet<String>,
    ) -> u64 {
        let mut d = self.declared.lock().unwrap();
        let ids = sessions.iter().map(|s| s.session_id.clone()).collect();
        *d = Declared {
            version: d.version + 1,
            sessions: Arc::new(sessions),
            ids: Arc::new(ids),
            members: members.map(Arc::new),
            revoked: Arc::new(revoked),
        };
        d.version
    }
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
    // The pid files name every push socket: no need to wait on Claude's listing here.
    let Some(session) = sessions::all(&rows, None, &daemon.claude_home, &daemon.identity.short)
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
        let framed = frame::framed(&ipc::sender_label(&daemon, &m), &m.request_id, &m.body);
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
            // Its `delivered` goes to the sender now, not on the flush loop's next tick.
            Ok(Some(_)) => daemon.peers.outbox.notify_one(),
            Ok(None) => return,
            Err(e) => {
                eprintln!("push to {}: {e}", to.0);
                return;
            }
        }
    }
}

/// `dir`'s lock file (`lock_dir`). Whether an engine runs is whether this file is locked,
/// never the pid in it: the pid stays after an exit and may name any process by now.
pub(crate) fn lock_path(dir: &Path) -> PathBuf {
    dir.join("daemon.lock")
}

/// `dir`'s `daemon.lock`, held (and our pid written in it) until the file drops: one engine,
/// daemon or embedded instance, per state dir. With `wait` it blocks until the holder is gone;
/// without, it fails at once.
pub(crate) fn lock_dir(dir: &Path, wait: bool) -> Result<File> {
    let mut lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(dir))?;
    if wait {
        lock.lock()?;
    } else if lock.try_lock().is_err() {
        return Err("already running: another engine holds this state dir".into());
    }
    lock.set_len(0)?;
    writeln!(lock, "{}", std::process::id())?;
    Ok(lock)
}

/// Runs the daemon for `dir` until a `shutdown` op. While another daemon holds `daemon.lock`
/// it waits, touching nothing, and takes over when that one exits: so launchd's copy, started
/// while a spawned daemon serves, neither exits into a respawn loop nor serves twice.
pub async fn run(dir: &Path) -> Result<()> {
    // Blocks this (otherwise idle) thread; nothing else runs until the lock is ours.
    let _lock = lock_dir(dir, true)?;
    let config = Config::load(dir)?;
    let (daemon, net) = crate::embed::boot(dir, config, None).await?;

    // We hold the lock, so any socket file left behind is stale.
    let sock = dir.join(paths::SOCKET);
    match fs::remove_file(&sock) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    // The secret patterns compile now, not inside the first capture write the hook waits for.
    crate::memory::refuse_secret("");
    let listener = UnixListener::bind(&sock)?;
    // Briefly wider than 0600 after bind; the 0700 state dir closes that window.
    fs::set_permissions(&sock, Permissions::from_mode(0o600))?;
    tokio::spawn(crate::admission::route(daemon.clone()));
    cli::refresh_setup(dir);
    tokio::spawn(auto_update(daemon.clone(), dir.to_path_buf()));
    tokio::spawn(housekeeping(daemon.clone()));
    tokio::spawn(crate::telemetry::run_daily(daemon.clone()));
    // While signed in: the route link for this person's hosted apps.
    tokio::spawn(crate::link::run(daemon.clone()));
    // While signed in and the person chose it: closed sessions go to Lightbulb.
    tokio::spawn(crate::upload::run(daemon.clone()));
    // SIGTERM (launchd's stop) and SIGINT end it like a `shutdown` op: the socket and the
    // loopback address file (`net::local`) are removed on the way out.
    let d = daemon.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let (Ok(mut term), Ok(mut int)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) else {
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        d.shutdown.notify_one();
    });
    ipc::serve(listener, daemon).await;
    net.close().await;
    let _ = fs::remove_file(&sock);
    Ok(()) // `lock` drops here, after the socket is gone.
}

/// Checks for a newer release ten minutes after the start and then daily (`cli::auto_update`).
/// An installed one ends the daemon, and the service manager starts the new binary; a failed
/// check logs one line and waits for the next day.
async fn auto_update(daemon: Arc<Daemon>, dir: PathBuf) {
    tokio::time::sleep(Duration::from_secs(600)).await;
    loop {
        // From the file each time: `update --auto off` reaches a running service.
        let on = crate::config::Config::load(&dir).is_ok_and(|c| cli::auto_update_on(&c));
        if on {
            let dir = dir.clone();
            match tokio::task::spawn_blocking(move || cli::auto_update(&dir)).await {
                Ok(Ok(true)) => return daemon.shutdown.notify_one(),
                Ok(Ok(false)) => {}
                Ok(Err(e)) => eprintln!("auto-update: {e}"),
                Err(_) => eprintln!("auto-update: the check failed"),
            }
        }
        tokio::time::sleep(Duration::from_secs(86_400)).await;
    }
}

/// Housekeeping, from the start: the vector backfill every minute, and the turns older than
/// `capture_turns_days` deleted once a day.
async fn housekeeping(daemon: Arc<Daemon>) {
    let mut pruned: Option<Instant> = None;
    loop {
        ipc::embed_missing(&daemon);
        if pruned.is_none_or(|t| t.elapsed() >= Duration::from_secs(86_400)) {
            let days = Config::load(&daemon.dir).map_or(90, |c| c.capture_turns_days);
            let now = ipc::now() as i64;
            if let Err(e) = crate::capture::prune_turns(&daemon.store.lock().unwrap(), now, days) {
                eprintln!("prune turns: {e}");
            }
            pruned = Some(Instant::now());
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
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
    crate::telemetry::sync_env_optout(&dir);
    let sock = dir.join(paths::SOCKET);
    if answers_ping(&sock) {
        return Ok(());
    }
    // A daemon would refuse at boot (`embed::KEY_MISMATCH`): say so now, so a hook or the MCP
    // server does not wait out a start that cannot happen.
    if crate::embed::key_mismatch(&dir).unwrap_or(false) {
        return Err(crate::embed::KEY_MISMATCH.into());
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
    ensure_running_at(&daemon_exe()?, &dir)
}

/// The binary a daemon is started from: for the real install (the default state dir) the
/// canonical one when it exists, never the caller's own: a hook of an old build still running
/// during an upgrade would otherwise start an old daemon, which then holds the lock against
/// the new one. A side-by-side state dir (`LIGHTBULB_CONNECT_HOME`) runs the caller's.
fn daemon_exe() -> Result<PathBuf> {
    let canonical = dirs::home_dir()
        .filter(|_| std::env::var_os(paths::HOME_ENV).is_none())
        .map(|h| cli::canonical(&h))
        .filter(|p| p.exists());
    Ok(match canonical {
        Some(p) => p,
        None => std::env::current_exe()?,
    })
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
    let holder = fs::read_to_string(lock_path(dir)).ok();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A read that panics counts as a failed one: the next look may start another.
    #[test]
    fn a_panicking_read_leaves_the_listing_usable() {
        let cache = Mutex::new(ListingCache {
            refreshing: true,
            rows: Some(vec![]),
            ..Default::default()
        });
        std::thread::scope(|s| {
            let read = s.spawn(|| {
                let _done = Refreshed {
                    cache: &cache,
                    rows: None,
                    began: 0,
                };
                panic!("the read");
            });
            assert!(read.join().is_err());
        });
        let c = cache.lock().unwrap();
        assert!(!c.refreshing && c.rows.is_none() && c.at.is_some());
    }
}
