//! The person-facing commands: `who`, `send`, `listen`, `receipt`, `doctor`, `leave`,
//! `daemon --install/--uninstall`, the `lightbulb` alias, and forwarding to the Lightbulb app.
//! Each is a client of the daemon, like the hook.

use crate::hook::{self, Conn};
use crate::{daemon, paths, setup, Result};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const LABEL: &str = "company.thelightbulb.network";
/// Set to skip every `launchctl` call (tests).
const NO_LAUNCHCTL: &str = "LIGHTBULB_NETWORK_NO_LAUNCHCTL";
/// Overrides where the Lightbulb app's binary is looked for (tests).
const DESKTOP_ENV: &str = "LIGHTBULB_NETWORK_DESKTOP";
const DESKTOP: &str = "/Applications/Lightbulb.app/Contents/MacOS/lightbulb-desktop";
/// Names a session the invocation speaks as (nothing sets it yet).
const SESSION_ENV: &str = "LIGHTBULB_NETWORK_SESSION";

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| "no home directory".into())
}

fn canonical(home: &Path) -> PathBuf {
    home.join(".lightbulb/network/bin/lightbulb-network")
}

pub(crate) fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// Starts the daemon if needed, then opens a connection to it.
fn connect() -> Result<Conn> {
    daemon::ensure_running()?;
    Conn::open()
}

/// `ask` that turns a refusal into an error.
fn ask(conn: &mut Conn, op: &str, body: Value) -> Result<Value> {
    let r = conn.call(op, body)?;
    if r["ok"] == true {
        Ok(r)
    } else {
        Err(format!(
            "{op}: {}",
            r["reason"]
                .as_str()
                .or(r["error"].as_str())
                .unwrap_or("failed")
        )
        .into())
    }
}

fn cell(s: &str) -> String {
    s.replace(['\n', '\t'], " ")
}

pub fn who(as_json: bool) -> Result<i32> {
    let mut conn = connect()?;
    let sessions = ask(&mut conn, "sessions", json!({}))?["sessions"].clone();
    if as_json {
        println!("{sessions}");
        return Ok(0);
    }
    let mut rows =
        vec![["session", "agent", "folder", "status", "tier", "doing"].map(String::from)];
    for s in sessions.as_array().into_iter().flatten() {
        let text = |f: &str| s[f].as_str().map(cell);
        let doing = text("doing").or(text("title")).unwrap_or_default();
        rows.push(
            [
                text("key"),
                text("provider"),
                text("cwd"),
                text("status"),
                text("tier"),
                Some(doing),
            ]
            .map(Option::unwrap_or_default),
        );
    }
    let widths: Vec<usize> = (0..6)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for r in rows {
        let line: Vec<String> = (0..6).map(|i| format!("{:<1$}", r[i], widths[i])).collect();
        println!("{}", line.join("  ").trim_end());
    }
    Ok(0)
}

/// Whether this invocation may be a session: the env names one, an agent is above us, or the
/// ancestry could not be walked to a person (fails closed).
fn session_identity() -> bool {
    std::env::var_os(SESSION_ENV).is_some()
        || hook::calling_agent(None).is_ok()
        || !crate::agents::caller::walked_to_a_person()
}

/// The session of the agent above this invocation, resolved as the MCP server resolves it.
fn calling_session(conn: &mut Conn) -> Option<String> {
    let me = hook::calling_agent(None).ok()?;
    let sessions = ask(conn, "sessions", json!({})).ok()?["sessions"].clone();
    let sessions: Vec<crate::sessions::Session> = serde_json::from_value(sessions).ok()?;
    crate::agents::source::resolve(&me, &json!({}), &sessions)
        .ok()
        .map(|k| k.0)
}

pub fn send(
    target: &str,
    text: &str,
    from: Option<String>,
    reply_to: Option<String>,
    request_id: Option<String>,
) -> Result<i32> {
    let group = target == "*" || target.ends_with("/*");
    let from = from.or_else(|| std::env::var(SESSION_ENV).ok());
    let identity = from.is_none() && session_identity();
    if group && (from.is_some() || identity) {
        return Err("group sends are for people at a terminal".into());
    }
    let mut conn = connect()?;
    let short = conn.member()?;
    let from = match from {
        Some(f) => f,
        // An agent's shell speaks as its session, never as the terminal.
        None if identity => calling_session(&mut conn)
            .ok_or("send from an agent's shell must name --from <session_id>")?,
        None => format!("{short}/terminal"),
    };
    let rid = request_id.unwrap_or_else(|| ulid::Ulid::generate().to_string());
    let targets: Vec<(String, String)> = if group {
        let sessions = ask(&mut conn, "sessions", json!({}))?["sessions"].clone();
        let prefix = target.trim_end_matches('*');
        sessions
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s["key"].as_str())
            .filter(|k| target == "*" || k.starts_with(prefix))
            .enumerate()
            .map(|(i, k)| (k.to_string(), format!("{rid}.{}", i + 1)))
            .collect()
    } else {
        vec![(target.to_string(), rid)]
    };
    if targets.is_empty() {
        return Err("no live sessions match".into());
    }
    let mut code = 0;
    for (to, rid) in targets {
        let r = ask(
            &mut conn,
            "send",
            json!({ "from": from, "to": to, "text": text, "request_id": rid, "reply_to": reply_to }),
        )?;
        let receipt = &r["receipt"];
        let state = receipt["state"].as_str().unwrap_or("unknown");
        let mut line = match receipt["reason"].as_str() {
            Some(reason) if state == "refused" => format!("refused: {reason}"),
            _ => state.to_string(),
        };
        if state == "refused" {
            code = 1;
        }
        line.push_str(&format!(" {rid}"));
        if group {
            line.push_str(&format!(" {to}"));
        }
        println!("{line}");
    }
    Ok(code)
}

pub fn listen(name: &str, once: bool) -> Result<i32> {
    if !crate::sessions::valid_name(name) {
        return Err("--as takes [A-Za-z0-9._-], 1 to 64 characters".into());
    }
    let mut backoff = 1;
    loop {
        // A daemon error (it restarted, say) reconnects with a 1 s to 10 s backoff.
        match key_for(name).and_then(|k| listen_on(&k, once)) {
            Ok(()) => return Ok(0),
            Err(e) => {
                eprintln!("listen: {e}; retrying in {backoff} s");
                std::thread::sleep(Duration::from_secs(backoff));
                backoff = (backoff * 2).min(10);
            }
        }
    }
}

fn key_for(name: &str) -> Result<String> {
    Ok(format!("{}/{name}", connect()?.member()?))
}

/// One connection's worth of listening; `Ok` only for `--once` done.
fn listen_on(key: &str, once: bool) -> Result<()> {
    let mut conn = connect()?;
    conn.wait_up_to(Duration::from_secs(90))?;
    loop {
        let r = ask(&mut conn, "inbox", json!({ "key": key, "wait_secs": 60 }))?;
        let messages = r["messages"].as_array().cloned().unwrap_or_default();
        for m in &messages {
            println!("{}", m["framed"].as_str().unwrap_or_default());
        }
        if once && !messages.is_empty() {
            return Ok(());
        }
    }
}

pub fn receipt(request_id: &str, from: Option<String>) -> Result<i32> {
    let mut conn = connect()?;
    let from = match from {
        Some(f) => f,
        None => format!("{}/terminal", conn.member()?),
    };
    let r = conn.call("receipt", json!({ "from": from, "request_id": request_id }))?;
    if r["ok"] == true {
        println!("{}", r["receipt"]);
        Ok(0)
    } else if r["error"] == "not_found" {
        Err(format!("no receipt for {request_id} from {from}").into())
    } else {
        Err(format!("receipt: {r}").into())
    }
}

pub fn leave() {
    println!("This computer is on no network yet.");
}

/// Hands an unowned subcommand to the Lightbulb app's binary (replacing this process), or says
/// what is needed.
pub fn forward(args: Vec<OsString>) -> Result<i32> {
    let app = std::env::var_os(DESKTOP_ENV).map_or_else(|| PathBuf::from(DESKTOP), PathBuf::from);
    if !app.exists() {
        eprintln!("needs the Lightbulb app: thelightbulb.company/download");
        return Ok(2);
    }
    Err(Command::new(app).args(args).exec().into())
}

// ---- alias and launchd ------------------------------------------------------------------

/// Puts this executable at the canonical path (unless it is already there with the same
/// bytes) and the `~/.local/bin/lightbulb` symlink at that. Returns what happened, one line.
/// A regular file or a foreign symlink at the alias path is left alone and reported.
pub fn install_alias(home: &Path, exe: &Path) -> Result<String> {
    let canon = canonical(home);
    if exe != canon {
        let same = fs::read(&canon)
            .ok()
            .zip(fs::read(exe).ok())
            .is_some_and(|(a, b)| a == b);
        if !same {
            fs::create_dir_all(canon.parent().unwrap())?;
            // Written beside and renamed over, so a running daemon's binary is never rewritten.
            let tmp = canon.with_extension("tmp");
            fs::copy(exe, &tmp)?;
            fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
            fs::rename(&tmp, &canon)?;
        }
    }
    let link = home.join(".local/bin/lightbulb");
    match fs::symlink_metadata(&link) {
        Ok(m) if m.is_symlink() && fs::read_link(&link)? == canon => {
            Ok(format!("alias: {} (unchanged)", link.display()))
        }
        Ok(_) => Ok(format!(
            "alias: {} exists and is not ours; left alone",
            link.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(link.parent().unwrap())?;
            symlink(&canon, &link)?;
            Ok(format!("alias: {} -> {}", link.display(), canon.display()))
        }
        Err(e) => Err(e.into()),
    }
}

fn launchctl(args: &[&str]) -> bool {
    Command::new("launchctl")
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

pub(crate) fn use_launchctl() -> bool {
    std::env::var_os(NO_LAUNCHCTL).is_none()
}

/// Asks launchd to start the installed job if it is not running; whether launchctl agreed.
pub(crate) fn kickstart() -> bool {
    launchctl(&[
        "kickstart",
        &format!("gui/{}/{LABEL}", unsafe { libc::getuid() }),
    ])
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plist(exe: &Path, log: &Path) -> String {
    let (exe, log) = (xml(&exe.to_string_lossy()), xml(&log.to_string_lossy()));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#
    )
}

/// Installs the alias (printing its line) and the launchd job.
pub fn daemon_install(exe: &Path) -> Result<i32> {
    if !cfg!(target_os = "macos") {
        eprintln!("not supported yet");
        return Ok(1);
    }
    let home = home()?;
    println!("{}", install_alias(&home, exe)?);
    let path = plist_path(&home);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(
        &path,
        plist(&canonical(&home), &paths::state_dir().join("daemon.log")),
    )?;
    // Replace any loaded job so an update restarts it: bootout (result ignored), bootstrap,
    // and `kickstart -k` if bootstrap fails.
    let uid = unsafe { libc::getuid() };
    let (domain, service, file) = (
        format!("gui/{uid}"),
        format!("gui/{uid}/{LABEL}"),
        path.to_string_lossy().into_owned(),
    );
    let sequence = format!("bootout {service}; bootstrap {domain} {file}; kickstart -k {service}");
    if use_launchctl() {
        launchctl(&["bootout", &service]);
        let up = launchctl(&["bootstrap", &domain, &file])
            || launchctl(&["load", "-w", &file])
            || launchctl(&["kickstart", "-k", &service]);
        if !up {
            println!("launchd: could not load {}", path.display());
            return Ok(1);
        }
        println!("launchd: {LABEL}");
    } else {
        println!("launchd: {LABEL} (launchctl skipped; would run: {sequence})");
    }
    Ok(0)
}

pub fn daemon_uninstall() -> Result<i32> {
    if !cfg!(target_os = "macos") {
        eprintln!("not supported yet");
        return Ok(1);
    }
    let path = plist_path(&home()?);
    if use_launchctl() {
        let target = format!("gui/{}/{LABEL}", unsafe { libc::getuid() });
        launchctl(&["bootout", &target]);
    }
    match fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    // Stop a running daemon (the job is gone, so it will not come back); state stays.
    if let Ok(mut conn) = Conn::open() {
        let _ = conn.call("shutdown", json!({}));
    }
    println!("daemon stopped and launchd job removed; state kept");
    Ok(0)
}

// ---- doctor -----------------------------------------------------------------------------

pub fn doctor() -> Result<i32> {
    let home = home()?;
    let mut missing = false;
    let mut line = |name: &str, ok: bool| {
        missing |= !ok;
        println!("{name}: {}", if ok { "ok" } else { "missing" });
    };
    line(
        "daemon",
        Conn::open()
            .and_then(|mut c| c.call("ping", json!({})))
            .is_ok_and(|r| r["ok"] == true),
    );
    let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777).ok();
    let dir = paths::state_path();
    line("state dir", mode(&dir) == Some(0o700));
    line("socket", mode(&dir.join(paths::SOCKET)) == Some(0o600));
    for (agent, dot) in [
        ("claude", ".claude"),
        ("codex", ".codex"),
        ("cursor", ".cursor"),
        ("grok", ".grok"),
    ] {
        if home.join(dot).is_dir() {
            line(agent, setup::wired(&home, agent));
        } else {
            println!("{agent}: not installed");
        }
    }
    let alias = home.join(".local/bin/lightbulb");
    match fs::read_link(&alias) {
        Ok(t) if t == canonical(&home) => line("alias", true),
        // Someone else's file or link at our alias path is theirs, not a gap in ours.
        _ if alias.symlink_metadata().is_ok() => println!("alias: not ours (left alone)"),
        _ => line("alias", false),
    }
    if cfg!(target_os = "macos") {
        let loaded = if use_launchctl() {
            launchctl(&[
                "print",
                &format!("gui/{}/{LABEL}", unsafe { libc::getuid() }),
            ])
        } else {
            plist_path(&home).exists()
        };
        line("launchd", loaded);
    }
    Ok(i32::from(missing))
}
