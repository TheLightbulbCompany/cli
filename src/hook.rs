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
        _ => None,
    }
}

/// Whether this event hands mail to the model.
pub fn takes_mail(h: &Hook) -> bool {
    match (h.provider, h.event) {
        (_, Event::SessionEnd) => false,
        ("cursor" | "grok", e) => matches!(e, Event::PostToolUse | Event::Stop),
        _ => true,
    }
}

/// What the agent reads from stdout to see `text`.
pub fn output(h: &Hook, text: &str) -> Value {
    match (h.provider, h.event) {
        ("cursor", e) => cursor_grok::cursor_output(e, text),
        (_, Event::Stop) => json!({ "decision": "block", "reason": text }),
        (_, e) => json!({
            "hookSpecificOutput": { "hookEventName": e.name(), "additionalContext": text }
        }),
    }
}

/// Runs one hook; `Ok(Some(stdout))` when there is something for the agent.
pub fn run(agent: &str, event: &str, stdin: &str) -> Result<Option<String>> {
    let Some(h) = normalize(agent, event, &serde_json::from_str(stdin)?) else {
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
    if !takes_mail(&h) {
        return Ok(None);
    }
    let r = conn.call("inbox", json!({ "key": key, "caller": proc_json(&me) }))?;
    let framed: Vec<&str> = r["messages"]
        .as_array()
        .ok_or_else(|| format!("inbox: {r}"))?
        .iter()
        .filter_map(|m| m["framed"].as_str())
        .collect();
    if framed.is_empty() {
        return Ok(None);
    }
    Ok(Some(output(&h, &framed.join("\n\n")).to_string()))
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
            Ok(Process {
                pid: pid.parse().map_err(bad)?,
                birth_micros: birth.parse().map_err(bad)?,
                executable: std::path::PathBuf::new(),
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
    }
}
