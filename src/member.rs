//! `lightbulb member`: the engine as a sidecar in a cloud workspace's microVM, so the workspace
//! is one more computer on its team's network. An embedded instance (`embed`) behind a Unix
//! socket; its one client is the Lightbulb bridge, which starts it with the token on stdin.
//!
//! Each socket line is `{token, id, op, ...body}` and runs as `Caller::Instance`; the answer is
//! `{id, ok: true, result}` or `{id, ok: false, error, ...}`, the body never merged into the
//! envelope. A line without the token is answered `unauthorized` and its connection closed.
//! `subscribe` streams `Instance::events` (`lag` when some were missed); `stop`, or SIGTERM,
//! shuts the instance down (leaving its snapshot) and exits 0.

use crate::embed::{self, EmbedOpts, Instance};
use crate::roster::MemberKind;
use crate::Result;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    /// `{workspace, sessions: [{name, agent, title?, ...}], wake?}`: the sessions declared
    /// before the transport starts.
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
    let sessions = file["sessions"].as_array().cloned().unwrap_or_default();
    let relay = opts
        .relay
        .as_deref()
        .map(str::parse::<iroh::RelayUrl>)
        .transpose()?;
    let instance = embed::start(
        &opts.home,
        EmbedOpts {
            kind: MemberKind::Cloud,
            relay,
            snapshot: Some(opts.snapshot),
            name: opts.name,
            sessions,
        },
    )
    .await?;
    let instance = Arc::new(instance);
    let _ = std::fs::remove_file(&opts.socket);
    let listener = UnixListener::bind(&opts.socket)?;
    std::fs::set_permissions(&opts.socket, std::fs::Permissions::from_mode(0o600))?;
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

/// What a connection does after a line's answer.
enum Then {
    Next,
    Close,
    Stop,
}

async fn connection(stream: UnixStream, i: Arc<Instance>, token: Arc<String>, stop: Arc<Notify>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut events: Option<Receiver<Value>> = None;
    loop {
        let (frame, then) = tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => answer(&line, &i, &token, &mut events).await,
                _ => return,
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
