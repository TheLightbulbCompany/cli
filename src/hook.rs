//! `hook <agent> <event>`: run by an agent's hook config with the agent's payload on stdin.
//! Registers or updates the session, ends it on SessionEnd, and hands waiting mail to the
//! agent in the shape that agent reads. A client of the daemon, like every other.

use crate::agents::caller::{self, CallerError};
use crate::agents::cursor_grok;
use crate::os::Process;
use crate::{daemon, paths, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Debug builds only: `<pid>:<birth_micros>` stands in for the calling agent (tests).
/// Compiled out of release builds.
#[cfg(debug_assertions)]
pub const CALLER_ENV: &str = "LIGHTBULB_NETWORK_HOOK_CALLER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PostToolUse,
    Stop,
    SessionEnd,
}

const EVENTS: [Event; 5] = [
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
    fn arg(self) -> &'static str {
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
    let caller = calling_agent(agent);
    if h.event == Event::SessionEnd {
        // The agent may have exited before its SessionEnd hook ran, so the caller check
        // can fail; then the daemon ends the row only if its recorded process is gone too.
        daemon::ensure_running()?;
        let mut conn = Conn::open()?;
        let short = conn.call("status", json!({}))?["member_key_short"]
            .as_str()
            .ok_or("status without member_key_short")?
            .to_string();
        let key = crate::sessions::Key::new(&short, h.provider, &h.conversation);
        let r = conn.call("end", json!({ "key": key, "if_gone": caller.is_err() }))?;
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
    let r = conn.call(
        "register",
        json!({ "session": { "provider": h.provider, "conversation": h.conversation,
            "pid": me.pid, "birth_micros": me.birth_micros, "cwd": h.cwd, "status": status } }),
    )?;
    let key = r["key"].clone();
    if !key.is_string() {
        return Err(format!("register: {r}").into());
    }
    if !takes_mail(&h) {
        return Ok(None);
    }
    let r = conn.call("inbox", json!({ "key": key }))?;
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

fn calling_agent(agent: &str) -> std::result::Result<Process, CallerError> {
    #[cfg(debug_assertions)]
    {
        if let Some((pid, birth)) = std::env::var(CALLER_ENV)
            .ok()
            .as_deref()
            .and_then(|v| v.split_once(':'))
        {
            let bad = |_| CallerError::Lookup(format!("{CALLER_ENV} is not <pid>:<birth_micros>"));
            return Ok(Process {
                pid: pid.parse().map_err(bad)?,
                birth_micros: birth.parse().map_err(bad)?,
                executable: std::path::PathBuf::new(),
            });
        }
    }
    caller::identify(agent)
}

/// One blocking connection to the daemon.
struct Conn {
    stream: BufReader<UnixStream>,
    next: u64,
}

impl Conn {
    fn open() -> Result<Conn> {
        let stream = UnixStream::connect(paths::socket())?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(Conn {
            stream: BufReader::new(stream),
            next: 1,
        })
    }

    fn call(&mut self, op: &str, mut body: Value) -> Result<Value> {
        body["id"] = self.next.into();
        body["op"] = op.into();
        self.next += 1;
        self.stream
            .get_mut()
            .write_all(format!("{body}\n").as_bytes())?;
        let mut line = String::new();
        self.stream.read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
