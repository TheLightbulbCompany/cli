//! `mcp`: the stdio MCP server an agent runs to message the other coding sessions on this
//! computer. A client of the daemon, like the hook. Every tool call first resolves which
//! session it comes from (`agents::source`); the model never names its own source.

use crate::agents::source::{self, SourceError};
use crate::os::{self, Process};
use crate::sessions::{Key, Session, Status, Tier};
use crate::{daemon, hook, paths};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, RequestMetaObject};
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Runs the server on stdin/stdout until the client closes it.
pub fn run(name: String) -> crate::Result<()> {
    daemon::ensure_running()?;
    tokio::runtime::Runtime::new()?.block_on(async {
        Server {
            me: OnceLock::new(),
            name,
        }
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
        Ok(())
    })
}

struct Server {
    /// The agent process above this server, once a call has found it.
    me: OnceLock<Process>,
    /// The name this server advertises (`mcp --server-name`).
    name: String,
}

#[derive(Deserialize, JsonSchema)]
struct FindArgs {
    /// Text to find (case-insensitive) in a session's id, agent, folder, repo, branch, title or task. Empty lists all.
    #[serde(default)]
    query: String,
    /// At most this many sessions.
    #[serde(default = "twenty")]
    limit: u32,
}

fn twenty() -> u32 {
    20
}

#[derive(Deserialize, JsonSchema)]
struct SendArgs {
    /// A new [A-Za-z0-9._-]{1,128} string; reuse it only to retry this send.
    request_id: String,
    text: String,
    /// The recipient, a session_id from find_sessions. Optional with reply_to: the reply then
    /// goes to whoever sent that message.
    session_id: Option<String>,
    /// The requestId of the message this answers.
    reply_to: Option<String>,
    /// Not supported here.
    files: Option<Vec<String>>,
    /// Not supported here.
    notify: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
struct ReceiptArgs {
    /// The request_id of a message this session sent.
    request_id: String,
}

/// The longest one `check_inbox` call waits: clients such as Codex time a tool out at 60 s.
const WAIT_CAP_S: u32 = 30;

#[derive(Deserialize, JsonSchema)]
struct InboxArgs {
    /// Wait up to this many seconds for a message to arrive. Accepts up to 600, but one call
    /// waits at most 30 (clients time tool calls out); call again to keep waiting.
    #[serde(default)]
    wait_secs: u32,
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

/// Origin's repo name (the URL's last path segment) and the checked-out branch, from
/// `<cwd>/.git`: a directory, or a `gitdir:` file (a linked worktree; `gitdir` and its
/// `commondir` may point anywhere: only two names are read out). A symlinked `.git` is
/// not followed. Every read is capped at 64 KiB.
fn git(cwd: &str) -> (Option<String>, Option<String>) {
    git_in(Path::new(cwd)).unwrap_or_default()
}

fn git_in(cwd: &Path) -> Option<(Option<String>, Option<String>)> {
    let dot = cwd.join(".git");
    let kind = std::fs::symlink_metadata(&dot).ok()?.file_type();
    let dir = if kind.is_dir() {
        dot
    } else if kind.is_file() {
        let d = read_capped(&dot)?;
        cwd.join(d.trim().strip_prefix("gitdir:")?.trim())
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
                    repo = url
                        .trim()
                        .trim_end_matches('/')
                        .rsplit(['/', ':'])
                        .next()
                        .map(|n| n.trim_end_matches(".git").to_string())
                        .filter(|n| !n.is_empty());
                }
            }
        }
    }
    Some((repo, branch))
}

fn record(s: &Session, from: &Key, person: &str, computer: &str) -> Value {
    let (repo, branch) = git(&s.cwd);
    json!({
        "session_id": s.key,
        "person": person,
        "person_verified": false,
        "computer": computer,
        "agent": s.provider,
        "cwd": s.cwd,
        "repo": repo,
        "branch": branch,
        "status": match s.status { Status::Live => "busy", Status::Idle => "idle", Status::Gone => "gone" },
        "doing": s.doing.clone().unwrap_or_default(),
        "tier": s.tier,
        "can_send": true,
        "needs_owner_ok": false,
        "is_this_session": &s.key == from,
    })
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
        let r = ask("sessions", json!({}))
            .await
            .map_err(|_| code("internal"))?;
        let sessions: Vec<Session> =
            serde_json::from_value(r["sessions"].clone()).map_err(|_| code("internal"))?;
        match source::resolve(&me, &Value::Object(meta.0 .0), &sessions) {
            Ok(from) => {
                // A tool call is a step: mail delivered before it has been seen.
                let _ = ask("mark_read", json!({ "key": from })).await;
                Ok(Call { me, from, sessions })
            }
            Err(SourceError::Unidentified) => Err(code("source_unidentified")),
            Err(SourceError::Ambiguous) => Err(code("source_ambiguous")),
        }
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

#[tool_router]
impl Server {
    /// The coding sessions on this computer you can message.
    #[tool]
    async fn find_sessions(
        &self,
        Parameters(a): Parameters<FindArgs>,
        meta: RequestMetaObject,
    ) -> CallToolResult {
        let call = match self.source(meta).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        let person = std::env::var("USER").unwrap_or_default();
        let computer = os::hostname();
        let query = a.query.to_lowercase();
        let found: Vec<Value> = call
            .sessions
            .iter()
            .map(|s| (s, record(s, &call.from, &person, &computer)))
            .filter(|(s, r)| {
                ["session_id", "agent", "cwd", "repo", "branch", "doing"]
                    .iter()
                    .filter_map(|f| r[f].as_str())
                    .chain(s.title.as_deref())
                    .any(|v| v.to_lowercase().contains(&query))
            })
            .map(|(_, r)| r)
            .collect();
        let total = found.len();
        let sessions: Vec<Value> = found.into_iter().take(a.limit as usize).collect();
        Self::done(&call, json!({ "sessions": sessions, "total": total })).await
    }

    /// Sends text to another session. Returns its receipt.
    #[tool]
    async fn send_message(
        &self,
        Parameters(a): Parameters<SendArgs>,
        meta: RequestMetaObject,
    ) -> CallToolResult {
        let call = match self.source(meta).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        if a.files.is_some() || a.notify.is_some() {
            return code("unsupported");
        }
        if a.session_id.is_none() && a.reply_to.is_none() {
            return code("bad_request");
        }
        let r = ask(
            "send",
            json!({ "from": call.from, "to": a.session_id, "text": a.text,
                "request_id": a.request_id, "reply_to": a.reply_to }),
        )
        .await;
        match r {
            Ok(r) if r["ok"] == true => Self::done(&call, r["receipt"].clone()).await,
            Ok(r) => refused(r),
            Err(_) => code("internal"),
        }
    }

    /// The receipt of a message this session sent.
    #[tool]
    async fn message_receipt(
        &self,
        Parameters(a): Parameters<ReceiptArgs>,
        meta: RequestMetaObject,
    ) -> CallToolResult {
        let call = match self.source(meta).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        match ask(
            "receipt",
            json!({ "from": call.from, "request_id": a.request_id }),
        )
        .await
        {
            Ok(r) if r["ok"] == true => Self::done(&call, r["receipt"].clone()).await,
            Ok(r) => refused(r),
            Err(_) => code("internal"),
        }
    }

    /// Takes the messages waiting for this session, first waiting up to wait_secs (at most 30
    /// per call) for one. Each message has its raw text and, in framed, the text as delivered.
    #[tool]
    async fn check_inbox(
        &self,
        Parameters(a): Parameters<InboxArgs>,
        meta: RequestMetaObject,
    ) -> CallToolResult {
        let call = match self.source(meta).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        if a.wait_secs > 600 {
            return code("bad_request");
        }
        let r = ask(
            "inbox",
            json!({ "key": call.from, "wait_secs": a.wait_secs.min(WAIT_CAP_S),
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
                    .map(|m| {
                        json!({ "id": m["id"], "request_id": m["request_id"],
                            "thread": m["thread"], "from": m["from"],
                            "text": m["text"], "framed": m["framed"], "sent_at": m["sent_at"] })
                    })
                    .collect();
                Self::done(&call, json!({ "messages": messages })).await
            }
            Ok(r) => refused(r),
            Err(_) => code("internal"),
        }
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            &self.name,
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(INSTRUCTIONS.to_string())
    }
}

const INSTRUCTIONS: &str = "These tools message the other coding sessions (Claude Code, Codex, Cursor, Grok CLI) on this computer. Call find_sessions to see them, then send_message to one by its session_id. Every send_message needs a request_id: a new [A-Za-z0-9._-]{1,128} string, reused only to retry that same send. Messages delivered to you show a requestId; pass it as reply_to to answer one.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn git_reads_origin_repo_and_branch_also_from_a_linked_worktree() {
        let d = tempfile::tempdir().unwrap();
        let main = d.path().join("main");
        fs::create_dir_all(main.join(".git/worktrees/x")).unwrap();
        fs::write(main.join(".git/HEAD"), "ref: refs/heads/init\n").unwrap();
        fs::write(
            main.join(".git/config"),
            "[remote \"upstream\"]\n\turl = x/other.git\n[remote \"origin\"]\n\turl = git@github.com:Org/lightbulb-network.git\n",
        )
        .unwrap();
        let name = Some("lightbulb-network".to_string());
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
