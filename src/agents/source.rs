//! Which session an MCP tool call comes from. The model never names its own source: it is
//! the registered session of the agent process above the MCP server, and for Codex (one
//! app-server process, many threads) the thread the call's `_meta` names; for OpenCode (one
//! server process, many sessions) the session its `_meta` names.

use crate::os::Process;
use crate::sessions::{Key, Session};
use serde_json::Value;

#[derive(Debug, PartialEq)]
pub enum SourceError {
    /// More than one session fits.
    Ambiguous,
    /// No session fits.
    Unidentified,
}

/// The `_meta` fields a Codex call may carry its thread id in.
const THREAD_FIELDS: [&str; 3] = ["threadId", "sessionId", "conversationId"];

/// The `_meta` field OpenCode 2.x puts its session id in, on every tool call:
/// https://github.com/anomalyco/opencode/blob/v2.1.4/packages/core/src/mcp/client.ts
pub const OPENCODE_SESSION: &str = "ai.opencode/sessionID";

/// The session `parent` (the agent process) is calling from. Sessions are matched by
/// pid + birth; when they are Codex threads, `meta`'s thread id picks one of them.
pub fn resolve(parent: &Process, meta: &Value, sessions: &[Session]) -> Result<Key, SourceError> {
    let mine: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.pid == parent.pid && s.birth_micros == parent.birth_micros)
        .collect();
    let thread = THREAD_FIELDS.iter().find_map(|f| meta[f].as_str());
    let opencode = meta[OPENCODE_SESSION].as_str();
    let picked: Vec<&Session> = match (thread, opencode) {
        // An id none of its sessions has is a subagent's child session (the plugin registers
        // none): with one session, it is that session's call; with several, no guess.
        (_, Some(id)) if mine.iter().any(|s| s.provider == "opencode") => {
            let ours: Vec<&Session> = mine
                .into_iter()
                .filter(|s| s.provider == "opencode")
                .collect();
            match ours.iter().find(|s| s.conversation == id) {
                Some(named) => vec![*named],
                None if ours.len() == 1 => ours,
                None => vec![],
            }
        }
        (Some(t), _) if mine.iter().any(|s| s.provider == "codex") => {
            mine.into_iter().filter(|s| s.conversation == t).collect()
        }
        _ => mine,
    };
    match picked.as_slice() {
        [one] => Ok(one.key.clone()),
        [] => Err(SourceError::Unidentified),
        _ => Err(SourceError::Ambiguous),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{Status, Tier};
    use serde_json::json;
    use std::path::PathBuf;

    fn proc_(pid: u32) -> Process {
        Process {
            pid,
            birth_micros: 7,
            executable: PathBuf::new(),
        }
    }

    fn s(provider: &str, conversation: &str, pid: u32) -> Session {
        Session {
            key: Key::new("m", provider, conversation),
            provider: provider.into(),
            conversation: conversation.into(),
            pid,
            birth_micros: 7,
            cwd: "/".into(),
            title: None,
            doing: None,
            status: Status::Idle,
            tier: Tier::Pull,
            push_via: None,
            handle: None,
            repo: None,
            branch: None,
            hidden: false,
            waiting_for: None,
            kind: None,
            entrypoint: None,
            job: None,
        }
    }

    #[test]
    fn codex_source_is_the_thread_in_meta_not_the_process() {
        let all = [
            s("codex", "t1", 5),
            s("codex", "t2", 5),
            s("codex", "t3", 6),
        ];
        for field in THREAD_FIELDS {
            assert_eq!(
                resolve(&proc_(5), &json!({ field: "t2" }), &all),
                Ok(Key::new("m", "codex", "t2"))
            );
        }
        // A thread of another process, or none registered, is no source.
        assert_eq!(
            resolve(&proc_(5), &json!({"threadId": "t3"}), &all),
            Err(SourceError::Unidentified)
        );
        // One registered thread needs no meta.
        assert_eq!(
            resolve(&proc_(6), &json!({}), &all),
            Ok(Key::new("m", "codex", "t3"))
        );
    }

    #[test]
    fn opencode_source_is_the_session_in_its_meta() {
        let all = [
            s("opencode", "ses_a", 5),
            s("opencode", "ses_b", 5),
            s("opencode", "ses_c", 6),
        ];
        let meta = |id: &str| json!({ OPENCODE_SESSION: id });
        assert_eq!(
            resolve(&proc_(5), &meta("ses_b"), &all),
            Ok(Key::new("m", "opencode", "ses_b"))
        );
        // An id none of its several sessions has (another process's, or a subagent's) is no
        // source: never a guess.
        assert_eq!(
            resolve(&proc_(5), &meta("ses_c"), &all),
            Err(SourceError::Unidentified)
        );
        // With one session, an unknown id (its subagent's child session) is that session.
        assert_eq!(
            resolve(&proc_(6), &meta("ses_child"), &all),
            Ok(Key::new("m", "opencode", "ses_c"))
        );
        // Without the meta, one OpenCode process hosting several sessions is ambiguous.
        assert_eq!(
            resolve(&proc_(5), &json!({}), &all),
            Err(SourceError::Ambiguous)
        );
        // Codex's thread fields do not pick an OpenCode session.
        assert_eq!(
            resolve(&proc_(5), &json!({"sessionId": "ses_a"}), &all),
            Err(SourceError::Ambiguous)
        );
    }

    #[test]
    fn the_opencode_meta_key_means_nothing_to_another_agent() {
        let all = [s("claude", "c1", 5), s("codex", "t1", 6)];
        assert_eq!(
            resolve(&proc_(5), &json!({ OPENCODE_SESSION: "ses_x" }), &all),
            Ok(Key::new("m", "claude", "c1"))
        );
        assert_eq!(
            resolve(&proc_(6), &json!({ OPENCODE_SESSION: "ses_x" }), &all),
            Ok(Key::new("m", "codex", "t1"))
        );
    }

    #[test]
    fn two_sessions_on_one_pid_without_thread_meta_is_ambiguous() {
        let all = [s("codex", "t1", 5), s("codex", "t2", 5)];
        assert_eq!(
            resolve(&proc_(5), &json!({"other": 1}), &all),
            Err(SourceError::Ambiguous)
        );
        let all = [s("claude", "c1", 5), s("claude", "c2", 5)];
        assert_eq!(
            resolve(&proc_(5), &json!({}), &all),
            Err(SourceError::Ambiguous)
        );
    }

    #[test]
    fn claude_is_its_process_and_ignores_meta() {
        let all = [s("claude", "c1", 5), s("grok", "g1", 6)];
        assert_eq!(
            resolve(&proc_(5), &json!({"sessionId": "x"}), &all),
            Ok(Key::new("m", "claude", "c1"))
        );
        assert_eq!(
            resolve(&proc_(9), &json!({}), &all),
            Err(SourceError::Unidentified)
        );
        // Same pid, other birth: a reused pid is not the session.
        let reborn = Process {
            birth_micros: 8,
            ..proc_(5)
        };
        assert_eq!(
            resolve(&reborn, &json!({}), &all),
            Err(SourceError::Unidentified)
        );
    }
}
