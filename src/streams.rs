//! Streams: a viewer watching a session's live bytes over its own bidirectional QUIC stream
//! (not the control stream), opened by `opener` and echoed back by the owner when it accepts;
//! a refusal resets the stream with `REFUSED`. The owner accepts the same person's computers
//! and the viewers a live `share_grant` it signed names; it drops every viewer→owner frame
//! after the opener, except on an `input` grant to the person's own workspace member, where
//! they become `stream_input` events. One task per stream writes and reads it; QUIC flow
//! control is the backpressure. Logs carry ids and sizes, never the bytes.

use crate::admission::{signed, signed_by};
use crate::daemon::ipc::{self, refused, Response, BAD_REQUEST, NOT_FOUND, UNSUPPORTED};
use crate::daemon::net_ops::{person_only, NOT_MEMBER};
use crate::daemon::Daemon;
use crate::net::frames::{self, Frame};
use crate::net::short;
use crate::roster::Member;
use crate::store::Store;
use iroh::endpoint::{ReadError, RecvStream, SendStream, VarInt};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

pub const NO_GRANT: &str = "no_grant";
pub const UNREACHABLE: &str = "unreachable";
/// A grant ends at most this far ahead.
pub const GRANT_MAX_S: i64 = 7 * 86_400;
/// Clock skew allowed on an inbound grant's end, past `GRANT_MAX_S`.
const SKEW_S: i64 = 300;
/// An owner's chunk, decoded.
pub const CHUNK_MAX: usize = 65_536;
/// A viewer's input frame, decoded.
pub const INPUT_MAX: usize = 4_096;
/// How long either end waits for the other's opener.
const OPEN_WAIT: Duration = Duration::from_secs(10);
/// The reset code of a refused stream (the viewer reads `no_grant`).
pub const REFUSED: u32 = 1;
/// The reset code of a stream past `ACCEPTS` (the viewer reads `unreachable`).
pub const BUSY: u32 = 2;
/// Streams one connection may have waiting for their opener (`net`).
pub const ACCEPTS: usize = 8;
/// Why a stream ended at its grant's end: its record stays, so the next send says `no_grant`.
const ENDED: &str = "the grant ended";

/// The open streams, and the viewer frames dropped.
#[derive(Default)]
pub struct Streams {
    open: Mutex<HashMap<String, Open>>,
    /// Viewer→owner frames not taken as input.
    pub dropped_input: AtomicU64,
}

/// What this end is to a stream.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    /// It owns the session and sends; `input` takes the viewer's frames; the stream ends at
    /// `expires_at` (its grant's end; `None` for the same person).
    Owner {
        expires_at: Option<i64>,
        input: bool,
    },
    /// It watches.
    Viewer,
}

impl Role {
    /// What `stream.send` may write, decoded.
    fn cap(self) -> usize {
        match self {
            Role::Owner { .. } => CHUNK_MAX,
            Role::Viewer => INPUT_MAX,
        }
    }

    fn expired(self, now: i64) -> bool {
        matches!(self, Role::Owner { expires_at: Some(e), .. } if e <= now)
    }
}

struct Open {
    session: String,
    /// The member at the other end.
    peer: String,
    role: Role,
    /// Chunks for the stream's task to write.
    tx: mpsc::Sender<String>,
    /// Dropped (with this `Open`) to close the stream, even mid-write.
    _stop: oneshot::Sender<()>,
}

/// A stream's first frame, and the owner's echo of it.
pub fn opener(grant_id: &str, session: &str) -> Frame {
    Frame::Stream {
        grant_id: grant_id.into(),
        session: session.into(),
        seq: 0,
        bytes: String::new(),
    }
}

/// Whether grant `f` names `session` (under the granter's prefix), `viewer` and `granter`:
/// the check that fails, if one does.
fn allows(f: &Frame, session: &str, viewer: &str, granter: &str) -> Result<(), &'static str> {
    let Frame::ShareGrant {
        session: s,
        viewer: v,
        granter: g,
        ..
    } = f
    else {
        return Err("not a grant");
    };
    if g != granter {
        Err("granter")
    } else if s != session || !frames::own(short(granter), session) {
        Err("session")
    } else if v != viewer {
        Err("viewer")
    } else {
        Ok(())
    }
}

/// The inbound grant checks, all before storing: from the peer, for a session under its
/// prefix, naming this computer (`me`), signed by the peer, ending in the future and at most
/// 7 days (and 5 minutes of clock skew) ahead.
pub fn grant_ok(f: &Frame, peer: &Member, me: &str, now: i64) -> Result<(), &'static str> {
    let Frame::ShareGrant {
        grant_id,
        session,
        expires_at,
        ..
    } = f
    else {
        return Err("not a grant");
    };
    if ulid::Ulid::from_string(grant_id).is_err() {
        return Err("grant_id");
    }
    allows(f, session, me, &peer.key)?;
    if !signed_by(&peer.key, f) {
        return Err("sig");
    }
    // The granter's clock may run ahead.
    if *expires_at <= now || *expires_at > now + GRANT_MAX_S + SKEW_S {
        return Err("expires_at");
    }
    Ok(())
}

/// Stores grant `f` (a first copy wins: a grant id is never overwritten), pruning the
/// expired ones.
pub fn store(store: &Store, f: &Frame, now: i64) -> crate::Result<()> {
    let Frame::ShareGrant {
        grant_id,
        expires_at,
        ..
    } = f
    else {
        return Ok(());
    };
    let conn = store.conn();
    conn.execute("DELETE FROM grants WHERE expires_at <= ?1", [now])?;
    conn.execute(
        "INSERT OR IGNORE INTO grants (grant_id, \"grant\", expires_at) VALUES (?1, ?2, ?3)",
        params![grant_id, serde_json::to_string(f)?, expires_at],
    )?;
    Ok(())
}

/// The live grant `grant_id`.
fn by_id(store: &Store, grant_id: &str, now: i64) -> Option<Frame> {
    let q = "SELECT \"grant\" FROM grants WHERE grant_id = ?1 AND expires_at > ?2";
    let g: Option<String> = store
        .conn()
        .query_row(q, params![grant_id, now], |r| r.get(0))
        .optional()
        .ok()?;
    serde_json::from_str(&g?).ok()
}

/// The id of a live grant `granter` gave `viewer` for `session`.
fn grant_for(
    store: &Store,
    session: &str,
    viewer: &str,
    granter: &str,
    now: i64,
) -> Option<String> {
    let q = "SELECT grant_id FROM grants WHERE expires_at > ?1
        AND json_extract(\"grant\", '$.session') = ?2 AND json_extract(\"grant\", '$.viewer') = ?3
        AND json_extract(\"grant\", '$.granter') = ?4";
    store
        .conn()
        .query_row(q, params![now, session, viewer, granter], |r| r.get(0))
        .optional()
        .ok()?
}

/// The decoded length of base64 `s`, if it is base64 (alphabet and padding; no decode).
fn decoded_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let pad = b.iter().rev().take_while(|c| **c == b'=').count();
    let body = &b[..b.len() - pad];
    let ok = b.len().is_multiple_of(4)
        && pad <= 2
        && body
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/');
    ok.then(|| b.len() / 4 * 3 - pad)
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
            Some(sid) if d.streams.close(sid) => Response::ok(id, json!({})),
            Some(_) => Response::err(id, NOT_FOUND),
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
    if let Err(e) = store(&d.store.lock().unwrap(), &f, now) {
        eprintln!("streams: grant {grant_id}: {e}");
        return Response::err(id, ipc::INTERNAL);
    }
    eprintln!("streams: granted {grant_id} to {}", short(viewer));
    Response::ok(id, json!({ "grant_id": grant_id }))
}

/// `stream.open {session}` -> `{stream_id}` (viewer side): a stream to `session`'s member,
/// on a stored live grant naming this computer, or as the same person's computer. A refusal
/// is `no_grant`; no answer in 10 s, or a lost connection, `unreachable`.
async fn open(id: u64, body: &Value, d: &Arc<Daemon>) -> Response {
    let Some(session) = body["session"].as_str() else {
        return Response::err(id, BAD_REQUEST);
    };
    let me = &d.identity.key;
    let (owner, same) = {
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
        (o.key.clone(), r.same_person_computer(&o.key, me))
    };
    let now = ipc::now() as i64;
    let grant = grant_for(&d.store.lock().unwrap(), session, me, &owner, now);
    let grant_id = match grant {
        Some(g) => g,
        None if same => String::new(),
        None => return Response::err(id, NO_GRANT),
    };
    let Some(conn) = d.peers.conn(&owner) else {
        return Response::err(id, UNREACHABLE);
    };
    let Ok((mut send, mut recv)) = conn.open_bi().await else {
        return Response::err(id, UNREACHABLE);
    };
    if frames::write(&mut send, &opener(&grant_id, session))
        .await
        .is_err()
    {
        return Response::err(id, UNREACHABLE);
    }
    match tokio::time::timeout(OPEN_WAIT, frames::read(&mut recv)).await {
        Ok(Ok(Frame::Stream { seq: 0, .. })) => {}
        Ok(Err(e)) if reset_code(&*e) == Some(REFUSED) => return Response::err(id, NO_GRANT),
        _ => return Response::err(id, UNREACHABLE),
    }
    let sid = register(d, session, &owner, Role::Viewer, &grant_id, (send, recv));
    Response::ok(id, json!({ "stream_id": sid }))
}

/// The code a read error carries when the other end reset the stream.
fn reset_code(e: &(dyn std::error::Error + 'static)) -> Option<u32> {
    let io = e.downcast_ref::<std::io::Error>()?;
    match io.get_ref()?.downcast_ref::<ReadError>()? {
        ReadError::Reset(code) => u32::try_from(code.into_inner()).ok(),
        _ => None,
    }
}

/// A stream `viewer` opened to this computer (`net` hands over every stream after the
/// control one): accepted or refused, by the rule at the top.
pub async fn accept(d: Arc<Daemon>, viewer: String, mut send: SendStream, mut recv: RecvStream) {
    let me = &d.identity;
    let (same, own) = {
        let team = d.team.lock().unwrap();
        let r = &team.roster;
        if !r.members.contains_key(&viewer) {
            return;
        }
        let own = r.my_workspace_member(&me.key).map(|m| &m.key) == Some(&viewer);
        (r.same_person_computer(&me.key, &viewer), own)
    };
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
    let now = ipc::now() as i64;
    let granted = || match by_id(&d.store.lock().unwrap(), &grant_id, now)? {
        g @ Frame::ShareGrant {
            input, expires_at, ..
        } => {
            allows(&g, &session, &viewer, &me.key).ok()?;
            Some(Role::Owner {
                expires_at: Some(expires_at),
                input: input && own,
            })
        }
        _ => None,
    };
    let role = if !frames::own(&me.short, &session) {
        None
    } else if same {
        Some(Role::Owner {
            expires_at: None,
            input: false,
        })
    } else {
        granted()
    };
    let Some(role) = role else {
        let _ = send.reset(VarInt::from_u32(REFUSED));
        eprintln!(
            "streams: refused a stream from {} ({NO_GRANT})",
            short(&viewer)
        );
        return;
    };
    if frames::write(&mut send, &opener(&grant_id, &session))
        .await
        .is_err()
    {
        return;
    }
    let sid = register(&d, &session, &viewer, role, &grant_id, (send, recv));
    eprintln!("streams: {sid} opened to {}", short(&viewer));
    event(
        &d,
        json!({"event": "stream_opened", "stream_id": sid, "viewer": viewer, "session": session}),
    );
}

/// Keeps the stream under a new id, closing the one it replaces (one per session and viewer),
/// and starts its task.
fn register(
    d: &Arc<Daemon>,
    session: &str,
    peer: &str,
    role: Role,
    grant_id: &str,
    halves: (SendStream, RecvStream),
) -> String {
    let sid = ulid::Ulid::generate().to_string();
    let (tx, rx) = mpsc::channel(1);
    let (stop, stopped) = oneshot::channel();
    let viewer = role == Role::Viewer;
    let mut open = d.streams.open.lock().unwrap();
    // Dropping the one it replaces closes it.
    // ponytail: a scan of every open stream per open; key by (session, peer) if they pile up.
    open.retain(|_, o| {
        !(o.session == session && o.peer == peer && (o.role == Role::Viewer) == viewer)
    });
    let o = Open {
        session: session.into(),
        peer: peer.into(),
        role,
        tx,
        _stop: stop,
    };
    open.insert(sid.clone(), o);
    let ends = (grant_id.to_string(), session.to_string());
    tokio::spawn(run(d.clone(), sid.clone(), role, ends, halves, rx, stopped));
    sid
}

/// One stream's task: writes the chunks `stream.send` queues (`seq` from 1, at QUIC's pace)
/// and reads the other end, until a close (the `Open` dropped: finished, or reset mid-write),
/// the grant's end, or the other end goes. Then it says `stream_closed`.
async fn run(
    d: Arc<Daemon>,
    sid: String,
    role: Role,
    (grant_id, session): (String, String),
    (mut send, mut recv): (SendStream, RecvStream),
    mut rx: mpsc::Receiver<String>,
    mut stop: oneshot::Receiver<()>,
) {
    let end = match role {
        Role::Owner {
            expires_at: Some(e),
            ..
        } => Some(Instant::now() + Duration::from_secs((e - ipc::now() as i64).max(0) as u64)),
        _ => None,
    };
    let ended = async move {
        match end {
            Some(t) => tokio::time::sleep_until(t).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(ended);
    let writer = async {
        let mut seq = 0;
        loop {
            tokio::select! {
                _ = &mut stop => break "closed",
                _ = &mut ended => break ENDED,
                chunk = rx.recv() => {
                    let Some(bytes) = chunk else { break "closed" };
                    seq += 1;
                    let f = Frame::Stream {
                        grant_id: grant_id.clone(),
                        session: session.clone(),
                        seq,
                        bytes,
                    };
                    tokio::select! {
                        r = frames::write(&mut send, &f) => if r.is_err() { break "the other end went" },
                        _ = &mut stop => {
                            let _ = send.reset(VarInt::from_u32(0));
                            break "closed";
                        }
                        _ = &mut ended => {
                            let _ = send.reset(VarInt::from_u32(0));
                            break ENDED;
                        }
                    }
                }
            }
        }
    };
    let reader = async {
        while let Ok(f) = frames::read(&mut recv).await {
            on_frame(&d, &sid, role, f);
        }
        "the other end went"
    };
    let why = tokio::select! {
        w = writer => w,
        r = reader => r,
    };
    let _ = send.finish();
    if why != ENDED {
        d.streams.close(&sid);
    }
    eprintln!("streams: {sid} closed ({why})");
    event(&d, json!({"event": "stream_closed", "stream_id": sid}));
}

/// A frame from the other end: the owner's chunk becomes `stream` on the viewer side; on
/// the owner side the viewer's becomes `stream_input` on an input grant, and anything else
/// is dropped and counted.
fn on_frame(d: &Daemon, sid: &str, role: Role, f: Frame) {
    let (name, cap) = match role {
        Role::Viewer => (Some("stream"), CHUNK_MAX),
        Role::Owner { input: true, .. } => (Some("stream_input"), INPUT_MAX),
        Role::Owner { .. } => (None, 0),
    };
    match f {
        Frame::Stream { seq, bytes, .. }
            if name.is_some() && decoded_len(&bytes).is_some_and(|n| n <= cap) =>
        {
            event(
                d,
                json!({"event": name, "stream_id": sid, "seq": seq, "data_b64": bytes}),
            );
        }
        _ if role != Role::Viewer => {
            d.streams.dropped_input.fetch_add(1, Ordering::Relaxed);
            eprintln!("streams: {sid}: dropped a viewer {} frame", f.kind());
        }
        _ => {}
    }
}

impl Streams {
    /// Closes stream `sid` (its task finishes, or resets, it and says so); `false` if none.
    pub fn close(&self, sid: &str) -> bool {
        self.open.lock().unwrap().remove(sid).is_some()
    }
}

/// `stream.send {stream_id, data_b64}` -> `{}`: a chunk on an open stream, at most 64 KB
/// decoded from the owner and 4 KB from a viewer, written in order by the stream's task;
/// this waits while the stream is behind. On a stream its grant's end closed, `no_grant`.
async fn send(id: u64, body: &Value, d: &Arc<Daemon>) -> Response {
    let (Some(sid), Some(data)) = (body["stream_id"].as_str(), body["data_b64"].as_str()) else {
        return Response::err(id, BAD_REQUEST);
    };
    let Some(n) = decoded_len(data) else {
        return Response::err(id, BAD_REQUEST);
    };
    let found = d
        .streams
        .open
        .lock()
        .unwrap()
        .get(sid)
        .map(|o| (o.tx.clone(), o.role));
    let Some((tx, role)) = found else {
        return Response::err(id, NOT_FOUND);
    };
    if n > role.cap() {
        return Response::err(id, BAD_REQUEST);
    }
    if tx.send(data.into()).await.is_ok() {
        return Response::ok(id, json!({}));
    }
    d.streams.close(sid);
    let code = if role.expired(ipc::now() as i64) {
        NO_GRANT
    } else {
        NOT_FOUND
    };
    Response::err(id, code)
}
