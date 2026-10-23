//! Streams: a viewer watching a session's live bytes over its own bidirectional QUIC stream
//! (not the control stream), opened by `Stream {grant_id, session, seq: 0, bytes: ""}` and
//! echoed back by the owner when it accepts. The owner accepts the same person's computers
//! and the viewers a live `share_grant` it signed names; it drops every viewer→owner frame
//! after the opener, except on an `input` grant to the person's own workspace member, where
//! they become `stream_input` events. QUIC flow control is the backpressure. Logs carry ids
//! and sizes, never the bytes.

use crate::daemon::ipc::{self, refused, Response, BAD_REQUEST, NOT_FOUND, UNSUPPORTED};
use crate::daemon::net_ops::{person_only, NOT_MEMBER};
use crate::daemon::Daemon;
use crate::identity::{self, Identity};
use crate::net::frames::{self, Frame};
use crate::net::short;
use crate::roster::{Member, MemberKind};
use crate::store::Store;
use data_encoding::BASE64;
use iroh::endpoint::{RecvStream, SendStream, VarInt};
use rusqlite::params;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const NO_GRANT: &str = "no_grant";
pub const UNREACHABLE: &str = "unreachable";
/// A grant ends at most this far ahead.
pub const GRANT_MAX_S: i64 = 7 * 86_400;
/// An owner's chunk, decoded.
pub const CHUNK_MAX: usize = 65_536;
/// A viewer's input frame, decoded.
pub const INPUT_MAX: usize = 4_096;
/// How long either end waits for the other's opener.
const OPEN_WAIT: Duration = Duration::from_secs(10);
/// Clock skew allowed on an inbound grant's end, past `GRANT_MAX_S`.
const SKEW_S: i64 = 300;

/// The open streams, and what was dropped.
#[derive(Default)]
pub struct Streams {
    open: Mutex<HashMap<String, Open>>,
    /// Inbound grants that failed a check.
    pub dropped_grants: AtomicU64,
    /// Viewer→owner frames not taken as input.
    pub dropped_input: AtomicU64,
}

struct Open {
    grant_id: String,
    session: String,
    /// The member at the other end.
    peer: String,
    /// This end owns the session (it sends; the other end watches).
    owner: bool,
    /// The grant's end (owner side, `None` for the same person).
    expires_at: Option<i64>,
    /// The stream's write half and the last `seq` written.
    send: Arc<tokio::sync::Mutex<(SendStream, u32)>>,
    /// The task reading the other end (`register` starts it).
    reader: Option<tokio::task::AbortHandle>,
    /// Set by `shut`: a send waiting on flow control gives up and resets the stream.
    closed: tokio::sync::watch::Sender<bool>,
}

/// `f` (a `share_grant`) with its `sig` set: `id`'s signature over it without `t` or `sig`.
pub fn signed(id: &Identity, mut f: Frame) -> Frame {
    let s = id.sign(&signed_part(&f));
    if let Frame::ShareGrant { sig, .. } = &mut f {
        *sig = s;
    }
    f
}

fn signed_part(f: &Frame) -> Value {
    let mut v = serde_json::to_value(f).expect("a frame serializes");
    if let Some(m) = v.as_object_mut() {
        m.remove("t");
    }
    v
}

/// The inbound grant checks, all before storing: signed by `granter`, which is the peer, for
/// a session under the peer's prefix, naming this computer (`me`) as the viewer, ending in
/// the future and at most 7 days (and 5 minutes of clock skew) ahead.
pub fn grant_ok(f: &Frame, peer: &Member, me: &str, now: i64) -> bool {
    let Frame::ShareGrant {
        grant_id,
        session,
        viewer,
        expires_at,
        granter,
        sig,
        ..
    } = f
    else {
        return false;
    };
    ulid::Ulid::from_string(grant_id).is_ok()
        && *granter == peer.key
        && identity::verify(granter, &signed_part(f), sig)
        && frames::own(&peer.short, session)
        && viewer == me
        && *expires_at > now
        && *expires_at <= now + GRANT_MAX_S + SKEW_S // the granter's clock may run ahead
}

/// Stores grant `f` (a first copy wins: a grant id is never overwritten).
pub fn store(store: &Store, f: &Frame) -> crate::Result<()> {
    let Frame::ShareGrant {
        grant_id,
        expires_at,
        ..
    } = f
    else {
        return Ok(());
    };
    store.conn().execute(
        "INSERT OR IGNORE INTO grants (grant_id, \"grant\", expires_at) VALUES (?1, ?2, ?3)",
        params![grant_id, serde_json::to_string(f)?, expires_at],
    )?;
    Ok(())
}

/// The first live grant `pred` holds for; the expired ones are pruned first.
// ponytail: a scan of every live grant; index by (session, viewer) if grants pile up.
fn live(store: &Store, now: i64, pred: impl Fn(&Frame) -> bool) -> Option<Frame> {
    let conn = store.conn();
    let _ = conn.execute("DELETE FROM grants WHERE expires_at <= ?1", [now]);
    let mut q = conn.prepare("SELECT \"grant\" FROM grants").ok()?;
    let rows: Vec<String> = q.query_map([], |r| r.get(0)).ok()?.flatten().collect();
    rows.iter()
        .filter_map(|g| serde_json::from_str(g).ok())
        .find(|f| pred(f))
}

fn event(d: &Daemon, v: Value) {
    let _ = d.events.send(v);
}

/// The ops: `share.grant` (person-only), `stream.open`, `stream.send`, `stream.close`.
pub async fn handle(
    id: u64,
    op: &str,
    body: &Value,
    d: &Arc<Daemon>,
    peer_pid: Option<u32>,
) -> Response {
    match op {
        "share.grant" if !peer_pid.is_some_and(person_only) => refused(id, "not_a_person"),
        "share.grant" => share(id, body, d),
        "stream.open" => open(id, body, d).await,
        "stream.send" => send(id, body, d).await,
        _ => match body["stream_id"].as_str() {
            Some(sid) => match d.streams.take(sid) {
                Some(o) => {
                    shut(d, sid, o);
                    Response::ok(id, json!({}))
                }
                None => Response::err(id, NOT_FOUND),
            },
            None => Response::err(id, BAD_REQUEST),
        },
    }
}

/// `share.grant {session, viewer, input?, expires_at}` -> `{grant_id}`: a signed grant for
/// this computer's own `session`, stored here and sent to `viewer`. `input: true` only for
/// this person's own workspace member (`unsupported` otherwise).
fn share(id: u64, body: &Value, d: &Daemon) -> Response {
    let (Some(session), Some(viewer), Some(expires_at)) = (
        body["session"].as_str(),
        body["viewer"].as_str(),
        body["expires_at"].as_i64(),
    ) else {
        return Response::err(id, BAD_REQUEST);
    };
    let input = match body.get("input") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Response::err(id, BAD_REQUEST),
    };
    let me = &d.identity;
    let now = ipc::now() as i64;
    if !frames::own(&me.short, session)
        || viewer == me.key
        || expires_at <= now
        || expires_at > now + GRANT_MAX_S
    {
        return Response::err(id, BAD_REQUEST);
    }
    {
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        if !r.members.contains_key(viewer) {
            return Response::err(id, NOT_MEMBER);
        }
        if input && r.my_workspace_member(&me.key).map(|m| m.key.as_str()) != Some(viewer) {
            return Response::err(id, UNSUPPORTED);
        }
    }
    let grant_id = ulid::Ulid::generate().to_string();
    let f = signed(
        me,
        Frame::ShareGrant {
            grant_id: grant_id.clone(),
            session: session.into(),
            viewer: viewer.into(),
            input,
            expires_at,
            granter: me.key.clone(),
            sig: String::new(),
        },
    );
    if d.peers.send(viewer, f.clone()).is_err() {
        return Response::err(id, UNREACHABLE);
    }
    if let Err(e) = store(&d.store.lock().unwrap(), &f) {
        eprintln!("streams: grant {grant_id}: {e}");
        return Response::err(id, ipc::INTERNAL);
    }
    eprintln!("streams: granted {grant_id} to {}", short(viewer));
    Response::ok(id, json!({ "grant_id": grant_id }))
}

/// `stream.open {session}` -> `{stream_id}` (viewer side): a stream to `session`'s member,
/// on a stored live grant naming this computer, or as the same person's computer.
async fn open(id: u64, body: &Value, d: &Arc<Daemon>) -> Response {
    let Some(session) = body["session"].as_str() else {
        return Response::err(id, BAD_REQUEST);
    };
    let me = &d.identity.key;
    let now = ipc::now() as i64;
    let (owner, grant_id) = {
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        let prefix = crate::sessions::member_of(session).unwrap_or("");
        let Some(o) = r
            .by_short(prefix)
            .filter(|o| frames::own(&o.short, session))
        else {
            return Response::err(id, NOT_MEMBER);
        };
        if o.key == *me {
            return Response::err(id, BAD_REQUEST);
        }
        let same = r
            .members
            .get(me)
            .is_some_and(|m| m.kind == MemberKind::Computer && m.person_id == o.person_id);
        let grant = live(&d.store.lock().unwrap(), now, |f| {
            matches!(f, Frame::ShareGrant { session: s, viewer, granter, .. }
                if s == session && viewer == me && *granter == o.key)
        });
        match grant {
            Some(Frame::ShareGrant { grant_id, .. }) => (o.key.clone(), grant_id),
            _ if same => (o.key.clone(), String::new()),
            _ => return Response::err(id, NO_GRANT),
        }
    };
    let Some(conn) = d.peers.conn(&owner) else {
        return Response::err(id, UNREACHABLE);
    };
    let Ok((mut send, mut recv)) = conn.open_bi().await else {
        return Response::err(id, UNREACHABLE);
    };
    let opener = Frame::Stream {
        grant_id: grant_id.clone(),
        session: session.into(),
        seq: 0,
        bytes: String::new(),
    };
    if frames::write(&mut send, &opener).await.is_err() {
        return Response::err(id, UNREACHABLE);
    }
    // The owner echoes the opener when it accepts, and drops the stream when it does not.
    match tokio::time::timeout(OPEN_WAIT, frames::read(&mut recv)).await {
        Ok(Ok(Frame::Stream { seq: 0, .. })) => {}
        // Connected but silent: not a refusal.
        Err(_) => return Response::err(id, UNREACHABLE),
        Ok(_) => return Response::err(id, NO_GRANT),
    }
    let o = Open {
        grant_id,
        session: session.into(),
        peer: owner,
        owner: false,
        expires_at: None,
        send: Arc::new(tokio::sync::Mutex::new((send, 0))),
        reader: None,
        closed: Default::default(),
    };
    let sid = register(d, o, recv, false);
    Response::ok(id, json!({ "stream_id": sid }))
}

/// A stream `viewer` opened to this computer (`net` hands over every stream after the
/// control one): accepted or dropped, by the rule at the top.
pub async fn accept(d: Arc<Daemon>, viewer: String, mut send: SendStream, mut recv: RecvStream) {
    let first = tokio::time::timeout(OPEN_WAIT, frames::read(&mut recv)).await;
    let Ok(Ok(Frame::Stream {
        grant_id,
        session,
        seq: 0,
        ..
    })) = first
    else {
        eprintln!(
            "streams: dropped a stream from {} (no opener)",
            short(&viewer)
        );
        return;
    };
    let me = &d.identity;
    let now = ipc::now() as i64;
    let decision = {
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        let mine = r.members.get(&me.key);
        match r.members.get(&viewer) {
            _ if !frames::own(&me.short, &session) => None,
            None => None,
            Some(v)
                if v.kind == MemberKind::Computer
                    && mine.is_some_and(|m| m.person_id == v.person_id) =>
            {
                Some((None, false))
            }
            Some(_) => live(&d.store.lock().unwrap(), now, |f| {
                matches!(f, Frame::ShareGrant { grant_id: g, session: s, viewer: v, granter, .. }
                    if *g == grant_id && *s == session && *v == viewer && *granter == me.key)
            })
            .and_then(|f| match f {
                Frame::ShareGrant {
                    input, expires_at, ..
                } => {
                    let own = r.my_workspace_member(&me.key).map(|m| &m.key) == Some(&viewer);
                    Some((Some(expires_at), input && own))
                }
                _ => None,
            }),
        }
    };
    let Some((expires_at, input)) = decision else {
        eprintln!(
            "streams: refused a stream from {} ({NO_GRANT})",
            short(&viewer)
        );
        return; // dropping the stream tells the viewer
    };
    let echo = Frame::Stream {
        grant_id: grant_id.clone(),
        session: session.clone(),
        seq: 0,
        bytes: String::new(),
    };
    if frames::write(&mut send, &echo).await.is_err() {
        return;
    }
    let o = Open {
        grant_id,
        session: session.clone(),
        peer: viewer.clone(),
        owner: true,
        expires_at,
        send: Arc::new(tokio::sync::Mutex::new((send, 0))),
        reader: None,
        closed: Default::default(),
    };
    let sid = register(&d, o, recv, input);
    eprintln!("streams: {sid} opened to {}", short(&viewer));
    event(
        &d,
        json!({"event": "stream_opened", "stream_id": sid, "viewer": viewer, "session": session}),
    );
}

/// Keeps `o` under a new stream id, closing the one it replaces (one per session and
/// viewer), and starts reading the other end.
fn register(d: &Arc<Daemon>, mut o: Open, recv: RecvStream, input: bool) -> String {
    let sid = ulid::Ulid::generate().to_string();
    let mut open = d.streams.open.lock().unwrap();
    let old: Vec<String> = open
        .iter()
        .filter(|(_, x)| x.session == o.session && x.peer == o.peer && x.owner == o.owner)
        .map(|(k, _)| k.clone())
        .collect();
    for k in old {
        if let Some(x) = open.remove(&k) {
            shut(d, &k, x);
        }
    }
    let reader = pump(d.clone(), sid.clone(), recv, o.owner, input, o.expires_at);
    o.reader = Some(tokio::spawn(reader).abort_handle());
    open.insert(sid.clone(), o);
    sid
}

/// Reads the other end until it closes: on the viewer side the owner's chunks become
/// `stream` events; on the owner side the viewer's frames become `stream_input` on an input
/// grant, and are dropped (and counted) otherwise; a frame past the grant's end closes it.
async fn pump(
    d: Arc<Daemon>,
    sid: String,
    mut recv: RecvStream,
    owner: bool,
    input: bool,
    expires_at: Option<i64>,
) {
    while let Ok(f) = frames::read(&mut recv).await {
        if owner && expires_at.is_some_and(|e| e <= ipc::now() as i64) {
            eprintln!("streams: {sid}: the grant ended");
            break;
        }
        let Frame::Stream { seq, bytes, .. } = f else {
            if owner {
                d.streams.dropped_input.fetch_add(1, Ordering::Relaxed);
                eprintln!("streams: {sid}: dropped a {} frame", f.kind());
            }
            continue;
        };
        let size = BASE64.decode(bytes.as_bytes()).map(|b| b.len());
        if !owner {
            if size.is_ok_and(|n| n <= CHUNK_MAX) {
                event(
                    &d,
                    json!({"event": "stream", "stream_id": sid, "seq": seq, "data_b64": bytes}),
                );
            }
        } else if input && size.is_ok_and(|n| n <= INPUT_MAX) {
            event(
                &d,
                json!({"event": "stream_input", "stream_id": sid, "seq": seq, "data_b64": bytes}),
            );
        } else {
            d.streams.dropped_input.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "streams: {sid}: dropped a viewer frame ({} bytes)",
                bytes.len()
            );
        }
    }
    if let Some(mut o) = d.streams.take(&sid) {
        o.reader = None; // this task
        shut(&d, &sid, o);
    }
}

/// Closes `o` (its stream finished, its reader stopped) and says so. A send in flight holds
/// the write half: told by `closed`, it gives up and resets the stream.
fn shut(d: &Daemon, sid: &str, o: Open) {
    if let Some(r) = o.reader {
        r.abort();
    }
    o.closed.send_replace(true);
    if let Ok(mut s) = o.send.try_lock() {
        let _ = s.0.finish();
    }
    eprintln!("streams: {sid} closed");
    event(d, json!({"event": "stream_closed", "stream_id": sid}));
}

impl Streams {
    fn take(&self, sid: &str) -> Option<Open> {
        self.open.lock().unwrap().remove(sid)
    }
}

/// `stream.send {stream_id, data_b64}` -> `{seq}`: a chunk on an open stream, at most 64 KB
/// decoded from the owner and 4 KB from a viewer. Past its grant's end the owner's stream is
/// closed instead (`no_grant`).
async fn send(id: u64, body: &Value, d: &Arc<Daemon>) -> Response {
    let (Some(sid), Some(data)) = (body["stream_id"].as_str(), body["data_b64"].as_str()) else {
        return Response::err(id, BAD_REQUEST);
    };
    let Ok(bytes) = BASE64.decode(data.as_bytes()) else {
        return Response::err(id, BAD_REQUEST);
    };
    let now = ipc::now() as i64;
    let (half, grant_id, session, cap, expired, mut closed) = {
        let open = d.streams.open.lock().unwrap();
        let Some(o) = open.get(sid) else {
            return Response::err(id, NOT_FOUND);
        };
        let cap = if o.owner { CHUNK_MAX } else { INPUT_MAX };
        let expired = o.expires_at.is_some_and(|e| e <= now);
        let (g, s) = (o.grant_id.clone(), o.session.clone());
        (o.send.clone(), g, s, cap, expired, o.closed.subscribe())
    };
    if bytes.len() > cap {
        return Response::err(id, BAD_REQUEST);
    }
    if expired {
        if let Some(o) = d.streams.take(sid) {
            shut(d, sid, o);
        }
        return Response::err(id, NO_GRANT);
    }
    let write = async {
        let mut s = half.lock().await;
        s.1 += 1;
        let f = Frame::Stream {
            grant_id,
            session,
            seq: s.1,
            bytes: data.into(),
        };
        // QUIC flow control: this waits while the other end is behind.
        frames::write(&mut s.0, &f).await.map(|()| s.1)
    };
    let sent = tokio::select! {
        r = write => Some(r),
        _ = closed.wait_for(|c| *c) => None,
    };
    match sent {
        Some(Ok(seq)) => Response::ok(id, json!({ "seq": seq })),
        Some(Err(_)) => {
            if let Some(o) = d.streams.take(sid) {
                shut(d, sid, o);
            }
            Response::err(id, UNREACHABLE)
        }
        // Closed while waiting: the half-written stream is reset, not finished.
        None => {
            if let Ok(mut s) = half.try_lock() {
                let _ = s.0.reset(VarInt::from_u32(0));
            }
            Response::err(id, NOT_FOUND)
        }
    }
}
