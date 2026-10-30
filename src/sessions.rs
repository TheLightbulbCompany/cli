//! Sessions: the coding-agent conversations on this machine, registered or discovered.

use crate::agents::{codex, listing};
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

pub const PROVIDERS: [&str; 5] = ["claude", "codex", "cursor", "grok", "opencode"];

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

/// An address's member prefix; `None` without a `/`.
pub fn member_of(a: &str) -> Option<&str> {
    split_addr(a).map(|(m, _)| m)
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
    /// Stopped for its person (a permission prompt, a question).
    Waiting,
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
    /// The short name `who` shows and `send` takes (`codex-1`): its row's place (`store`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// Its repo and branch as its hook read them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Computed at read time (`ipc::live_sessions`): kept from teammates (its folder is
    /// hidden, or its register was refused for one). Local listings show it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// What a `waiting` session waits for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// Claude's own kind and entrypoint for it (`interactive`, `cli`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
    /// The background job it belongs to, as Claude lists it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<Job>,
}

/// A background job of a session: its id and state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub state: String,
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
        handle: r.handle.clone(),
        repo: r.repo.clone(),
        branch: r.branch.clone(),
        hidden: false,
        waiting_for: r.waiting_for.clone(),
        kind: None,
        entrypoint: None,
        job: None,
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
        } else if r.status == "waiting" {
            Status::Waiting
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
        let (Ok(p), Ok(started)) = (os::process(pid), os::started_ms(pid)) else {
            continue;
        };
        if started.abs_diff(started_ms) > 10_000 {
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
            handle: None,
            repo: None,
            branch: None,
            hidden: false,
            waiting_for: None,
            kind: v["kind"].as_str().map(String::from),
            entrypoint: v["entrypoint"].as_str().map(String::from),
            job: None,
        });
    }
    out
}

/// Registered rows plus Claude discoveries, deduplicated by key (a discovery wins for its
/// status, which is fresher, except that a hook's `waiting` stands until the discovery says
/// `idle`; the row for its folder, which its hook reported resolved, and its handle, repo and
/// branch), `Gone` dropped, `title`/`doing` filled from the transcript. Discoveries are Claude's
/// listing (`listing::to_sessions`) when `listed` is `Some`, the pid files (`scan_claude`) when
/// `None`; the pid files supply the push socket and entrypoint either way. A session found only
/// by discovery keeps Claude's own cwd as given (not resolved) and has no handle until its hook
/// registers it.
pub fn all(
    rows: &[store::Session],
    listed: Option<&[listing::Listed]>,
    home: &Path,
    member_short: &str,
) -> Vec<Session> {
    let mut out: Vec<Session> = rows.iter().filter_map(|r| from_row(r, home)).collect();
    let scanned = scan_claude(home, member_short);
    let found = match listed {
        Some(listed) => {
            let sockets = scanned
                .iter()
                .filter_map(|s| Some((s.conversation.clone(), s.push_via.clone()?)))
                .collect();
            let mut found = listing::to_sessions(listed, member_short, &sockets);
            for f in &mut found {
                f.entrypoint = scanned
                    .iter()
                    .find(|s| s.conversation == f.conversation)
                    .and_then(|s| s.entrypoint.clone());
            }
            found
        }
        None => scanned,
    };
    for found in found {
        match out.iter_mut().find(|s| s.key == found.key) {
            Some(s) => {
                let hook_waits = s.status == Status::Waiting && found.status != Status::Idle;
                *s = Session {
                    title: found.title.or(s.title.take()),
                    cwd: std::mem::take(&mut s.cwd),
                    handle: s.handle.take(),
                    repo: s.repo.take(),
                    branch: s.branch.take(),
                    status: if hook_waits {
                        Status::Waiting
                    } else {
                        found.status
                    },
                    waiting_for: if hook_waits {
                        s.waiting_for.take()
                    } else {
                        found.waiting_for
                    },
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
    fn an_opencode_address_qualifies() {
        let a = "k3f9ab7cd2ef/opencode:ses_0a1b2c";
        assert_eq!(qualify("k3f9ab7cd2ef", a).as_deref(), Some(a));
    }

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
        let started = os::started_ms(me).unwrap();
        fs::write(
            dir.join(format!("{me}.json")),
            format!(
                r#"{{"pid":{me},"sessionId":"live-conv","cwd":"/tmp/proj","startedAt":{},"kind":"interactive","name":"live one","status":"busy"}}"#,
                started + 500
            ),
        )
        .unwrap();
        // Same pid, but born long before the file says: a reused pid is not the session.
        fs::write(
            dir.join("reused.json"),
            format!(
                r#"{{"pid":{me},"sessionId":"stale","cwd":"/","startedAt":{}}}"#,
                started - 3_600_000
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
