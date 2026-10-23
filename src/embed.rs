//! The engine in process: what the Lightbulb bridge links against. An embedded instance is one
//! `kind: cloud` member, a Lightbulb workspace, whose declared sessions are the workspace's
//! agents (`cloud.sessions.set`). It opens no socket, installs no launchd job or hooks and
//! raises no dialog; otherwise it is the daemon: `boot` is the one start path both share.

use crate::config::Config;
use crate::daemon::ipc::{Request, Response, BAD_REQUEST, INTERNAL};
use crate::daemon::{ipc, net_ops, Cloud, Daemon, TeamState};
use crate::identity::Identity;
use crate::roster::{self, MemberKind};
use crate::sessions::{self, Key};
use crate::store::Store;
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
    /// Where our signed relay list comes from (`config.toml: issuer`).
    pub issuer: String,
    /// Where `Instance::shutdown` leaves a final copy of `state.db`, if anywhere.
    pub snapshot: Option<PathBuf>,
    /// The workspace's display name: the name it joins as, and its solo network's.
    pub name: String,
}

/// A running instance. Dropping it without `shutdown` leaves its endpoint up.
pub struct Instance {
    daemon: Arc<Daemon>,
    net: net::Net,
    snapshot: Option<PathBuf>,
    /// `home`'s `daemon.lock`: a second start on one home fails.
    _lock: std::fs::File,
}

/// Starts the instance whose state is `home` (made 0700 if missing): the daemon's roster,
/// transport and outbox, with `opts`' relay and issuer and no LAN discovery. `Err` while
/// another engine (instance or daemon) holds `home`.
pub async fn start(home: &Path, opts: EmbedOpts) -> Result<Instance> {
    if opts.kind != MemberKind::Cloud {
        return Err("an embedded instance is a workspace's cloud member".into());
    }
    std::fs::create_dir_all(home)?;
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
    let lock = crate::daemon::lock_dir(home, false)?;
    let config = Config {
        issuer: opts.issuer,
        relay: opts.relay.map(|u| u.to_string()),
        lan: false,
        ..Config::load(home)?
    };
    let (daemon, net) = boot(home, config, Some(opts.name)).await?;
    Ok(Instance {
        daemon,
        net,
        snapshot: opts.snapshot,
        _lock: lock,
    })
}

/// The start path the daemon and an instance share, under `dir`'s lock (the caller's):
/// identity, store, the solo genesis on first start, the team, the transport and the presence
/// sender. A `cloud` instance (its workspace's name) has no dialogs and declares its sessions;
/// a computer's are its agents' (`claude_home`).
pub(crate) async fn boot(
    dir: &Path,
    config: Config,
    cloud: Option<String>,
) -> Result<(Arc<Daemon>, net::Net)> {
    let identity = Identity::load_or_create(dir)?;
    let store = Store::open(&dir.join("state.db"))?;
    let now = ipc::now() as i64;
    // The solo network's genesis, written once: a computer that has a roster keeps it.
    let mut entries = roster::entries(&store)?;
    if entries.is_empty() {
        let g = net_ops::solo_genesis(&identity, cloud.as_deref(), &config.computer, now);
        roster::accept(&store, &g, now)?;
        entries.push(g);
    }
    let team = TeamState {
        roster: roster::replay(&entries),
        config,
    };
    net_ops::sync_team_row(&store, &roster::Roster::default(), &team.roster)?;
    let latest = presence::load(&store);
    let peers = net::Peers::default();
    let (dialogs, answers) = if cloud.is_some() {
        (None, None)
    } else {
        let (q, a) = dialog::Queue::new(peers.knobs().osascript.clone());
        q.set_knob(peers.knobs().dialog.clone());
        (Some(q), Some(a))
    };
    // An instance has no agents of its own to discover: its state dir holds no `.claude`.
    let claude_home = match cloud {
        Some(_) => dir.to_path_buf(),
        None => std::env::var_os("LIGHTBULB_CONNECT_CLAUDE_HOME")
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
        dialog_answers: Mutex::new(answers),
        admissions: Mutex::default(),
        joining: tokio::sync::Mutex::new(()),
        presence: Default::default(),
        built: Mutex::default(),
        latest: Mutex::new(latest),
        streams: Default::default(),
        cloud: cloud.map(|name| Cloud {
            name,
            sessions: Mutex::default(),
        }),
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
    /// mail as Task 10's records, read.
    pub async fn call(&self, req: Value) -> Value {
        let id = req["id"].as_u64().unwrap_or(0);
        let Ok(req) = serde_json::from_value::<Request>(req) else {
            return json!(Response::err(id, BAD_REQUEST));
        };
        let (d, body) = (&self.daemon, &req.body);
        let r = match req.op.as_str() {
            "team.join" if body["kind"] != "cloud" || !body["proof"].is_string() => {
                Response::err(id, BAD_REQUEST)
            }
            "team.join" => crate::admission::join(id, body, d).await,
            "team.leave" => net_ops::leave(id, d, true).await,
            // The bridge stops an instance with `Instance::shutdown`.
            "shutdown" => Response::err(id, ipc::UNSUPPORTED),
            "inbox" => inbox(id, body, d),
            // Events come from `events`.
            "subscribe" => Response::ok(id, json!({})),
            op => ipc::op(id, op, body, d, None).await,
        };
        json!(r)
    }

    /// Every event a socket subscriber gets (`message`, `held`, `hold_closed`,
    /// `receipt_changed`, `member_removed`, `stream_*`, …), from now on.
    pub fn events(&self) -> broadcast::Receiver<Value> {
        self.daemon.events.subscribe()
    }

    /// Closes the endpoint (the instance's tasks end with it), then leaves the final snapshot.
    pub async fn shutdown(self) {
        let _ = self.daemon.events.send(json!({ "event": "shutdown" }));
        self.net.close().await;
        if let Some(p) = &self.snapshot {
            if let Err(e) = self.daemon.store.lock().unwrap().snapshot(p) {
                eprintln!("snapshot: {e}");
            }
        }
    }
}

/// The fields of Task 10's `inbox` record.
const RECORD: [&str; 10] = [
    "id",
    "request_id",
    "thread",
    "from",
    "label",
    "person",
    "person_verified",
    "computer",
    "text",
    "sent_at",
];

/// `inbox {session}` -> `{messages}`: takes what waits for declared session `session` (a
/// name), each marked read: the bridge hands it to that agent's turn.
fn inbox(id: u64, body: &Value, d: &Daemon) -> Response {
    let Some(name) = body["session"].as_str().filter(|n| sessions::valid_name(n)) else {
        return Response::err(id, BAD_REQUEST);
    };
    let key = Key(format!("{}/{name}", d.identity.short));
    let taken = {
        let store = d.store.lock().unwrap();
        mail::take(&store, &key, ipc::now()).and_then(|m| {
            m.iter().try_for_each(|m| mail::mark_read(&store, &m.id))?;
            Ok(m)
        })
    };
    let Ok(taken) = taken else {
        return Response::err(id, INTERNAL);
    };
    let messages: Vec<Value> = taken
        .iter()
        .map(|m| {
            let v = ipc::record(d, m);
            RECORD
                .iter()
                .map(|k| (k.to_string(), v[k].clone()))
                .collect()
        })
        .collect();
    Response::ok(id, json!({ "messages": messages }))
}
