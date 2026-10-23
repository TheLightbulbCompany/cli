//! The member transport: a QUIC endpoint (iroh) whose key is this computer's member key, one
//! connection per member pair carrying one stream of `Frame`s, the membership gate, and delta
//! roster sync. Peers are found on the local network (mDNS, `config.lan`), this user's other
//! daemons on this computer over loopback (`local`, also `config.lan`), and through the
//! relays in effect (`relay`), which also carry a connection when no direct path exists
//! (ciphertext only: QUIC is end to end between the two member keys).
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

use crate::daemon::{ipc, net_ops, Daemon, TeamState};
use crate::mail::{self, State};
use crate::roster::{self, Entry, Roster};
use crate::sessions::Key;
use crate::Result;
use crate::{holds, identity, outbox, relay};
use frames::Frame;
use iroh::endpoint::{presets, Connection, VarInt};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, RelayUrl, SecretKey, Watcher};
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
/// Every member is redialed this often (through each relay too), within the 60 s cap.
const REDIAL: Duration = Duration::from_secs(30);
/// How often the relay loop asks whether a fetch of ours is due (`relay::State::fetch_if_due`).
const RELAY_FETCH: Duration = Duration::from_secs(3_600);
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
    /// The keys `relays.json` may be signed by: `relay::KEYS`, or
    /// `LIGHTBULB_CONNECT_DEBUG_RELAY_KEYS=<minisign public key>,…`.
    pub relay_keys: Vec<String>,
    /// `LIGHTBULB_CONNECT_DEBUG_RELAY_ONLY=1`: no direct (IP) transport, only the relays.
    relay_only: bool,
}

fn compiled_keys() -> Vec<String> {
    relay::KEYS.iter().map(|k| k.to_string()).collect()
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
            relay_keys: var("LIGHTBULB_CONNECT_DEBUG_RELAY_KEYS")
                .map_or_else(compiled_keys, |v| v.split(',').map(String::from).collect()),
            relay_only: var("LIGHTBULB_CONNECT_DEBUG_RELAY_ONLY").is_some_and(|v| v == "1"),
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
            relay_keys: compiled_keys(),
            relay_only: false,
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
    /// Frames dropped by `frames::from_peer_ok`, by kind.
    pub dropped: Mutex<std::collections::BTreeMap<&'static str, u64>>,
    /// The relays in effect (`relay::list`; the relay loop brings the endpoint after).
    pub relays: Mutex<relay::List>,
    /// What is known of our list: loaded once at start, changed only by a fetch.
    pub relay_state: Mutex<relay::State>,
    /// Poked by `relays_changed`: the relay loop brings the endpoint to the list.
    pub relay_changed: Notify,
    /// `config.lan` at start.
    pub lan: OnceLock<bool>,
    /// The last mDNS probe's answer (`lan_probe`); `""` before the first.
    lan_probe: Mutex<&'static str>,
    /// Each member's direct addresses as it last told them (its `hello`, or the ticket for
    /// the inviter), kept in `ADDRS` across restarts.
    known: Mutex<HashMap<String, Vec<SocketAddr>>>,
}

/// Where `Peers::known` is kept, in the state dir.
const ADDRS: &str = "addrs.json";
/// The most direct addresses taken from a peer or a ticket.
pub const MAX_ADDRS: usize = 8;

/// `v` as direct addresses: IP literals with a port (never a host name), at most
/// `MAX_ADDRS`, none port 0, unspecified, multicast, loopback (ours come only from the
/// private local file) or the IPv4 broadcast, judged on the IPv4 inside an IPv4-mapped
/// IPv6 too; link-local stays (a real LAN path). `None` when any is not.
pub fn parse_addrs(v: &[String]) -> Option<Vec<SocketAddr>> {
    if v.len() > MAX_ADDRS {
        return None;
    }
    let ok = |a: &SocketAddr| {
        let ip = a.ip().to_canonical();
        let broadcast = ip == std::net::IpAddr::V4(std::net::Ipv4Addr::BROADCAST);
        a.port() != 0
            && !ip.is_unspecified()
            && !ip.is_multicast()
            && !ip.is_loopback()
            && !broadcast
    };
    v.iter()
        .map(|s| s.parse::<SocketAddr>().ok().filter(ok))
        .collect()
}

/// A connection's outgoing frames, each with whoever waits to hear it was written.
pub(crate) type Tx = mpsc::UnboundedSender<(Frame, Option<tokio::sync::oneshot::Sender<()>>)>;

struct Link {
    conn: Connection,
    // ponytail: unbounded; only members get a queue, bound it if one shows up growing.
    tx: Tx,
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

    /// Where to dial member `key`: its key, the knob's addresses for it, its loopback address
    /// when it runs on this computer (`local`), and every relay in
    /// effect (iroh tries them all, and upgrades to a direct path where it can).
    pub fn addr(&self, key: &str) -> Option<EndpointAddr> {
        let id = PublicKey::from_bytes(&identity::key_bytes(key)?).ok()?;
        let here = self.lan.get() == Some(&true);
        let here = here.then(|| local::addr(key)).flatten();
        let known = self.known.lock().unwrap().get(key).cloned();
        let addrs = self.knobs().peers.get(key).into_iter().flatten().copied();
        let a = addrs
            .chain(here)
            .chain(known.into_iter().flatten())
            .fold(EndpointAddr::new(id), |a, s| a.with_ip_addr(s));
        let relays = self.relays.lock().unwrap().urls.clone();
        Some(relays.into_iter().fold(a, |a, u| a.with_relay_url(u)))
    }

    /// The one entry for a relay change (start, `relay.set`, a roster install whose team relay
    /// changed, a fetch): makes the list for `t` and our state the one in effect at once
    /// (`relay.get`, tickets and dials read it), and pokes the relay loop for the endpoint.
    /// In memory only: no disk, no crypto.
    pub fn relays_changed(&self, t: &TeamState) {
        let new = relay::list(
            &t.config.relay,
            &t.roster.relay,
            &self.relay_state.lock().unwrap(),
        );
        let mut l = self.relays.lock().unwrap();
        if (&new.urls, new.source) != (&l.urls, l.source) {
            log_relays(&new);
        }
        *l = new;
        drop(l);
        self.relay_changed.notify_one();
    }

    /// `"off"` (`config.lan = false`), else `lan_probe`'s answer: `"on"`, `"blocked"` (macOS
    /// refuses our mDNS: no Local Network permission) or `"no network"`. Until it is `"on"`
    /// each call probes again, so an Allow (or Wi-Fi coming back) shows at once.
    pub fn lan_status(&self) -> &'static str {
        if self.lan.get() != Some(&true) {
            return "off";
        }
        let mut last = self.lan_probe.lock().unwrap();
        if *last != "on" {
            *last = lan_probe();
        }
        *last
    }

    /// The first relay in effect, for a ticket's `relay_hint` (`""` when none).
    pub fn relay_hint(&self) -> String {
        let l = self.relays.lock().unwrap();
        l.urls.first().map(|u| u.to_string()).unwrap_or_default()
    }

    /// This computer's direct addresses now, as a ticket or `hello` carries them.
    pub fn own_addrs(&self) -> Vec<String> {
        let Some(ep) = self.endpoint.get() else {
            return vec![];
        };
        let a = ep.addr();
        a.ip_addrs()
            .take(MAX_ADDRS)
            .map(|a| a.to_string())
            .collect()
    }

    /// What a ticket carries to be dialed: `relay_hint` and `own_addrs`.
    pub fn hints(&self) -> (String, Vec<String>) {
        (self.relay_hint(), self.own_addrs())
    }

    /// Member `key` told its direct addresses `addrs` (already checked, `parse_addrs`):
    /// kept for its dials, and in `dir`'s `ADDRS` when they changed. Only the keys `keep`
    /// names stay (the roster's members, and a join's inviter), so the file has no stale ones.
    pub fn learn(
        &self,
        dir: &std::path::Path,
        key: &str,
        addrs: Vec<SocketAddr>,
        keep: impl Fn(&str) -> bool,
    ) {
        let mut known = self.known.lock().unwrap();
        if addrs.is_empty() || known.get(key) == Some(&addrs) {
            return;
        }
        known.retain(|k, _| keep(k));
        known.insert(key.to_string(), addrs);
        if let Ok(b) = serde_json::to_vec(&*known) {
            let _ = std::fs::write(dir.join(ADDRS), b);
        }
    }

    /// Loads `ADDRS` from `dir`, keeping the entries of `members` that still check.
    fn load_known(&self, dir: &std::path::Path, members: &Roster) {
        let read: HashMap<String, Vec<String>> = std::fs::read(dir.join(ADDRS))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let ok = read
            .into_iter()
            .filter(|(k, _)| members.members.contains_key(k))
            .filter_map(|(k, v)| Some((k, parse_addrs(&v)?)));
        *self.known.lock().unwrap() = ok.collect();
    }

    /// `status.relay` and `status.relay_error`: `none` without relays, `ok` while a home relay
    /// is connected, else `unreachable`; and why our list was refused, while it is.
    pub fn relay_status(&self) -> (&'static str, Option<String>) {
        let l = self.relays.lock().unwrap();
        if l.urls.is_empty() {
            return ("none", l.error.clone());
        }
        let up = self
            .endpoint
            .get()
            .is_some_and(|e| e.home_relay_status().get().iter().any(|r| r.is_connected()));
        (if up { "ok" } else { "unreachable" }, l.error.clone())
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

    /// For `status`, in one pass: how many members are online, how many of those are reached
    /// directly (their selected path is not a relay), and `(key, their protocol)` for each
    /// that needs an update, by key.
    pub fn status(&self) -> (usize, usize, Vec<(String, u32)>) {
        let live = self.live.lock().unwrap();
        let (mut online, mut direct) = (0, 0);
        let mut outdated = Vec::new();
        for (k, l) in live.iter() {
            match l.protocol {
                Protocol::Ok(_) => {
                    online += 1;
                    if l.conn.paths().iter().any(|p| p.is_selected() && p.is_ip()) {
                        direct += 1;
                    }
                }
                Protocol::Outdated(p) => outdated.push((k.clone(), p)),
                Protocol::Unknown => {}
            }
        }
        outdated.sort();
        (online, direct, outdated)
    }

    /// `f` on `member`'s link, once it said `hello` in our protocol.
    fn ok_link<R>(&self, member: &str, f: impl FnOnce(&Link) -> R) -> Option<R> {
        let live = self.live.lock().unwrap();
        live.get(member)
            .filter(|l| matches!(l.protocol, Protocol::Ok(_)))
            .map(f)
    }

    /// `member`'s connection (`streams` opens on it).
    pub fn conn(&self, member: &str) -> Option<Connection> {
        self.ok_link(member, |l| l.conn.clone())
    }

    /// Queues `f` on `member`'s connection; fails when it has none.
    pub fn send(&self, member: &str, f: Frame) -> Result<()> {
        match self.ok_link(member, |l| l.tx.send((f, None)).is_ok()) {
            Some(true) => Ok(()),
            Some(false) => Err("connection closed".into()),
            None => Err("not connected".into()),
        }
    }

    /// `send` `f`, and hear once it was written to the stream (`app_send` marks mail read
    /// only then); `None` when it could not be queued.
    pub(crate) fn send_written(
        &self,
        member: &str,
        f: Frame,
    ) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let queued = self.ok_link(member, |l| l.tx.send((f, Some(tx))).is_ok());
        queued.unwrap_or(false).then_some(rx)
    }

    /// Counts a dropped frame of `kind` (`status`'s `dropped`).
    pub(crate) fn dropped(&self, kind: &'static str) {
        *self.dropped.lock().unwrap().entry(kind).or_default() += 1;
    }

    /// Takes `conn` (dialed by `dialer`) as `key`'s connection, unless the one held wins: of
    /// two connections, the one the lower member key dialed; of two dialed by the same side,
    /// the newer. The loser is closed.
    fn adopt(&self, key: &str, me: &str, conn: &Connection, dialer: &str, tx: &Tx) -> bool {
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
    /// Binds the endpoint (secret = the member key, the relays known without the network,
    /// mDNS when `config.lan`) and starts accepting, dialing every active member, and keeping
    /// the relay list.
    pub async fn start(daemon: Arc<Daemon>) -> Result<Net> {
        daemon
            .peers
            .load_known(&daemon.dir, &daemon.team.lock().unwrap().roster);
        let state = relay::State::load(&daemon.dir, &daemon.peers.knobs().relay_keys);
        *daemon.peers.relay_state.lock().unwrap() = state;
        let lan = {
            let t = daemon.team.lock().unwrap();
            for v in [&t.config.relay, &t.roster.relay].into_iter().flatten() {
                if !v.trim().is_empty() && relay::parse(v).is_none() {
                    eprintln!("relay: ignored {v:?} (not an https relay URL)");
                }
            }
            daemon.peers.relays_changed(&t);
            t.config.lan
        };
        let secret = SecretKey::from_bytes(&daemon.identity.signing.to_bytes());
        // Always a relay transport (an empty map is direct only), so a list that comes later
        // can be added at runtime.
        let urls = daemon.peers.relays.lock().unwrap().urls.clone();
        let map = urls.into_iter().map(relay_config);
        let mut b = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .alpns(vec![ALPN.to_vec(), JOIN_ALPN.to_vec()])
            .relay_mode(RelayMode::Custom(map.collect()));
        if daemon.peers.knobs().relay_only {
            b = b.clear_ip_transports();
        }
        if lan {
            b = b.address_lookup(iroh_mdns_address_lookup::MdnsAddressLookup::builder());
        }
        let endpoint = b.bind().await?;
        let port = endpoint.bound_sockets().first().map_or(0, |a| a.port());
        let _ = daemon.peers.port.set(port);
        let _ = daemon.peers.lan.set(lan);
        if lan {
            local::publish(&daemon.identity.key, port);
            if daemon.peers.lan_status() == "blocked" {
                eprintln!(
                    "net: macOS refuses this program the local network, so computers on it cannot find this one. \
                     Allow lightbulb-connect in System Settings > Privacy & Security > Local Network."
                );
            }
        }
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
        tokio::spawn(net.clone().relay_loop());
        Ok(net)
    }

    /// Brings the endpoint's relays to the list in effect (`peers.relays`) whenever
    /// `relays_changed` pokes, and tries the fetch of ours hourly (`relay::State::
    /// fetch_if_due` decides) on a blocking thread, swapping the state in after. iroh keeps
    /// the connection to its home relay up itself (and the dial loop redials members through
    /// every relay), so this only changes which relays there are.
    async fn relay_loop(self) {
        let d = &self.0.daemon;
        // What the endpoint has.
        let mut applied = d.peers.relays.lock().unwrap().urls.clone();
        let mut first = true;
        loop {
            // A retry is due an hour after the last attempt, whatever started that attempt (a
            // poke too); with none pending, the hourly look at the cache's age.
            let now = std::time::Instant::now();
            let last = d.peers.relay_state.lock().unwrap().last_fetch_at;
            let due = last.map(|t| t + relay::RETRY).filter(|t| *t > now);
            let wake = match (first, due) {
                (true, _) => now,
                (_, Some(t)) => t,
                _ => now + RELAY_FETCH,
            };
            first = false;
            tokio::select! {
                _ = tokio::time::sleep_until(wake.into()) => {}
                _ = d.peers.relay_changed.notified() => {}
                _ = self.0.endpoint.closed() => return,
            }
            let (mut state, issuer, set) = {
                let t = d.team.lock().unwrap();
                let set = (t.config.relay.clone(), t.roster.relay.clone());
                (
                    d.peers.relay_state.lock().unwrap().clone(),
                    t.config.issuer.clone(),
                    set,
                )
            };
            let d2 = d.clone();
            let fetched = tokio::task::spawn_blocking(move || {
                let keys = &d2.peers.knobs().relay_keys;
                let f = state.fetch_if_due(&d2.dir, &issuer, (&set.0, &set.1), keys);
                (f, state)
            })
            .await;
            if let Ok((true, state)) = fetched {
                *d.peers.relay_state.lock().unwrap() = state;
                d.peers.relays_changed(&d.team.lock().unwrap());
            }
            let urls = d.peers.relays.lock().unwrap().urls.clone();
            if urls == applied {
                continue;
            }
            let ep = &self.0.endpoint;
            for u in applied.iter().filter(|u| !urls.contains(u)) {
                ep.remove_relay(u).await;
            }
            for u in urls.iter().filter(|u| !applied.contains(u)) {
                ep.insert_relay(u.clone(), Arc::new(relay_config(u.clone())))
                    .await;
            }
            applied = urls;
        }
    }

    pub async fn close(&self) {
        if self.0.daemon.peers.lan.get() == Some(&true) {
            local::withdraw(&self.0.daemon.identity.key);
        }
        self.0.endpoint.close().await;
    }

    /// Whether every task this endpoint started has ended (this handle is the last one).
    pub fn idle(&self) -> bool {
        Arc::strong_count(&self.0) == 1
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
    /// and in the outbox, and lapses and renews sign-in proofs (`proof::tick`).
    // ponytail: a 1 s poll (two small queries) picks up receipts that mail state changes queue;
    // wake it from `mail` if that ever shows.
    async fn flush_loop(self) {
        let d = &self.0.daemon;
        let every = Duration::from_secs(d.peers.knobs().outbox_s.clamp(1, 60));
        let mut expired_at: Option<std::time::Instant> = None;
        let mut renew_at = 0;
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
                crate::proof::tick(d, &mut renew_at);
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
                eprintln!("net: closed {} (no longer a member)", short(key));
                // A removed member on a live link hears of it first (`removal_pages`).
                let told = match &l.protocol {
                    Protocol::Ok(heads) => removal_pages(r, key, heads),
                    _ => vec![],
                };
                if told.is_empty() {
                    l.conn.close(VarInt::from_u32(NOT_MEMBER), b"not a member");
                } else {
                    tokio::spawn(tell_then_close(l.conn.clone(), l.tx.clone(), told));
                }
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
                let _ = l.tx.send((hello(r, protocol, d.peers.own_addrs()), None));
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
                    let protocol = d.peers.knobs().protocol;
                    let addrs = d.peers.own_addrs();
                    let _ = tx.send((hello(&team.roster, protocol, addrs), None));
                    None
                } else {
                    Some((SUPERSEDED, b"superseded"))
                }
            }
        };
        if let Some((code, reason)) = refused {
            if code == NOT_MEMBER {
                eprintln!("net: refused {} (not a member)", short(&key));
                // A removed member that dials in hears of it first (`tell_removed`).
                let r = d.team.lock().unwrap().roster.clone();
                if removal(&r, &key).is_some() && !outgoing {
                    return tell_removed(conn, r, key).await;
                }
            }
            conn.close(VarInt::from_u32(code), reason);
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
                while let Some((f, done)) = rx.recv().await {
                    if frames::write(&mut send, &f).await.is_err() {
                        return;
                    }
                    if let Some(done) = done {
                        let _ = done.send(());
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
            // Every stream after the control one is a viewer's (`streams`); at most
            // `streams::ACCEPTS` wait for their opener at once, the rest are reset.
            let accepts = Arc::new(tokio::sync::Semaphore::new(crate::streams::ACCEPTS));
            let viewers = async {
                while let Ok((mut s, r)) = conn.accept_bi().await {
                    let Ok(permit) = accepts.clone().try_acquire_owned() else {
                        let _ = s.reset(VarInt::from_u32(crate::streams::BUSY));
                        continue;
                    };
                    let (d, key) = (d.clone(), key.clone());
                    tokio::spawn(async move {
                        crate::streams::accept(d, key, s, r).await;
                        drop(permit);
                    });
                }
            };
            tokio::select! {
                _ = writer => {}
                _ = reader => {}
                _ = viewers => {}
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
        tx: &Tx,
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
        let (mut grant, mut join, mut relayed) = (None, None, None);
        if let Err(why) = frames::from_peer_ok(&f, peer, d) {
            d.peers.dropped(f.kind());
            eprintln!("net: dropped a {} frame from {short} ({why})", f.kind());
        } else if let Frame::Hello {
            protocol: p,
            heads,
            addrs,
        } = f
        {
            // A hint: bad ones are dropped, the hello is served.
            let addrs = parse_addrs(&addrs).unwrap_or_default();
            let r = &team.roster;
            d.peers
                .learn(&d.dir, key, addrs, |k| r.members.contains_key(k));
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
        } else if matches!(f, Frame::ShareGrant { .. }) {
            grant = Some(f);
        } else if matches!(f, Frame::JoinRequest { .. } | Frame::JoinDecision { .. }) {
            join = Some(f);
        } else if matches!(f, Frame::HoldNotice { .. } | Frame::Decision { .. }) {
            relayed = Some(f);
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
                let _ = tx.send((crate::presence::for_hello(&d), None));
            });
        }
        if let Some(p) = presence.filter(|_| d.peers.speaks_ours(key, conn)) {
            let first = !std::mem::replace(&mut sync.presence_stored, true);
            crate::presence::put(d, key, &p, ipc::now(), first);
        }
        if let Some(f) = join {
            tokio::spawn(crate::admission::from_member(d.clone(), key.to_string(), f));
        }
        if let Some(env) = envelope.filter(|_| d.peers.speaks_ours(key, conn)) {
            receive(d, key, &env, tx);
        }
        if let Some(f) = relayed.filter(|_| d.peers.speaks_ours(key, conn)) {
            crate::decision::on_frame(d, key, &f, tx);
        }
        if let Some(g) = grant.filter(|_| d.peers.speaks_ours(key, conn)) {
            if let Err(e) = crate::streams::store(&d.store.lock().unwrap(), &g, ipc::now() as i64) {
                eprintln!("net: a share_grant from {short}: {e}");
            }
        }
        if let Some((id, state, reason, _)) = answer
            .as_ref()
            .filter(|a| crate::decision::is_control(&a.0))
        {
            crate::decision::acked(d, key, id, state, reason.as_deref());
        } else if let Some((id, state, reason, limit)) = answer {
            let store = d.store.lock().unwrap();
            let state = State::parse(&state).expect("checked by from_peer_ok");
            let (reason, limit) = (reason.as_deref(), limit.as_deref());
            match outbox::acked(&store, key, &id, state, reason, limit, ipc::now()) {
                Ok(Some(moved)) => {
                    eprintln!("net: {id} at {short}: {}", state.name());
                    d.peers.acked.notify_waiters();
                    if moved {
                        receipt_changed(d, &id, state.name(), reason);
                    }
                }
                Ok(None) => {
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
/// The `app_*` kinds go to `app_send` instead: no mailbox, hold, hop count or `ack`.
fn receive(d: &Arc<Daemon>, key: &str, env: &Frame, tx: &Tx) {
    let Frame::Envelope { id, to, kind, .. } = env else {
        return;
    };
    if crate::app_send::is_app(kind) {
        return crate::app_send::on_envelope(d, key, env.clone());
    }
    let team = d.team.lock().unwrap();
    let Some(peer) = team.roster.members.get(key) else {
        return;
    };
    let short = &peer.short;
    let ack = |state: State, reason: Option<String>, limit: Option<String>| {
        let ack = Frame::Ack {
            id: id.clone(),
            state: state.name().into(),
            reason,
            limit,
        };
        let _ = tx.send((ack, None));
    };
    // An instance's sessions are the ones it declared: new mail for any other is refused.
    let declared = d.cloud.as_ref().map(|c| c.declared().ids);
    let store = d.store.lock().unwrap();
    let me = &d.identity.short;
    let now = ipc::now();
    match mail::receive(
        &store,
        env,
        peer,
        &team.roster,
        me,
        declared.as_deref(),
        now,
    ) {
        Ok(mail::Answer {
            state,
            reason,
            limit,
            held,
            fresh,
        }) => {
            if let Some(h) = held {
                holds::shown(d, &team.roster, &store, &h);
            }
            eprintln!("net: envelope {id} from {short}: {}", state.name());
            ack(state, reason, limit);
            if state == State::Stored {
                drop((store, team));
                ipc::stored(d, &Key(to.clone()), fresh.then_some(id.as_str()));
            }
        }
        Err(e) => eprintln!("net: dropped envelope {id} from {short}: {e}"),
    }
}

/// Every subscriber hears that envelope `id`'s receipt moved to `state`.
fn receipt_changed(d: &Daemon, id: &str, state: &str, reason: Option<&str>) {
    let e =
        serde_json::json!({"event": "receipt_changed", "id": id, "state": state, "reason": reason});
    let _ = d.events.send(e);
}

/// One flush (`Net::flush_loop`): with `all`, every queued envelope of an online member,
/// whatever its backoff; with `expire`, first what waited too long.
fn flush(d: &Daemon, all: bool, expire: bool) -> Result<()> {
    let now = ipc::now();
    let online = d.peers.online();
    let store = d.store.lock().unwrap();
    if expire {
        mail::expire(&store, now)?;
        for id in outbox::expire(&store, now, d.peers.knobs().outbox_s)? {
            receipt_changed(d, &id, State::Expired.name(), Some("unreachable"));
        }
        holds::expire_here(d, &store, now as i64)?;
    }
    let since = now.saturating_sub(mail::HELD_S);
    let due = outbox::due(&store, &online, if all { u64::MAX } else { now }, since)?;
    for (id, member, f) in due {
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

fn hello(r: &Roster, protocol: u32, addrs: Vec<String>) -> Frame {
    Frame::Hello {
        protocol,
        heads: r.heads.clone(),
        addrs,
    }
}

/// Answers a removed member's `hello` with the roster pages it lacks, the removal among them,
/// then refuses it: its install of them tells it it was removed (`net_ops::install`), and it
/// closes. Its `hello` is awaited 1 s, its close 3 s; the refusal follows whatever happened.
async fn tell_removed(conn: Connection, r: Roster, key: String) {
    let hello = async {
        let (send, mut recv) = conn.accept_bi().await.ok()?;
        match frames::read(&mut recv).await.ok()? {
            Frame::Hello { heads, .. } => Some((send, heads)),
            _ => None,
        }
    };
    let told = async {
        let (mut send, heads) = tokio::time::timeout(Duration::from_secs(1), hello)
            .await
            .ok()??;
        for f in removal_pages(&r, &key, &heads) {
            frames::write(&mut send, &f).await.ok()?;
        }
        tokio::time::timeout(Duration::from_secs(3), conn.closed())
            .await
            .ok()
    };
    told.await;
    conn.close(VarInt::from_u32(NOT_MEMBER), b"not a member");
}

/// The hash of the last `remove` of `key` in `r`.
fn removal(r: &Roster, key: &str) -> Option<String> {
    let mut removes = r
        .log()
        .iter()
        .filter(|(_, e)| e.kind == roster::Kind::Remove && e.body["member"] == key);
    removes.next_back().map(|(h, _)| h.clone())
}

/// What a removed `key` at `heads` is told: the `remove` of it and those of its ancestors it
/// lacks, never anything after (later links and their secrets, later members).
fn removal_pages(r: &Roster, key: &str, heads: &[String]) -> Vec<Frame> {
    let Some(h) = removal(r, key) else {
        return vec![];
    };
    let mut upto = r.ancestors_of(&[&h]);
    upto.insert(h);
    let d = delta(r, heads);
    pages(d.into_iter().filter(|e| upto.contains(&e.hash())).collect())
}

/// Writes `told` on a live link (its writer, through `tx`), then waits up to 3 s for the
/// peer to close before closing it as not a member.
async fn tell_then_close(conn: Connection, tx: Tx, told: Vec<Frame>) {
    let mut last = None;
    for f in told {
        let (done, written) = tokio::sync::oneshot::channel();
        let _ = tx.send((f, Some(done)));
        last = Some(written);
    }
    if let Some(w) = last {
        let _ = tokio::time::timeout(Duration::from_secs(1), w).await;
        let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
    }
    drop(tx);
    conn.close(VarInt::from_u32(NOT_MEMBER), b"not a member");
}

/// Queues on `tx` the `roster` pages of what `heads` lacks (`memo` keeps them by heads); with
/// `quiet`, nothing when that is nothing.
fn push_delta(
    tx: &Tx,
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
        let _ = tx.send((f.clone(), None));
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

/// Relay `url`, without QUIC address discovery (off in Phase 1: the relay serves HTTPS only).
fn relay_config(url: RelayUrl) -> iroh::RelayConfig {
    iroh::RelayConfig::new(url, None)
}

/// The relays in effect, in the log: their URLs and where they came from.
fn log_relays(l: &relay::List) {
    let source = serde_json::to_value(l.source).unwrap_or_default();
    let urls: Vec<String> = l.urls.iter().map(ToString::to_string).collect();
    match &l.error {
        Some(e) => eprintln!("relay: {source} {urls:?} ({e})"),
        None => eprintln!("relay: {source} {urls:?}"),
    }
}

pub(crate) fn short(key: &str) -> &str {
    key.get(..12).unwrap_or(key)
}

/// Whether an mDNS send goes out: `"on"`; `"blocked"` when it is refused with
/// `EHOSTUNREACH` while an interface that can multicast is up, which is what macOS answers
/// every multicast of a program without the Local Network permission; `"no network"` when no
/// such interface is up (Wi-Fi off gives the same error). iroh's mDNS lookup only logs a
/// refusal (at `tracing` level, which this daemon does not print), so without this the daemon
/// is deaf to the LAN and says nothing. The probe is a 12-byte mDNS header with no questions:
/// nobody answers it.
fn lan_probe() -> &'static str {
    let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return "on";
    };
    let r = s.send_to(&[0; 12], "224.0.0.251:5353");
    if !r.is_err_and(|e| e.raw_os_error() == Some(libc::EHOSTUNREACH)) {
        "on"
    } else if multicast_up() {
        "blocked"
    } else {
        "no network"
    }
}

/// Whether an interface other than loopback is up, running, multicast-capable and has an
/// IPv4 address.
fn multicast_up() -> bool {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `head` with a list we only read, then `freeifaddrs` it.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return false;
    }
    let want = (libc::IFF_UP | libc::IFF_RUNNING | libc::IFF_MULTICAST) as u32;
    let mut found = false;
    let mut p = head;
    while let Some(i) = unsafe { p.as_ref() } {
        let v4 =
            unsafe { i.ifa_addr.as_ref() }.is_some_and(|a| a.sa_family as i32 == libc::AF_INET);
        found |= v4 && i.ifa_flags & want == want && i.ifa_flags & libc::IFF_LOOPBACK as u32 == 0;
        p = i.ifa_next;
    }
    unsafe { libc::freeifaddrs(head) };
    found
}

/// This user's daemons on this computer, found over loopback: each writes
/// `<dir>/<member key>` = `127.0.0.1:<port>` at start and removes it at close. Loopback needs
/// no Local Network permission and no multicast, so two daemons on one Mac connect even when
/// mDNS cannot. A wrong or stale address costs one failed path, nothing more: iroh checks the
/// member key on every connection.
mod local {
    use std::net::SocketAddr;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    use std::path::PathBuf;

    /// `/tmp/lightbulb-connect-<uid>`, if it is ours (a directory, not a link, mode 0700);
    /// one log line the first time it is not.
    fn dir() -> Option<PathBuf> {
        static WARNED: std::sync::Once = std::sync::Once::new();
        let uid = unsafe { libc::getuid() };
        let d = PathBuf::from(format!("/tmp/lightbulb-connect-{uid}"));
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&d);
        let m = std::fs::symlink_metadata(&d).ok();
        let ok = m.is_some_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o077 == 0);
        if !ok {
            WARNED.call_once(|| {
                eprintln!(
                    "net: {} is not a private folder of this user; daemons on this computer are not found through it",
                    d.display()
                )
            });
        }
        ok.then_some(d)
    }

    /// `127.0.0.1:<port> <pid>`, mode 0600.
    pub fn publish(key: &str, port: u16) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let Some(d) = dir() else {
            return;
        };
        let f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(d.join(key));
        if let Ok(mut f) = f {
            let _ = write!(f, "127.0.0.1:{port} {}", std::process::id());
        }
    }

    pub fn withdraw(key: &str) {
        if let Some(d) = dir() {
            let _ = std::fs::remove_file(d.join(key));
        }
    }

    /// `key`'s address, unless the daemon that wrote it is gone (a crash leaves the file).
    pub fn addr(key: &str) -> Option<SocketAddr> {
        let s = std::fs::read_to_string(dir()?.join(key)).ok()?;
        let (addr, pid) = s.split_once(' ')?;
        let pid: libc::pid_t = pid.trim().parse().ok()?;
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        alive.then(|| addr.parse().ok()).flatten()
    }
}
