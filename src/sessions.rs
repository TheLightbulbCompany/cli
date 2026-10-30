//! Sessions: the coding-agent conversations on this machine, registered or discovered.

use crate::agents::{codex, listing};
use crate::{doing, os, store};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    /// A `claude -p` or Agent SDK run with a row (`store::Session::headless`): it can be told
    /// apart and messaged, and no list shows it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub headless: bool,
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

impl Session {
    /// A `claude -p` or Agent SDK run (Claude's entrypoint `sdk-*`), by its pid file: never
    /// discovered. Its hook registers it, headless.
    pub fn sdk_run(&self) -> bool {
        self.entrypoint
            .as_deref()
            .is_some_and(|e| e.starts_with("sdk-"))
    }
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
        headless: r.headless,
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
/// Lists those whose pid is alive and was born when the file says it started (±10 s), and that
/// are not parked into the agent view.
/// Dead ones are skipped and left alone: Claude owns the files.
pub fn scan_claude(home: &Path, member_short: &str) -> Vec<Session> {
    let files = pid_files(home, member_short).into_iter();
    files
        .filter(|(_, parked, _)| !parked)
        .map(|(s, ..)| s)
        .collect()
}

/// Every verified pid file as a session, whether it is parked into the agent view (then the
/// terminal is not the session any more), and the unix second Claude last updated it.
fn pid_files(home: &Path, member_short: &str) -> Vec<(Session, bool, Option<i64>)> {
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
        let parked = v["parkedJobId"].as_str().is_some_and(|j| !j.is_empty());
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
        let session = Session {
            key: Key::new(member_short, "claude", conversation),
            provider: "claude".into(),
            conversation: conversation.into(),
            pid,
            birth_micros: p.birth_micros,
            cwd: v["cwd"].as_str().unwrap_or_default().into(),
            title: v["name"].as_str().map(String::from),
            doing: None,
            // `shell`: Claude itself reports it as busy.
            status: match v["status"].as_str() {
                Some("busy" | "shell") => Status::Live,
                Some("waiting") => Status::Waiting,
                _ => Status::Idle,
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
            headless: false,
            waiting_for: v["waitingFor"]
                .as_str()
                .filter(|_| v["status"] == "waiting")
                .map(|w| listing::waiting_word(w).into()),
            kind: v["kind"].as_str().map(String::from),
            entrypoint: v["entrypoint"].as_str().map(String::from),
            job: None,
        };
        let updated = v["updatedAt"].as_i64().map(|ms| ms / 1000);
        out.push((session, parked, updated));
    }
    out
}

/// `s` as one shell word.
pub fn shell_quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The command that resumes an ended conversation, on its owner's computer, from any folder: an
/// agent looks its conversations up by the folder it starts in.
pub fn resume(provider: &str, conversation: &str, cwd: &str) -> Option<String> {
    let command = match provider {
        "claude" => "claude --resume",
        "codex" => "codex resume",
        "cursor" => "cursor-agent --resume",
        "grok" => "grok -r",
        "opencode" => "opencode -s",
        _ => return None,
    };
    Some(if cwd.is_empty() {
        format!("{command} {conversation}")
    } else {
        format!("cd {} && {command} {conversation}", shell_quoted(cwd))
    })
}

/// How long after a row's last word a listing without it counts against it: a conversation just
/// started or a hook just fired may not be listed yet.
const QUIET_SECS: i64 = 15;

/// Whether a live process has left a Claude conversation that no live listed row and no verified
/// pid file holds: its own verified pid file names another or is parked (at once); or it has no
/// such file and a listing read `QUIET_SECS` after the row's `seen_at` lacks it. With neither,
/// nothing is known against it and it stays. A `waiting` row has only the first: it may sit quiet
/// for hours on its person, and that is no sign it has gone.
/// ponytail: a session under a custom `CLAUDE_CONFIG_DIR` is not in the default listing, so it
/// ends after 15 s quiet and revives on its next hook, unless it is `waiting`, which stays until
/// its process exits. Ask each config dir's listing if that shows up in use.
fn moved_on(has_pid_file: bool, waiting: bool, read_at: Option<i64>, seen_at: i64) -> bool {
    has_pid_file || (!waiting && read_at.is_some_and(|at| at >= seen_at + QUIET_SECS))
}

/// Whether a verified pid file, parked or not, says Claude is running here.
pub fn any_pid_file(home: &Path) -> bool {
    !pid_files(home, "").is_empty()
}

/// Registered rows that are still theirs plus Claude discoveries, deduplicated by key (a discovery wins for its
/// status, except that a hook's `waiting` stands over anything read before the hook: a listing
/// read that began after it, or a pid file updated after it, is the newer word;
/// the row for its folder, which its hook reported resolved, and its handle, repo and branch),
/// `Gone` dropped, `title`/`doing` filled from the transcript. Discoveries are Claude's listing
/// (`listing::to_sessions`) when `listed` is `Some`, the pid files (`scan_claude`) when `None`.
/// The pid files supply the push socket and entrypoint either way, and the listing ends nothing
/// they say is alive: a verified pid file it has no live row for stays, so `[]`, or a Claude that
/// stops listing a kind of row, degrades to them. An SDK run (`Session::sdk_run`) is no session,
/// whichever says it. A session found only
/// by discovery keeps Claude's own cwd as given (not resolved) and has no handle until its hook
/// registers it (`ipc::live_sessions` writes its row, which gives it one).
/// `listed` comes with the unix second its read began. A live process alone does not keep a
/// Claude row, since `/clear`, `/resume` and the agent view leave it running on another
/// conversation; but a row needs evidence to go (`moved_on`). Other providers' rows live with
/// their process, as does a headless row.
pub fn all(
    rows: &[store::Session],
    listed: Option<(&[listing::Listed], i64)>,
    home: &Path,
    member_short: &str,
) -> Vec<Session> {
    let files = pid_files(home, member_short);
    let filed_pids: Vec<u32> = files.iter().map(|(s, ..)| s.pid).collect();
    let updated: HashMap<String, i64> = files
        .iter()
        .filter_map(|(s, _, at)| Some((s.conversation.clone(), (*at)?)))
        .collect();
    let mut scanned: Vec<Session> = files
        .into_iter()
        .filter(|(_, parked, _)| !parked)
        .map(|(s, ..)| s)
        .collect();
    let read_at = listed.map(|(_, at)| at);
    let live = listed.map(|(listed, _)| listing::to_sessions(listed, member_short, &scanned));
    scanned.retain(|s| !s.sdk_run());
    let held = |c: &str| {
        let mut holders = live.iter().flatten().chain(&scanned);
        holders.any(|s| s.conversation == c)
    };
    let mut out: Vec<Session> = rows
        .iter()
        .filter_map(|r| Some((from_row(r, home)?, r.seen_at)))
        .filter(|(s, seen_at)| {
            s.provider != "claude"
                || s.headless
                || s.status == Status::Gone
                || held(&s.conversation)
                || !moved_on(
                    filed_pids.contains(&s.pid),
                    s.status == Status::Waiting,
                    read_at,
                    *seen_at,
                )
        })
        .map(|(s, _)| s)
        .collect();
    // Each discovery, and whether it is the listing's word.
    let found: Vec<(Session, bool)> = match live {
        Some(live) => {
            let kept: Vec<Session> = scanned
                .into_iter()
                .filter(|s| live.iter().all(|f| f.key != s.key))
                .collect();
            let live = live.into_iter().map(|s| (s, true));
            live.chain(kept.into_iter().map(|s| (s, false))).collect()
        }
        None => scanned.into_iter().map(|s| (s, false)).collect(),
    };
    for (found, from_listing) in found {
        match out.iter_mut().find(|s| s.key == found.key) {
            Some(s) => {
                // The unix second this discovery's status is known as of.
                let known_at = if from_listing {
                    read_at
                } else {
                    updated.get(&found.conversation).copied()
                };
                let row = rows.iter().find(|r| r.key == found.key.0);
                let newer = known_at.zip(row).is_some_and(|(at, r)| at > r.seen_at);
                let hook_waits = s.status == Status::Waiting && !newer;
                *s = Session {
                    title: found.title.or(s.title.take()),
                    cwd: std::mem::take(&mut s.cwd),
                    handle: s.handle.take(),
                    repo: s.repo.take(),
                    branch: s.branch.take(),
                    headless: s.headless,
                    status: if hook_waits {
                        Status::Waiting
                    } else {
                        found.status
                    },
                    // Still waiting, by Claude's word too: the hook may know what for.
                    waiting_for: if hook_waits || found.status == Status::Waiting {
                        found.waiting_for.or(s.waiting_for.take())
                    } else {
                        None
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

    /// A pid file of this test process in `home`, with `extra` JSON fields.
    fn pid_file(home: &Path, conversation: &str, extra: &str) {
        let dir = home.join(".claude/sessions");
        fs::create_dir_all(&dir).unwrap();
        let me = std::process::id();
        fs::write(
            dir.join(format!("{conversation}.json")),
            format!(
                r#"{{"pid":{me},"sessionId":"{conversation}","cwd":"/tmp","startedAt":{},"status":"busy"{extra}}}"#,
                os::started_ms(me).unwrap()
            ),
        )
        .unwrap();
    }

    /// The listing cannot end a process Claude's own pid file says is alive; `[]` degrades to
    /// the pid files. Parked terminals and SDK runs are not such sessions.
    #[test]
    fn what_the_listing_leaves_out_stays_if_its_pid_file_is_verified() {
        let home = tempfile::tempdir().unwrap();
        pid_file(home.path(), "kept", "");
        pid_file(home.path(), "in-a-shell", r#","status":"shell""#);
        pid_file(home.path(), "empty-park", r#","parkedJobId":"""#);
        pid_file(home.path(), "parked", r#","parkedJobId":"ab12""#);
        pid_file(home.path(), "sdk", r#","entrypoint":"sdk-ts""#);
        let live = all(&[], Some((&[], 0)), home.path(), "m");
        let mut names: Vec<&str> = live.iter().map(|s| s.conversation.as_str()).collect();
        names.sort();
        assert_eq!(names, ["empty-park", "in-a-shell", "kept"]);
        assert!(live.iter().all(|s| s.status == Status::Live));
    }

    fn row(conversation: &str, provider: &str) -> store::Session {
        let me = os::process(std::process::id()).unwrap();
        store::Session {
            key: format!("m/{provider}:{conversation}"),
            provider: provider.into(),
            conversation: Some(conversation.into()),
            pid: Some(me.pid.into()),
            birth_micros: Some(me.birth_micros as i64),
            cwd: "/tmp".into(),
            title: None,
            status: "idle".into(),
            seen_at: 1,
            executable: None,
            repo: None,
            branch: None,
            handle: None,
            reason: None,
            ended_at: None,
            waiting_for: None,
            headless: false,
        }
    }

    fn live_of(
        rows: &[store::Session],
        listed: Option<(&[listing::Listed], i64)>,
        home: &Path,
    ) -> Vec<String> {
        let mut names: Vec<String> = all(rows, listed, home, "m")
            .into_iter()
            .map(|s| s.conversation)
            .collect();
        names.sort();
        names
    }

    /// The process lives on after `/clear`: only knowing better ends its old conversation.
    #[test]
    fn a_live_process_keeps_its_claude_row_only_while_nothing_says_otherwise() {
        let home = tempfile::tempdir().unwrap();
        let rows = [row("old", "claude"), row("c", "codex")];
        // Nothing known: the listing is out and no pid file speaks for the process.
        assert_eq!(live_of(&rows, None, home.path()), ["c", "old"]);
        // A listing without it, read too soon after the row's last word (`seen_at` 1) to count.
        let seen = rows[0].seen_at;
        assert_eq!(
            live_of(&rows, Some((&[], seen + 14)), home.path()),
            ["c", "old"]
        );
        assert_eq!(live_of(&rows, Some((&[], seen + 15)), home.path()), ["c"]);
        // Waiting on its person: quiet is no evidence, however long.
        let waiting = [store::Session {
            status: "waiting".into(),
            ..rows[0].clone()
        }];
        assert_eq!(
            live_of(&waiting, Some((&[], seen + 3_600)), home.path()),
            ["old"]
        );
        // The pid file names the process on another conversation: at once, listing or not.
        pid_file(home.path(), "new", "");
        assert_eq!(live_of(&rows, None, home.path()), ["c", "new"]);
        // Parked on the row's own conversation: the terminal has left it.
        pid_file(
            home.path(),
            "new",
            r#","sessionId":"old","parkedJobId":"ab12""#,
        );
        assert_eq!(live_of(&rows, None, home.path()), ["c"]);
        // An SDK run's pid file holds nothing, and is not discovered: not a session.
        pid_file(home.path(), "old", r#","entrypoint":"sdk-ts""#);
        assert_eq!(live_of(&rows, Some((&[], seen + 15)), home.path()), ["c"]);
        assert_eq!(live_of(&rows, None, home.path()), ["c"]);
        // Registered by its hook, headless: it lives with its process, whatever its pid file says.
        let headless = [store::Session {
            headless: true,
            ..rows[0].clone()
        }];
        assert_eq!(
            live_of(&headless, Some((&[], seen + 15)), home.path()),
            ["old"]
        );
    }

    /// One listed Claude session of this test process.
    fn listed_row(conversation: &str, status: &str) -> listing::Listed {
        listing::Listed {
            conversation: conversation.into(),
            pid: Some(std::process::id()),
            kind: "interactive".into(),
            status: Some(status.into()),
            waiting_for: None,
            state: None,
            job_id: None,
            name: None,
            cwd: "/tmp".into(),
            started_ms: 0,
        }
    }

    /// A listed SDK run is dropped by its pid file's entrypoint.
    #[test]
    fn the_listing_drops_an_sdk_run() {
        let home = tempfile::tempdir().unwrap();
        pid_file(home.path(), "sdk", r#","entrypoint":"sdk-cli""#);
        pid_file(home.path(), "term", r#","entrypoint":"cli""#);
        let rows = [listed_row("sdk", "busy"), listed_row("term", "busy")];
        assert_eq!(live_of(&[], Some((&rows, 0)), home.path()), ["term"]);
    }

    /// A hook's `waiting` holds over a read that began before it, whatever the read says, and
    /// gives way to one that began after it.
    #[test]
    fn a_hook_waiting_gives_way_only_to_a_newer_word() {
        let home = tempfile::tempdir().unwrap();
        let waits = [store::Session {
            status: "waiting".into(),
            waiting_for: Some("permission prompt".into()),
            seen_at: 100,
            ..row("w", "claude")
        }];
        let status = |word: &str, at: i64| {
            let rows = [listed_row("w", word)];
            let s = all(&waits, Some((&rows, at)), home.path(), "m");
            (s[0].status, s[0].waiting_for.clone())
        };
        let held = (Status::Waiting, Some("permission prompt".to_string()));
        assert_eq!(status("busy", 99), held);
        assert_eq!(status("busy", 100), held);
        assert_eq!(status("idle", 100), held);
        assert_eq!(status("busy", 101), (Status::Live, None));
        assert_eq!(status("idle", 101), (Status::Idle, None));
        // Claude says waiting too, with no word for what: the hook's stays.
        assert_eq!(status("waiting", 101), held);

        // No listing: a pid file updated after the hook wins; one with no stamp, or older, not.
        let pid_status = |extra: &str| {
            pid_file(home.path(), "w", extra);
            all(&waits, None, home.path(), "m")[0].status
        };
        assert_eq!(pid_status(""), Status::Waiting);
        assert_eq!(pid_status(r#","updatedAt":100900"#), Status::Waiting);
        assert_eq!(pid_status(r#","updatedAt":101000"#), Status::Live);
    }

    #[test]
    fn resume_starts_in_the_conversations_folder() {
        assert_eq!(
            resume("claude", "c1", "/tmp/my proj").as_deref(),
            Some("cd '/tmp/my proj' && claude --resume c1")
        );
        assert_eq!(
            resume("codex", "t1", "/tmp/it's").as_deref(),
            Some(r"cd '/tmp/it'\''s' && codex resume t1")
        );
        assert_eq!(resume("grok", "g", "").as_deref(), Some("grok -r g"));
        assert_eq!(resume("vim", "x", "/tmp"), None);
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
