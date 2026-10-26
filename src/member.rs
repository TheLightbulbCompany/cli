//! `lightbulb member`: the engine as a sidecar in a cloud workspace's microVM, so the workspace
//! is one more computer on its team's network. An embedded instance (`embed`) behind a Unix
//! socket; its one client is the Lightbulb bridge, which starts it with the token on stdin.
//!
//! Each socket line is `{token, id, op, ...body}` and runs as `Caller::Instance`; the answer is
//! `{id, ok: true, result}` or `{id, ok: false, error, ...}`, the body never merged into the
//! envelope. A line without the token is answered `unauthorized` and its connection closed.
//! `subscribe` streams `Instance::events` (`lag` when some were missed); `stop`, or SIGTERM,
//! shuts the instance down (leaving its snapshot) and exits 0.

use crate::daemon::net_ops;
use crate::embed::{self, EmbedOpts, Instance};
use crate::Result;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast::{error::RecvError, Receiver};
use tokio::sync::Notify;
use tokio::task::JoinSet;

pub struct MemberOpts {
    /// The instance's state dir.
    pub home: PathBuf,
    /// Where the final `state.db` copy goes at stop (outside `home`).
    pub snapshot: PathBuf,
    pub socket: PathBuf,
    /// The workspace's display name.
    pub name: String,
    pub relay: Option<String>,
    /// `{workspace, sessions: [{name, agent, title?, ...}], wake?, members?}`: the sessions
    /// declared before the transport starts; `members`, the workspace's people, gate who may
    /// message them: a computer whose sign-in proof names one of them (`proof::admitted`).
    pub sessions: PathBuf,
}

/// Runs the sidecar until `stop` or SIGTERM.
pub async fn run(opts: MemberOpts, token: String) -> Result<()> {
    if token.is_empty() {
        return Err("lightbulb member reads its token from the first line of stdin".into());
    }
    // Before the start: a SIGTERM during it still stops cleanly once it is up.
    let mut term = signal(SignalKind::terminate())?;
    let file: Value = serde_json::from_slice(&std::fs::read(&opts.sessions)?)?;
    let sessions = match file["sessions"].as_array() {
        Some(l) => l.clone(),
        None => {
            eprintln!("member: no sessions list in the sessions file; no sessions declared");
            vec![]
        }
    };
    let members = net_ops::gate_of(&file)
        .map_err(|()| "the sessions file's members must be null or a list of user id strings")?
        .flatten();
    let revoked = net_ops::revoked_of(&file)
        .map_err(|()| "the sessions file's revoked_keys must be a list of key strings")?;
    let relay = opts
        .relay
        .as_deref()
        .map(str::parse::<iroh::RelayUrl>)
        .transpose()?;
    let instance = embed::start(
        &opts.home,
        EmbedOpts {
            relay,
            snapshot: Some(opts.snapshot),
            name: opts.name,
            sessions,
            members,
            revoked,
        },
    )
    .await?;
    instance.publish_wake(&file["wake"])?;
    let instance = Arc::new(instance);
    let _ = std::fs::remove_file(&opts.socket);
    // Created 0600, never wider for a moment: the umask is the process's, put back at once.
    // ponytail: the umask is process-wide for those microseconds; a file another task makes
    // in that window comes out 0600 (narrower, never wider), and nothing makes directories
    // concurrently. A per-socket fchmod-before-listen would close it, if that ever matters.
    // SAFETY: umask takes a mode and cannot fail.
    let umask = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(&opts.socket);
    // SAFETY: as above.
    unsafe { libc::umask(umask) };
    let listener = listener?;
    let (token, stop) = (Arc::new(token), Arc::new(Notify::new()));
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((s, _)) => {
                    conns.spawn(connection(s, instance.clone(), token.clone(), stop.clone()));
                }
                Err(e) => eprintln!("accept: {e}"),
            },
            _ = term.recv() => break,
            _ = stop.notified() => break,
        }
    }
    drop(listener);
    conns.shutdown().await;
    let _ = std::fs::remove_file(&opts.socket);
    match Arc::try_unwrap(instance) {
        Ok(i) => i.shutdown().await,
        Err(_) => return Err("the instance is still in use".into()),
    }
    Ok(())
}

/// The longest line a connection reads (more is answered `unauthorized` and closed).
const MAX_LINE: usize = 1 << 20;

/// What a connection does after a line's answer.
enum Then {
    Next,
    Close,
    Stop,
}

async fn connection(stream: UnixStream, i: Arc<Instance>, token: Arc<String>, stop: Arc<Notify>) {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    // The line so far: `read_until` keeps what it read when the event branch wins the select.
    let mut buf = Vec::new();
    let mut events: Option<Receiver<Value>> = None;
    loop {
        let (frame, then) = tokio::select! {
            n = async {
                let room = (MAX_LINE + 1 - buf.len()) as u64;
                (&mut read).take(room).read_until(b'\n', &mut buf).await
            } => {
                if buf.len() > MAX_LINE {
                    (json!({"id": 0, "ok": false, "error": "unauthorized"}), Then::Close)
                } else if !matches!(n, Ok(n) if n > 0) || buf.last() != Some(&b'\n') {
                    // Closed (or an error), with no whole line left: the bridge ends every
                    // line with `\n`, so a last one without it is dropped.
                    return;
                } else {
                    let line = String::from_utf8(std::mem::take(&mut buf)).unwrap_or_default();
                    answer(&line, &i, &token, &mut events).await
                }
            },
            event = async { events.as_mut().unwrap().recv().await }, if events.is_some() => {
                match event {
                    Ok(e) => (e, Then::Next),
                    Err(RecvError::Lagged(_)) => (json!({ "event": "lag" }), Then::Next),
                    Err(RecvError::Closed) => return,
                }
            }
        };
        if write
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
        match then {
            Then::Next => {}
            Then::Close => return,
            Then::Stop => return stop.notify_one(),
        }
    }
}

/// One line's answer, enveloped.
async fn answer(
    line: &str,
    i: &Instance,
    token: &str,
    events: &mut Option<Receiver<Value>>,
) -> (Value, Then) {
    let mut req = serde_json::from_str::<Value>(line).unwrap_or_default();
    let id = req["id"].as_u64().unwrap_or(0);
    if !req["token"].as_str().is_some_and(|t| same(t, token)) {
        return (
            json!({"id": id, "ok": false, "error": "unauthorized"}),
            Then::Close,
        );
    }
    let ok = |result: Value| json!({"id": id, "ok": true, "result": result});
    match req["op"].as_str() {
        Some("subscribe") => {
            *events = Some(i.events());
            (ok(json!({})), Then::Next)
        }
        Some("stop") => (ok(json!({})), Then::Stop),
        _ => {
            req.as_object_mut().map(|o| o.remove("token"));
            let r = i.respond(req).await;
            if r.ok {
                (ok(r.body), Then::Next)
            } else {
                let mut e = r.body;
                e["id"] = id.into();
                e["ok"] = false.into();
                (e, Then::Next)
            }
        }
    }
}

/// `a == b` in time that does not depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |x, (p, q)| x | (p ^ q)) == 0
}
