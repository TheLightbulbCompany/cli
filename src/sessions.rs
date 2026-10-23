//! Sessions: the coding-agent conversations on this machine, registered or discovered.

use crate::agents::codex;
use crate::{doing, os, store};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// `"<member_key_short>/<provider>:<conversation>"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Key(pub String);

impl Key {
    /// `member_short` is the 12-character lowercase base32 member prefix.
    pub fn new(member_short: &str, provider: &str, conversation: &str) -> Key {
        Key(format!("{member_short}/{provider}:{conversation}"))
    }
}

pub const PROVIDERS: [&str; 4] = ["claude", "codex", "cursor", "grok"];

/// `[A-Za-z0-9._-]{1,max}`.
pub fn word(s: &str, max: usize) -> bool {
    (1..=max).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// A name address's name (`listen --as`): `[A-Za-z0-9._-]{1,64}`.
pub fn valid_name(s: &str) -> bool {
    word(s, 64)
}

/// An address as `(member prefix, the rest)`; `None` without a `/`.
pub fn split_addr(a: &str) -> Option<(&str, &str)> {
    a.split_once('/')
}

/// The one address grammar: `to` as `<member>/<provider>:<conversation>` or `<member>/<name>`,
/// a bare name taken as `member`'s. `None` for anything else (groups included).
pub fn qualify(member: &str, to: &str) -> Option<String> {
    if valid_name(to) {
        return Some(format!("{member}/{to}"));
    }
    let (m, rest) = split_addr(to)?;
    let ok = valid_name(m)
        && match rest.split_once(':') {
            Some((p, c)) => PROVIDERS.contains(&p) && word(c, 128),
            None => valid_name(rest),
        };
    ok.then(|| to.to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Live,
    Idle,
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Push,
    NextStep,
    Pull,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Push, Tier::NextStep, Tier::Pull];

    /// The wire name (as serde writes it).
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Push => "push",
            Tier::NextStep => "next_step",
            Tier::Pull => "pull",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub key: Key,
    pub provider: String,
    pub conversation: String,
    pub pid: u32,
    pub birth_micros: u64,
    pub cwd: String,
    pub title: Option<String>,
    /// Computed at read time from the agent's transcript, never stored.
    pub doing: Option<String>,
    pub status: Status,
    /// Computed at read time: `push` when the provider's push channel is reachable.
    pub tier: Tier,
    /// The push channel: Claude's messaging socket, or the Codex app-server daemon binary
    /// that serves the thread. `tier` is `push` exactly when this is set.
    #[serde(skip)]
    pub push_via: Option<PathBuf>,
}

/// Alive means the pid exists AND was born at `birth_micros`; a reused pid is not the session.
pub fn alive(pid: u32, birth_micros: u64) -> bool {
    os::process(pid).is_ok_and(|p| p.birth_micros == birth_micros)
}

/// A stored (registered) row as a `Session`; `Gone` when its process is not the registered one.
/// A Codex thread is push tier when its process (the executable the hook recorded, else the
/// pid's) is Codex's app-server daemon under `home` (`codex::daemon_program`).
fn from_row(r: &store::Session, home: &Path) -> Option<Session> {
    let (provider, conversation) = (r.provider.clone(), r.conversation.clone()?);
    let (pid, birth) = (
        u32::try_from(r.pid?).ok()?,
        u64::try_from(r.birth_micros?).ok()?,
    );
    let push_via = (provider == "codex")
        .then(|| {
            r.executable
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| os::process(pid).ok().map(|p| p.executable))
        })
        .flatten()
        .and_then(|exe| codex::daemon_program(home, &exe));
    Some(Session {
        key: Key(r.key.clone()),
        tier: if push_via.is_some() {
            Tier::Push
        } else {
            Tier::NextStep
        },
        push_via,
        provider,
        conversation,
        pid,
        birth_micros: birth,
        cwd: r.cwd.clone(),
        title: r.title.clone(),
        doing: None,
        status: if !alive(pid, birth) {
            Status::Gone
        } else if r.status == "live" {
            Status::Live
        } else {
            Status::Idle
        },
    })
}

/// Claude Code writes `<home>/.claude/sessions/<pid>.json` for each running process.
/// Lists those whose pid is alive and was born when the file says it started (±10 s).
/// Dead ones are skipped and left alone: Claude owns the files.
pub fn scan_claude(home: &Path, member_short: &str) -> Vec<Session> {
    let Ok(dir) = std::fs::read_dir(home.join(".claude/sessions")) else {
        return vec![];
    };
    let mut out = vec![];
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(v) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        else {
            continue;
        };
        let (Some(pid), Some(conversation), Some(started_ms)) = (
            v["pid"].as_u64().and_then(|p| u32::try_from(p).ok()),
            v["sessionId"].as_str(),
            v["startedAt"].as_u64(),
        ) else {
            continue;
        };
        let Ok(p) = os::process(pid) else { continue };
        if (p.birth_micros / 1000).abs_diff(started_ms) > 10_000 {
            continue;
        }
        // Pushing checks the socket's peer pid, which only `os::PEER_PID` platforms can.
        let socket = v["messagingSocketPath"]
            .as_str()
            .map(PathBuf::from)
            .filter(|s| os::PEER_PID && s.exists());
        out.push(Session {
            key: Key::new(member_short, "claude", conversation),
            provider: "claude".into(),
            conversation: conversation.into(),
            pid,
            birth_micros: p.birth_micros,
            cwd: v["cwd"].as_str().unwrap_or_default().into(),
            title: v["name"].as_str().map(String::from),
            doing: None,
            status: if v["status"] == "busy" {
                Status::Live
            } else {
                Status::Idle
            },
            tier: if socket.is_some() {
                Tier::Push
            } else {
                Tier::NextStep
            },
            push_via: socket,
        });
    }
    out
}

/// Registered rows plus Claude discoveries, deduplicated by key (a discovery wins:
/// its status is fresher), `Gone` dropped, `title`/`doing` filled from the transcript.
pub fn all(rows: &[store::Session], home: &Path, member_short: &str) -> Vec<Session> {
    let mut out: Vec<Session> = rows.iter().filter_map(|r| from_row(r, home)).collect();
    for found in scan_claude(home, member_short) {
        match out.iter_mut().find(|s| s.key == found.key) {
            Some(s) => {
                *s = Session {
                    title: found.title.or(s.title.take()),
                    ..found
                }
            }
            None => out.push(found),
        }
    }
    out.retain(|s| s.status != Status::Gone);
    for s in &mut out {
        let (title, doing) = doing::title_and_todo(home, s);
        s.title = title.or(s.title.take());
        s.doing = doing;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn key_is_member_slash_provider_colon_conversation() {
        assert_eq!(
            Key::new("k3f9ab7cd2ef", "codex", "01a1").0,
            "k3f9ab7cd2ef/codex:01a1"
        );
    }

    #[test]
    fn scan_claude_lists_live_sessions_with_cwd_and_status() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".claude/sessions");
        fs::create_dir_all(&dir).unwrap();
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-sessions");
        fs::copy(fixtures.join("999999.json"), dir.join("999999.json")).unwrap();

        let me = std::process::id();
        let birth = os::process(me).unwrap().birth_micros;
        fs::write(
            dir.join(format!("{me}.json")),
            format!(
                r#"{{"pid":{me},"sessionId":"live-conv","cwd":"/tmp/proj","startedAt":{},"kind":"interactive","name":"live one","status":"busy"}}"#,
                birth / 1000 + 500
            ),
        )
        .unwrap();
        // Same pid, but born long before the file says: a reused pid is not the session.
        fs::write(
            dir.join("reused.json"),
            format!(
                r#"{{"pid":{me},"sessionId":"stale","cwd":"/","startedAt":{}}}"#,
                birth / 1000 - 3_600_000
            ),
        )
        .unwrap();

        let found = scan_claude(home.path(), "k3f9ab7cd2ef");
        assert_eq!(found.len(), 1);
        let s = &found[0];
        assert_eq!(s.key.0, "k3f9ab7cd2ef/claude:live-conv");
        assert_eq!(
            (s.pid, s.birth_micros, s.cwd.as_str()),
            (me, birth, "/tmp/proj")
        );
        assert_eq!(
            (s.status, s.tier, s.title.as_deref()),
            (Status::Live, Tier::NextStep, Some("live one"))
        );
        assert!(dir.join("999999.json").exists(), "Claude owns its files");
    }

    #[test]
    fn wire_names_are_snake_case_and_lowercase() {
        assert_eq!(
            serde_json::to_string(&Tier::NextStep).unwrap(),
            "\"next_step\""
        );
        assert_eq!(serde_json::to_string(&Status::Gone).unwrap(), "\"gone\"");
        for t in Tier::ALL {
            assert_eq!(serde_json::to_value(t).unwrap(), t.as_str());
        }
    }
}
