//! Cursor's, Grok's and OpenCode's hook payloads and replies, written from their own
//! documentation (cursor.com/docs/agent/hooks; the Grok CLI manual) and payloads captured live.
//! OpenCode has no command hooks: our plugin (`setup/opencode.rs`) builds its payload.

use crate::hook::{Event, Hook};
use serde_json::{json, Value};

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// Cursor: `conversation_id` names the conversation (sessionStart/sessionEnd also carry
/// `session_id`); the cwd is `cwd` where given, else the first of `workspace_roots`.
pub fn normalize_cursor(event: Event, p: &Value) -> Option<Hook> {
    Some(Hook {
        provider: "cursor",
        event,
        conversation: text(&p["conversation_id"]).or_else(|| text(&p["session_id"]))?,
        cwd: text(&p["cwd"])
            .or_else(|| text(&p["workspace_roots"][0]))
            .unwrap_or_default(),
        transcript_path: text(&p["transcript_path"]),
    })
}

/// Grok: camelCase fields. Subagent hooks and a `stop` that is not `end_turn` (the
/// `shutdown`/`channel_closed` stops after a session ends) are ignored.
pub fn normalize_grok(event: Event, p: &Value) -> Option<Hook> {
    if !p["subagentType"].is_null() || (event == Event::Stop && p["reason"] != "end_turn") {
        return None;
    }
    Some(Hook {
        provider: "grok",
        event,
        conversation: text(&p["sessionId"])?,
        cwd: text(&p["cwd"])
            .or_else(|| text(&p["workspaceRoot"]))
            .unwrap_or_default(),
        transcript_path: text(&p["transcriptPath"]),
    })
}

/// OpenCode: the payload our plugin sends, `{session_id, cwd}` (it drops child sessions).
pub fn normalize_opencode(event: Event, p: &Value) -> Option<Hook> {
    Some(Hook {
        provider: "opencode",
        event,
        conversation: text(&p["session_id"])?,
        cwd: text(&p["cwd"]).unwrap_or_default(),
        transcript_path: None,
    })
}

/// Cursor reads `followup_message` from stop and `additional_context` from postToolUse.
/// (Grok reads Claude's shapes for its two mail events.)
pub fn cursor_output(event: Event, text: &str) -> Value {
    match event {
        Event::Stop => json!({ "followup_message": text }),
        _ => json!({ "additional_context": text }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/hooks/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn grok_subagent_hook_is_ignored() {
        assert!(
            normalize_grok(Event::PostToolUse, &fixture("grok-post-tool-use-subagent")).is_none()
        );
        let h = normalize_grok(Event::PostToolUse, &fixture("grok-post-tool-use")).unwrap();
        assert_eq!(
            (h.conversation.as_str(), h.cwd.as_str()),
            ("019a0000-0000-7000-8000-0000000000f1", "/tmp/proj")
        );
        assert!(normalize_grok(Event::Stop, &fixture("grok-stop-shutdown")).is_none());
        assert!(normalize_grok(Event::Stop, &fixture("grok-stop")).is_some());
    }

    #[test]
    fn opencode_reads_session_id_and_cwd() {
        for (event, name) in [
            (Event::SessionStart, "opencode-session-start"),
            (Event::PostToolUse, "opencode-post-tool-use"),
            (Event::Stop, "opencode-stop"),
            (Event::SessionEnd, "opencode-session-end"),
        ] {
            let h = normalize_opencode(event, &fixture(name)).unwrap();
            assert_eq!(
                (h.provider, h.event, h.conversation.as_str(), h.cwd.as_str()),
                (
                    "opencode",
                    event,
                    "ses_0a1b2c3d4e5fOpEnCoDe0001",
                    "/tmp/proj"
                )
            );
            assert_eq!(h.transcript_path, None);
        }
    }

    #[test]
    fn opencode_empty_payload_ignored() {
        assert!(normalize_opencode(Event::Stop, &json!({})).is_none());
        assert!(normalize_opencode(Event::Stop, &json!({"session_id": ""})).is_none());
    }

    #[test]
    fn cursor_reads_conversation_id_and_workspace_roots() {
        let h = normalize_cursor(Event::Stop, &fixture("cursor-stop")).unwrap();
        assert_eq!(
            (h.conversation.as_str(), h.cwd.as_str()),
            ("5c1e0000-0000-4000-8000-0000000000e1", "/tmp/proj")
        );
        assert!(normalize_cursor(Event::Stop, &json!({})).is_none());
    }
}
