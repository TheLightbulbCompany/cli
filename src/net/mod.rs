//! The member transport: a QUIC endpoint (iroh) whose key is this computer's member key, one
//! connection per member pair carrying one stream of `Frame`s, the membership gate, and delta
//! roster sync. Relays are off; peers are found on the local network (mDNS, `config.lan`).
//!
//! Sync: each side's first frame is `hello {heads}`. A `hello` is answered with the entries
//! that are not ancestors of its heads (the whole log when none is known), parents first,
//! in `roster` pages; after every roster change each side pushes that delta against the
//! peer's last heads and a fresh `hello`. Applied entries go through `roster::accept` and
//! the daemon's one install (`net_ops::install`), like a local write.

pub mod frames;

use crate::daemon::{ipc, net_ops, Daemon};
use crate::identity;
use crate::roster::{self, Entry, Roster};
use crate::Result;
use frames::Frame;
use iroh::endpoint::{presets, Connection, VarInt};
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

pub const ALPN: &[u8] = b"lightbulb-network/1";
pub const JOIN_ALPN: &[u8] = b"lightbulb-network/join/1";
pub const PROTOCOL: u32 = 1;

/// A `roster` page closes before the next entry would take it past this (half the frame cap).
const PAGE_BYTES: usize = 512 * 1024;
const PAGE_ENTRIES: usize = 500;
const REDIAL: Duration = Duration::from_secs(30);
/// Entries one connection may hold waiting for their parents.
const MAX_PENDING: usize = 10_000;

const NOT_MEMBER: u32 = 1;
const SUPERSEDED: u32 = 2;

/// `PROTOCOL`, or in debug builds `LIGHTBULB_NETWORK_DEBUG_PROTOCOL`.
pub fn protocol() -> u32 {
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var("LIGHTBULB_NETWORK_DEBUG_PROTOCOL")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return p;
    }
    PROTOCOL
}

/// Addresses to dial members at without discovery: in debug builds,
/// `LIGHTBULB_NETWORK_PEERS=<key>@<ip:port>,…`.
#[cfg_attr(not(debug_assertions), allow(unused_mut))]
fn known_addrs() -> HashMap<String, Vec<SocketAddr>> {
    let mut out: HashMap<String, Vec<SocketAddr>> = HashMap::new();
    #[cfg(debug_assertions)]
    for item in std::env::var("LIGHTBULB_NETWORK_PEERS")
        .unwrap_or_default()
        .split(',')
    {
        if let Some((key, addr)) = item.split_once('@') {
            if let Ok(a) = addr.parse() {
                out.entry(key.to_string()).or_default().push(a);
            }
        }
    }
    out
}

/// What the daemon knows of its connections; `ipc` (status) and later tasks read it.
#[derive(Default)]
pub struct Peers {
    live: Mutex<HashMap<String, Link>>,
    dialing: Mutex<HashSet<String>>,
    /// Members whose `hello` carried another protocol: key -> theirs.
    outdated: Mutex<BTreeMap<String, u32>>,
    /// Poked by every roster install.
    pub changed: Notify,
    /// The endpoint's UDP port.
    pub port: OnceLock<u16>,
}

struct Link {
    conn: Connection,
    // ponytail: unbounded; only members get a queue, bound it if one shows up growing.
    tx: mpsc::UnboundedSender<Frame>,
    /// The member key that dialed this connection.
    dialer: String,
    /// The peer's heads as of its last `hello`; `None` until a `hello` of our protocol.
    heads: Option<Vec<String>>,
}

impl Peers {
    /// Members with a live connection that said `hello` in our protocol.
    pub fn online(&self) -> Vec<String> {
        let live = self.live.lock().unwrap();
        live.iter()
            .filter(|(_, l)| l.heads.is_some())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Queues `f` on `member`'s connection; fails when it has none (the outbox is Task 8).
    pub fn send(&self, member: &str, f: Frame) -> Result<()> {
        let live = self.live.lock().unwrap();
        match live.get(member) {
            Some(l) if l.heads.is_some() => Ok(l.tx.send(f).map_err(|_| "connection closed")?),
            _ => Err("not connected".into()),
        }
    }

    /// `(key, their protocol)` for every member that needs an update.
    pub fn outdated(&self) -> Vec<(String, u32)> {
        let o = self.outdated.lock().unwrap();
        o.iter().map(|(k, p)| (k.clone(), *p)).collect()
    }

    /// Takes `conn` (dialed by `dialer`) as `key`'s connection, unless the one held wins: of
    /// two connections, the one the lower member key dialed; of two dialed by the same side,
    /// the newer. The loser is closed.
    fn adopt(
        &self,
        key: &str,
        me: &str,
        conn: &Connection,
        dialer: &str,
        tx: &mpsc::UnboundedSender<Frame>,
    ) -> bool {
        let lower = key.min(me);
        let mut live = self.live.lock().unwrap();
        if let Some(old) = live.get(key) {
            if old.dialer == lower && dialer != lower {
                return false;
            }
            old.conn.close(VarInt::from_u32(SUPERSEDED), b"superseded");
        }
        let link = Link {
            conn: conn.clone(),
            tx: tx.clone(),
            dialer: dialer.to_string(),
            heads: None,
        };
        live.insert(key.to_string(), link);
        true
    }

    /// Runs `f` on `key`'s link if it is still `conn`.
    fn with_link(&self, key: &str, conn: &Connection, f: impl FnOnce(&mut Link)) {
        let mut live = self.live.lock().unwrap();
        if let Some(l) = live
            .get_mut(key)
            .filter(|l| l.conn.stable_id() == conn.stable_id())
        {
            f(l);
        }
    }

    fn forget(&self, key: &str, conn: &Connection) {
        let mut live = self.live.lock().unwrap();
        if live
            .get(key)
            .is_some_and(|l| l.conn.stable_id() == conn.stable_id())
        {
            live.remove(key);
        }
    }
}

/// The running endpoint. Cheap to clone; the daemon's `Peers` hold the connection state.
#[derive(Clone)]
pub struct Net(Arc<Inner>);

struct Inner {
    endpoint: Endpoint,
    daemon: Arc<Daemon>,
    addrs: HashMap<String, Vec<SocketAddr>>,
    /// Members whose last dial failed, so a failure is logged once, not every 30 s.
    failed: Mutex<HashSet<String>>,
}

impl Net {
    /// Binds the endpoint (secret = the member key, relays off, mDNS when `config.lan`) and
    /// starts accepting and dialing every active member.
    pub async fn start(daemon: Arc<Daemon>) -> Result<Net> {
        let lan = daemon.team.lock().unwrap().config.lan;
        let secret = SecretKey::from_bytes(&daemon.identity.signing.to_bytes());
        let mut b = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec(), JOIN_ALPN.to_vec()]);
        if lan {
            b = b.address_lookup(iroh_mdns_address_lookup::MdnsAddressLookup::builder());
        }
        let endpoint = b.bind().await?;
        let port = endpoint.bound_sockets().iter().map(|a| a.port()).next();
        let _ = daemon.peers.port.set(port.unwrap_or(0));
        let net = Net(Arc::new(Inner {
            endpoint,
            daemon,
            addrs: known_addrs(),
            failed: Mutex::default(),
        }));
        tokio::spawn(net.clone().accept_loop());
        tokio::spawn(net.clone().keep_up());
        Ok(net)
    }

    pub fn send(&self, member: &str, f: Frame) -> Result<()> {
        self.0.daemon.peers.send(member, f)
    }

    pub fn online(&self) -> Vec<String> {
        self.0.daemon.peers.online()
    }

    pub async fn close(&self) {
        self.0.endpoint.close().await;
    }

    async fn accept_loop(self) {
        while let Some(incoming) = self.0.endpoint.accept().await {
            let net = self.clone();
            tokio::spawn(async move {
                let Ok(accepting) = incoming.accept() else {
                    return;
                };
                let Ok(conn) = accepting.await else {
                    return;
                };
                let key = identity::key_of(conn.remote_id().as_bytes());
                if conn.alpn() == JOIN_ALPN {
                    join(conn).await;
                } else {
                    net.serve(conn, key, false).await;
                }
            });
        }
    }

    /// Dials every active member on start, after every roster change and every 30 s; after
    /// a change, closes the connections of members no longer active and pushes the change.
    async fn keep_up(self) {
        let d = &self.0.daemon;
        let mut tick = tokio::time::interval(REDIAL);
        loop {
            let changed = tokio::select! {
                _ = tick.tick() => false,
                _ = d.peers.changed.notified() => true,
                _ = self.0.endpoint.closed() => return,
            };
            let to_dial = self.refresh(changed);
            for key in to_dial {
                tokio::spawn(self.clone().dial(key));
            }
        }
    }

    /// One sync section under `team`, then `store`, then the peer table: returns the active
    /// members to dial.
    fn refresh(&self, changed: bool) -> Vec<String> {
        let d = &self.0.daemon;
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        let all = if changed {
            roster::entries(&d.store.lock().unwrap()).map(roster::ordered)
        } else {
            Ok(vec![])
        };
        let all = all.unwrap_or_else(|e| {
            eprintln!("net: reading the roster: {e}");
            vec![]
        });
        let mut live = d.peers.live.lock().unwrap();
        live.retain(|key, l| {
            let active = r.members.contains_key(key);
            if !active {
                l.conn.close(VarInt::from_u32(NOT_MEMBER), b"not a member");
                eprintln!("net: closed {} (no longer a member)", short(key));
            }
            active
        });
        d.peers
            .outdated
            .lock()
            .unwrap()
            .retain(|k, _| r.members.contains_key(k));
        if changed {
            for l in live.values_mut() {
                let Some(heads) = &l.heads else { continue };
                let delta = delta(&all, r, heads);
                if !delta.is_empty() {
                    for page in pages(delta) {
                        let _ = l.tx.send(page);
                    }
                }
                let _ = l.tx.send(hello(r));
                // After the push the peer holds all of ours: the next change sends only itself.
                l.heads = Some(r.heads.clone());
            }
        }
        let mut dialing = d.peers.dialing.lock().unwrap();
        r.members
            .keys()
            .filter(|k| **k != d.identity.key && !live.contains_key(*k))
            .filter(|k| dialing.insert((*k).clone()))
            .cloned()
            .collect()
    }

    async fn dial(self, key: String) {
        let d = &self.0.daemon;
        let id = identity::key_bytes(&key).and_then(|b| PublicKey::from_bytes(&b).ok());
        let r = match id {
            Some(id) => {
                let addrs = self.0.addrs.get(&key).into_iter().flatten();
                let addr = addrs.fold(EndpointAddr::new(id), |a, s| a.with_ip_addr(*s));
                self.0.endpoint.connect(addr, ALPN).await.ok()
            }
            None => None,
        };
        d.peers.dialing.lock().unwrap().remove(&key);
        // Logged when the outcome changes, not on every redial.
        let newly_failed = {
            let mut failed = self.0.failed.lock().unwrap();
            if r.is_some() {
                failed.remove(&key);
                false
            } else {
                failed.insert(key.clone())
            }
        };
        match r {
            Some(conn) => self.serve(conn, key, true).await,
            None if newly_failed => eprintln!("net: dial {}: failed", short(&key)),
            None => {}
        }
    }

    /// One member connection, from the gate to its close.
    async fn serve(&self, conn: Connection, key: String, outgoing: bool) {
        let d = &self.0.daemon;
        let me = d.identity.key.as_str();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let dialer = if outgoing { me } else { key.as_str() };
        // The gate and the adoption are one section under `team`: a removal installed between
        // them would find nothing to close, and the ex-member's connection would be adopted.
        let refused = {
            let team = d.team.lock().unwrap();
            if key == me || !team.roster.members.contains_key(&key) {
                Some(NOT_MEMBER)
            } else {
                gate_pause();
                (!d.peers.adopt(&key, me, &conn, dialer, &tx)).then_some(SUPERSEDED)
            }
        };
        match refused {
            Some(NOT_MEMBER) => {
                conn.close(VarInt::from_u32(NOT_MEMBER), b"not a member");
                eprintln!("net: refused {} (not a member)", short(&key));
                return;
            }
            Some(code) => {
                conn.close(VarInt::from_u32(code), b"superseded");
                return;
            }
            None => {}
        }
        let streams = if outgoing {
            conn.open_bi().await
        } else {
            conn.accept_bi().await
        };
        if let Ok((mut send, mut recv)) = streams {
            eprintln!("net: connected {}", short(&key));
            let _ = tx.send(hello(&d.team.lock().unwrap().roster));
            let writer = async {
                while let Some(f) = rx.recv().await {
                    if frames::write(&mut send, &f).await.is_err() {
                        return;
                    }
                }
            };
            let reader = async {
                let mut pending = Vec::new();
                // After a `hello` of another protocol nothing more is served.
                let mut stale = false;
                while let Ok(f) = frames::read(&mut recv).await {
                    if !stale {
                        stale = !self.on_frame(&key, &conn, f, &mut pending, &tx);
                    }
                }
            };
            tokio::select! {
                _ = writer => {}
                _ = reader => {}
            }
        }
        d.peers.forget(&key, &conn);
        conn.close(VarInt::from_u32(0), b"");
        eprintln!("net: disconnected {}", short(&key));
    }

    /// Handles one frame from `key`; `false` once the peer speaks another protocol.
    fn on_frame(
        &self,
        key: &str,
        conn: &Connection,
        f: Frame,
        pending: &mut Vec<Entry>,
        tx: &mpsc::UnboundedSender<Frame>,
    ) -> bool {
        let d = &self.0.daemon;
        let team = d.team.lock().unwrap();
        let Some(peer) = team.roster.members.get(key) else {
            return true; // the next refresh closes it
        };
        let short = peer.short.clone();
        if !frames::from_peer_ok(&f, peer, d) {
            eprintln!("net: dropped a {} frame from {short}", f.kind());
        } else if let Frame::Hello { protocol: p, heads } = f {
            // ponytail: a hello costs O(heads × log) under `team`, and a member may send them
            // back to back; rate-limit per connection if one does.
            if p != protocol() {
                eprintln!("net: {short} speaks protocol {p}");
                d.peers.outdated.lock().unwrap().insert(key.to_string(), p);
                return false;
            }
            d.peers.outdated.lock().unwrap().remove(key);
            match roster::entries(&d.store.lock().unwrap()).map(roster::ordered) {
                Ok(all) => {
                    for page in pages(delta(&all, &team.roster, &heads)) {
                        let _ = tx.send(page);
                    }
                }
                Err(e) => eprintln!("net: reading the roster: {e}"),
            }
            d.peers.with_link(key, conn, |l| l.heads = Some(heads));
        } else if let Frame::Roster { entries, .. } = f {
            pending.extend(entries);
        }
        drop(team);
        // Waiting entries are retried after every frame: another connection may have
        // brought their parents.
        if !pending.is_empty() {
            if let Err(e) = apply(d, pending, &short) {
                eprintln!("net: applying roster from {short}: {e}");
            }
        }
        true
    }
}

/// In debug builds, `LIGHTBULB_NETWORK_DEBUG_GATE_PAUSE_MS` holds the gate (with `team`)
/// that long before adopting, so a test can land a removal in the window.
fn gate_pause() {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("LIGHTBULB_NETWORK_DEBUG_GATE_PAUSE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// Accepts the waiting entries whose parents are known, in order, until none is left that
/// can be; one replay and install after. Under `team`, then `store`.
fn apply(d: &Daemon, pending: &mut Vec<Entry>, from: &str) -> Result<()> {
    let mut team = d.team.lock().unwrap();
    let store = d.store.lock().unwrap();
    let now = ipc::now() as i64;
    let mut stored: HashSet<String> = HashSet::new();
    loop {
        let before = pending.len();
        pending.retain(|e| {
            let ready = e
                .parents
                .iter()
                .all(|p| team.roster.knows(p) || stored.contains(p));
            if ready {
                match roster::accept(&store, e, now) {
                    Ok(true) => {
                        stored.insert(e.hash());
                    }
                    Ok(false) => {}
                    Err(err) => eprintln!("net: refused a roster entry from {from}: {err}"),
                }
            }
            !ready
        });
        if pending.len() == before {
            break;
        }
    }
    if pending.len() > MAX_PENDING {
        eprintln!(
            "net: dropped {} waiting entries from {from}",
            pending.len() - MAX_PENDING
        );
        pending.truncate(MAX_PENDING);
    }
    if stored.is_empty() {
        return Ok(());
    }
    let new = roster::replay(&roster::entries(&store)?);
    net_ops::install(d, &mut team, &store, new)
}

/// The `JOIN_ALPN` door: Task 6 admits candidates; until then a connection whose first frame
/// is not a `join_request` is closed, and one that is goes unanswered.
async fn join(conn: Connection) {
    let first = async {
        let (_send, mut recv) = conn.accept_bi().await.ok()?;
        frames::read(&mut recv).await.ok()
    };
    match tokio::time::timeout(Duration::from_secs(10), first).await {
        Ok(Some(Frame::JoinRequest { .. })) => eprintln!("net: a join request (not served yet)"),
        _ => eprintln!("net: closed a join connection without a join request"),
    }
    conn.close(VarInt::from_u32(0), b"");
}

fn hello(r: &Roster) -> Frame {
    Frame::Hello {
        protocol: protocol(),
        heads: r.heads.clone(),
    }
}

/// Of `all` (in replay order), the entries that are neither `heads` nor their ancestors; all
/// of them when no head is known.
fn delta(all: &[Entry], r: &Roster, heads: &[String]) -> Vec<Entry> {
    let mut covered: HashSet<String> = HashSet::new();
    for h in heads.iter().filter(|h| r.knows(h)) {
        covered.extend(r.ancestors(h));
        covered.insert(h.clone());
    }
    all.iter()
        .filter(|e| !covered.contains(&e.hash()))
        .cloned()
        .collect()
}

/// `entries` as `roster` frames in order: each closes before the next entry would take its
/// encoding past `PAGE_BYTES`, or at `PAGE_ENTRIES`; `more` on all but the last. At least one
/// frame, empty when there is nothing.
fn pages(entries: Vec<Entry>) -> Vec<Frame> {
    let empty = Frame::Roster {
        entries: vec![],
        more: false,
    };
    // `"more":false` is the longer spelling, so `base` bounds every page's overhead.
    let base = serde_json::to_vec(&empty).map_or(0, |v| v.len());
    let mut out: Vec<Vec<Entry>> = vec![vec![]];
    let mut size = base;
    for e in entries {
        // The entry and a comma.
        let n = serde_json::to_vec(&e).map_or(0, |v| v.len()) + 1;
        let cur = out.last().expect("one page");
        if !cur.is_empty() && (size + n > PAGE_BYTES || cur.len() == PAGE_ENTRIES) {
            out.push(vec![]);
            size = base;
        }
        size += n;
        out.last_mut().expect("one page").push(e);
    }
    let last = out.len() - 1;
    out.into_iter()
        .enumerate()
        .map(|(i, entries)| Frame::Roster {
            entries,
            more: i < last,
        })
        .collect()
}

fn short(key: &str) -> &str {
    key.get(..12).unwrap_or(key)
}
