//! The engine in process: what the Lightbulb bridge links against. An embedded instance is one
//! `kind: cloud` member, a Lightbulb workspace, whose declared sessions are the workspace's
//! agents (`cloud.sessions.set`). It opens no socket, installs no launchd job or hooks and
//! raises no dialog; otherwise it is the daemon: `boot` is the one start path both share.

use crate::config::Config;
use crate::daemon::ipc::{Caller, Request, Response, BAD_REQUEST, INTERNAL, NOT_FOUND};
use crate::daemon::{ipc, net_ops, Cloud, Daemon, TeamState};
use crate::identity::Identity;
use crate::roster::{self, MemberKind};
use crate::sessions::{self, Key};
use crate::store::{self, Store};
use crate::{dialog, mail, net, presence, Result};
use iroh::RelayUrl;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

pub struct EmbedOpts {
    /// Always `Cloud`: an instance is a workspace's member.
    pub kind: MemberKind,
    /// The relay this instance uses (`config.toml: relay` for a computer).
    pub relay: Option<RelayUrl>,
    /// Where `Instance::shutdown` leaves a final copy of `state.db`, if anywhere.
    pub snapshot: Option<PathBuf>,
    /// The workspace's display name: the name it joins as, and its solo network's.
    pub name: String,
    /// The sessions declared before the transport starts (`cloud.sessions.set`'s list), so mail
    /// for them that arrives right after start is kept, not refused as unadvertised.
    pub sessions: Vec<Value>,
}

/// A running instance. Dropping it without `shutdown` closes its endpoint all the same (no
/// final snapshot); its home is free once the last of its tasks ends.
pub struct Instance {
    daemon: Arc<Daemon>,
    net: net::Net,
    snapshot: Option<PathBuf>,
    /// Set by `shutdown`: nothing is left for `drop` to close.
    closed: bool,
}

/// Starts the instance whose state is `home` (made 0700 if missing): the daemon's roster,
/// transport and outbox, with `opts`' relay and no LAN discovery. `Err` while
/// another engine (instance or daemon) holds `home`.
pub async fn start(home: &Path, opts: EmbedOpts) -> Result<Instance> {
    if opts.kind != MemberKind::Cloud {
        return Err("an embedded instance is a workspace's cloud member".into());
    }
    std::fs::create_dir_all(home)?;
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
    if let Some(p) = &opts.snapshot {
        if !p.to_str().is_some_and(|s| net_ops::outside(home, s)) {
            return Err(
                "the snapshot path must be absolute and outside the instance's home".into(),
            );
        }
    }
    let lock = crate::daemon::lock_dir(home, false)?;
    // An instance's computer is its workspace, never the host it runs on: the name lands in
    // every peer's roster.
    let config = Config {
        computer: opts.name.clone(),
        relay: opts.relay.map(|u| u.to_string()),
        lan: false,
        ..Config::load(home)?
    };
    let (daemon, net) = boot(home, config, Some((opts.name, lock, opts.sessions))).await?;
    Ok(Instance {
        daemon,
        net,
        snapshot: opts.snapshot,
        closed: false,
    })
}

/// `boot`'s refusal when `identity.key` was replaced (or lost) under a kept `state.db`: the
/// team record is signed for the old key, which nothing here can sign as any more.
pub const KEY_MISMATCH: &str = "identity.key does not match this computer's team record. Run lightbulb leave, then join again with a new invite.";

/// Whether `roster` never knew `identity`'s key, as a member or a signer: `identity.key` is
/// not the key the team record was made for.
fn mismatched(identity: &Identity, roster: &roster::Roster) -> bool {
    let key = identity.key.as_str();
    !roster.members.contains_key(key)
        && !(roster.log().iter()).any(|(_, e)| e.signer == key || e.body["member"] == key)
}

/// `mismatched` for `dir` as it is on disk, for a caller that does not hold the lock: nothing
/// is written. No roster: `false`; a roster but no `identity.key`: `true` (a start would make a
/// key the record never knew).
pub fn key_mismatch(dir: &Path) -> Result<bool> {
    if !dir.join("state.db").exists() {
        return Ok(false);
    }
    let entries = roster::entries(&Store::open(&dir.join("state.db"))?)?;
    if entries.is_empty() {
        return Ok(false);
    }
    if !dir.join("identity.key").exists() {
        return Ok(true);
    }
    Ok(mismatched(
        &Identity::load_or_create(dir)?,
        &roster::replay(&entries),
    ))
}

/// The start path the daemon and an instance share, under `dir`'s lock (the caller's):
/// identity, store, the solo genesis on first start, the team, the transport and the presence
/// sender. A `cloud` instance (its workspace's name, the home lock it keeps and the sessions it
/// declares from the start) has no dialogs; a computer's sessions are its agents'
/// (`claude_home`).
pub(crate) async fn boot(
    dir: &Path,
    config: Config,
    cloud: Option<(String, std::fs::File, Vec<Value>)>,
) -> Result<(Arc<Daemon>, net::Net)> {
    let (cloud, home_lock, declared) = match cloud {
        Some((name, lock, sessions)) => (Some(name), Some(lock), sessions),
        None => (None, None, vec![]),
    };
    let identity = Identity::load_or_create(dir)?;
    let declared = net_ops::remotes(&identity.short, &declared)
        .ok_or("the declared sessions are not valid")?;
    let store = Store::open(&dir.join("state.db"))?;
    let now = ipc::now() as i64;
    // The solo network's genesis, written once: a computer that has a roster keeps it.
    let mut entries = roster::entries(&store)?;
    if entries.is_empty() {
        let g = net_ops::solo_genesis(&identity, cloud.as_deref(), &config.computer, now);
        roster::accept(&store, &g, now)?;
        entries.push(g);
    }
    let mut roster = roster::replay(&entries);
    if mismatched(&identity, &roster) {
        return Err(KEY_MISMATCH.into());
    }
    // Known but no member: removed (a stop before `removed_here`'s new genesis, or a fork that
    // dropped us). On our own network again, as `removed_here` leaves it.
    if !roster.members.contains_key(&identity.key) {
        crate::outbox::refuse_all(&store, now as u64, "not_member")?;
        store.clear_roster()?;
        let at = now.max(roster.created_at + 1);
        let g = net_ops::solo_genesis(&identity, cloud.as_deref(), &config.computer, at);
        roster::accept(&store, &g, now)?;
        roster = roster::replay(&[g]);
    }
    roster.lapse(now);
    let mut team = TeamState {
        roster,
        config: Config::default(),
        off: vec![],
    };
    let off = crate::daemon::canonical_off(&config);
    team.set_config(config, off);
    net_ops::sync_team_row(&store, &roster::Roster::default(), &team.roster)?;
    let latest = presence::load(&store);
    let peers = net::Peers::default();
    let (dialogs, answers) = if cloud.is_some() {
        dialog::Queue::headless()
    } else {
        let (q, a) = dialog::Queue::new(peers.knobs().osascript.clone());
        q.set_knob(peers.knobs().dialog.clone());
        (q, a)
    };
    // An instance has no agents of its own to discover: its state dir holds no `.claude`.
    let claude_home = match cloud {
        Some(_) => dir.to_path_buf(),
        None => (cfg!(debug_assertions)
            .then(|| std::env::var_os("LIGHTBULB_CONNECT_CLAUDE_HOME"))
            .flatten())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .ok_or("no home directory")?,
    };
    let daemon = Arc::new(Daemon {
        dir: dir.to_path_buf(),
        store: Mutex::new(store),
        identity,
        team: Mutex::new(team),
        claude_home,
        events: broadcast::channel(256).0,
        shutdown: Default::default(),
        mail: Default::default(),
        mailbox_locks: Mutex::default(),
        push_failed: Mutex::default(),
        peers,
        dialogs,
        dialog_answers: Mutex::new(Some(answers)),
        admissions: Mutex::default(),
        answered_by: Mutex::default(),
        roster_changed: Default::default(),
        joining: tokio::sync::Mutex::new(()),
        presence: Default::default(),
        built: Mutex::default(),
        latest: Mutex::new(latest),
        streams: Default::default(),
        cloud: cloud.map(|name| {
            let c = Cloud::new(name);
            if !declared.is_empty() {
                c.set(declared);
            }
            c
        }),
        home_lock,
    });
    // No join connection outlives a restart: its open requests are over.
    daemon.store.lock().unwrap().expire_joins(i64::MAX, now)?;
    let net = net::Net::start(daemon.clone()).await?;
    tokio::spawn(presence::run(daemon.clone()));
    Ok((daemon, net))
}

impl Instance {
    /// One IPC op (`{id, op, ...}`, as on the socket), answered in process. Every person-only
    /// op is refused (`bad_request`, reason `not_a_person`) but `team.join` as the workspace's
    /// cloud member (with its proof) and `team.leave`: the caller is the bridge, acting for a
    /// workspace admin who confirmed in the app. `inbox {session}` takes a declared session's
    /// mail as Task 10's records, read. `app.send`, `app.inbox` and `app.receipt` carry a
    /// hosted app's call to its person's own computer (`app_send`); `decision.send` carries a
    /// person's tap on a hold notice back to the holding computer (`decision`).
    pub async fn call(&self, req: Value) -> Value {
        json!(self.respond(req).await)
    }

    /// `call`'s answer as a `Response`, its body apart (`member` nests it as `result`).
    pub(crate) async fn respond(&self, req: Value) -> Response {
        let id = req["id"].as_u64().unwrap_or(0);
        match serde_json::from_value::<Request>(req) {
            Ok(r) => ipc::op(id, &r.op, &r.body, &self.daemon, Caller::Instance).await,
            Err(_) => Response::err(id, BAD_REQUEST),
        }
    }

    /// Every event a socket subscriber gets (`message`, `held`, `hold_closed`,
    /// `receipt_changed`, `member_removed`, `stream_*`, …), and an instance's own
    /// (`hold_notice`, `decision_result`: `decision`), from now on.
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.daemon.events.subscribe()
    }

    /// Closes the endpoint and waits (up to 5 s) for the instance's tasks to end with it, then
    /// leaves the final snapshot. The home is free when this returns, unless a task outlived
    /// the wait.
    pub async fn shutdown(mut self) {
        self.closed = true;
        let _ = self.daemon.events.send(json!({ "event": "shutdown" }));
        self.net.close().await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        // Ours and the endpoint's hold the daemon; any other is a task still running.
        let running = |i: &Instance| !i.net.idle() || Arc::strong_count(&i.daemon) > 2;
        while running(&self) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if let Some(p) = &self.snapshot {
            if let Err(e) = self.daemon.store.lock().unwrap().snapshot(p) {
                eprintln!("snapshot: {e}");
            }
        }
    }
}

/// A dropped instance (a panic, a cancelled future) shuts down too: its endpoint is closed in
/// the background, and its tasks, which hold the daemon and so the home lock, end with it.
impl Drop for Instance {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let net = self.net.clone();
            rt.spawn(async move { net.close().await });
        }
    }
}

/// `inbox {session}` -> `{messages}`: takes what waits for declared session `session` (a
/// name), each marked read: the bridge hands it to that agent's turn.
pub(crate) fn inbox(id: u64, body: &Value, d: &Daemon) -> Response {
    let Some(name) = body["session"].as_str().filter(|n| sessions::valid_name(n)) else {
        return Response::err(id, BAD_REQUEST);
    };
    match take_read(d, &Key(format!("{}/{name}", d.identity.short))) {
        Ok(messages) => Response::ok(id, json!({ "messages": messages })),
        Err(_) => Response::err(id, INTERNAL),
    }
}

/// Takes what waits for `key`, each marked read, as Task 10's records (`ipc::INBOX_RECORD`).
pub(crate) fn take_read(d: &Daemon, key: &Key) -> Result<Vec<Value>> {
    let taken = {
        let store = d.store.lock().unwrap();
        let m = mail::take(&store, key, ipc::now())?;
        m.iter().try_for_each(|m| mail::mark_read(&store, &m.id))?;
        m
    };
    d.peers.outbox.notify_one();
    Ok(ipc::records(d, &taken)
        .iter()
        .map(ipc::inbox_record)
        .collect())
}

/// `inbox.peek {}` -> `{sessions: [{session, records}]}`: what waits for each declared name, as
/// Task 10's records (`ipc::INBOX_RECORD`), left waiting: `inbox.ack` moves each on.
pub(crate) fn peek(id: u64, d: &Daemon) -> Response {
    let prefix = format!("{}/", d.identity.short);
    let waiting = || -> Result<Vec<(String, Vec<store::Message>)>> {
        let store = d.store.lock().unwrap();
        let now = ipc::now();
        let mut out = vec![];
        for key in mail::waiting_keys(&store, now)? {
            let Some(name) = key
                .strip_prefix(&prefix)
                .filter(|n| sessions::valid_name(n))
            else {
                continue;
            };
            out.push((
                name.to_string(),
                mail::waiting(&store, &Key(key.clone()), now)?,
            ));
        }
        Ok(out)
    };
    let Ok(waiting) = waiting() else {
        return Response::err(id, INTERNAL);
    };
    // After the store lock: `records` takes the team's.
    let sessions: Vec<Value> = waiting
        .iter()
        .map(|(name, m)| {
            let records: Vec<Value> = ipc::records(d, m).iter().map(ipc::inbox_record).collect();
            json!({ "session": name, "records": records })
        })
        .collect();
    Response::ok(id, json!({ "sessions": sessions }))
}

/// `inbox.ack {session, record, stage}` -> `{}`: `delivered` takes record `record` (an id from
/// `inbox.peek`) from session `session`'s mailbox; `read` marks it read and drops its text.
/// `not_found` for a record that is not (or no longer) there; `bad_request` for another stage.
pub(crate) fn ack(id: u64, body: &Value, d: &Daemon) -> Response {
    let (Some(name), Some(record), Some(stage)) = (
        body["session"].as_str().filter(|n| sessions::valid_name(n)),
        body["record"].as_str(),
        body["stage"]
            .as_str()
            .filter(|s| ["delivered", "read"].contains(s)),
    ) else {
        return Response::err(id, BAD_REQUEST);
    };
    let key = Key(format!("{}/{name}", d.identity.short));
    let found = || -> Result<bool> {
        let store = d.store.lock().unwrap();
        if stage == "delivered" {
            return Ok(mail::take_one(&store, &key, record, ipc::now())?.is_some());
        }
        if store.message(record)?.is_none_or(|m| m.to_key != key.0) {
            return Ok(false);
        }
        mail::mark_read(&store, record)?;
        mail::forget(&store)?;
        Ok(true)
    };
    match found() {
        Ok(true) => {
            d.peers.outbox.notify_one();
            Response::ok(id, json!({}))
        }
        Ok(false) => Response::err(id, NOT_FOUND),
        Err(_) => Response::err(id, INTERNAL),
    }
}
