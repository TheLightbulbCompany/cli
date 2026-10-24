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
    pub holds_opened: u64,
    pub holds_allowed: u64,
    pub computers_on_network: u64,
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
            p["holds_opened"] = d.holds_opened.into();
            p["holds_allowed"] = d.holds_allowed.into();
            p["computers_on_network"] = d.computers_on_network.into();
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

/// Whether to send. `env` reads an environment variable; `debug` is "this is a debug build".
pub fn enabled(env: impl Fn(&str) -> Option<String>, config_on: bool, debug: bool) -> bool {
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty() && v != "0");
    !(set("DO_NOT_TRACK") || set("LIGHTBULB_NO_TELEMETRY") || !config_on || (debug && !set(FORCE)))
}

/// Why it is off, for `telemetry status`; `None` when on.
fn off_reason(dir: &Path) -> Option<&'static str> {
    let env = |k: &str| std::env::var(k).ok();
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty() && v != "0");
    let config_on = Config::load(dir).is_ok_and(|c| c.telemetry);
    if set("DO_NOT_TRACK") {
        Some("DO_NOT_TRACK is set")
    } else if set("LIGHTBULB_NO_TELEMETRY") {
        Some("LIGHTBULB_NO_TELEMETRY is set")
    } else if !config_on {
        Some("turned off in config")
    } else if !enabled(env, config_on, cfg!(debug_assertions)) {
        Some("debug build")
    } else {
        None
    }
}

pub fn active(dir: &Path) -> bool {
    off_reason(dir).is_none()
}

/// `telemetry [on|off]`: sets the config, then says where it stands.
pub fn command(dir: &Path, state: Option<&str>) -> Result<i32> {
    if let Some(s) = state {
        Config::set(dir, "telemetry", if s == "on" { "true" } else { "false" })?;
    }
    match off_reason(dir) {
        None => println!("Usage counts  on"),
        Some(why) => println!("Usage counts  off ({why})"),
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
    let mut b = rand::random::<[u8; 16]>();
    (b[6], b[8]) = ((b[6] & 0x0f) | 0x40, (b[8] & 0x3f) | 0x80);
    let h = data_encoding::HEXLOWER.encode(&b);
    let id = format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    );
    write_private(&path, &id)?;
    Ok(id)
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
    let path = dir.join(VERSION_FILE);
    let last = fs::read_to_string(&path).ok().map(|s| s.trim().to_string());
    if last.as_deref() == Some(VERSION) {
        return false;
    }
    let _ = write_private(&path, VERSION);
    let first = last.is_none();
    let event = match last {
        None => Event::Installed,
        Some(from) => Event::Updated {
            from,
            to: VERSION.into(),
        },
    };
    let on = active(dir);
    if on {
        send(dir, event);
    }
    on && first
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
        holds_opened: count(d, "SELECT COUNT(*) FROM holds WHERE created_at > ?1", since),
        holds_allowed: count(
            d,
            "SELECT COUNT(*) FROM holds WHERE state = 'allowed' AND created_at > ?1",
            since,
        ),
        computers_on_network: d.team.lock().unwrap().roster.members.len() as u64,
        agents,
    }
}

/// The daemon's task: five minutes after start, then hourly, sends `cli_daily` when a day has
/// passed since the last. Reads the store; no message path waits on it.
pub async fn run_daily(d: Arc<Daemon>) {
    tokio::time::sleep(Duration::from_secs(300)).await;
    loop {
        let now = crate::daemon::ipc::now();
        let stamp = d.dir.join(DAILY_FILE);
        let last = fs::read_to_string(&stamp)
            .ok()
            .and_then(|s| s.trim().parse().ok());
        if due(last, now) && active(&d.dir) {
            let _ = write_private(&stamp, &now.to_string());
            let counts = daily_counts(&d, now.saturating_sub(DAY) as i64);
            send(&d.dir, Event::Daily(counts));
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
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
                "holds_allowed",
                "holds_opened",
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
    fn off_switches_stop_sending() {
        let none = |_: &str| None;
        let with =
            |k: &'static str, v: &'static str| move |q: &str| (q == k).then(|| v.to_string());
        // Release build, defaults: on.
        assert!(enabled(none, true, false));
        assert!(!enabled(with("DO_NOT_TRACK", "1"), true, false));
        assert!(!enabled(with("LIGHTBULB_NO_TELEMETRY", "1"), true, false));
        assert!(!enabled(none, false, false));
        // Debug build: off unless forced, and the switches still win over the force.
        assert!(!enabled(none, true, true));
        assert!(enabled(with(FORCE, "1"), true, true));
        assert!(!enabled(
            |k| (k == FORCE || k == "DO_NOT_TRACK").then(|| "1".into()),
            true,
            true
        ));
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
