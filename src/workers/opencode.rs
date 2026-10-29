//! The OpenCode driver: `opencode serve` on a free loopback port behind a per-worker password,
//! spoken over HTTP/1.1 and its server-sent event stream (`http`, no client dependency): a
//! session in the folder, one message as the turn; its permission requests relayed to the
//! person as holds, always answered one-time.

use super::driver::{
    failed, find, uncommented, Decision, Driver, Outcome, PermissionRequest, Relay, Run, INIT_WAIT,
};
use crate::frame;
use crate::store::Store;
use http::Server;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};

/// The start's failure when no OpenCode is found (`program`).
pub const NOT_INSTALLED: &str = "OpenCode is not installed where the daemon can find it";
/// The start's failure when an OpenCode config it reads could allow without asking
/// (`bypass_mode`).
pub const BYPASS_MODE: &str = "this computer's OpenCode can run without approvals; a worker \
     needs prompts to reach a person (any \"allow\" under permission, agent.<name>.permission \
     or mode.<name>.permission, or a tool set true under tools, in config.json, opencode.json \
     or opencode.jsonc in ~/.config/opencode, $XDG_CONFIG_HOME/opencode or ~/.opencode; an \
     agent or mode file there whose header allows; or one it cannot read)";
/// What the turn ends with when OpenCode raises a prompt in a shape this host cannot relay (a
/// `permission.*` event other than `asked` and `replied`), so it can never be allowed.
pub const ASKS_V2: &str = "this OpenCode asks in a way the worker host does not read yet";

/// Where installers put OpenCode outside a home.
const SYSTEM: [&str; 2] = ["/opt/homebrew/bin/opencode", "/usr/local/bin/opencode"];
/// The person's own config folders, under a home (`$XDG_CONFIG_HOME/opencode` too when set).
const USER: [&str; 2] = [".config/opencode", ".opencode"];
/// The configs read in each of them.
const USER_FILES: [&str; 3] = ["config.json", "opencode.json", "opencode.jsonc"];
/// Where agents and modes are defined in each of them, as Markdown with a YAML header.
const AGENT_DIRS: [&str; 4] = ["agent", "agents", "mode", "modes"];
/// A folder's own configs.
const FOLDER: [&str; 4] = [
    "opencode.json",
    "opencode.jsonc",
    ".opencode/opencode.json",
    ".opencode/opencode.jsonc",
];
/// Variables that would hand OpenCode a config other than the files checked here: removed from
/// the child's environment.
const CONFIG_ENV: [&str; 3] = [
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_CONFIG_DIR",
];
/// What the child is told, over anything inherited: every permission asks (OpenCode's own
/// default is `{"*": "allow"}`; this is deep-merged over the person's files, whose own allows
/// `bypass_mode` refuses since a specific key of theirs would still win), and no config from
/// the worker's folder.
const FORCED_ENV: [(&str, &str); 2] = [
    ("OPENCODE_PERMISSION", r#"{"*":"ask"}"#),
    ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
];
/// The server's Basic-auth user, set in the child too so an inherited
/// `OPENCODE_SERVER_USERNAME` cannot change it.
const USER_NAME: &str = "opencode";

/// An OpenCode at `program`, served on `port` (chosen by `argv`) behind `password`.
pub struct OpenCode {
    program: PathBuf,
    /// 32 random bytes, hex: in the child's environment only, never logged.
    password: String,
    port: OnceLock<u16>,
}

impl OpenCode {
    pub fn new(program: PathBuf) -> OpenCode {
        OpenCode {
            program,
            password: data_encoding::HEXLOWER.encode(&rand::random::<[u8; 32]>()),
            port: OnceLock::new(),
        }
    }
}

/// The OpenCode a worker runs here, unless none is found (`NOT_INSTALLED`) or the person's own
/// config allows without asking (`BYPASS_MODE`). `home` holds `.config/opencode/`.
pub fn here(knob: Option<&Path>, home: &Path, store: &Store) -> Result<OpenCode, &'static str> {
    let path = std::env::var_os("PATH");
    let system = SYSTEM.map(Path::new);
    let Some(program) = program(knob, home, &system, store, path.as_deref()) else {
        return Err(NOT_INSTALLED);
    };
    let xdg = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty());
    if bypass_mode(home, xdg.as_deref().map(Path::new)) {
        return Err(BYPASS_MODE);
    }
    Ok(OpenCode::new(program))
}

/// The OpenCode to run, the first executable file of: the debug knob (when set, the only one);
/// `system`; `~/.local/bin/opencode`; `~/.opencode/bin/opencode`; the newest registered OpenCode
/// session's executable named `opencode`; `opencode` on `path`.
pub fn program(
    knob: Option<&Path>,
    home: &Path,
    system: &[&Path],
    store: &Store,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    let known = system
        .iter()
        .map(|p| p.to_path_buf())
        .chain([
            home.join(".local/bin/opencode"),
            home.join(".opencode/bin/opencode"),
        ])
        .collect();
    find(knob, known, store, "opencode", &["opencode"], path)
}

/// Whether the person's OpenCode config could allow without asking: a config file (`bypass`)
/// or an agent or mode file (`agents_allow`) in a folder of `USER` under `home`, or in
/// `xdg/opencode` (`$XDG_CONFIG_HOME`) when set.
pub fn bypass_mode(home: &Path, xdg: Option<&Path>) -> bool {
    let dirs: Vec<PathBuf> = USER
        .iter()
        .map(|d| home.join(d))
        .chain(xdg.map(|x| x.join("opencode")))
        .collect();
    dirs.iter().any(|d| {
        USER_FILES.iter().any(|f| bypass(&d.join(f)))
            || AGENT_DIRS.iter().any(|a| agents_allow(&d.join(a), 0))
    })
}

/// Whether a Markdown agent or mode file under `dir` (to 8 levels) could allow: its YAML
/// header says `allow` anywhere or `true` in its `tools`, or it (or a folder) cannot be read.
/// A missing folder is clean.
// ponytail: a text scan of the header, not a YAML parse; a false match refuses, never allows.
fn agents_allow(dir: &Path, depth: u8) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
        Ok(e) => e,
    };
    entries.into_iter().any(|e| {
        let Ok(e) = e else { return true };
        let p = e.path();
        if p.is_dir() {
            return depth >= 8 || agents_allow(&p, depth + 1);
        }
        if p.extension().is_none_or(|x| x != "md") {
            return false;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            return true;
        };
        let Some(header) = text.strip_prefix("---") else {
            return false;
        };
        let header = header.split("\n---").next().unwrap_or_default();
        let mut tools = false;
        header.contains("allow")
            || header.lines().any(|l| {
                if !l.starts_with([' ', '\t']) {
                    tools = l.starts_with("tools");
                }
                tools && l.contains("true")
            })
    })
}

/// Why a worker may not run in `folder`: an `opencode.json[c]` of its own (at its top or under
/// `.opencode/`) that allows without asking.
pub fn folder_bypass(folder: &Path) -> Option<String> {
    let found = FOLDER.iter().map(|f| folder.join(f)).find(|p| bypass(p))?;
    Some(format!("{BYPASS_MODE}: {}", found.display()))
}

/// Whether the OpenCode config at `path` could allow without asking (`allows`). Only a missing
/// file is clean: one that cannot be read, is not UTF-8, or does not parse (comments and
/// trailing commas allowed) refuses.
fn bypass(path: &Path) -> bool {
    let text = match std::fs::read_to_string(path) {
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
        Ok(t) => t,
    };
    match serde_json::from_str::<Value>(&uncommented(&text)) {
        Ok(d) => allows(&d),
        Err(_) => true,
    }
}

/// Whether config `d` could allow without asking, over the forced `{"*": "ask"}` (a key of its
/// own, or an agent's rules, are evaluated after it): any `"allow"` under `permission` or an
/// agent's or mode's `permission`, or any `true` under `tools` or theirs (a tool on is allow).
fn allows(d: &Value) -> bool {
    fn any(v: &Value, leaf: fn(&Value) -> bool) -> bool {
        match v {
            Value::Object(m) => m.values().any(|v| any(v, leaf)),
            Value::Array(a) => a.iter().any(|v| any(v, leaf)),
            v => leaf(v),
        }
    }
    let agents = ["agent", "mode"]
        .iter()
        .flat_map(|k| d[k].as_object().into_iter().flat_map(|m| m.values()));
    [d].into_iter()
        .chain(agents)
        .any(|a| any(&a["permission"], |v| *v == "allow") || any(&a["tools"], |v| *v == true))
}

/// A free loopback port: bound, read, and let go for the server to take.
fn free_port() -> Option<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    l.local_addr().ok().map(|a| a.port())
}

impl Driver for OpenCode {
    /// Exactly `[program, "serve", "--hostname", "127.0.0.1", "--port", <port>]`; empty (which
    /// `spawn` refuses) when no port is free.
    fn argv(&self, _folder: &Path, _session_id: &str) -> Vec<String> {
        let port = *self.port.get_or_init(|| free_port().unwrap_or(0));
        if port == 0 {
            return vec![];
        }
        let program = self.program.to_string_lossy().into_owned();
        [
            program.as_str(),
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
        ]
        .into_iter()
        .map(String::from)
        .chain([port.to_string()])
        .collect()
    }

    fn refuses(&self, folder: &Path) -> Option<String> {
        folder_bypass(folder)
    }

    fn env(&self) -> Vec<(&'static str, Option<String>)> {
        let removed = CONFIG_ENV.map(|k| (k, None));
        let forced = FORCED_ENV.map(|(k, v)| (k, Some(v.to_string())));
        [
            ("OPENCODE_SERVER_USERNAME", Some(USER_NAME.into())),
            ("OPENCODE_SERVER_PASSWORD", Some(self.password.clone())),
        ]
        .into_iter()
        .chain(forced)
        .chain(removed)
        .collect()
    }

    fn run(self: Box<Self>, child: Child, first_prompt: String, relay: Relay) -> Run {
        let port = self.port.get().copied().unwrap_or_default();
        let server = Server::new(port, USER_NAME, &self.password);
        Box::pin(serve(child, port, server, first_prompt, relay))
    }
}

/// Runs the turn on `child`'s server (`talk`); then its process group is told to end, and what
/// is left of it is killed after 5 s (`kill_on_drop`, `end_group`). Its stdout is read (for
/// its listening line on `port`) to the end, its stdin held open.
async fn serve(
    mut child: Child,
    port: u16,
    server: Server,
    prompt: String,
    relay: Relay,
) -> Outcome {
    let stdin = child.stdin.take();
    let (heard, listening) = oneshot::channel();
    if let Some(out) = child.stdout.take() {
        let want = format!("listening on http://127.0.0.1:{port}");
        tokio::spawn(async move {
            let (mut lines, mut heard) = (BufReader::new(out).lines(), Some(heard));
            while let Ok(Some(l)) = lines.next_line().await {
                if l.contains(&want) {
                    if let Some(h) = heard.take() {
                        let _ = h.send(());
                    }
                }
            }
        });
    }
    let outcome = talk(&mut child, listening, &server, prompt, relay).await;
    drop(stdin);
    let group = child.id();
    if let Some(pid) = group.filter(|&p| p > 0) {
        // SAFETY: a plain syscall on the server's own process group (`spawn` made it leader).
        unsafe { libc::killpg(pid as i32, libc::SIGTERM) };
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    super::driver::end_group(group);
    outcome
}

/// Whether `id` may name a session (`<m>/opencode:<id>`, a shell argument) or a request (a path
/// segment).
fn valid_id(id: &str) -> bool {
    crate::sessions::word(id, 128) && id != "new"
}

/// Waits (30 s in all) for the server to say it listens (`listening`; its stdout closing first
/// means it exited), then to answer `/global/health` with `healthy: true`.
async fn ready(
    child: &mut Child,
    listening: oneshot::Receiver<()>,
    server: &Server,
) -> Result<(), &'static str> {
    let until = tokio::time::Instant::now() + INIT_WAIT;
    match tokio::time::timeout_at(until, listening).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => return Err("the worker's OpenCode exited before it served"),
        Err(_) => return Err("the worker's OpenCode did not start within 30 s"),
    }
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return Err("the worker's OpenCode exited before it served");
        }
        let health = server.request("GET", "/global/health", None);
        match tokio::time::timeout(Duration::from_secs(1), health).await {
            Ok(Ok((200, v))) if v["healthy"] == true => return Ok(()),
            Ok(Ok((401, _))) => return Err("the worker's OpenCode refused its password"),
            _ => {}
        }
        if tokio::time::Instant::now() >= until {
            return Err("the worker's OpenCode did not start within 30 s");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A permission request's tool (`permission`, else `type`, else `tool`) and what Allow grants:
/// its title, its permission, and the patterns it would act on. Previewed long.
fn asked(p: &Value) -> (String, String) {
    let text = |v: &Value| {
        match v {
            Value::Array(a) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            v => v.as_str().map(String::from),
        }
        .filter(|t| !t.is_empty())
    };
    let tool = text(&p["permission"]).or_else(|| text(&p["type"]));
    let parts: Vec<String> = [
        text(&p["title"]).or_else(|| text(&p["metadata"]["title"])),
        tool.clone(),
        text(&p["patterns"]).or_else(|| text(&p["pattern"])),
    ]
    .into_iter()
    .flatten()
    .collect();
    let on = if parts.is_empty() {
        "permission.asked".to_string()
    } else {
        parts.join("; ")
    };
    (
        tool.unwrap_or_else(|| "tool".into()),
        frame::preview_long(&on),
    )
}

/// The turn's end from the message response: done with its text parts, or failed with its
/// `info.error`.
fn finished(v: &Value) -> Outcome {
    let e = &v["info"]["error"];
    if !e.is_null() {
        let why = e["data"]["message"].as_str().or(e["name"].as_str());
        return failed(why.unwrap_or("the worker's OpenCode failed the turn"));
    }
    let text: Vec<&str> = v["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect();
    Outcome {
        status: "done",
        reason: None,
        body: text.join("\n"),
    }
}

/// Failed with what the worker said so far (`last`), else `why`.
fn fail(last: &str, why: &str) -> Outcome {
    failed(if last.is_empty() { why } else { last })
}

/// Speaks to the server: ready, a session (its id to `relay.session`), the event stream, then
/// the message with `prompt`; each permission request of the session or a subagent's session
/// under it relayed (answered when the person answers, withdrawn when the server says it was
/// answered) until the message's response, an error, or the event stream ends.
async fn talk(
    child: &mut Child,
    listening: oneshot::Receiver<()>,
    server: &Server,
    prompt: String,
    relay: Relay,
) -> Outcome {
    if let Err(why) = ready(child, listening, server).await {
        return failed(why);
    }
    let title = json!({"title": "Lightbulb worker"});
    let session = match server.request("POST", "/session", Some(&title)).await {
        Ok((200, v)) => v["id"].as_str().unwrap_or_default().to_string(),
        Ok((status, _)) => {
            return failed(format!(
                "the worker's OpenCode refused a session: HTTP {status}"
            ))
        }
        Err(_) => return failed("the worker's OpenCode stopped answering"),
    };
    if !valid_id(&session) {
        return failed("the worker's OpenCode named no valid session");
    }
    let _ = relay.session.send(session.clone());
    let (mut events, reader) = match server.events().await {
        Ok(e) => e,
        Err(e) => return failed(format!("the worker's OpenCode sent no events: {e}")),
    };
    let message = {
        let (s, path) = (server.clone(), format!("/session/{session}/message"));
        let body = json!({"parts": [{"type": "text", "text": prompt}]});
        tokio::spawn(async move { s.request("POST", &path, Some(&body)).await })
    };
    tokio::pin!(message);
    // Answers to send, from the tasks waiting on the person.
    let (answers, mut answered) = mpsc::unbounded_channel::<(String, Decision)>();
    let mut waiting: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // The status of each answer sent.
    let (statuses, mut replied) = mpsc::unbounded_channel::<u16>();
    // The session and its subagents' sessions (a parent among them), as OpenCode's own `run`
    // follows them.
    let mut ours: HashSet<String> = HashSet::from([session.clone()]);
    // The assistant's messages, and its latest text part (id, text) so far.
    let mut assistant: HashSet<String> = HashSet::new();
    let (mut part, mut last) = (String::new(), String::new());
    let outcome = loop {
        let e = tokio::select! {
            r = &mut message => break match r {
                Ok(Ok((200, v))) => finished(&v),
                Ok(Ok((status, _))) => {
                    fail(&last, &format!("the worker's OpenCode refused the turn: HTTP {status}"))
                }
                _ => fail(&last, "the worker's OpenCode stopped answering"),
            },
            Some((id, d)) = answered.recv() => {
                waiting.remove(&id);
                let reply = match d {
                    Decision::Allow => json!({"reply": "once"}),
                    Decision::Deny(why) => json!({"reply": "reject", "message": why}),
                };
                let (s, statuses) = (server.clone(), statuses.clone());
                tokio::spawn(async move {
                    let path = format!("/permission/{id}/reply");
                    if let Ok((status, _)) = s.request("POST", &path, Some(&reply)).await {
                        let _ = statuses.send(status);
                    }
                });
                continue;
            }
            // A 404: the request was already answered or gone (`permission.replied`).
            Some(status) = replied.recv() => {
                if status != 200 && status != 404 {
                    let why = format!("the worker's OpenCode refused an answer: HTTP {status}");
                    break fail(&last, &why);
                }
                continue;
            }
            e = events.recv() => e,
        };
        let Some(e) = e else {
            break fail(&last, "the worker's OpenCode event stream ended");
        };
        let kind = e["type"].as_str().unwrap_or_default();
        // Prompts in any other shape (`permission.v2.*`, `permission.updated`) are not relayed:
        // unanswerable, so the turn ends rather than waits.
        let read = ["permission.asked", "permission.replied"];
        if kind.starts_with("permission.") && !read.contains(&kind) {
            break failed(ASKS_V2);
        }
        let p = &e["properties"];
        if matches!(kind, "session.created" | "session.updated") {
            let info = &p["info"];
            let parent = info["parentID"].as_str().is_some_and(|i| ours.contains(i));
            if let Some(child) = info["id"].as_str().filter(|_| parent) {
                ours.insert(child.into());
            }
            continue;
        }
        let Some(from) = p["sessionID"].as_str().filter(|s| ours.contains(*s)) else {
            continue;
        };
        // The worker's own words are its session's, never a subagent's.
        let root = from == session;
        let id = p["id"].as_str().filter(|i| valid_id(i)).map(String::from);
        match kind {
            "permission.asked" => {
                let Some(id) = id else { continue };
                let (tool, summary) = asked(p);
                let (tx, rx) = oneshot::channel();
                let ask = PermissionRequest {
                    tool,
                    summary,
                    reply: tx,
                };
                if relay.permission.send(ask).await.is_err() {
                    let no = Decision::Deny("the person could not be asked".into());
                    let _ = answers.send((id, no));
                    continue;
                }
                let (answers, key) = (answers.clone(), id.clone());
                let wait = tokio::spawn(async move {
                    let no = || Decision::Deny("the person could not be asked".into());
                    let _ = answers.send((key, rx.await.unwrap_or_else(|_| no())));
                });
                if let Some(old) = waiting.insert(id, wait) {
                    old.abort();
                }
            }
            "permission.replied" => {
                let pending = p["requestID"].as_str().and_then(|r| waiting.remove(r));
                if let Some(w) = pending {
                    w.abort();
                }
            }
            "question.asked" => {
                let Some(id) = id else { continue };
                let s = server.clone();
                tokio::spawn(async move {
                    let _ = s
                        .request("POST", &format!("/question/{id}/reject"), None)
                        .await;
                });
            }
            "message.updated" if root && p["info"]["role"] == "assistant" => {
                if let Some(m) = p["info"]["id"].as_str() {
                    assistant.insert(m.into());
                }
            }
            "message.part.updated" if root => {
                let t = &p["part"];
                let mine = t["messageID"]
                    .as_str()
                    .is_some_and(|m| assistant.contains(m));
                if mine && t["type"] == "text" && t["synthetic"] != true {
                    part = t["id"].as_str().unwrap_or_default().into();
                    last = t["text"].as_str().unwrap_or_default().into();
                }
            }
            "message.part.delta" if root && p["field"] == "text" => {
                let mine = p["messageID"]
                    .as_str()
                    .is_some_and(|m| assistant.contains(m));
                if mine {
                    if p["partID"] != part.as_str() {
                        part = p["partID"].as_str().unwrap_or_default().into();
                        last.clear();
                    }
                    last.push_str(p["delta"].as_str().unwrap_or_default());
                }
            }
            _ => {}
        }
    };
    message.abort();
    reader.abort();
    for (_, w) in waiting {
        w.abort();
    }
    outcome
}

/// A minimal HTTP/1.1 client for the worker's own loopback server: one request per connection,
/// Basic auth, a `Content-Length` or chunked body, and the server-sent event stream.
mod http {
    use serde_json::Value;
    use std::io::{Error, ErrorKind, Result};
    use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    /// The longest response body read whole.
    const MAX_BODY: usize = 16 << 20;
    /// The longest event (or line) of the event stream.
    pub const MAX_EVENT: usize = 1 << 20;
    /// The longest status or header line.
    const MAX_HEAD_LINE: usize = 8 << 10;
    /// The most a body piece reads at once.
    const PIECE: u64 = 64 << 10;

    fn invalid(why: impl Into<String>) -> Error {
        Error::new(ErrorKind::InvalidData, why.into())
    }

    /// The server on `127.0.0.1:port` and its `Authorization: Basic` credential.
    #[derive(Clone)]
    pub struct Server {
        port: u16,
        auth: String,
    }

    impl Server {
        pub fn new(port: u16, user: &str, password: &str) -> Server {
            let auth = data_encoding::BASE64.encode(format!("{user}:{password}").as_bytes());
            Server { port, auth }
        }

        /// Sends `method path` (with `body` as JSON) and reads the response's head.
        async fn send(
            &self,
            method: &str,
            path: &str,
            body: Option<&Value>,
            accept: &str,
        ) -> Result<(BufReader<TcpStream>, Head)> {
            let mut s = TcpStream::connect(("127.0.0.1", self.port)).await?;
            let body = body.map(Value::to_string).unwrap_or_default();
            let req = format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Basic {}\r\n\
                 Accept: {accept}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                self.port,
                self.auth,
                body.len()
            );
            s.write_all(req.as_bytes()).await?;
            let mut r = BufReader::new(s);
            let head = head(&mut r).await?;
            Ok((r, head))
        }

        /// `method path` with `body`: the response's status and its JSON (`null` when empty or
        /// not JSON).
        pub async fn request(
            &self,
            method: &str,
            path: &str,
            body: Option<&Value>,
        ) -> Result<(u16, Value)> {
            let (mut r, mut head) = self.send(method, path, body, "application/json").await?;
            let mut all = vec![];
            loop {
                let p = piece(&mut r, &mut head).await?;
                if p.is_empty() {
                    break;
                }
                all.extend(p);
                if all.len() > MAX_BODY {
                    return Err(invalid("a response over 16 MiB"));
                }
            }
            Ok((
                head.status,
                serde_json::from_slice(&all).unwrap_or_default(),
            ))
        }

        /// The event stream (`GET /event`): each event's JSON, on a task (aborted by the
        /// caller); the receiver closes when the stream ends or breaks.
        pub async fn events(&self) -> Result<(mpsc::Receiver<Value>, JoinHandle<()>)> {
            let (mut r, mut head) = self
                .send("GET", "/event", None, "text/event-stream")
                .await?;
            if head.status != 200 {
                return Err(invalid(format!("HTTP {}", head.status)));
            }
            let (tx, rx) = mpsc::channel(64);
            let task = tokio::spawn(async move {
                let _ = sse(&mut r, &mut head, &tx).await;
            });
            Ok((rx, task))
        }
    }

    /// A response's status, and how much of its body is left: chunked, `left` bytes, or to EOF.
    pub struct Head {
        pub status: u16,
        chunked: bool,
        left: Option<u64>,
    }

    /// One line, without its `\r\n`; an error at EOF or past `max` bytes.
    async fn line<R: AsyncBufRead + Unpin>(r: &mut R, max: usize) -> Result<Vec<u8>> {
        let mut buf = vec![];
        (&mut *r)
            .take(max as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await?;
        if buf.len() > max {
            return Err(invalid("a line too long"));
        }
        if buf.last() != Some(&b'\n') {
            return Err(ErrorKind::UnexpectedEof.into());
        }
        while buf.last().is_some_and(|b| b"\r\n".contains(b)) {
            buf.pop();
        }
        Ok(buf)
    }

    /// The status line and headers.
    pub async fn head<R: AsyncBufRead + Unpin>(r: &mut R) -> Result<Head> {
        let first = line(r, MAX_HEAD_LINE).await?;
        let status = std::str::from_utf8(&first)
            .ok()
            .and_then(|l| l.strip_prefix("HTTP/1."))
            .and_then(|l| l.get(2..5))
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| invalid("not an HTTP response"))?;
        let (mut chunked, mut left) = (false, None);
        for _ in 0..100 {
            let l = line(r, MAX_HEAD_LINE).await?;
            if l.is_empty() {
                return Ok(Head {
                    status,
                    chunked,
                    left,
                });
            }
            let l = String::from_utf8_lossy(&l);
            let Some((k, v)) = l.split_once(':') else {
                continue;
            };
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("content-length") {
                left = Some(v.parse().map_err(|_| invalid("a bad Content-Length"))?);
            } else if k.eq_ignore_ascii_case("transfer-encoding") {
                chunked = v.to_ascii_lowercase().contains("chunked");
            }
        }
        Err(invalid("too many headers"))
    }

    /// The body's next piece; empty at its end.
    pub async fn piece<R: AsyncBufRead + Unpin>(r: &mut R, head: &mut Head) -> Result<Vec<u8>> {
        if head.chunked {
            let size = line(r, MAX_HEAD_LINE).await?;
            let size = String::from_utf8_lossy(&size);
            let size = size.split(';').next().unwrap_or_default().trim();
            let size = usize::from_str_radix(size, 16).map_err(|_| invalid("a bad chunk size"))?;
            if size == 0 {
                // The end (any trailers go unread: the connection closes).
                (head.chunked, head.left) = (false, Some(0));
                return Ok(vec![]);
            }
            if size > MAX_BODY {
                return Err(invalid("a chunk over 16 MiB"));
            }
            let mut buf = vec![0; size];
            r.read_exact(&mut buf).await?;
            line(r, 2).await?;
            return Ok(buf);
        }
        let want = match head.left {
            Some(0) => return Ok(vec![]),
            Some(n) => n.min(PIECE),
            None => PIECE,
        };
        let mut buf = vec![0; want as usize];
        let n = r.read(&mut buf).await?;
        if n == 0 {
            return match head.left {
                Some(_) => Err(ErrorKind::UnexpectedEof.into()),
                None => Ok(vec![]),
            };
        }
        buf.truncate(n);
        if let Some(left) = &mut head.left {
            *left -= n as u64;
        }
        Ok(buf)
    }

    /// Sends each event's JSON (its `data:` lines joined by `\n`; a blank line ends it; data
    /// that is not JSON is skipped) to `tx` until the stream ends, breaks, or has a line or
    /// event over 1 MiB.
    pub async fn sse<R: AsyncBufRead + Unpin>(
        r: &mut R,
        head: &mut Head,
        tx: &mpsc::Sender<Value>,
    ) -> Result<()> {
        let (mut pending, mut data) = (Vec::<u8>::new(), String::new());
        loop {
            let p = piece(r, head).await?;
            if p.is_empty() {
                return Ok(());
            }
            pending.extend(p);
            while let Some(at) = pending.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = pending.drain(..=at).collect();
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    let event = serde_json::from_str::<Value>(&data);
                    data.clear();
                    if let Ok(v) = event {
                        if tx.send(v).await.is_err() {
                            return Ok(());
                        }
                    }
                } else if let Some(d) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(d.strip_prefix(' ').unwrap_or(d));
                    if data.len() > MAX_EVENT {
                        return Err(invalid("an event over 1 MiB"));
                    }
                }
            }
            if pending.len() > MAX_EVENT {
                return Err(invalid("a line over 1 MiB"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workers::driver::tests::{exe, session};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn the_argv_is_exactly_serve_on_loopback() {
        let d = OpenCode::new("/x/opencode".into());
        let argv = d.argv(Path::new("/w/r1"), "s1");
        assert_eq!(
            argv[..5],
            ["/x/opencode", "serve", "--hostname", "127.0.0.1", "--port"]
        );
        assert_eq!(argv.len(), 6);
        let port: u16 = argv[5].parse().unwrap();
        assert!(port > 0);
        // The same port on a second call (the run speaks to it).
        assert_eq!(d.argv(Path::new("/w/r1"), "s1"), argv);
        assert!(crate::workers::launchable(&argv));
        // The password is the child's alone: 32 random bytes, never in the argv.
        let env = d.env();
        assert_eq!(
            env[0],
            ("OPENCODE_SERVER_USERNAME", Some("opencode".into()))
        );
        let (k, pw) = &env[1];
        let pw = pw.as_ref().unwrap();
        assert_eq!(*k, "OPENCODE_SERVER_PASSWORD");
        // Every permission asks, the folder's own config is off, and a config handed over in
        // the environment is not the child's.
        assert_eq!(
            env[2..],
            [
                ("OPENCODE_PERMISSION", Some(r#"{"*":"ask"}"#.into())),
                ("OPENCODE_DISABLE_PROJECT_CONFIG", Some("1".into())),
                ("OPENCODE_CONFIG", None),
                ("OPENCODE_CONFIG_CONTENT", None),
                ("OPENCODE_CONFIG_DIR", None),
            ],
            "{:?}",
            &env[2..]
        );
        assert_eq!(pw.len(), 64);
        assert!(pw.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!argv.iter().any(|a| a.contains(pw.as_str())));
        assert_ne!(
            OpenCode::new("/x/opencode".into()).env()[1].1.as_ref(),
            Some(pw)
        );
    }

    #[test]
    fn the_program_is_found_without_a_login_path() {
        let d = tempfile::tempdir().unwrap();
        let (home, sys, bin) = (
            d.path().join("home"),
            d.path().join("sys"),
            d.path().join("bin"),
        );
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let system_exe = sys.join("opencode");
        let system = [system_exe.as_path()];
        let path = std::env::join_paths([&bin]).unwrap();
        let find = || program(None, &home, &system, &s, Some(&path));
        assert_eq!(find(), None);
        let on_path = exe(&bin.join("opencode"));
        assert_eq!(find(), Some(on_path));
        // Before it: the newest registered OpenCode session's, named `opencode`.
        let old = exe(&d.path().join("old/opencode"));
        let newest = exe(&d.path().join("new/opencode"));
        let other = exe(&d.path().join("opencode-helper"));
        session(&s, "opencode", "m/opencode:a", &old, 1);
        session(&s, "opencode", "m/opencode:b", &newest, 2);
        session(&s, "opencode", "m/opencode:c", &other, 3);
        assert_eq!(find(), Some(newest));
        // Before those: `~/.opencode/bin`, then `~/.local/bin`, then the system installs.
        let installer = exe(&home.join(".opencode/bin/opencode"));
        assert_eq!(find(), Some(installer));
        let local = exe(&home.join(".local/bin/opencode"));
        assert_eq!(find(), Some(local));
        let installed = exe(&system_exe);
        assert_eq!(find(), Some(installed));
        // The debug knob, when set, is the only one.
        let fake = exe(&d.path().join("fake"));
        assert_eq!(
            program(Some(&fake), &home, &system, &s, Some(&path)),
            Some(fake)
        );
        let none = d.path().join("nope");
        assert_eq!(program(Some(&none), &home, &system, &s, Some(&path)), None);
    }

    #[test]
    fn an_opencode_that_allows_without_asking_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        assert!(here(Some(&fake), d.path(), &s).is_ok());
        let files = USER
            .iter()
            .flat_map(|d| USER_FILES.map(|f| format!("{d}/{f}")));
        for file in files {
            let config = d.path().join(&file);
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            for (text, ok) in [
                (r#"{"permission": "allow"}"#, false),
                (r#"{"permission": {"edit": "allow"}}"#, false),
                (r#"{"permission": {"bash": "allow"}}"#, false),
                (r#"{"permission": {"webfetch": "allow"}}"#, false),
                (
                    r#"{"permission": {"bash": {"*": "allow", "rm *": "ask"}}}"#,
                    false,
                ),
                (r#"{"permission": {"edit": {"*": "allow"}}}"#, false),
                (r#"{"permission": {"*": "allow"}}"#, false),
                ("// mine\n{\"permission\": /* all */ \"allow\",}", false),
                // Any allow at all: a specific key of the file's wins over the forced `*`.
                (
                    r#"{"permission": {"bash": {"git status": "allow", "*": "ask"}}}"#,
                    false,
                ),
                (r#"{"permission": {"read": "allow", "edit": "ask"}}"#, false),
                // An agent's or mode's rules come after it, too; a tool on is allow.
                (
                    r#"{"agent": {"build": {"permission": {"bash": "allow"}}}}"#,
                    false,
                ),
                (r#"{"agent": {"x": {"permission": "allow"}}}"#, false),
                (
                    r#"{"mode": {"y": {"permission": {"edit": {"*": "allow"}}}}}"#,
                    false,
                ),
                (r#"{"agent": {"x": {"tools": {"bash": true}}}}"#, false),
                (r#"{"tools": {"*": true}}"#, false),
                (
                    r#"{"agent": {"x": {"permission": {"*": "deny"}, "tools": {"bash": false}}}}"#,
                    true,
                ),
                (r#"{"tools": {"webfetch": false}}"#, true),
                (
                    r#"{"permission": {"bash": {"*": "ask", "rm *": "deny"}}}"#,
                    true,
                ),
                (r#"{"permission": "ask"}"#, true),
                (
                    "{\"$schema\": \"https://opencode.ai/config.json\", // see\n\"mcp\": {},}",
                    true,
                ),
                // A file that does not parse refuses.
                (r#"{"permission": "ask""#, false),
                ("garbage", false),
            ] {
                std::fs::write(&config, text).unwrap();
                let r = here(Some(&fake), d.path(), &s);
                assert_eq!(r.err(), (!ok).then_some(BYPASS_MODE), "{file}: {text}");
            }
            // Nor one that is not UTF-8.
            std::fs::write(&config, b"{\"a\": \"\xff\"}").unwrap();
            assert_eq!(here(Some(&fake), d.path(), &s).err(), Some(BYPASS_MODE));
            std::fs::remove_file(&config).unwrap();
        }
        let none = d.path().join("nope");
        assert_eq!(here(Some(&none), d.path(), &s).err(), Some(NOT_INSTALLED));
    }

    #[test]
    fn an_agent_file_that_allows_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let h = d.path();
        let header = |yaml: &str| format!("---\n{yaml}\n---\nYou allow true things.\n");
        for dir in [".config/opencode", ".opencode"] {
            for sub in ["agent", "agents/deep/er", "mode", "modes"] {
                let file = h.join(dir).join(sub).join("a.md");
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                for (text, ok) in [
                    // The founder's own: a hidden subagent that denies everything.
                    (
                        header("description: x\nmode: subagent\nhidden: true\npermission:\n  \"*\": deny"),
                        true,
                    ),
                    ("no header; allow true\n".into(), true),
                    (header("tools:\n  bash: false\nhidden: true"), true),
                    (header("permission:\n  bash: allow"), false),
                    (header("permission:\n  edit:\n    \"*\": allow"), false),
                    (header("tools:\n  bash: true"), false),
                    (header("tools: {write: true}"), false),
                ] {
                    std::fs::write(&file, &text).unwrap();
                    assert_eq!(bypass_mode(h, None), !ok, "{dir}/{sub}: {text}");
                }
                std::fs::write(&file, b"---\n\xff\n---\n").unwrap();
                assert!(bypass_mode(h, None), "{dir}/{sub}: not UTF-8");
                std::fs::remove_file(&file).unwrap();
                // Not Markdown: not an agent.
                std::fs::write(file.with_extension("txt"), header("tools:\n  bash: true")).unwrap();
                assert!(!bypass_mode(h, None), "{dir}/{sub}");
                std::fs::remove_file(file.with_extension("txt")).unwrap();
            }
        }
        let xdg = tempfile::tempdir().unwrap();
        let file = xdg.path().join("opencode/agents/a.md");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, header("permission:\n  bash: allow")).unwrap();
        assert!(bypass_mode(h, Some(xdg.path())));
        assert!(!bypass_mode(h, None));
    }

    #[test]
    fn an_xdg_config_that_allows_without_asking_refuses_a_worker() {
        let (home, xdg) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (h, x) = (home.path(), Some(xdg.path()));
        assert!(!bypass_mode(h, x));
        std::fs::create_dir_all(xdg.path().join("opencode")).unwrap();
        for file in USER_FILES.map(|f| format!("opencode/{f}")) {
            let config = xdg.path().join(&file);
            std::fs::write(&config, r#"{"permission": {"bash": "ask"}}"#).unwrap();
            assert!(!bypass_mode(h, x), "{file}");
            std::fs::write(&config, "// all\n{\"permission\": \"allow\",}").unwrap();
            assert!(bypass_mode(h, x), "{file}");
            // Unset, it is not read.
            assert!(!bypass_mode(h, None), "{file}");
            // One it cannot parse refuses.
            std::fs::write(&config, "{").unwrap();
            assert!(bypass_mode(h, x), "{file}");
            std::fs::remove_file(&config).unwrap();
        }
    }

    #[test]
    fn a_folder_that_allows_without_asking_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(folder_bypass(d.path()), None);
        std::fs::create_dir_all(d.path().join(".opencode")).unwrap();
        for file in FOLDER {
            let config = d.path().join(file);
            std::fs::write(&config, r#"{"permission": {"bash": "ask"}}"#).unwrap();
            assert_eq!(folder_bypass(d.path()), None, "{file}");
            std::fs::write(&config, r#"{"permission": {"bash": "allow"}}"#).unwrap();
            let why = OpenCode::new("/x/opencode".into())
                .refuses(d.path())
                .unwrap();
            assert!(
                why.starts_with(BYPASS_MODE) && why.ends_with(&*config.to_string_lossy()),
                "{why}"
            );
            std::fs::remove_file(&config).unwrap();
        }
    }

    #[test]
    fn a_summary_says_what_allow_grants() {
        let p = json!({"id": "per_1", "sessionID": "ses_1", "permission": "bash",
            "patterns": ["rm -rf x", "ls"], "metadata": {"title": "Run it"}, "always": []});
        assert_eq!(
            asked(&p),
            ("bash".into(), "Run it; bash; rm -rf x ls".into())
        );
        let legacy = json!({"type": "edit", "pattern": "/w/a.rs", "title": "Edit a.rs"});
        assert_eq!(
            asked(&legacy),
            ("edit".into(), "Edit a.rs; edit; /w/a.rs".into())
        );
        assert_eq!(
            asked(&json!({})),
            ("tool".into(), "permission.asked".into())
        );
        let long = json!({"permission": "bash", "patterns": ["y".repeat(500)]});
        assert_eq!(asked(&long).1.chars().count(), 401);
    }

    #[test]
    fn the_turn_is_its_text_parts_or_its_error() {
        let o = finished(&json!({"info": {"finish": "stop"}, "parts": [
            {"type": "step-start"}, {"type": "text", "text": "one"},
            {"type": "tool", "text": "no"}, {"type": "text", "text": "two"}]}));
        assert_eq!(
            (o.status, o.reason, o.body.as_str()),
            ("done", None, "one\ntwo")
        );
        let o = finished(&json!({"info": {"error": {"name": "UnknownError",
            "data": {"message": "rate limited"}}}, "parts": [{"type": "text", "text": "x"}]}));
        assert_eq!(
            (o.status, o.reason, o.body.as_str()),
            ("failed", Some("error"), "rate limited")
        );
        let o = finished(&json!({"info": {"error": {"name": "MessageAbortedError"}}}));
        assert_eq!(o.body, "MessageAbortedError");
    }

    #[tokio::test]
    async fn no_health_poll_before_it_says_it_listens() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("5")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // A server whose stdout ends before its listening line: never polled (port 1 would
        // otherwise be retried for 30 s).
        let (said, listening) = oneshot::channel();
        drop(said);
        let server = Server::new(1, "opencode", "pw");
        let r = tokio::time::timeout(
            Duration::from_secs(2),
            ready(&mut child, listening, &server),
        )
        .await;
        assert_eq!(r, Ok(Err("the worker's OpenCode exited before it served")));
    }

    /// A one-connection server on a free port: it reads the request, then writes `pieces` with
    /// a pause between them and closes. The request comes back on the handle.
    async fn canned(pieces: Vec<Vec<u8>>) -> (u16, tokio::task::JoinHandle<String>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut req = vec![];
            let mut buf = [0; 4096];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = s.read(&mut buf).await.unwrap();
                req.extend(&buf[..n]);
            }
            for p in pieces {
                // The client may hang up first (an event over the limit).
                let _ = s.write_all(&p).await;
                let _ = s.flush().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            String::from_utf8(req).unwrap()
        });
        (port, task)
    }

    fn bytes(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[tokio::test]
    async fn a_response_has_a_length_or_chunks() {
        let (port, req) = canned(bytes(&[
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Len",
            "gth: 36\r\n\r\n{\"healthy\":true,",
            "\"version\":\"1.18.32\"}",
        ]))
        .await;
        let server = Server::new(port, "opencode", "pw");
        let (status, v) = server.request("GET", "/global/health", None).await.unwrap();
        assert_eq!(
            (status, v),
            (200, json!({"healthy": true, "version": "1.18.32"}))
        );
        let req = req.await.unwrap();
        assert!(req.starts_with("GET /global/health HTTP/1.1\r\n"), "{req}");
        // `opencode:pw`, base64.
        assert!(
            req.contains("\r\nAuthorization: Basic b3BlbmNvZGU6cHc=\r\n"),
            "{req}"
        );
        assert!(
            req.contains(&format!("\r\nHost: 127.0.0.1:{port}\r\n")),
            "{req}"
        );

        let (port, req) = canned(bytes(&[
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n{\"id\":",
            "\r\nA;x=1\r\n\"ses_1\"  }\r\n",
            "0\r\n\r\n",
        ]))
        .await;
        let server = Server::new(port, "opencode", "pw");
        let body = json!({"title": "w"});
        let (status, v) = server
            .request("POST", "/session", Some(&body))
            .await
            .unwrap();
        assert_eq!((status, v), (200, json!({"id": "ses_1"})));
        let req = req.await.unwrap();
        assert!(req.contains("\r\nContent-Type: application/json\r\nContent-Length: 13\r\n"));

        // No password: refused, with nothing to read.
        let (port, _) = canned(bytes(&[
            "HTTP/1.1 401 Unauthorized\r\nwww-authenticate: Basic realm=\"Secure Area\"\r\n\
             Content-Length: 0\r\n\r\n",
        ]))
        .await;
        let server = Server::new(port, "opencode", "wrong");
        let r = server.request("GET", "/global/health", None).await.unwrap();
        assert_eq!(r, (401, Value::Null));

        // A body cut short of its length is an error, not a short body.
        let (port, _) = canned(bytes(&["HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n{}"])).await;
        let server = Server::new(port, "opencode", "pw");
        assert!(server.request("GET", "/x", None).await.is_err());
        // Nor is a non-HTTP answer anything.
        let (port, _) = canned(bytes(&["SSH-2.0-x\r\n"])).await;
        let server = Server::new(port, "opencode", "pw");
        assert!(server.request("GET", "/x", None).await.is_err());
    }

    #[tokio::test]
    async fn events_split_across_reads_come_whole() {
        let one = r#"{"type":"permission.asked","properties":{"id":"per_1"}}"#;
        let (a, b) = one.split_at(20);
        let chunk = |s: &str| format!("{:x}\r\n{s}\r\n", s.len());
        let (port, req) = canned(vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec(),
            chunk("data: {\"type\":\"server.connected\",\"properties\":{}}\n\n").into_bytes(),
            chunk(&format!("data: {a}")).into_bytes(),
            chunk(&format!("{b}\r\n\r\n: a comment\n")).into_bytes(),
            // Two data lines are one event; data that is not JSON is skipped.
            chunk("data: {\"type\":\"x\",\ndata: \"n\":1}\n\ndata: nope\n\n").into_bytes(),
            b"0\r\n\r\n".to_vec(),
        ])
        .await;
        let server = Server::new(port, "opencode", "pw");
        let (mut rx, _task) = server.events().await.unwrap();
        let mut got = vec![];
        while let Some(v) = rx.recv().await {
            got.push(v);
        }
        assert_eq!(
            got,
            [
                json!({"type": "server.connected", "properties": {}}),
                serde_json::from_str::<Value>(one).unwrap(),
                json!({"type": "x", "n": 1}),
            ]
        );
        let req = req.await.unwrap();
        assert!(req.starts_with("GET /event HTTP/1.1\r\n"), "{req}");
        assert!(req.contains("\r\nAccept: text/event-stream\r\n"), "{req}");

        // An event over 1 MiB ends the stream before it.
        let long = format!("data: \"{}\"\n\n", "x".repeat(http::MAX_EVENT));
        let (port, _) = canned(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 2000000\r\n\r\n".to_vec(),
            b"data: {\"a\":1}\n\n".to_vec(),
            long.into_bytes(),
        ])
        .await;
        let server = Server::new(port, "opencode", "pw");
        let (mut rx, _task) = server.events().await.unwrap();
        assert_eq!(rx.recv().await, Some(json!({"a": 1})));
        assert_eq!(rx.recv().await, None);

        // A stream refused is an error.
        let (port, _) = canned(bytes(&[
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
        ]))
        .await;
        let server = Server::new(port, "opencode", "pw");
        assert!(server.events().await.is_err());
    }
}
