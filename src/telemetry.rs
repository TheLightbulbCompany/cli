//! Anonymous, opt-out usage counts. Every event is built by `build` from a typed `Event`: there
//! is no free-form field, so no caller can add message text, a name, a key, a ticket, a code,
//! a path or a team id. Sending is fire-and-forget (`post`): failures are dropped, nothing waits.
//! The wire is `curl`, as `lightbulb update` uses; the body goes in on stdin.

use crate::config::Config;
use crate::setup::{self, AgentReport};
use crate::{daemon::Daemon, Result};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const URL: &str = "https://us.i.posthog.com/batch/";
/// PostHog's public ingestion key: it can only add events to this project.
const KEY: &str = "phc_nzqnGGPwHfrKycptH3TViSAo3uaxT8CHAXovvHnBGEma";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Debug builds send nothing unless this is set (tests that force a send).
const FORCE: &str = "LIGHTBULB_TELEMETRY_FORCE";
/// Set only by the test harness (`tests/common`): a build that sets it never posts.
const NO_TELEMETRY_ENV: &str = "LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY";
const ID_FILE: &str = "install-id";
const VERSION_FILE: &str = "telemetry-version";
const DAILY_FILE: &str = "telemetry-daily";
const DAY: u64 = 86_400;

pub const NOTICE: &str = "Lightbulb sends anonymous usage counts (no messages, names or keys). Turn off: lightbulb telemetry off";

/// How a network was joined or an invite was shared.
#[derive(Clone, Copy)]
pub enum Via {
    Code,
    Link,
    Line,
}

impl Via {
    /// How `input` (what `join` was given) reads: a four-word code, a link, or a full line.
    pub fn of_join(input: &str) -> Via {
        let words: Vec<&str> = input.split('-').collect();
        if words.len() == 4
            && words
                .iter()
                .all(|w| w.bytes().all(|b| b.is_ascii_lowercase()))
        {
            Via::Code
        } else if input.contains("://") && !input.starts_with("lightbulb://") {
            Via::Link
        } else {
            Via::Line
        }
    }

    fn name(self) -> &'static str {
        match self {
            Via::Code => "code",
            Via::Link => "link",
            Via::Line => "line",
        }
    }
}

/// Counts over the last day. Numbers and agent kinds only.
#[derive(Default)]
pub struct Daily {
    pub messages_sent: u64,
    pub messages_received: u64,
    pub computers_on_network: u64,
    /// Sessions handed the end-of-turn nudge, and those of them that then wrote a memory entry.
    pub memory_nudged: u64,
    pub memory_written_after_nudge: u64,
    /// Kinds of agent connected (`setup::AGENTS` names; anything else is dropped).
    pub agents: Vec<&'static str>,
}

pub enum Event {
    Installed,
    Updated { from: String, to: String },
    NetworkJoined(Via),
    InviteCreated(Via),
    Daily(Daily),
}

/// `v` if it is a plain version number, else "unknown".
fn version_text(v: &str) -> String {
    let ok = !v.is_empty() && v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    if ok { v } else { "unknown" }.to_string()
}

/// The PostHog batch body for `event`: the only place an event is made.
pub fn build(event: &Event, install_id: &str) -> Value {
    let mut p = json!({
        "version": VERSION, "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "$process_person_profile": false, "$geoip_disable": true,
    });
    let name = match event {
        Event::Installed => "cli_installed",
        Event::Updated { from, to } => {
            p["from"] = version_text(from).into();
            p["to"] = version_text(to).into();
            "cli_updated"
        }
        Event::NetworkJoined(via) => {
            p["via"] = via.name().into();
            "network_joined"
        }
        Event::InviteCreated(via) => {
            p["via"] = via.name().into();
            "invite_created"
        }
        Event::Daily(d) => {
            let known = |a: &&str| setup::AGENTS.iter().any(|(n, _)| n == a);
            p["messages_sent"] = d.messages_sent.into();
            p["messages_received"] = d.messages_received.into();
            p["computers_on_network"] = d.computers_on_network.into();
            p["memory_nudged"] = d.memory_nudged.into();
            p["memory_written_after_nudge"] = d.memory_written_after_nudge.into();
            p["agents"] = d
                .agents
                .iter()
                .copied()
                .filter(known)
                .collect::<Vec<_>>()
                .into();
            "cli_daily"
        }
    };
    json!({"api_key": KEY, "batch": [{"event": name, "distinct_id": install_id, "properties": p}]})
}

/// Whether to send: the config allows it, and this is not a debug build or a test harness
/// (`LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY`) unless `force` (`LIGHTBULB_TELEMETRY_FORCE`).
pub fn enabled(config_on: bool, quiet_build: bool, force: bool) -> bool {
    config_on && (!quiet_build || force)
}

fn env_set(k: &str) -> bool {
    std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0")
}

fn env_off() -> Option<&'static str> {
    ["DO_NOT_TRACK", "LIGHTBULB_NO_TELEMETRY"]
        .into_iter()
        .find(|k| env_set(k))
}

/// The daemon's gate: the config only (an opt-out in a shell's environment never reaches a
/// launchd/systemd daemon, so a CLI command saves it to the config: `sync_env_optout`).
pub fn active(dir: &Path) -> bool {
    let config_on = Config::load(dir).is_ok_and(|c| c.telemetry);
    let quiet = cfg!(debug_assertions) || std::env::var_os(NO_TELEMETRY_ENV).is_some();
    enabled(config_on, quiet, env_set(FORCE))
}

/// Saves an environment opt-out as `telemetry = false`, once. Whether it did.
fn sync_env_optout_if(dir: &Path, opted_out: bool) -> bool {
    let on = Config::load(dir).is_ok_and(|c| c.telemetry);
    // A missing state folder is left alone: `Config::set` would create it with loose modes.
    opted_out && on && dir.is_dir() && Config::set(dir, "telemetry", "false").is_ok()
}

/// What every CLI command that sends (or reports) runs first.
pub fn sync_env_optout(dir: &Path) -> bool {
    sync_env_optout_if(dir, env_off().is_some())
}

/// `telemetry [on|off]`: sets the config, then says where it stands. An environment opt-out
/// keeps it off: `on` writes nothing then.
pub fn command(dir: &Path, state: Option<&str>) -> Result<i32> {
    let saved = sync_env_optout(dir);
    if let Some(s) = state.filter(|s| *s != "on" || env_off().is_none()) {
        Config::set(dir, "telemetry", if s == "on" { "true" } else { "false" })?;
    }
    let config_on = Config::load(dir).is_ok_and(|c| c.telemetry);
    match (config_on, env_off(), active(dir)) {
        (_, Some(k), _) => {
            let note = if saved { "; saved to config" } else { "" };
            println!("Usage counts  off ({k} is set and keeps it off{note})")
        }
        (false, ..) => println!("Usage counts  off (turned off in config)"),
        (true, _, false) => println!("Usage counts  off (debug build)"),
        _ => println!("Usage counts  on"),
    }
    Ok(0)
}

/// A random UUID (v4) made once and kept in the state folder. Not derived from anything.
fn install_id(dir: &Path) -> Result<String> {
    let path = dir.join(ID_FILE);
    if let Ok(id) = fs::read_to_string(&path) {
        if id.trim().len() == 36 {
            return Ok(id.trim().to_string());
        }
    }
    let id = uuid_v4();
    write_private(&path, &id)?;
    Ok(id)
}

/// A random UUID (v4).
pub(crate) fn uuid_v4() -> String {
    let mut b = rand::random::<[u8; 16]>();
    (b[6], b[8]) = ((b[6] & 0x0f) | 0x40, (b[8] & 0x3f) | 0x80);
    let h = data_encoding::HEXLOWER.encode(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

/// Sends `event` if usage counts are on. Never waits, never fails the caller.
pub fn send(dir: &Path, event: Event) {
    sync_env_optout(dir);
    if env_off().is_none() {
        send_if_active(dir, event);
    }
}

/// `send` on the config's word alone (the daemon).
fn send_if_active(dir: &Path, event: Event) {
    if active(dir) {
        if let Ok(id) = install_id(dir) {
            post(&build(&event, &id));
        }
    }
}

/// One `curl` with a 3 s limit, the body on its stdin; the answer is ignored.
fn post(body: &Value) {
    let Ok(mut child) = Command::new("curl")
        .args([
            "-q",
            "-s",
            "-m",
            "3",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
        ])
        .args(["--data-binary", "@-", URL])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body.to_string().as_bytes());
    }
    // Reaped off the caller's path; a CLI that exits first leaves curl to finish alone.
    std::thread::spawn(move || child.wait());
}

/// Called by `setup`: records the version this computer runs. The first run sends
/// `cli_installed` and returns true (print `NOTICE`); a changed version sends `cli_updated`.
pub fn on_setup(dir: &Path) -> bool {
    sync_env_optout(dir);
    if env_off().is_some() || !active(dir) {
        return false; // nothing recorded, so a later opt-in still counts as the first run
    }
    let path = dir.join(VERSION_FILE);
    let last = fs::read_to_string(&path).ok().map(|s| s.trim().to_string());
    if last.as_deref() == Some(VERSION) {
        return false;
    }
    let _ = write_private(&path, VERSION);
    let first = last.is_none();
    send_if_active(
        dir,
        match last {
            None => Event::Installed,
            Some(from) => Event::Updated {
                from,
                to: VERSION.into(),
            },
        },
    );
    first
}

/// Whether the daily summary is due: never sent, or a day ago.
fn due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|t| now >= t.saturating_add(DAY))
}

fn count(d: &Daemon, sql: &str, since: i64) -> u64 {
    let store = d.store.lock().unwrap();
    store
        .conn()
        .query_row(sql, [since], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
        .max(0) as u64
}

fn daily_counts(d: &Daemon, since: i64) -> Daily {
    let home = dirs::home_dir().unwrap_or_default();
    let agents = (setup::AGENTS.iter())
        .filter(|(name, dot)| {
            let state = setup::status(&home, name, dot);
            AgentReport {
                name,
                state,
                ..Default::default()
            }
            .connected()
        })
        .map(|(name, _)| *name)
        .collect();
    Daily {
        messages_sent: count(
            d,
            "SELECT COUNT(*) FROM messages WHERE origin = 'local' AND sent_at > ?1",
            since,
        ),
        messages_received: count(
            d,
            "SELECT COUNT(*) FROM messages WHERE origin LIKE 'net:%' AND sent_at > ?1",
            since,
        ),
        computers_on_network: d.team.lock().unwrap().roster.members.len() as u64,
        // `nudged` is when the nudge came. A nudged session had written nothing, so any entry
        // whose source session (`<member>/<id>`) is it came after.
        memory_nudged: count(
            d,
            "SELECT COUNT(*) FROM capture_sessions WHERE nudged > ?1",
            since,
        ),
        memory_written_after_nudge: count(
            d,
            "SELECT COUNT(*) FROM capture_sessions s WHERE s.nudged > ?1 AND EXISTS \
             (SELECT 1 FROM memory_entries WHERE substr(json_extract(entry, \
             '$.op.source.session'), -length(s.id) - 1) = '/' || s.id)",
            since,
        ),
        agents,
    }
}

/// The daemon's task: five minutes after start, then hourly, sends `cli_daily` when a day has
/// passed since the last. Reads the store; no message path waits on it.
pub async fn run_daily(d: Arc<Daemon>) {
    tokio::time::sleep(Duration::from_secs(300)).await;
    loop {
        let d2 = d.clone();
        let _ = tokio::task::spawn_blocking(move || daily_once(&d2)).await;
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

/// One tick: counts and sends when a day has passed and the config allows. Blocking work.
fn daily_once(d: &Daemon) {
    let now = crate::daemon::ipc::now();
    let stamp = d.dir.join(DAILY_FILE);
    let last = fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse().ok());
    if due(last, now) && active(&d.dir) {
        let _ = write_private(&stamp, &now.to_string());
        let counts = daily_counts(d, now.saturating_sub(DAY) as i64);
        send_if_active(&d.dir, Event::Daily(counts));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(v: &Value) -> Vec<String> {
        let mut k: Vec<String> = v["batch"][0]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    }

    #[test]
    fn events_carry_only_allowed_fields() {
        let base = [
            "$geoip_disable",
            "$process_person_profile",
            "arch",
            "os",
            "version",
        ];
        let with = |extra: &[&str]| {
            let mut k: Vec<String> = base.iter().chain(extra).map(|s| s.to_string()).collect();
            k.sort();
            k
        };
        let id = "00000000-0000-4000-8000-000000000000";
        assert_eq!(keys(&build(&Event::Installed, id)), with(&[]));
        let up = Event::Updated {
            from: "0.3.2".into(),
            to: "0.3.3".into(),
        };
        assert_eq!(keys(&build(&up, id)), with(&["from", "to"]));
        assert_eq!(
            keys(&build(&Event::NetworkJoined(Via::Code), id)),
            with(&["via"])
        );
        assert_eq!(
            keys(&build(&Event::InviteCreated(Via::Link), id)),
            with(&["via"])
        );
        let daily = Daily {
            agents: vec!["claude", "/home/me/secret"],
            ..Default::default()
        };
        let b = build(&Event::Daily(daily), id);
        assert_eq!(
            keys(&b),
            with(&[
                "agents",
                "computers_on_network",
                "memory_nudged",
                "memory_written_after_nudge",
                "messages_received",
                "messages_sent"
            ])
        );
        assert_eq!(b["batch"][0]["properties"]["agents"], json!(["claude"]));
        assert_eq!(b["batch"][0]["distinct_id"], id);
        // An odd version string cannot carry anything else.
        let odd = Event::Updated {
            from: "x; secret".into(),
            to: "1.2".into(),
        };
        assert_eq!(build(&odd, id)["batch"][0]["properties"]["from"], "unknown");
    }

    #[test]
    fn the_gate_reads_config_and_build() {
        assert!(enabled(true, false, false));
        assert!(!enabled(false, false, true), "config off beats force");
        assert!(!enabled(true, true, false), "debug or test folder: off");
        assert!(enabled(true, true, true));
    }

    #[test]
    fn an_env_opt_out_is_saved_to_config_and_gates_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert!(Config::load(d).unwrap().telemetry);
        assert!(!sync_env_optout_if(d, false));
        assert!(Config::load(d).unwrap().telemetry);
        assert!(sync_env_optout_if(d, true));
        assert!(!Config::load(d).unwrap().telemetry);
        assert!(!sync_env_optout_if(d, true), "once");
        // The daemon's gate sees it without the environment.
        assert!(!active(d));
        assert!(!enabled(Config::load(d).unwrap().telemetry, false, true));
    }

    #[test]
    fn install_id_is_random_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let a = install_id(dir.path()).unwrap();
        assert_eq!(a, install_id(dir.path()).unwrap());
        let other = tempfile::tempdir().unwrap();
        assert_ne!(a, install_id(other.path()).unwrap());
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
    }

    #[test]
    fn daily_is_due_after_a_day() {
        assert!(due(None, 5));
        assert!(!due(Some(100), 100 + DAY - 1));
        assert!(due(Some(100), 100 + DAY));
    }

    #[test]
    fn join_input_is_classified() {
        assert!(matches!(Via::of_join("calm-river-stone-lamp"), Via::Code));
        assert!(matches!(
            Via::of_join("https://thelightbulb.company/j#abc"),
            Via::Link
        ));
        assert!(matches!(Via::of_join("lightbulb://join/abc"), Via::Line));
    }
}
