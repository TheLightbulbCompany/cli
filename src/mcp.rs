//! `mcp`: the stdio MCP server named `lightbulb` that an agent runs. Its network tools message
//! the other coding sessions, through the daemon, like the hook. While this computer is signed
//! in (`login`) it also has the workspace tools, which call Lightbulb's API as the signed-in
//! person. Every tool call first resolves which session it comes from (`agents::source`); the
//! model never names its own source. Names, descriptions and input schemas come from
//! `mcp/tools.json`, the one spec the hosted MCP serves too.

use crate::agents::source::{self, SourceError};
use crate::os::Process;
use crate::sessions::{Key, Session, Tier};
use crate::{daemon, hook, login, mail, paths};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, RequestMetaObject,
};
use rmcp::service::{Peer, RequestContext};
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Runs the server on stdin/stdout until the client closes it.
pub fn run(name: String) -> crate::Result<()> {
    STARTED.get_or_init(exe_id);
    daemon::ensure_running()?;
    tokio::runtime::Runtime::new()?.block_on(async {
        let signed = Arc::new(AtomicBool::new(signed_in()));
        let service = Server {
            me: OnceLock::new(),
            name,
            signed: signed.clone(),
        }
        .serve(rmcp::transport::stdio())
        .await?;
        // A sign-in that ends (or a login, logout) with no request in between still shows.
        let peer = service.peer().clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(poll()).await;
                changed(&signed, &peer).await;
            }
        });
        service.waiting().await?;
        Ok(())
    })
}

/// How often the sign-in is checked between requests. Debug builds:
/// `LIGHTBULB_CONNECT_DEBUG_TOOLS_POLL_MS` (tests).
fn poll() -> Duration {
    let ms = std::env::var("LIGHTBULB_CONNECT_DEBUG_TOOLS_POLL_MS")
        .ok()
        .filter(|_| cfg!(debug_assertions))
        .and_then(|v| v.parse().ok());
    Duration::from_millis(ms.unwrap_or(30_000))
}

/// Tells the client its tool list changed when the sign-in state differs from `signed`'s.
async fn changed(signed: &AtomicBool, peer: &Peer<RoleServer>) {
    let now = signed_in();
    if signed.swap(now, Ordering::SeqCst) != now {
        let _ = peer.notify_tool_list_changed().await;
    }
}

struct Server {
    /// The agent process above this server, once a call has found it.
    me: OnceLock<Process>,
    /// The name this server advertises (`mcp --server-name`).
    name: String,
    /// Whether the tools last listed held the workspace tools (`changed`).
    signed: Arc<AtomicBool>,
}

/// The tools: `[{name, description, inputSchema, scope: "network"|"workspace"}]`. `mcp
/// --tools-json` prints it byte for byte (the desktop app's and the backend's drift check).
pub const TOOLS: &str = include_str!("mcp/tools.json");

fn spec() -> &'static [Value] {
    static SPEC: OnceLock<Vec<Value>> = OnceLock::new();
    SPEC.get_or_init(|| serde_json::from_str(TOOLS).expect("tools.json is a list"))
}

/// Whether this computer has a sign-in that has not ended: the workspace tools exist only then.
fn signed_in() -> bool {
    let dir = paths::state_path();
    login::stored_proof(&dir).is_some() && !login::ended(&dir, crate::daemon::ipc::now() as i64)
}

#[derive(Deserialize)]
struct FindArgs {
    #[serde(default)]
    query: String,
    #[serde(default = "twenty")]
    limit: u32,
}

fn twenty() -> u32 {
    20
}

#[derive(Deserialize)]
struct SendArgs {
    request_id: String,
    text: String,
    session_id: Option<String>,
    reply_to: Option<String>,
    room_id: Option<String>,
    person_id: Option<String>,
    reply_to_message_id: Option<String>,
    mention_ids: Option<Vec<String>>,
    status: Option<String>,
}

#[derive(Deserialize)]
struct AssignArgs {
    request_id: String,
    to: String,
    brief: String,
    done_when: String,
    deadline_s: Option<u64>,
    repo: Option<String>,
    branch: Option<String>,
}

#[derive(Deserialize)]
struct CancelArgs {
    assignment: String,
    request_id: String,
    note: Option<String>,
}

#[derive(Deserialize)]
struct ReceiptArgs {
    request_id: String,
}

/// The longest one `check_inbox` call waits. Codex times a tool call out at 60 s, so a Codex
/// session's wait is held to `CODEX_WAIT_CAP_S` whatever it asks for.
const WAIT_CAP_S: u32 = 600;
const CODEX_WAIT_CAP_S: u32 = 55;

#[derive(Deserialize)]
struct InboxArgs {
    /// Accepts up to 600, but one call waits at most `WAIT_CAP_S`.
    #[serde(default)]
    wait_secs: u32,
}

/// The rows of a directory listing that are always-on agents: online, at a name address
/// (`<member>/<name>`, never a terminal or a coding session), each marked `always_on`.
fn always_on(short: &str, sessions: Vec<Value>) -> Vec<Value> {
    (sessions.into_iter())
        .filter(|s| {
            s["online"] == true
                && s["session_id"].as_str().is_some_and(|id| {
                    matches!(
                        crate::frame::Sender::of(short, id),
                        crate::frame::Sender::Name(_)
                    )
                })
        })
        .map(|mut s| {
            s["always_on"] = true.into();
            s
        })
        .collect()
}

/// The calling session, resolved, and every live session as the daemon sees them.
struct Call {
    me: Process,
    from: Key,
    sessions: Vec<Session>,
}

fn fail(body: Value) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(body.to_string())])
}

fn code(code: &str) -> CallToolResult {
    fail(json!({ "error": code }))
}

/// The hint when `lightbulb update` has replaced this server's binary since it started.
const REPLACED: &str = "This tool server is older than the installed Lightbulb. Start a new session (or restart your agent).";

/// This server's binary (device, inode) when it started (`run`).
static STARTED: OnceLock<Option<(u64, u64)>> = OnceLock::new();

/// The (device, inode) at this process's executable path now.
fn exe_id() -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    Some((m.dev(), m.ino()))
}

/// Whether the binary this server runs was replaced on disk (an update renames the new one
/// over it). The Keychain then refuses this process: its access check reads the code on disk,
/// which no longer matches what is running.
fn replaced() -> bool {
    STARTED
        .get()
        .copied()
        .flatten()
        .is_some_and(|s| exe_id() != Some(s))
}

/// An unexpected failure: `internal`, with what went wrong (or that this server is stale).
fn internal(detail: impl std::fmt::Display) -> Value {
    let hint = if replaced() {
        REPLACED.to_string()
    } else {
        detail.to_string()
    };
    json!({ "error": "internal", "hint": hint })
}

/// A daemon refusal as the tool's error: its body without the envelope.
fn refused(mut r: Value) -> CallToolResult {
    if let Some(o) = r.as_object_mut() {
        o.remove("id");
        o.remove("ok");
    }
    fail(r)
}

/// One request to the daemon on a fresh connection, starting the daemon if it is not up.
async fn ask(op: &str, mut body: Value) -> crate::Result<Value> {
    body["id"] = 1.into();
    body["op"] = op.into();
    let stream = match UnixStream::connect(paths::socket()).await {
        Ok(s) => s,
        Err(_) => {
            tokio::task::spawn_blocking(daemon::ensure_running).await??;
            UnixStream::connect(paths::socket()).await?
        }
    };
    let (read, mut write) = stream.into_split();
    write.write_all(format!("{body}\n").as_bytes()).await?;
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    Ok(serde_json::from_str(&line)?)
}

/// A regular file's text, at most 64 KiB of it. Opened non-blocking, so a FIFO (or any
/// other special file) planted under a name is refused, never waited on.
fn read_capped(p: &Path) -> Option<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(p)
        .ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    f.take(64 * 1024).read_to_string(&mut text).ok()?;
    Some(text)
}

/// Origin's repo key (`repo_key`) and the checked-out branch, from the
/// `.git` of `cwd` or of the nearest folder above it that has one: a directory, or a
/// `gitdir:` file (a linked worktree; `gitdir` and its `commondir` may point anywhere: only
/// two names are read out). A symlinked `.git` is not followed. Every read is capped at 64 KiB.
pub(crate) fn git(cwd: &str) -> (Option<String>, Option<String>) {
    git_in(Path::new(cwd)).unwrap_or_default()
}

fn git_in(cwd: &Path) -> Option<(Option<String>, Option<String>)> {
    let (root, kind) = cwd.ancestors().find_map(|p| {
        Some((
            p,
            std::fs::symlink_metadata(p.join(".git")).ok()?.file_type(),
        ))
    })?;
    let dot = root.join(".git");
    let dir = if kind.is_dir() {
        dot
    } else if kind.is_file() {
        let d = read_capped(&dot)?;
        root.join(d.trim().strip_prefix("gitdir:")?.trim())
    } else {
        return None;
    };
    let branch = read_capped(&dir.join("HEAD"))
        .and_then(|h| Some(h.trim().strip_prefix("ref: refs/heads/")?.to_string()));
    let common = match read_capped(&dir.join("commondir")) {
        Some(c) => dir.join(c.trim()),
        None => dir,
    };
    let config = read_capped(&common.join("config")).unwrap_or_default();
    let mut origin = false;
    let mut repo = None;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            origin = line == "[remote \"origin\"]";
        } else if let Some(url) = line.strip_prefix("url").map(str::trim_start) {
            if origin {
                if let Some(url) = url.strip_prefix('=') {
                    repo = repo_key(url);
                }
            }
        }
    }
    Some((repo, branch))
}

/// The repo key of origin `url`: `owner/name`, lowercase (the last two segments of its path),
/// or the bare `name` when the path has one segment. The host, user and port are not part of
/// it: `git@host:path`, `scheme://user@host:port/path` and a local path all give the path.
fn repo_key(url: &str) -> Option<String> {
    let url = url.trim().to_lowercase();
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map_or("", |(_, p)| p),
        None => match url.split_once(':') {
            Some((host, p)) if !host.contains('/') => p,
            _ => &url,
        },
    };
    let mut segs = path.split('/').filter(|s| !s.is_empty()).rev();
    let last = segs.next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty() {
        return None;
    }
    Some(
        segs.next()
            .map_or(name.to_string(), |o| format!("{o}/{name}")),
    )
}

/// Receipt `r` with its `note`, when it has one.
fn with_note(mut r: Value) -> Value {
    if let Some(n) = mail::receipt_note(&r, None) {
        r["note"] = n.into();
    }
    r
}

impl Server {
    /// Resolves the calling session, or the error result the tool returns instead.
    async fn source(&self, meta: RequestMetaObject) -> Result<Call, CallToolResult> {
        // Looked up on each call until found: the agent may not be above us yet at startup.
        let me = match self.me.get() {
            Some(p) => p.clone(),
            None => match hook::calling_agent(None) {
                Ok(p) => self.me.get_or_init(|| p).clone(),
                Err(_) => return Err(code("source_unidentified")),
            },
        };
        let meta = Value::Object(meta.0 .0);
        let mut sessions = Self::sessions().await?;
        // Only for an OpenCode process with no session yet: once it has one, an id none of
        // its sessions has is a subagent's child session, never registered.
        let has_one = sessions.iter().any(|s| {
            s.provider == "opencode" && s.pid == me.pid && s.birth_micros == me.birth_micros
        });
        if !has_one
            && source::resolve(&me, &meta, &sessions) == Err(SourceError::Unidentified)
            && Self::register_opencode(&me, &meta).await
        {
            sessions = Self::sessions().await?;
        }
        match source::resolve(&me, &meta, &sessions) {
            Ok(from) => {
                // A tool call is a step: mail delivered before it has been seen.
                let _ = ask("mark_read", json!({ "key": from })).await;
                Ok(Call { me, from, sessions })
            }
            Err(SourceError::Unidentified) => Err(code("source_unidentified")),
            Err(SourceError::Ambiguous) => Err(code("source_ambiguous")),
        }
    }

    async fn sessions() -> Result<Vec<Session>, CallToolResult> {
        let r = ask("sessions", json!({}))
            .await
            .map_err(|e| fail(internal(e)))?;
        serde_json::from_value(r["sessions"].clone()).map_err(|e| fail(internal(e)))
    }

    /// OpenCode 2.x names its session in every call's `_meta`, and one OpenCode process hosts
    /// all of its sessions: the first session of an OpenCode process no hook has registered yet
    /// (the plugin missed its start, or its first tool call is this one) is registered here, to the OpenCode process above
    /// this server, as the hook would (`register`, with that process as the caller, so its
    /// ownership rules hold). Only for an OpenCode caller and a valid id; whether it did.
    async fn register_opencode(me: &Process, meta: &Value) -> bool {
        let Some(id) = meta[source::OPENCODE_SESSION].as_str() else {
            return false;
        };
        if crate::agents::caller::classify(&me.executable) != Some("opencode")
            || !crate::sessions::word(id, 128)
        {
            return false;
        }
        // The folder OpenCode started this server in: the session's location.
        let cwd = std::env::current_dir().unwrap_or_default();
        let session = json!({ "provider": "opencode", "conversation": id, "pid": me.pid,
            "birth_micros": me.birth_micros, "cwd": crate::presence::resolved(&cwd),
            "status": "idle", "executable": me.executable });
        let r = ask(
            "register",
            json!({ "session": session, "caller": hook::proc_json(me) }),
        )
        .await;
        r.is_ok_and(|r| r["key"].is_string() && r["ignored"] != true)
    }

    /// `body` as the tool's result, with the unread note when mail waits for a session
    /// that does not have it pushed.
    async fn done(call: &Call, body: Value) -> CallToolResult {
        let mut text = body.to_string();
        let push = call
            .sessions
            .iter()
            .any(|s| s.key == call.from && s.tier == Tier::Push);
        let unread = match ask("unread", json!({ "key": call.from })).await {
            Ok(r) => r["unread"].as_u64().unwrap_or(0),
            Err(_) => 0,
        };
        if unread > 0 && !push {
            text.push_str(&format!(
                "\n\n{unread} unread message(s) for this session; they arrive at your next step."
            ));
        }
        CallToolResult::success(vec![ContentBlock::text(text)])
    }
}

/// `s` percent-encoded for a URL path segment or query value.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `tool`'s schema properties present in `args`, by name, leaving out `skip` (path parts).
fn props(tool: &Value, args: &Value, skip: &[&str]) -> Map<String, Value> {
    let mut keys: Vec<&String> = tool["inputSchema"]["properties"]
        .as_object()
        .map(|p| p.keys().collect())
        .unwrap_or_default();
    keys.sort();
    keys.into_iter()
        .filter(|k| !skip.contains(&k.as_str()) && !args[k.as_str()].is_null())
        .map(|k| (k.clone(), args[k.as_str()].clone()))
        .collect()
}

/// `props` as a query string: `""`, or `?k=v&...`.
fn query(props: Map<String, Value>) -> String {
    let pairs: Vec<String> = props
        .iter()
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k}={}", enc(s)),
            v => format!("{k}={}", enc(&v.to_string())),
        })
        .collect();
    if pairs.is_empty() {
        String::new()
    } else {
        format!("?{}", pairs.join("&"))
    }
}

/// A UUID for `what` (`message`, `dm`) of request `rid` from session `from`: a retry of one
/// send reuses its ids, and two sessions' request ids never collide (version 8, from SHA-256).
fn uuid_of(what: &str, from: &Key, rid: &str) -> String {
    let mut b: [u8; 16] = Sha256::digest(format!("{what}\0{}\0{rid}", from.0))[..16]
        .try_into()
        .expect("16 bytes");
    b[6] = (b[6] & 0x0f) | 0x80;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// Where a room send's room is remembered, so `message_receipt` needs only its request id.
/// ponytail: one small file per room send, never pruned; prune by age if it ever matters.
fn room_record(from: &Key, rid: &str) -> std::path::PathBuf {
    let h: String = Sha256::digest(format!("{}\0{rid}", from.0))
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    paths::state_path().join("room-sends").join(h)
}

/// A room member's kind by its id: people are Clerk user ids, anything else is an agent.
fn kind_of(id: &str) -> &'static str {
    if id.starts_with("user_") {
        "human"
    } else {
        "agent"
    }
}

/// One signed call to Lightbulb's API as this computer's signed-in person, for the session
/// `label` (the engine's own name for the caller, never the model's): `X-Lightbulb-Proof` is
/// the sign-in proof, `X-Lightbulb-Challenge` (base64url) is `"<unix ts> <M> <P> <sha256(B)
/// hex> <label>"` with P the URL's path and query, and `X-Lightbulb-Sig` is this computer's
/// signature over it (`login::proof`, purpose `tools`). The answer of a 2xx, else the error
/// result.
pub(crate) fn rest(
    method: &str,
    path: &str,
    body: Option<&Value>,
    label: &str,
) -> Result<Value, Value> {
    let dir = paths::state_path();
    let Some(proof) = login::stored_proof(&dir) else {
        return Err(json!({"error": "not_signed_in", "hint": "run lightbulb login"}));
    };
    if login::ended(&dir, crate::daemon::ipc::now() as i64) {
        return Err(json!({"error": "not_signed_in", "hint": login::SIGN_IN_AGAIN}));
    }
    // Checked first: a stale server's Keychain read fails (or asks the person to allow it).
    if replaced() {
        return Err(json!({"error": "identity_unavailable", "hint": REPLACED}));
    }
    let id = login::identity()
        .map_err(|e| json!({"error": "identity_unavailable", "hint": e.to_string()}))?;
    let body = body.map(Value::to_string).unwrap_or_default();
    let hash: String = Sha256::digest(body.as_bytes())
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect();
    let api = login::api();
    // The path as the server sees it: the API base's own path, then this call's.
    let base = api
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
        .unwrap_or("");
    let challenge = format!(
        "{} {method} {base}{path} {hash} {label}",
        crate::daemon::ipc::now()
    );
    let sig = login::proof(&id, "tools", &challenge, None)["sig"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let challenge = data_encoding::BASE64URL_NOPAD.encode(challenge.as_bytes());
    let headers = [
        ("X-Lightbulb-Proof", proof.as_str()),
        ("X-Lightbulb-Challenge", challenge.as_str()),
        ("X-Lightbulb-Sig", sig.as_str()),
    ];
    let body = (!body.is_empty()).then_some(body.as_str());
    match login::request(method, path, body, &headers) {
        Ok((code, v)) if (200..300).contains(&code) => Ok(v),
        Ok((code, v)) => Err(json!({"error": login::said(code, &v), "status": code})),
        Err(_) => Err(json!({"error": "unreachable"})),
    }
}

impl Server {
    /// `tool`, called with `args` for the session `meta` names.
    async fn call(&self, name: &str, args: Value, meta: RequestMetaObject) -> CallToolResult {
        let Some(tool) = spec().iter().find(|t| t["name"] == name) else {
            return code("unknown_tool");
        };
        let call = match self.source(meta).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        if tool["scope"] == "workspace" {
            return self.workspace(&call, tool, name, &args).await;
        }
        fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, CallToolResult> {
            serde_json::from_value(v).map_err(|_| code("bad_request"))
        }
        match name {
            "find_sessions" => match parse(args) {
                Ok(a) => self.find_sessions(call, a).await,
                Err(e) => e,
            },
            "send_message" => match parse(args) {
                Ok(a) => self.send_message(call, a).await,
                Err(e) => e,
            },
            "find_workers" => match parse(args) {
                Ok(a) => self.find_workers(call, a).await,
                Err(e) => e,
            },
            "assign" => match parse(args) {
                Ok(a) => self.assign(call, a).await,
                Err(e) => e,
            },
            "cancel_assignment" => match parse(args) {
                Ok(a) => self.cancel_assignment(call, a).await,
                Err(e) => e,
            },
            "recall" | "remember" => self.memory(call, name, args).await,
            "message_receipt" => match parse(args) {
                Ok(a) => self.message_receipt(call, a).await,
                Err(e) => e,
            },
            _ => match parse(args) {
                Ok(a) => self.check_inbox(call, a).await,
                Err(e) => e,
            },
        }
    }

    /// The calling session's own row.
    fn me(call: &Call) -> Option<&Session> {
        call.sessions.iter().find(|s| s.key == call.from)
    }

    /// The caller as Lightbulb is told: `Claude Code on mac`, from this engine's own records.
    fn label(call: &Call) -> String {
        let agent =
            Self::me(call).map_or("An agent".into(), |s| crate::frame::agent_name(&s.provider));
        let computer = crate::config::Config::load(&paths::state_path())
            .map(|c| c.computer)
            .unwrap_or_default();
        format!("{agent} on {computer}")
    }

    /// `rest`, off the async runtime: its answer, or the error result.
    async fn signed(
        call: &Call,
        method: &'static str,
        path: String,
        body: Option<Value>,
    ) -> Result<Value, CallToolResult> {
        let label = Self::label(call);
        tokio::task::spawn_blocking(move || rest(method, &path, body.as_ref(), &label))
            .await
            .unwrap_or_else(|e| Err(internal(e)))
            .map_err(fail)
    }

    /// A workspace tool: its one route.
    async fn workspace(&self, call: &Call, tool: &Value, name: &str, a: &Value) -> CallToolResult {
        let part = |k: &str| a[k].as_str().filter(|s| !s.is_empty()).map(enc);
        let (method, path, body) = match name {
            "find_rooms" => (
                "GET",
                format!("/local-sessions/device/rooms{}", query(props(tool, a, &[]))),
                None,
            ),
            "read_messages" => {
                let Some(room) = part("room_id") else {
                    return code("bad_request");
                };
                let q = query(props(tool, a, &["room_id"]));
                (
                    "GET",
                    format!("/local-sessions/device/rooms/{room}/messages{q}"),
                    None,
                )
            }
            "find_tasks" => (
                "GET",
                format!("/local-sessions/device/tasks{}", query(props(tool, a, &[]))),
                None,
            ),
            "read_task" | "update_task" => {
                let Some(task) = part("task") else {
                    return code("bad_request");
                };
                let rest = props(tool, a, &["task"]);
                if name == "read_task" {
                    (
                        "GET",
                        format!("/local-sessions/device/tasks/{task}{}", query(rest)),
                        None,
                    )
                } else {
                    let path = format!("/local-sessions/device/tasks/{task}");
                    ("PATCH", path, Some(Value::Object(rest)))
                }
            }
            _ => {
                // use_browser: the calling session's own Claude session id or Codex thread id.
                let Some(native) = Self::me(call).map(|s| s.conversation.clone()) else {
                    return code("source_unidentified");
                };
                let body = json!({"where": a["where"], "native_id": native});
                let path = "/local-sessions/device/sessions/browser-place".to_string();
                ("POST", path, Some(body))
            }
        };
        match Self::signed(call, method, path, body).await {
            Ok(v) => Self::done(call, v).await,
            Err(e) => e,
        }
    }

    /// `recall` and `remember`: the daemon's `memory.*` ops, `remember` as this session.
    async fn memory(&self, call: Call, name: &str, args: Value) -> CallToolResult {
        // Only what the tool declares: `when` and the page budget are the daemon's.
        let Some(tool) = spec().iter().find(|t| t["name"] == name) else {
            return code("unknown_tool");
        };
        let mut args = Value::Object(props(tool, &args, &[]));
        args["session"] = call.from.0.clone().into();
        // `recall`'s turns come from this repo's sessions only; an empty question (every fact,
        // the hook's view) carries no repo.
        let asked = name == "recall" && !args["question"].as_str().unwrap_or("").trim().is_empty();
        if asked {
            let cwd = std::env::current_dir().unwrap_or_default();
            if let (Some(repo), _) = git(&cwd.to_string_lossy()) {
                args["repo"] = repo.into();
            }
        }
        match ask(&format!("memory.{name}"), args).await {
            // A question's answer is the one page (up to about 9,000 tokens): the daemon's
            // `block` and its `facts` and `turns` arrays repeat it, so only their counts go.
            Ok(r) if r["ok"] == true && asked => {
                let count = |k: &str| r[k].as_array().map_or(0, Vec::len);
                let counts = json!({ "facts": count("facts"), "turns": count("turns") });
                Self::done(&call, json!({ "page": r["page"], "counts": counts })).await
            }
            Ok(mut r) if r["ok"] == true => {
                if let Some(o) = r.as_object_mut() {
                    o.remove("id");
                    o.remove("ok");
                }
                Self::done(&call, r).await
            }
            Ok(r) => code(r["error"].as_str().unwrap_or("internal")),
            Err(e) => fail(internal(e)),
        }
    }

    /// The coding sessions you can message: this computer's, then your teammates'.
    async fn find_sessions(&self, call: Call, a: FindArgs) -> CallToolResult {
        let r = match ask("directory", json!({ "query": a.query })).await {
            Ok(r) => r,
            Err(e) => return fail(internal(e)),
        };
        let mut found: Vec<Value> = match r["sessions"].clone() {
            Value::Array(v) => v,
            _ => vec![],
        };
        for r in &mut found {
            r["is_this_session"] = (r["session_id"] == call.from.0.as_str()).into();
            // `who`'s name for a session on this computer (`claude-5`), as `name`.
            if let Some(o) = r.as_object_mut() {
                match o.remove("handle") {
                    Some(Value::String(h)) if !h.is_empty() => {
                        o.insert("name".into(), h.into());
                    }
                    _ => {}
                }
            }
        }
        let total = found.len();
        let sessions: Vec<Value> = found.into_iter().take(a.limit as usize).collect();
        Self::done(&call, json!({ "sessions": sessions, "total": total })).await
    }

    /// Sends to exactly one target: a session peer to peer (or a reply to whoever sent
    /// `reply_to`), or a room or person through Lightbulb.
    async fn send_message(&self, call: Call, a: SendArgs) -> CallToolResult {
        let targets = [&a.session_id, &a.room_id, &a.person_id]
            .iter()
            .filter(|t| t.is_some())
            .count();
        let workspace = a.room_id.is_some() || a.person_id.is_some();
        let threaded = a.reply_to_message_id.is_some() || a.mention_ids.is_some();
        if targets > 1
            || (workspace && a.reply_to.is_some())
            || (!workspace && threaded)
            || (targets == 0 && a.reply_to.is_none())
        {
            return code("bad_request");
        }
        if workspace {
            return self.room_send(call, a).await;
        }
        if a.session_id
            .as_deref()
            .is_some_and(|t| t == "*" || t.ends_with("/*"))
        {
            return code("group_send_not_allowed");
        }
        let r = ask(
            "send",
            json!({ "from": call.from, "to": a.session_id, "text": a.text,
                "request_id": a.request_id, "reply_to": a.reply_to, "wait": true,
                "kind": a.status.is_some().then_some("result"), "status": a.status }),
        )
        .await;
        Self::sent(&call, r).await
    }

    /// The tool result for the daemon's answer to a `send`.
    async fn sent(call: &Call, r: Result<Value, impl std::fmt::Display>) -> CallToolResult {
        match r {
            Ok(r) if r["ok"] == true => Self::done(call, with_note(r["receipt"].clone())).await,
            Ok(r) => refused(r),
            Err(e) => fail(internal(e)),
        }
    }

    /// Hands a job to one agent; the daemon checks the deadline and the rest.
    async fn assign(&self, call: Call, a: AssignArgs) -> CallToolResult {
        let r = ask(
            "send",
            json!({ "from": call.from, "to": a.to, "text": a.brief, "request_id": a.request_id,
                "kind": "assign", "done_when": a.done_when, "deadline_s": a.deadline_s,
                "repo": a.repo, "branch": a.branch, "wait": true }),
        )
        .await;
        Self::sent(&call, r).await
    }

    /// Cancels an assignment of the caller's: the daemon addresses its target.
    async fn cancel_assignment(&self, call: Call, a: CancelArgs) -> CallToolResult {
        let r = ask(
            "send",
            json!({ "from": call.from, "reply_to": a.assignment,
                "text": a.note.unwrap_or_else(|| "cancelled".into()),
                "request_id": a.request_id, "kind": "cancel", "wait": true }),
        )
        .await;
        Self::sent(&call, r).await
    }

    /// The always-on agents: online sessions with a name address (not a terminal).
    async fn find_workers(&self, call: Call, a: FindArgs) -> CallToolResult {
        let short = match ask("status", json!({})).await {
            Ok(s) => s["member_key_short"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            Err(e) => return fail(internal(e)),
        };
        let r = match ask("directory", json!({ "query": a.query })).await {
            Ok(r) => r,
            Err(e) => return fail(internal(e)),
        };
        let found = always_on(
            &short,
            r["sessions"].as_array().cloned().unwrap_or_default(),
        );
        let total = found.len();
        let sessions: Vec<Value> = found.into_iter().take(a.limit as usize).collect();
        Self::done(&call, json!({ "sessions": sessions, "total": total })).await
    }

    /// A room or person send: the person's DM opened first when needed (Lightbulb finds an
    /// existing one), the room remembered for the receipt, then the post.
    async fn room_send(&self, call: Call, a: SendArgs) -> CallToolResult {
        if !crate::sessions::word(&a.request_id, 128) {
            return code("bad_request");
        }
        let room = match (a.room_id, a.person_id) {
            (Some(room), _) => room,
            (_, Some(person)) => {
                let kind = kind_of(&person);
                let member = json!({"kind": kind, "id": person, "role": "member",
                    "listenMode": (kind == "agent").then_some("active")});
                let body = json!({"clientRoomId": uuid_of("dm", &call.from, &a.request_id),
                    "name": null, "kind": "dm", "members": [member]});
                match Self::signed(&call, "POST", "/messaging/rooms".into(), Some(body)).await {
                    Ok(v) => match v["roomId"].as_str() {
                        Some(r) => r.to_string(),
                        None => return fail(internal("Lightbulb answered without a roomId.")),
                    },
                    Err(e) => return e,
                }
            }
            _ => unreachable!("a workspace send has a room or a person"),
        };
        if room.is_empty() {
            return code("bad_request");
        }
        let record = room_record(&call.from, &a.request_id);
        let written = std::fs::create_dir_all(record.parent().expect("in room-sends"))
            .and_then(|()| std::fs::write(&record, &room));
        if let Err(e) = written {
            return fail(internal(e));
        }
        let mentions: Vec<Value> = (a.mention_ids.unwrap_or_default().iter())
            .map(|id| json!({"kind": kind_of(id), "id": id}))
            .collect();
        let body = json!({"clientMessageId": uuid_of("message", &call.from, &a.request_id),
            "text": a.text, "mentions": mentions, "replyToMessageId": a.reply_to_message_id,
            "threadRootMessageId": null});
        let path = format!("/messaging/rooms/{}/messages", enc(&room));
        match Self::signed(&call, "POST", path, Some(body)).await {
            Ok(m) => {
                let posted = json!({"roomId": m["roomId"], "messageId": m["messageId"],
                    "messageSeq": m["messageSeq"], "state": "posted"});
                Self::done(&call, posted).await
            }
            Err(e) => e,
        }
    }

    /// The receipt of a message this session sent: a room send's from Lightbulb, else the
    /// network's.
    async fn message_receipt(&self, call: Call, a: ReceiptArgs) -> CallToolResult {
        if let Ok(room) = std::fs::read_to_string(room_record(&call.from, &a.request_id)) {
            let q = format!(
                "?request_id={}&room_id={}",
                uuid_of("message", &call.from, &a.request_id),
                enc(&room)
            );
            let path = format!("/local-sessions/device/receipt{q}");
            return match Self::signed(&call, "GET", path, None).await {
                Ok(v) => Self::done(&call, v).await,
                Err(e) => e,
            };
        }
        match ask(
            "receipt",
            json!({ "from": call.from, "request_id": a.request_id }),
        )
        .await
        {
            Ok(r) if r["ok"] == true => Self::done(&call, with_note(r["receipt"].clone())).await,
            Ok(r) => refused(r),
            Err(e) => fail(internal(e)),
        }
    }

    /// Takes the messages waiting for this session, first waiting up to wait_secs (at most 30
    /// per call) for one.
    async fn check_inbox(&self, call: Call, a: InboxArgs) -> CallToolResult {
        if a.wait_secs > 600 {
            return code("bad_request");
        }
        let member = crate::sessions::member_of(&call.from.0).unwrap_or_default();
        let cap = match crate::frame::Sender::of(member, &call.from.0) {
            crate::frame::Sender::Agent("codex") => CODEX_WAIT_CAP_S,
            _ => WAIT_CAP_S,
        };
        let r = ask(
            "inbox",
            json!({ "key": call.from, "wait_secs": a.wait_secs.min(cap),
                "caller": hook::proc_json(&call.me) }),
        )
        .await;
        match r {
            Ok(r) if r["ok"] == true => {
                let messages: Vec<Value> = r["messages"]
                    .as_array()
                    .map(|m| m.as_slice())
                    .unwrap_or_default()
                    .iter()
                    .map(crate::daemon::ipc::inbox_record)
                    .collect();
                Self::done(&call, json!({ "messages": messages })).await
            }
            Ok(r) => refused(r),
            Err(e) => fail(internal(e)),
        }
    }
}

impl ServerHandler for Server {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            &self.name,
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(INSTRUCTIONS.to_string())
    }

    /// The network tools, and the workspace tools while this computer is signed in.
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let workspace = signed_in();
        self.signed.store(workspace, Ordering::SeqCst);
        let tools: Vec<Value> = spec()
            .iter()
            .filter(|t| workspace || t["scope"] == "network")
            .map(|t| {
                json!({"name": t["name"], "description": t["description"],
                    "inputSchema": t["inputSchema"]})
            })
            .collect();
        serde_json::from_value(json!({ "tools": tools }))
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        changed(&self.signed, &context.peer).await;
        let args = Value::Object(request.arguments.unwrap_or_default());
        Ok(self.call(&request.name, args, context.meta).await.into())
    }
}

const INSTRUCTIONS: &str = "These tools message the other coding sessions (Claude Code, Codex, Cursor, Grok CLI, OpenCode). Use these tools for sessions on this computer and on teammates' computers. Call find_sessions to see them, then send_message to one by its session_id. Every send_message needs a request_id: a new [A-Za-z0-9._-]{1,128} string, reused only to retry that same send. Messages delivered to you show a requestId; pass it as reply_to to answer one. While this computer is signed in to Lightbulb (lightbulb login), the workspace tools also read and post in your workspace's rooms and tasks. The lightbulb skill explains invites and etiquette.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn always_on_keeps_online_name_rows_only() {
        let rows = vec![
            json!({"session_id": "abc/friday", "online": true}),
            json!({"session_id": "abc/claude:1", "online": true}),
            json!({"session_id": "abc/sleepy", "online": false}),
            json!({"session_id": "abc/terminal", "online": true}),
        ];
        assert_eq!(
            always_on("abc", rows),
            vec![json!({"session_id": "abc/friday", "online": true, "always_on": true})]
        );
    }

    #[test]
    fn git_reads_origin_repo_and_branch_also_from_a_linked_worktree() {
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("main");
        fs::create_dir_all(main.join(".git/worktrees/x")).unwrap();
        fs::write(main.join(".git/HEAD"), "ref: refs/heads/init\n").unwrap();
        fs::write(
            main.join(".git/config"),
            "[remote \"upstream\"]\n\turl = x/other.git\n[remote \"origin\"]\n\turl = git@github.com:Org/cli.git\n",
        )
        .unwrap();
        let name = Some("org/cli".to_string());
        assert_eq!(
            git(main.to_str().unwrap()),
            (name.clone(), Some("init".into()))
        );
        assert_eq!(git(d.path().to_str().unwrap()), (None, None));

        // A linked worktree elsewhere, laid out as `git worktree add` writes it.
        let wt = d.path().join("elsewhere/x");
        fs::create_dir_all(&wt).unwrap();
        let gitdir = main.join(".git/worktrees/x");
        fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        fs::write(gitdir.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();
        fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        assert_eq!(git(wt.to_str().unwrap()), (name, Some("feat/x".into())));

        // A symlinked .git is not followed.
        let linked = d.path().join("linked");
        fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(main.join(".git"), linked.join(".git")).unwrap();
        assert_eq!(git(linked.to_str().unwrap()), (None, None));
    }

    #[test]
    fn the_repo_key_is_lowercase_owner_and_name() {
        for (url, key) in [
            ("https://github.com/acme/api.git", Some("acme/api")),
            ("https://github.com/acme/api", Some("acme/api")),
            ("https://github.com/acme/api/", Some("acme/api")),
            ("git@github.com:acme/api.git", Some("acme/api")),
            ("ssh://git@host:2222/acme/api.git", Some("acme/api")),
            ("ssh://host:2222/name", Some("name")),
            ("git@host:name.git", Some("name")),
            ("https://GitHub.com/Acme/API.git", Some("acme/api")),
            ("", None),
        ] {
            assert_eq!(repo_key(url).as_deref(), key, "{url}");
        }
    }

    #[test]
    fn a_fifo_named_head_does_not_block() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".git")).unwrap();
        let head = std::ffi::CString::new(d.path().join(".git/HEAD").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(head.as_ptr(), 0o600) }, 0);
        let cwd = d.path().to_str().unwrap().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(git(&cwd)).unwrap());
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("git() blocked on a FIFO");
        assert_eq!(got, (None, None));
    }
}
