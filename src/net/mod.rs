//! The member transport: a QUIC endpoint (iroh) whose key is this computer's member key, one
//! connection per member pair carrying one stream of `Frame`s, the membership gate, and delta
//! roster sync. Relays are off; peers are found on the local network (mDNS, `config.lan`).
//!
//! Sync: each side's first frame is `hello {heads}`. A `hello` is answered with the entries
//! that are not ancestors of its heads (the whole log when none is known), parents first,
//! in `roster` pages; after every roster change each side pushes that delta against the
//! peer's last heads and a fresh `hello`. Applied entries go through `roster::accept` and
//! the daemon's one install (`net_ops::install`), like a local write.
//!
//! Mail: an `envelope` is checked and stored by `mail::receive` and answered with an `ack`;
//! later changes to it go back as `receipt`s. The sender's outbox (`outbox`) is flushed on
//! each `hello` and every second.

pub mod frames;

use crate::daemon::{ipc, net_ops, Daemon};
use crate::mail::{self, State};
use crate::roster::{self, Entry, Roster};
use crate::sessions::Key;
use crate::Result;
use crate::{holds, identity, outbox};
use frames::Frame;
use iroh::endpoint::{presets, Connection, VarInt};
use iroh::{Endpoint, EndpointAddr, PublicKey, SecretKey};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

pub const ALPN: &[u8] = b"lightbulb-connect/1";
pub const JOIN_ALPN: &[u8] = b"lightbulb-connect/join/1";
pub const PROTOCOL: u32 = 1;

/// A `roster` page closes before the next entry would take it past this (half the frame cap).
const PAGE_BYTES: usize = 512 * 1024;
const PAGE_ENTRIES: usize = 500;
const REDIAL: Duration = Duration::from_secs(30);
/// Entries one connection may hold waiting for their parents.
const MAX_PENDING: usize = 10_000;
const JOIN_WAIT: Duration = Duration::from_secs(600);

const NOT_MEMBER: u32 = 1;
const SUPERSEDED: u32 = 2;

/// The debug knobs, read once (`Peers::knobs`); release builds take the defaults.
pub struct Knobs {
    /// `PROTOCOL`, or `LIGHTBULB_CONNECT_DEBUG_PROTOCOL`.
    pub protocol: u32,
    /// Addresses to dial members at without discovery: `LIGHTBULB_CONNECT_PEERS=<key>@<ip:port>,…`.
    peers: HashMap<String, Vec<SocketAddr>>,
    /// `LIGHTBULB_CONNECT_DEBUG_GATE_PAUSE_MS`: the gate holds `team` that long before adopting,
    /// so a test can land a removal in the window.
    gate_pause: Option<Duration>,
    /// `LIGHTBULB_CONNECT_DIALOG=<button>`: every OS dialog answers that button, in its turn,
    /// without a window (empty: answers `None`).
    pub dialog: Option<Option<String>>,
    /// How long a join request waits for its person (600 s), or
    /// `LIGHTBULB_CONNECT_DEBUG_JOIN_WAIT_S`.
    pub join_wait: Duration,
    /// The program dialogs run: `/usr/bin/osascript`, or `LIGHTBULB_CONNECT_DEBUG_OSASCRIPT`.
    pub osascript: PathBuf,
    /// `LIGHTBULB_CONNECT_DEBUG_DROP_ACKS=1`: every `ack` that arrives is dropped.
    drop_acks: bool,
    /// How long the outbox keeps trying (`outbox::TTL_S`), or `LIGHTBULB_CONNECT_DEBUG_OUTBOX_S`.
    outbox_s: u64,
}

const OSASCRIPT: &str = "/usr/bin/osascript";

impl Knobs {
    #[cfg(debug_assertions)]
    fn read() -> Knobs {
        let var = |name| std::env::var(name).ok();
        let mut peers: HashMap<String, Vec<SocketAddr>> = HashMap::new();
        for item in var("LIGHTBULB_CONNECT_PEERS")
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
            protocol: var("LIGHTBULB_CONNECT_DEBUG_PROTOCOL")
                .and_then(|v| v.parse().ok())
                .unwrap_or(PROTOCOL),
            peers,
            gate_pause: var("LIGHTBULB_CONNECT_DEBUG_GATE_PAUSE_MS")
                .and_then(|v| v.parse().ok())
                .map(Duration::from_millis),
            dialog: var("LIGHTBULB_CONNECT_DIALOG").map(|b| Some(b).filter(|b| !b.is_empty())),
            join_wait: var("LIGHTBULB_CONNECT_DEBUG_JOIN_WAIT_S")
                .and_then(|v| v.parse().ok())
                .map_or(JOIN_WAIT, Duration::from_secs),
            osascript: var("LIGHTBULB_CONNECT_DEBUG_OSASCRIPT")
                .map_or(OSASCRIPT.into(), Into::into),
            drop_acks: var("LIGHTBULB_CONNECT_DEBUG_DROP_ACKS").is_some_and(|v| v == "1"),
            outbox_s: var("LIGHTBULB_CONNECT_DEBUG_OUTBOX_S")
                .and_then(|v| v.parse().ok())
                .unwrap_or(outbox::TTL_S),
        }
    }

    #[cfg(not(debug_assertions))]
    fn read() -> Knobs {
        Knobs {
            protocol: PROTOCOL,
            peers: HashMap::new(),
            gate_pause: None,
            dialog: None,
            join_wait: JOIN_WAIT,
            osascript: OSASCRIPT.into(),
            drop_acks: false,
            outbox_s: outbox::TTL_S,
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
    /// Woken (`notify_waiters`) whenever a member says `hello` in our protocol.
    pub connected: Notify,
    /// Poked when mail joins the outbox.
    pub outbox: Notify,
    /// Woken (`notify_waiters`) whenever a member's connection ends.
    pub disconnected: Notify,
    /// Woken (`notify_waiters`) whenever a member answers for mail sent it (`outbox::acked`).
    pub acked: Notify,
    /// The endpoint's UDP port.
    pub port: OnceLock<u16>,
    /// The endpoint, for the candidate side of a join.
    pub endpoint: OnceLock<Endpoint>,
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

    /// Where to dial member `key`: its key, and the knob's addresses for it.
    pub fn addr(&self, key: &str) -> Option<EndpointAddr> {
        let id = PublicKey::from_bytes(&identity::key_bytes(key)?).ok()?;
        let addrs = self.knobs().peers.get(key).into_iter().flatten();
        Some(addrs.fold(EndpointAddr::new(id), |a, s| a.with_ip_addr(*s)))
    }

    /// Members with a live connection that said `hello` in our protocol.
    pub fn online(&self) -> Vec<String> {
        let live = self.live.lock().unwrap();
        live.iter()
            .filter(|(_, l)| matches!(l.protocol, Protocol::Ok(_)))
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Whether member `key` is `online`.
    pub fn is_online(&self, key: &str) -> bool {
        let live = self.live.lock().unwrap();
        live.get(key)
            .is_some_and(|l| matches!(l.protocol, Protocol::Ok(_)))
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

    /// Queues `f` on `member`'s connection; fails when it has none.
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

    /// Whether `key`'s link is still `conn` and said `hello` in our protocol.
    fn speaks_ours(&self, key: &str, conn: &Connection) -> bool {
        current(&mut self.live.lock().unwrap(), key, conn)
            .is_some_and(|l| matches!(l.protocol, Protocol::Ok(_)))
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
            self.disconnected.notify_waiters();
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
    /// A presence was stored from this connection: later ones must carry a newer `seq`.
    presence_stored: bool,
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
        let _ = daemon.peers.endpoint.set(endpoint.clone());
        daemon.peers.knobs();
        let net = Net(Arc::new(Inner {
            endpoint,
            daemon,
            dials: Mutex::default(),
        }));
        tokio::spawn(net.clone().accept_loop());
        tokio::spawn(net.clone().keep_up());
        tokio::spawn(net.clone().flush_loop());
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
                if conn.alpn() == JOIN_ALPN {
                    crate::admission::serve(net.0.daemon.clone(), conn).await;
                } else {
                    let key = identity::key_of(conn.remote_id().as_bytes());
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

    /// Sends what members are owed: on each `hello` every queued envelope of an online member,
    /// and every second the envelopes due (`outbox::due`'s per-row backoff) and the receipts
    /// waiting; every 60 s (or the outbox knob, if shorter) expires what waited too long, here
    /// and in the outbox.
    // ponytail: a 1 s poll (two small queries) picks up receipts that mail state changes queue;
    // wake it from `mail` if that ever shows.
    async fn flush_loop(self) {
        let d = &self.0.daemon;
        let every = Duration::from_secs(d.peers.knobs().outbox_s.clamp(1, 60));
        let mut expired_at: Option<std::time::Instant> = None;
        loop {
            // Armed before the flush, so a `hello` during it is not missed.
            let connected = d.peers.connected.notified();
            tokio::pin!(connected);
            connected.as_mut().enable();
            let all = tokio::select! {
                _ = &mut connected => true,
                _ = d.peers.outbox.notified() => false,
                _ = tokio::time::sleep(Duration::from_secs(1)) => false,
                _ = self.0.endpoint.closed() => return,
            };
            let expire = expired_at.is_none_or(|t| t.elapsed() >= every);
            if expire {
                expired_at = Some(std::time::Instant::now());
            }
            if let Err(e) = flush(d, all, expire) {
                eprintln!("net: outbox: {e}");
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
        let r = match self.0.daemon.peers.addr(&key) {
            Some(addr) => self.0.endpoint.connect(addr, ALPN).await.ok(),
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
        crate::presence::seen(d, &key);
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
        let mut hello_ok = false;
        let (mut presence, mut envelope, mut answer) = (None, None, None);
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
            d.peers.connected.notify_waiters();
            hello_ok = true;
        } else if let Frame::Roster { entries, more } = f {
            sync.page.extend(entries);
            // A sync is applied once, at its last page (or when it alone would overflow).
            if !more || sync.page.len() >= MAX_PENDING {
                synced = !sync.page.is_empty();
                sync.pending.append(&mut sync.page);
            }
        } else if matches!(f, Frame::Presence { .. }) {
            presence = Some(f);
        } else if matches!(f, Frame::Envelope { .. }) {
            envelope = Some(f);
        } else if matches!(f, Frame::Ack { .. }) && d.peers.knobs().drop_acks {
            eprintln!("net: dropped an ack from {short} (debug)");
        } else if let Frame::Ack {
            id,
            state,
            reason,
            limit,
        }
        | Frame::Receipt {
            id,
            state,
            reason,
            limit,
        } = f
        {
            answer = Some((id, state, reason, limit));
        }
        // Waiting entries are retried after each sync, and when the roster changed: another
        // connection may have brought their parents.
        let due = !sync.pending.is_empty() && (synced || team.roster.heads != sync.tried);
        drop(team);
        // Outside `team`: ours may need a build (transcripts, git files: off the async
        // threads); storing theirs takes `store`.
        if hello_ok {
            let (d, tx) = (d.clone(), tx.clone());
            tokio::task::spawn_blocking(move || {
                let _ = tx.send(crate::presence::for_hello(&d));
            });
        }
        if let Some(p) = presence.filter(|_| d.peers.speaks_ours(key, conn)) {
            let first = !std::mem::replace(&mut sync.presence_stored, true);
            crate::presence::put(d, key, &p, ipc::now(), first);
        }
        if let Some(env) = envelope.filter(|_| d.peers.speaks_ours(key, conn)) {
            receive(d, key, &env, tx);
        }
        if let Some((id, state, reason, limit)) = answer {
            let store = d.store.lock().unwrap();
            let state = State::parse(&state).expect("checked by from_peer_ok");
            let (reason, limit) = (reason.as_deref(), limit.as_deref());
            match outbox::acked(&store, key, &id, state, reason, limit, ipc::now()) {
                Ok(true) => {
                    eprintln!("net: {id} at {short}: {}", state.name());
                    d.peers.acked.notify_waiters();
                }
                Ok(false) => {
                    eprintln!("net: dropped an answer from {short} for {id} (not sent there)")
                }
                Err(e) => eprintln!("net: {id} at {short}: {e}"),
            }
        }
        if due {
            match apply(d, &mut sync.pending, &short) {
                Ok(heads) => sync.tried = heads,
                Err(e) => eprintln!("net: applying roster from {short}: {e}"),
            }
        }
        true
    }
}

/// Member `key`'s envelope: checked and stored (`mail::receive`), answered with an `ack`.
/// One whose `from` is not the member's own is dropped unanswered.
fn receive(d: &Arc<Daemon>, key: &str, env: &Frame, tx: &mpsc::UnboundedSender<Frame>) {
    let Frame::Envelope { id, to, .. } = env else {
        return;
    };
    let team = d.team.lock().unwrap();
    let Some(peer) = team.roster.members.get(key) else {
        return;
    };
    let short = &peer.short;
    let store = d.store.lock().unwrap();
    let me = &d.identity.short;
    match mail::receive(&store, env, peer, &team.roster, me, ipc::now()) {
        Ok(mail::Answer {
            state,
            reason,
            limit,
            held,
        }) => {
            if let Some(h) = held {
                holds::shown(d, &team.roster, &store, &h);
            }
            eprintln!("net: envelope {id} from {short}: {}", state.name());
            let _ = tx.send(Frame::Ack {
                id: id.clone(),
                state: state.name().into(),
                reason,
                limit,
            });
            if state == State::Stored {
                drop((store, team));
                ipc::stored(d, &Key(to.clone()));
            }
        }
        Err(e) => eprintln!("net: dropped envelope {id} from {short}: {e}"),
    }
}

/// One flush (`Net::flush_loop`): with `all`, every queued envelope of an online member,
/// whatever its backoff; with `expire`, first what waited too long.
fn flush(d: &Daemon, all: bool, expire: bool) -> Result<()> {
    let now = ipc::now();
    let online = d.peers.online();
    let store = d.store.lock().unwrap();
    if expire {
        mail::expire(&store, now)?;
        outbox::expire(&store, now, d.peers.knobs().outbox_s)?;
        holds::expire_here(d, &store, now as i64)?;
    }
    let due = outbox::due(&store, &online, if all { u64::MAX } else { now })?;
    for (member, f) in due {
        let Frame::Envelope { id, .. } = &f else {
            continue;
        };
        let id = id.clone();
        if d.peers.send(&member, f).is_ok() {
            outbox::sent(&store, &id, now)?;
        }
    }
    for (member, f) in outbox::receipts(&store)? {
        let Frame::Receipt { id, state, .. } = &f else {
            continue;
        };
        let (id, state) = (id.clone(), state.clone());
        if online.contains(&member) && d.peers.send(&member, f).is_ok() {
            outbox::told(&store, &member, &id, &state)?;
        }
    }
    Ok(())
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
pub(crate) fn delta(r: &Roster, heads: &[String]) -> Vec<Entry> {
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
pub(crate) fn pages(entries: Vec<Entry>) -> Vec<Frame> {
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

pub(crate) fn short(key: &str) -> &str {
    key.get(..12).unwrap_or(key)
}
