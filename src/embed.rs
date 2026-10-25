//! The engine in process: what the Lightbulb bridge links against. An embedded instance is a
//! Lightbulb workspace's member, a member like any other, whose declared sessions are the
//! workspace's agents (`cloud.sessions.set`). It opens no socket, installs no launchd job or hooks and
//! raises no dialog; otherwise it is the daemon: `boot` is the one start path both share.

use crate::config::Config;
use crate::daemon::ipc::{Caller, Request, Response, BAD_REQUEST, INTERNAL, NOT_FOUND};
use crate::daemon::{ipc, net_ops, Cloud, Daemon, TeamState};
use crate::identity::Identity;
use crate::roster;
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
    /// The relay this instance uses (`config.toml: relay` for a computer).
    pub relay: Option<RelayUrl>,
    /// Where `Instance::shutdown` leaves a final copy of `state.db`, if anywhere.
    pub snapshot: Option<PathBuf>,
    /// The workspace's display name: the name it joins as, and its solo network's.
    pub name: String,
    /// The sessions declared before the transport starts (`cloud.sessions.set`'s list), so mail
    /// for them that arrives right after start is kept, not refused as unadvertised.
    pub sessions: Vec<Value>,
    /// The member keys (computers) that may message the sessions; `None`: no gate.
    pub members: Option<std::collections::HashSet<String>>,
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
    let (daemon, net) = boot(
        home,
        config,
        Some((opts.name, lock, opts.sessions, opts.members)),
    )
    .await?;
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

/// The sessions of `list` that `cloud.sessions.set` would take, one by one: a name that is not
/// valid (or is there twice) is skipped and logged, and only the first `presence::MAX` are
/// kept, so one bad entry does not keep the whole workspace off the network.
fn declarable(short: &str, list: &[Value]) -> Vec<net::frames::Remote> {
    let mut kept: Vec<net::frames::Remote> = vec![];
    for s in list {
        match net_ops::remotes(short, std::slice::from_ref(s)) {
            Some(r) if !kept.iter().any(|k| k.session_id == r[0].session_id) => kept.extend(r),
            _ => eprintln!(
                "sessions: skipped {} (not a valid session, or declared twice)",
                s["name"]
            ),
        }
    }
    if kept.len() > presence::MAX {
        eprintln!(
            "sessions: kept the first {} of {}",
            presence::MAX,
            kept.len()
        );
        kept.truncate(presence::MAX);
    }
    kept
}

/// An instance's start: its workspace's name, the home lock it keeps, the sessions it declares
/// from the start and the member keys that may message them.
pub(crate) type CloudStart = (
    String,
    std::fs::File,
    Vec<Value>,
    Option<std::collections::HashSet<String>>,
);

/// The start path the daemon and an instance share, under `dir`'s lock (the caller's):
/// identity, store, the solo genesis on first start, the team, the transport and the presence
/// sender. An instance (its workspace's name, the home lock it keeps and the sessions it
/// declares from the start) has no dialogs; a computer's sessions are its agents'
/// (`claude_home`).
pub(crate) async fn boot(
    dir: &Path,
    config: Config,
    cloud: Option<CloudStart>,
) -> Result<(Arc<Daemon>, net::Net)> {
    let (cloud, home_lock, declared, members) = match cloud {
        Some((name, lock, sessions, members)) => (Some(name), Some(lock), sessions, members),
        None => (None, None, vec![], None),
    };
    let identity = Identity::load_or_create(dir)?;
    let declared = declarable(&identity.short, &declared);
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
    // Known but no member: removed (a stop before `removed_here`'s new genesis, a fork that
    // dropped us, or a roster this build's rules no longer replay us into). On our own network
    // again, as `removed_here` leaves it.
    if !roster.members.contains_key(&identity.key) {
        crate::outbox::refuse_all(&store, now as u64, "not_member")?;
        store.clear_roster()?;
        let at = now.max(roster.created_at + 1);
        let g = net_ops::solo_genesis(&identity, cloud.as_deref(), &config.computer, at);
        roster::accept(&store, &g, now)?;
        roster = roster::replay(&[g]);
    }
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
        codes: Mutex::default(),
        presence: Default::default(),
        built: Mutex::default(),
        latest: Mutex::new(latest),
        streams: Default::default(),
        cloud: cloud.map(|name| {
            let c = Cloud::new(name);
            if !declared.is_empty() || members.is_some() {
                c.set(declared, members);
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
    /// op is refused (`bad_request`, reason `not_a_person`) but `team.join` and `team.leave`:
    /// the caller is the bridge, acting for a
    /// workspace admin who confirmed in the app. `inbox {session}` takes a declared session's
    /// mail as Task 10's records, read. `app.send`, `app.inbox` and `app.receipt` send and read
    /// as a hosted app's declared session `{app}-{person}` (`app_send`).
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
    /// `receipt_changed`, `member_removed`, `stream_*`, …), from now on.
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.daemon.events.subscribe()
    }

    /// `wake` (the sessions file's, `member`) is this member's own doorbell: signed into the
    /// roster as its `meta` when it differs from the roster's (`publish`), and again after a
    /// join. Nothing when it is absent; skipped and logged (never the token) when malformed.
    pub(crate) fn publish_wake(&self, wake: &Value) -> Result<()> {
        if wake.is_null() {
            return Ok(());
        }
        let Some(w) = roster::Wake::parse(wake) else {
            eprintln!("wake: not a valid {{url, token}}; not published");
            return Ok(());
        };
        let d = &self.daemon;
        if let Some(c) = &d.cloud {
            *c.wake.lock().unwrap() = Some(w);
        }
        publish(d)
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

/// Signs this member's doorbell (`Cloud::wake`) into its roster as a `meta` when the roster's
/// differs.
pub(crate) fn publish(d: &Daemon) -> Result<()> {
    let Some(w) = d
        .cloud
        .as_ref()
        .and_then(|c| c.wake.lock().unwrap().clone())
    else {
        return Ok(());
    };
    let mut team = d.team.lock().unwrap();
    if team.roster.wakes.get(&d.identity.key) == Some(&w) {
        return Ok(());
    }
    let store = d.store.lock().unwrap();
    let body = json!({"wake": {"url": w.url, "token": w.token}});
    net_ops::write(
        d,
        &mut team,
        &store,
        roster::Kind::Meta,
        body,
        ipc::now() as i64,
    )?;
    eprintln!("wake: published");
    Ok(())
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

/// `inbox.peek {}` -> `{sessions: [{session, records}]}`: what waits for each declared name and
/// what was delivered but not yet read, as Task 10's records (`ipc::INBOX_RECORD`) with
/// `stage` (`delivered`, then `waiting`, each oldest first), all left as they are: `inbox.ack`
/// moves each on. A record lists until it is read, so a turn that died is handed out again.
pub(crate) fn peek(id: u64, d: &Daemon) -> Response {
    let short = &d.identity.short;
    type Group = (String, Vec<(store::Message, &'static str)>);
    let listed = || -> Result<Vec<Group>> {
        let store = d.store.lock().unwrap();
        let now = ipc::now();
        let mut out: Vec<Group> = vec![];
        let mut push = |m: store::Message, stage| {
            // Declared sessions only: mail for an agent since removed stays with the engine.
            let declared = d.cloud.as_ref().is_some_and(|c| c.declares(&m.to_key));
            let name = m.to_key.split_once('/').map(|(_, n)| n.to_string());
            let Some(name) = name.filter(|_| declared) else {
                return;
            };
            match out.iter_mut().find(|(n, _)| *n == name) {
                Some((_, ms)) => ms.push((m, stage)),
                None => out.push((name, vec![(m, stage)])),
            }
        };
        for m in mail::delivered_unread(&store, short, now)? {
            push(m, "delivered");
        }
        for key in mail::waiting_keys(&store, now)? {
            if key.starts_with(&format!("{short}/")) {
                for m in mail::waiting(&store, &Key(key), now)? {
                    push(m, "waiting");
                }
            }
        }
        Ok(out)
    };
    let Ok(listed) = listed() else {
        return Response::err(id, INTERNAL);
    };
    // After the store lock: `records` takes the team's.
    let sessions: Vec<Value> = listed
        .into_iter()
        .map(|(name, ms)| {
            let (ms, stages): (Vec<_>, Vec<_>) = ms.into_iter().unzip();
            let records: Vec<Value> = (ipc::records(d, &ms).iter().zip(stages))
                .map(|(r, stage)| {
                    let mut r = ipc::inbox_record(r);
                    r["stage"] = stage.into();
                    r
                })
                .collect();
            json!({ "session": name, "records": records })
        })
        .collect();
    Response::ok(id, json!({ "sessions": sessions }))
}

/// `inbox.ack {session, record, stage}` -> `{}`: `delivered` takes record `record` (an id from
/// `inbox.peek`) from session `session`'s mailbox (again: `{}`); `read` takes it if it still
/// waits, marks it read and drops its text. `not_found` for a record that was never session
/// `session`'s; `bad_request` for another stage.
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
        // Taken from the mailbox if it still waits; a record delivered before is as it was.
        mail::take_one(&store, &key, record, ipc::now())?;
        if store.message(record)?.is_none_or(|m| m.to_key != key.0) {
            return Ok(false);
        }
        if stage == "delivered" {
            return Ok(true);
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
