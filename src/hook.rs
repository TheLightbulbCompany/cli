//! `hook <agent> <event>`: run by an agent's hook config with the agent's payload on stdin.
//! Registers or updates the session, ends it on SessionEnd, and hands waiting mail to the
//! agent in the shape that agent reads. A client of the daemon, like every other.
//! Every op carries the identified agent process as `caller`, so a hook run from an agent's
//! own tools cannot end, take over or drain another live session; this is the ceiling,
//! since any same-uid client can speak raw IPC: it stops an agent's ordinary tools, not a
//! hostile user.

use crate::agents::caller::{self, CallerError};
use crate::agents::cursor_grok;
use crate::os::Process;
use crate::{daemon, paths, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Debug builds only: `<pid>:<birth_micros>` stands in for the calling agent (tests).
/// Compiled out of release builds.
#[cfg(debug_assertions)]
pub const CALLER_ENV: &str = "LIGHTBULB_CONNECT_HOOK_CALLER";

/// Debug builds only: any value makes capture's daemon connection a dead one (tests).
#[cfg(debug_assertions)]
pub const CAPTURE_DOWN_ENV: &str = "LIGHTBULB_CONNECT_DEBUG_CAPTURE_DOWN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    Stop,
    SessionEnd,
}

pub const EVENTS: [Event; 5] = [
    Event::SessionStart,
    Event::UserPromptSubmit,
    Event::PostToolUse,
    Event::Stop,
    Event::SessionEnd,
];

impl Event {
    /// Claude Code's (and Codex's) `hook_event_name`.
    pub fn name(self) -> &'static str {
        match self {
            Event::SessionStart => "SessionStart",
            Event::UserPromptSubmit => "UserPromptSubmit",
            Event::PostToolUse => "PostToolUse",
            Event::Stop => "Stop",
            Event::SessionEnd => "SessionEnd",
        }
    }

    /// The command-line argument: `hook <agent> <arg>`.
    pub fn arg(self) -> &'static str {
        match self {
            Event::SessionStart => "session-start",
            Event::UserPromptSubmit => "user-prompt-submit",
            Event::PostToolUse => "post-tool-use",
            Event::Stop => "stop",
            Event::SessionEnd => "session-end",
        }
    }
}

/// One agent's hook payload in one shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Hook {
    pub provider: &'static str,
    pub event: Event,
    pub conversation: String,
    pub cwd: String,
    pub transcript_path: Option<String>,
}

/// `None` for anything to ignore: unknown agents or events, Grok subagent hooks,
/// a Grok stop that is not the end of a turn, a payload without a conversation.
pub fn normalize(provider: &str, event_arg: &str, payload: &Value) -> Option<Hook> {
    let from_arg = EVENTS.into_iter().find(|e| e.arg() == event_arg);
    match provider {
        "claude" | "codex" => {
            // Claude and Codex name the event in the payload; the argument is the fallback.
            let event = match payload["hook_event_name"].as_str() {
                Some(n) => EVENTS.into_iter().find(|e| e.name() == n)?,
                None => from_arg?,
            };
            Some(Hook {
                provider: if provider == "claude" {
                    "claude"
                } else {
                    "codex"
                },
                event,
                conversation: payload["session_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())?
                    .into(),
                cwd: payload["cwd"].as_str().unwrap_or_default().into(),
                transcript_path: payload["transcript_path"].as_str().map(String::from),
            })
        }
        "cursor" => cursor_grok::normalize_cursor(from_arg?, payload),
        "grok" => cursor_grok::normalize_grok(from_arg?, payload),
        "opencode" => cursor_grok::normalize_opencode(from_arg?, payload),
        _ => None,
    }
}

/// Whether this event hands mail to the model.
pub fn takes_mail(h: &Hook) -> bool {
    match (h.provider, h.event) {
        (_, Event::SessionEnd) => false,
        ("cursor" | "grok" | "opencode", e) => matches!(e, Event::PostToolUse | Event::Stop),
        _ => true,
    }
}

/// What the agent reads from stdout to see `text`.
pub fn output(h: &Hook, text: &str) -> Value {
    match (h.provider, h.event) {
        ("cursor", e) => cursor_grok::cursor_output(e, text),
        // Our OpenCode plugin, not OpenCode, reads this.
        ("opencode", _) => json!({ "text": text }),
        (_, Event::Stop) => json!({ "decision": "block", "reason": text }),
        (_, e) => json!({
            "hookSpecificOutput": { "hookEventName": e.name(), "additionalContext": text }
        }),
    }
}

/// Runs one hook; `Ok(Some(stdout))` when there is something for the agent.
pub fn run(agent: &str, event: &str, stdin: &str) -> Result<Option<String>> {
    crate::telemetry::sync_env_optout(&crate::paths::state_path());
    let payload: Value = serde_json::from_str(stdin)?;
    if agent == "opencode" && event == "ack" {
        return ack_opencode(&payload).map(|()| None);
    }
    let Some(h) = normalize(agent, event, &payload) else {
        return Ok(None);
    };
    // Cursor runs Claude's hooks too, and ours are installed for both: its own hooks own the
    // session, so a Claude hook it fired registers and delivers nothing (no second session).
    if agent == "claude" && caller::fired_by_cursor() {
        if cfg!(debug_assertions) {
            eprintln!("hook: claude {event} fired by Cursor; Cursor's hooks own this session");
        }
        return Ok(None);
    }
    let caller = calling_agent(Some(agent));
    if h.event == Event::SessionEnd {
        // The agent may have exited before its SessionEnd hook ran, so the caller check
        // can fail; then the daemon ends the row only if its recorded process is gone too.
        daemon::ensure_running()?;
        let mut conn = Conn::open()?;
        let key = crate::sessions::Key::new(&conn.member()?, h.provider, &h.conversation);
        capture(&h, &payload, &json!({}));
        let r = match &caller {
            Ok(me) => conn.call("end", json!({ "key": key, "caller": proc_json(me) }))?,
            Err(_) => conn.call("end", json!({ "key": key, "if_gone": true }))?,
        };
        if r["ok"] != true && r["error"] != "not_found" {
            return Err(format!("end: {r}").into());
        }
        return Ok(None);
    }
    let me = match caller {
        Ok(p) => p,
        Err(CallerError::NotThatAgent) => return Ok(None),
        Err(CallerError::Lookup(e)) => return Err(e.into()),
    };
    daemon::ensure_running()?;
    let mut conn = Conn::open()?;
    let status = match h.event {
        Event::UserPromptSubmit | Event::PostToolUse => "busy",
        _ => "idle",
    };
    // Read here, in the agent's process, which has the folder access the person gave it: the
    // folder resolved (what `folders_off` is matched against) and, at a session's start and
    // each prompt, its repo and branch (absent otherwise: the daemon keeps the last).
    let cwd = crate::presence::resolved(std::path::Path::new(&h.cwd));
    let mut session = json!({ "provider": h.provider, "conversation": h.conversation,
        "pid": me.pid, "birth_micros": me.birth_micros, "cwd": cwd, "status": status,
        "executable": me.executable });
    if matches!(h.event, Event::SessionStart | Event::UserPromptSubmit) {
        let (repo, branch) = crate::mcp::git(&h.cwd);
        (session["repo"], session["branch"]) = (repo.into(), branch.into());
    }
    let r = conn.call(
        "register",
        json!({ "session": session, "caller": proc_json(&me) }),
    )?;
    let key = r["key"].clone();
    if !key.is_string() {
        return Err(format!("register: {r}").into());
    }
    capture(&h, &payload, &session);
    // Our OpenCode plugin sends `mail: false` when it could not hand mail over (it has not
    // seen the session's agent and model yet): the mail waits for `check_inbox`.
    if !takes_mail(&h) || (h.provider == "opencode" && payload["mail"] == false) {
        return Ok(None);
    }
    // OpenCode's plugin is offered the mail and acks it once it is in the session
    // (`hook opencode ack`): a push that fails leaves it waiting, to be offered again.
    let op = if h.provider == "opencode" {
        "inbox.peek"
    } else {
        "inbox"
    };
    let r = conn.call(op, json!({ "key": key, "caller": proc_json(&me) }))?;
    let messages = r["messages"]
        .as_array()
        .ok_or_else(|| format!("{op}: {r}"))?;
    let mut framed: Vec<&str> = messages
        .iter()
        .filter_map(|m| m["framed"].as_str())
        .collect();
    // At a session's start, after the mail: what the team and the person know of this repo.
    let block = match (h.event, session["repo"].as_str()) {
        (Event::SessionStart, Some(repo)) => {
            // Any failure is "no block": the mail is still handed over.
            conn.call(
                "memory.recall",
                json!({ "question": "", "repo": repo, "budget_chars": 1600, "session": key }),
            )
            .ok()
            .and_then(|r| {
                r["block"]
                    .as_str()
                    .filter(|b| !b.is_empty())
                    .map(String::from)
            })
        }
        _ => None,
    };
    framed.extend(block.as_deref());
    if framed.is_empty() {
        return Ok(None);
    }
    let mut out = output(&h, &framed.join("\n\n"));
    if h.provider == "opencode" {
        out["ids"] = messages.iter().map(|m| m["id"].clone()).collect();
    }
    Ok(Some(out.to_string()))
}

/// How long the hook waits for each capture write's answer: capture never slows the agent.
/// The daemon finishes a write it was not waited for.
const CAPTURE_WAIT: Duration = Duration::from_millis(10);

/// Tells the daemon what this event adds to the local capture (`capture::*`, written by the
/// daemon), on a connection of its own so a slow write holds nothing behind it. Any failure
/// is silent: capture never breaks the agent or the mail.
fn capture(h: &Hook, payload: &Value, session: &Value) {
    // Debug builds only: capture's connection is a dead one, for tests of the failure path.
    #[cfg(debug_assertions)]
    let dead = std::env::var_os(CAPTURE_DOWN_ENV).and_then(|_| {
        let (a, _b) = UnixStream::pair().ok()?;
        Some(Conn {
            stream: BufReader::new(a),
            next: 1,
        })
    });
    #[cfg(not(debug_assertions))]
    let dead = None;
    if let Some(mut conn) = dead.or_else(|| Conn::open().ok()) {
        capture_on(&mut conn, h, payload, session);
    }
}

/// `try_capture` on `conn`, waiting at most `CAPTURE_WAIT` for each answer. Past it, one line
/// says so and the rest of this event's capture is skipped; whether that happened.
fn capture_on(conn: &mut Conn, h: &Hook, payload: &Value, session: &Value) -> bool {
    if conn.wait_up_to(CAPTURE_WAIT).is_err() {
        return false;
    }
    match try_capture(conn, h, payload, session) {
        Err(e) if e.to_string() == no_answer(CAPTURE_WAIT) => {
            eprintln!(
                "lightbulb hook {} {}: capture took over {} ms; not waiting for it",
                h.provider,
                h.event.arg(),
                CAPTURE_WAIT.as_millis()
            );
            true
        }
        _ => false,
    }
}

/// `body` with the fields of `extra` added; never panics.
fn with(body: &Value, extra: Value) -> Value {
    let mut b = body.clone();
    if let (Some(m), Some(e)) = (b.as_object_mut(), extra.as_object()) {
        m.extend(e.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    b
}

/// Most bytes of a tool's input sent to the daemon, which keeps 200 characters of one field.
const ARGS_MAX: usize = 4 * 1024;

/// A tool input as sent for its summary: its top-level text fields (the only ones a summary
/// reads), each cut to 256 characters, kept while the whole stays within `ARGS_MAX`. A
/// multi-megabyte Write never makes the request outlast the hook's wait.
fn capped_args(args: &Value) -> String {
    let mut kept = serde_json::Map::new();
    let mut size = 2;
    for (k, v) in args.as_object().into_iter().flatten() {
        let Some(text) = v.as_str() else { continue };
        let text: String = text.chars().take(256).collect();
        let n = json!({ k: text }).to_string().len();
        if size + n > ARGS_MAX {
            break;
        }
        size += n;
        kept.insert(k.clone(), text.into());
    }
    Value::Object(kept).to_string()
}

fn try_capture(conn: &mut Conn, h: &Hook, payload: &Value, session: &Value) -> Result<()> {
    let who = json!({ "harness": h.provider, "conversation": h.conversation });
    let start = |conn: &mut Conn, repo: &Value, branch: &Value| {
        let body = json!({ "cwd": session["cwd"], "repo": repo, "branch": branch });
        conn.call("capture.session_start", with(&who, body))
    };
    match h.event {
        Event::SessionStart => {
            start(conn, &session["repo"], &session["branch"])?;
        }
        Event::PostToolUse => {
            let tool = ["tool_name", "toolName", "tool"]
                .iter()
                .find_map(|k| payload[k].as_str());
            let Some(tool) = tool else { return Ok(()) };
            let args = ["tool_input", "args"]
                .iter()
                .map(|k| &payload[k])
                .find(|a| !a.is_null());
            let call = json!({ "tool": tool, "args": args.map(capped_args),
                "duration_ms": payload["duration"].as_i64(),
                "ok": payload["tool_response"]["is_error"] != true });
            let r = conn.call("capture.tool_call", with(&who, call.clone()))?;
            // A session already running before capture existed has no row: open one now (the
            // daemon applies the hidden-folder rules), then record the call.
            if r["stored"] == false {
                let cwd = session["cwd"].as_str().unwrap_or_default();
                let (repo, branch) = crate::mcp::git(cwd);
                let r = start(conn, &repo.into(), &branch.into())?;
                if r["stored"] == true {
                    conn.call("capture.tool_call", with(&who, call))?;
                }
            }
        }
        Event::Stop => {
            // A Stop fires every turn: it reads only the `usage` its own payload carries, never
            // the transcript (that is read once, at SessionEnd).
            let u = &payload["usage"];
            let (i, o) = (u["input_tokens"].as_i64(), u["output_tokens"].as_i64());
            if i.is_some() || o.is_some() {
                let body = json!({ "prompt_tokens": i, "completion_tokens": o });
                conn.call("capture.stop", with(&who, body))?;
            }
        }
        Event::SessionEnd => {
            // Claude Code's transcript is the one verified source of tokens; other agents'
            // payloads have none we have confirmed, so theirs stay null. One call: a write
            // not waited for must not cost the end.
            let tokens = (h.provider == "claude")
                .then_some(h.transcript_path.as_deref())
                .flatten()
                .and_then(crate::capture::transcript_tokens);
            let body = match tokens {
                Some((i, o)) => json!({ "prompt_tokens": i, "completion_tokens": o }),
                None => json!({}),
            };
            conn.call("capture.session_end", with(&who, body))?;
        }
        Event::UserPromptSubmit => {}
    }
    Ok(())
}

/// `hook opencode ack`, payload `{session_id, ids}`: the plugin put those messages into the
/// session, so they are delivered. Only that session's own OpenCode may ack its mail.
fn ack_opencode(payload: &Value) -> Result<()> {
    let Some(conversation) = payload["session_id"].as_str().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let me = match calling_agent(Some("opencode")) {
        Ok(p) => p,
        Err(CallerError::NotThatAgent) => return Ok(()),
        Err(CallerError::Lookup(e)) => return Err(e.into()),
    };
    daemon::ensure_running()?;
    let mut conn = Conn::open()?;
    let key = crate::sessions::Key::new(&conn.member()?, "opencode", conversation);
    let r = conn.call(
        "inbox.ack",
        json!({ "key": key, "caller": proc_json(&me), "ids": payload["ids"] }),
    )?;
    if r["ok"] != true {
        return Err(format!("ack: {r}").into());
    }
    Ok(())
}

pub(crate) fn proc_json(p: &Process) -> Value {
    json!({ "pid": p.pid, "birth_micros": p.birth_micros })
}

/// The agent process this runs for: `agent`'s, or with `None` any agent's (the MCP server).
pub(crate) fn calling_agent(agent: Option<&str>) -> std::result::Result<Process, CallerError> {
    if let Some(pinned) = pinned_caller() {
        return pinned;
    }
    match agent {
        Some(a) => caller::identify(a),
        None => caller::identify_any(),
    }
}

/// The agent process `CALLER_ENV` (`<pid>:<birth_micros>`) names, when set; always `None`
/// in release builds.
pub(crate) fn pinned_caller() -> Option<std::result::Result<Process, CallerError>> {
    #[cfg(not(debug_assertions))]
    return None;
    #[cfg(debug_assertions)]
    {
        let v = std::env::var(CALLER_ENV).ok()?;
        let (pid, birth) = v.split_once(':')?;
        let bad = |_| CallerError::Lookup(format!("{CALLER_ENV} is not <pid>:<birth_micros>"));
        Some((|| {
            let pid = pid.parse().map_err(bad)?;
            Ok(Process {
                pid,
                birth_micros: birth.parse().map_err(bad)?,
                // As the walk would see it, so what it is (`classify`) holds in tests too.
                executable: crate::os::process(pid)
                    .map(|p| p.executable)
                    .unwrap_or_default(),
            })
        })())
    }
}

/// One blocking connection to the daemon.
pub(crate) struct Conn {
    stream: BufReader<UnixStream>,
    next: u64,
}

impl Conn {
    /// How long one `call` waits for its answer (default 10 s).
    pub(crate) fn wait_up_to(&self, d: Duration) -> Result<()> {
        Ok(self.stream.get_ref().set_read_timeout(Some(d))?)
    }

    pub(crate) fn open() -> Result<Conn> {
        let stream = UnixStream::connect(paths::socket())?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(Conn {
            stream: BufReader::new(stream),
            next: 1,
        })
    }

    /// This computer's member prefix, from the daemon's `status`.
    pub(crate) fn member(&mut self) -> Result<String> {
        Ok(self.call("status", json!({}))?["member_key_short"]
            .as_str()
            .ok_or("status without member_key_short")?
            .to_string())
    }

    /// The next line the daemon sends (an event, on a subscribed connection).
    pub(crate) fn next_line(&mut self) -> Result<Value> {
        let mut line = String::new();
        if self.stream.read_line(&mut line)? == 0 {
            return Err("the background service closed the connection".into());
        }
        Ok(serde_json::from_str(&line)?)
    }

    pub(crate) fn call(&mut self, op: &str, mut body: Value) -> Result<Value> {
        body["id"] = self.next.into();
        body["op"] = op.into();
        self.next += 1;
        self.stream
            .get_mut()
            .write_all(format!("{body}\n").as_bytes())?;
        let mut line = String::new();
        match self.stream.read_line(&mut line) {
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                let waited = self.stream.get_ref().read_timeout().ok().flatten();
                Err(no_answer(waited.unwrap_or_default()).into())
            }
            Err(e) => Err(e.into()),
            Ok(_) => Ok(serde_json::from_str(&line)?),
        }
    }
}

/// What a person is told when the daemon does not answer within `waited`.
fn no_answer(waited: Duration) -> String {
    format!(
        "The background service did not answer in {} s. If macOS is asking whether lightbulb may read a folder, click Allow, then try again.",
        waited.as_secs()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_daemon_that_does_not_answer_is_a_sentence() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut conn = Conn {
            stream: BufReader::new(a),
            next: 1,
        };
        conn.wait_up_to(Duration::from_secs(1)).unwrap();
        let e = conn.call("ping", json!({})).unwrap_err().to_string();
        assert_eq!(e, "The background service did not answer in 1 s. If macOS is asking whether lightbulb may read a folder, click Allow, then try again.");
    }

    #[test]
    fn capture_on_a_dead_connection_errs_and_the_wrapper_swallows_it() {
        for e in EVENTS.into_iter().filter(|e| *e != Event::UserPromptSubmit) {
            let (a, b) = UnixStream::pair().unwrap();
            drop(b);
            let mut conn = Conn {
                stream: BufReader::new(a),
                next: 1,
            };
            let p = json!({"tool_name": "Bash", "tool_input": {}, "usage": {"input_tokens": 1}});
            let h = Hook {
                transcript_path: Some(format!(
                    "{}/tests/fixtures/hooks/claude-transcript.jsonl",
                    env!("CARGO_MANIFEST_DIR")
                )),
                ..hook("claude", e)
            };
            assert!(try_capture(&mut conn, &h, &p, &json!({})).is_err(), "{e:?}");
            assert!(!capture_on(&mut conn, &h, &p, &json!({})));
        }
    }

    #[test]
    fn a_huge_tool_input_is_cut_before_it_is_sent() {
        let (a, b) = UnixStream::pair().unwrap();
        let sent = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(&b).read_line(&mut line).unwrap();
            let _ = (&b).write_all(b"{\"ok\":true,\"stored\":true}\n");
            line
        });
        let mut conn = Conn {
            stream: BufReader::new(a),
            next: 1,
        };
        let p = json!({"tool_name": "Write", "tool_input": {"file_path": "/x/y.rs",
            "content": "x".repeat(2 * 1024 * 1024)}});
        try_capture(
            &mut conn,
            &hook("claude", Event::PostToolUse),
            &p,
            &json!({}),
        )
        .unwrap();
        let line = sent.join().unwrap();
        assert!(line.len() < 8 * 1024, "{}", line.len());
        // What the summary needs is still there.
        let v: Value = serde_json::from_str(&line).unwrap();
        let args: Value = serde_json::from_str(v["args"].as_str().unwrap()).unwrap();
        assert_eq!(args["file_path"], "/x/y.rs");
    }

    #[test]
    fn a_daemon_slow_to_answer_capture_is_not_waited_for() {
        let (a, b) = UnixStream::pair().unwrap();
        // The daemon reads the write and answers it 500 ms later.
        let held = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(&b).read_line(&mut line).unwrap();
            std::thread::sleep(Duration::from_millis(500));
            let _ = (&b).write_all(b"{\"ok\":true}\n");
            line
        });
        let mut conn = Conn {
            stream: BufReader::new(a),
            next: 1,
        };
        let t = std::time::Instant::now();
        let session = json!({"cwd": "/p"});
        assert!(capture_on(
            &mut conn,
            &hook("claude", Event::SessionStart),
            &json!({}),
            &session
        ));
        let took = t.elapsed();
        assert!(took < Duration::from_millis(100), "{took:?}");
        assert!(held.join().unwrap().contains("capture.session_start"));
    }

    fn hook(provider: &'static str, event: Event) -> Hook {
        Hook {
            provider,
            event,
            conversation: "c".into(),
            cwd: "/".into(),
            transcript_path: None,
        }
    }

    #[test]
    fn claude_prefers_the_payload_event_name() {
        let p = json!({"session_id": "s1", "cwd": "/p", "hook_event_name": "Stop"});
        let h = normalize("codex", "post-tool-use", &p).unwrap();
        assert_eq!((h.event, h.conversation.as_str()), (Event::Stop, "s1"));
        let h = normalize("claude", "stop", &json!({"session_id": "s1"})).unwrap();
        assert_eq!(h.event, Event::Stop);
        assert!(normalize(
            "claude",
            "stop",
            &json!({"session_id": "s1", "hook_event_name": "PreToolUse"})
        )
        .is_none());
        assert!(normalize("vim", "stop", &p).is_none());
        assert!(normalize("cursor", "nope", &p).is_none());
        let h = normalize(
            "opencode",
            "stop",
            &json!({"session_id": "ses_1", "cwd": "/p"}),
        )
        .unwrap();
        assert_eq!((h.provider, h.event), ("opencode", Event::Stop));
    }

    #[test]
    fn which_events_take_mail() {
        for e in EVENTS {
            let both = e != Event::SessionEnd;
            let tool_or_stop = matches!(e, Event::PostToolUse | Event::Stop);
            assert_eq!(takes_mail(&hook("claude", e)), both);
            assert_eq!(takes_mail(&hook("codex", e)), both);
            assert_eq!(takes_mail(&hook("cursor", e)), tool_or_stop);
            assert_eq!(takes_mail(&hook("grok", e)), tool_or_stop);
            assert_eq!(takes_mail(&hook("opencode", e)), tool_or_stop);
        }
    }

    #[test]
    fn output_shapes() {
        assert_eq!(
            output(&hook("grok", Event::PostToolUse), "t"),
            json!({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": "t"}})
        );
        assert_eq!(
            output(&hook("codex", Event::UserPromptSubmit), "t"),
            json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "t"}})
        );
        assert_eq!(
            output(&hook("claude", Event::Stop), "t"),
            json!({"decision": "block", "reason": "t"})
        );
        assert_eq!(
            output(&hook("cursor", Event::PostToolUse), "t"),
            json!({"additional_context": "t"})
        );
        for e in [Event::PostToolUse, Event::Stop] {
            assert_eq!(output(&hook("opencode", e), "t"), json!({"text": "t"}));
        }
    }
}
