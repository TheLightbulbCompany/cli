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
use std::collections::{HashMap, HashSet};
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

/// The debug knobs, read once (`Peers::knobs`); release builds take the defaults.
pub struct Knobs {
    /// `PROTOCOL`, or `LIGHTBULB_NETWORK_DEBUG_PROTOCOL`.
    pub protocol: u32,
    /// Addresses to dial members at without discovery: `LIGHTBULB_NETWORK_PEERS=<key>@<ip:port>,…`.
    peers: HashMap<String, Vec<SocketAddr>>,
    /// `LIGHTBULB_NETWORK_DEBUG_GATE_PAUSE_MS`: the gate holds `team` that long before adopting,
    /// so a test can land a removal in the window.
    gate_pause: Option<Duration>,
}

impl Knobs {
    #[cfg(debug_assertions)]
    fn read() -> Knobs {
        let var = |name| std::env::var(name).ok();
        let mut peers: HashMap<String, Vec<SocketAddr>> = HashMap::new();
        for item in var("LIGHTBULB_NETWORK_PEERS")
            .unwrap_or_default()
            .split(',')
        {
            if let Some((key, addr)) = item.split_once('@') {
                if let Ok(a) = addr.parse() {
                    peers.entry(key.to_string()).or_default().push(a);
                }
            }
        }
        Knobs {
            protocol: var("LIGHTBULB_NETWORK_DEBUG_PROTOCOL")
                .and_then(|v| v.parse().ok())
                .unwrap_or(PROTOCOL),
            peers,
            gate_pause: var("LIGHTBULB_NETWORK_DEBUG_GATE_PAUSE_MS")
                .and_then(|v| v.parse().ok())
                .map(Duration::from_millis),
        }
    }

    #[cfg(not(debug_assertions))]
    fn read() -> Knobs {
        Knobs {
            protocol: PROTOCOL,
            peers: HashMap::new(),
            gate_pause: None,
        }
    }
}

/// What the daemon knows of its connections; `ipc` (status) and later tasks read it.
#[derive(Default)]
pub struct Peers {
    live: Mutex<HashMap<String, Link>>,
    knobs: OnceLock<Knobs>,
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
    protocol: Protocol,
}

/// What a link's peer said in its last `hello`.
enum Protocol {
    /// No `hello` yet.
    Unknown,
    /// Our protocol, with its heads as of then.
    Ok(Vec<String>),
    /// Another protocol (theirs): nothing more is served.
    Outdated(u32),
}

impl Peers {
    pub fn knobs(&self) -> &Knobs {
        self.knobs.get_or_init(Knobs::read)
    }

    /// Members with a live connection that said `hello` in our protocol.
    pub fn online(&self) -> Vec<String> {
        let live = self.live.lock().unwrap();
        live.iter()
            .filter(|(_, l)| matches!(l.protocol, Protocol::Ok(_)))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// For `status`, in one pass: how many members are online, and `(key, their protocol)`
    /// for each that needs an update, by key.
    pub fn status(&self) -> (usize, Vec<(String, u32)>) {
        let live = self.live.lock().unwrap();
        let mut online = 0;
        let mut outdated = Vec::new();
        for (k, l) in live.iter() {
            match l.protocol {
                Protocol::Ok(_) => online += 1,
                Protocol::Outdated(p) => outdated.push((k.clone(), p)),
                Protocol::Unknown => {}
            }
        }
        outdated.sort();
        (online, outdated)
    }

    /// Queues `f` on `member`'s connection; fails when it has none (the outbox is Task 8).
    pub fn send(&self, member: &str, f: Frame) -> Result<()> {
        let live = self.live.lock().unwrap();
        match live.get(member) {
            Some(l) if matches!(l.protocol, Protocol::Ok(_)) => {
                l.tx.send(f).map_err(|_| "connection closed".into())
            }
            _ => Err("not connected".into()),
        }
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
            protocol: Protocol::Unknown,
        };
        live.insert(key.to_string(), link);
        true
    }

    /// Runs `f` on `key`'s link if it is still `conn`.
    fn with_link(&self, key: &str, conn: &Connection, f: impl FnOnce(&mut Link)) {
        if let Some(l) = current(&mut self.live.lock().unwrap(), key, conn) {
            f(l);
        }
    }

    fn forget(&self, key: &str, conn: &Connection) {
        let mut live = self.live.lock().unwrap();
        if current(&mut live, key, conn).is_some() {
            live.remove(key);
        }
    }
}

/// `key`'s link, if it is still `conn`.
fn current<'a>(
    live: &'a mut HashMap<String, Link>,
    key: &str,
    conn: &Connection,
) -> Option<&'a mut Link> {
    live.get_mut(key)
        .filter(|l| l.conn.stable_id() == conn.stable_id())
}

/// A member's dialing: one in flight, and whether the last one failed (logged once, not
/// every 30 s).
#[derive(Default)]
struct DialState {
    dialing: bool,
    failed: bool,
}

/// One connection's incoming roster sync.
#[derive(Default)]
struct Incoming {
    /// The pages of the sync in progress.
    page: Vec<Entry>,
    /// Entries waiting for their parents.
    pending: Vec<Entry>,
    /// The roster heads at the last `apply`.
    tried: Vec<String>,
}

/// The running endpoint. Cheap to clone; the daemon's `Peers` hold the connection state.
#[derive(Clone)]
pub struct Net(Arc<Inner>);

struct Inner {
    endpoint: Endpoint,
    daemon: Arc<Daemon>,
    dials: Mutex<HashMap<String, DialState>>,
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
        let port = endpoint.bound_sockets().first().map_or(0, |a| a.port());
        let _ = daemon.peers.port.set(port);
        daemon.peers.knobs();
        let net = Net(Arc::new(Inner {
            endpoint,
            daemon,
            dials: Mutex::default(),
        }));
        tokio::spawn(net.clone().accept_loop());
        tokio::spawn(net.clone().keep_up());
        Ok(net)
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

    /// One sync section under `team`, then the peer table: returns the active
    /// members to dial.
    fn refresh(&self, changed: bool) -> Vec<String> {
        let d = &self.0.daemon;
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        let mut live = d.peers.live.lock().unwrap();
        live.retain(|key, l| {
            let active = r.members.contains_key(key);
            if !active {
                l.conn.close(VarInt::from_u32(NOT_MEMBER), b"not a member");
                eprintln!("net: closed {} (no longer a member)", short(key));
            }
            active
        });
        if changed {
            // Peers with equal heads share one delta.
            let mut memo = HashMap::new();
            let protocol = d.peers.knobs().protocol;
            for l in live.values_mut() {
                let Protocol::Ok(heads) = &l.protocol else {
                    continue;
                };
                push_delta(&l.tx, r, heads, &mut memo, true);
                let _ = l.tx.send(hello(r, protocol));
                // After the push the peer holds all of ours: the next change sends only itself.
                l.protocol = Protocol::Ok(r.heads.clone());
            }
        }
        let mut dials = self.0.dials.lock().unwrap();
        r.members
            .keys()
            .filter(|k| **k != d.identity.key && !live.contains_key(*k))
            .filter(|k| {
                !std::mem::replace(&mut dials.entry((*k).clone()).or_default().dialing, true)
            })
            .cloned()
            .collect()
    }

    async fn dial(self, key: String) {
        let d = &self.0.daemon;
        let id = identity::key_bytes(&key).and_then(|b| PublicKey::from_bytes(&b).ok());
        let r = match id {
            Some(id) => {
                let addrs = d.peers.knobs().peers.get(&key).into_iter().flatten();
                let addr = addrs.fold(EndpointAddr::new(id), |a, s| a.with_ip_addr(*s));
                self.0.endpoint.connect(addr, ALPN).await.ok()
            }
            None => None,
        };
        // Logged when the outcome changes, not on every redial.
        let newly_failed = {
            let mut dials = self.0.dials.lock().unwrap();
            let s = dials.entry(key.clone()).or_default();
            s.dialing = false;
            !std::mem::replace(&mut s.failed, r.is_none()) && r.is_none()
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
        let refused: Option<(u32, &[u8])> = {
            let team = d.team.lock().unwrap();
            if key == me || !team.roster.members.contains_key(&key) {
                Some((NOT_MEMBER, b"not a member"))
            } else {
                if let Some(p) = d.peers.knobs().gate_pause {
                    std::thread::sleep(p);
                }
                if d.peers.adopt(&key, me, &conn, dialer, &tx) {
                    // The first frame, written once the stream is open.
                    let _ = tx.send(hello(&team.roster, d.peers.knobs().protocol));
                    None
                } else {
                    Some((SUPERSEDED, b"superseded"))
                }
            }
        };
        if let Some((code, reason)) = refused {
            conn.close(VarInt::from_u32(code), reason);
            if code == NOT_MEMBER {
                eprintln!("net: refused {} (not a member)", short(&key));
            }
            return;
        }
        let streams = if outgoing {
            conn.open_bi().await
        } else {
            conn.accept_bi().await
        };
        if let Ok((mut send, mut recv)) = streams {
            eprintln!("net: connected {}", short(&key));
            let writer = async {
                while let Some(f) = rx.recv().await {
                    if frames::write(&mut send, &f).await.is_err() {
                        return;
                    }
                }
            };
            let reader = async {
                let mut sync = Incoming::default();
                // After a `hello` of another protocol nothing more is served.
                let mut stale = false;
                while let Ok(f) = frames::read(&mut recv).await {
                    if !stale {
                        stale = !self.on_frame(&key, &conn, f, &mut sync, &tx);
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
        sync: &mut Incoming,
        tx: &mpsc::UnboundedSender<Frame>,
    ) -> bool {
        let d = &self.0.daemon;
        let team = d.team.lock().unwrap();
        let Some(peer) = team.roster.members.get(key) else {
            return true; // the next refresh closes it
        };
        let short = peer.short.clone();
        let mut synced = false;
        if !frames::from_peer_ok(&f, peer, d) {
            eprintln!("net: dropped a {} frame from {short}", f.kind());
        } else if let Frame::Hello { protocol: p, heads } = f {
            // ponytail: a hello costs O(log) under `team`, and a member may send them back to
            // back; rate-limit per connection if one does.
            if p != d.peers.knobs().protocol {
                eprintln!("net: {short} speaks protocol {p}");
                d.peers
                    .with_link(key, conn, |l| l.protocol = Protocol::Outdated(p));
                return false;
            }
            push_delta(tx, &team.roster, &heads, &mut HashMap::new(), false);
            d.peers
                .with_link(key, conn, |l| l.protocol = Protocol::Ok(heads));
        } else if let Frame::Roster { entries, more } = f {
            sync.page.extend(entries);
            // A sync is applied once, at its last page (or when it alone would overflow).
            if !more || sync.page.len() >= MAX_PENDING {
                synced = !sync.page.is_empty();
                sync.pending.append(&mut sync.page);
            }
        }
        // Waiting entries are retried after each sync, and when the roster changed: another
        // connection may have brought their parents.
        let due = !sync.pending.is_empty() && (synced || team.roster.heads != sync.tried);
        drop(team);
        if due {
            match apply(d, &mut sync.pending, &short) {
                Ok(heads) => sync.tried = heads,
                Err(e) => eprintln!("net: applying roster from {short}: {e}"),
            }
        }
        true
    }
}

/// Accepts the waiting entries whose parents are known, in order, until none is left that
/// can be; one replay and install after. Under `team`, then `store`. Returns the roster's
/// heads after.
fn apply(d: &Daemon, pending: &mut Vec<Entry>, from: &str) -> Result<Vec<String>> {
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
    if !stored.is_empty() {
        let new = roster::replay(&roster::entries(&store)?);
        net_ops::install(d, &mut team, &store, new)?;
    }
    Ok(team.roster.heads.clone())
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

fn hello(r: &Roster, protocol: u32) -> Frame {
    Frame::Hello {
        protocol,
        heads: r.heads.clone(),
    }
}

/// Queues on `tx` the `roster` pages of what `heads` lacks (`memo` keeps them by heads); with
/// `quiet`, nothing when that is nothing.
fn push_delta(
    tx: &mpsc::UnboundedSender<Frame>,
    r: &Roster,
    heads: &[String],
    memo: &mut HashMap<Vec<String>, Vec<Frame>>,
    quiet: bool,
) {
    let frames = memo.entry(heads.to_vec()).or_insert_with(|| {
        let d = delta(r, heads);
        if quiet && d.is_empty() {
            vec![]
        } else {
            pages(d)
        }
    });
    for f in frames.iter() {
        let _ = tx.send(f.clone());
    }
}

/// Of the roster's log (parents first), the entries that are neither `heads` nor their
/// ancestors; all of them when no head is known.
fn delta(r: &Roster, heads: &[String]) -> Vec<Entry> {
    let known: Vec<&String> = heads.iter().filter(|h| r.knows(h)).collect();
    let mut covered = r.ancestors_of(&known);
    covered.extend(known.into_iter().cloned());
    r.log()
        .iter()
        .filter(|(h, _)| !covered.contains(h))
        .map(|(_, e)| e.clone())
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
