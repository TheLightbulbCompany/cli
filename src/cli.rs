//! The person-facing commands: `who`, `send`, `listen`, `receipt`, `doctor`, `leave`,
//! `daemon --install/--uninstall`, the `lightbulb` alias, and forwarding to the Lightbulb app.
//! Each is a client of the daemon, like the hook.

use crate::daemon::ipc;
use crate::daemon::net_ops::Invite;
use crate::hook::{self, Conn};
use crate::{admission, daemon, paths, setup, Result};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
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

pub fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| "no home directory".into())
}

pub fn canonical(home: &Path) -> PathBuf {
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

/// `s` as one table cell: line breaks and tabs as spaces, and nothing a terminal would act on
/// (escape sequences, bidi controls), since a teammate's text is printed here.
fn cell(s: &str) -> String {
    s.chars()
        .map(|c| if matches!(c, '\n' | '\t') { ' ' } else { c })
        .filter(|c| crate::label::printable(*c))
        .collect()
}

/// `who`'s columns: header, then the `sessions` field it shows.
const WHO: [(&str, &str); 6] = [
    ("session", "key"),
    ("agent", "provider"),
    ("folder", "cwd"),
    ("status", "status"),
    ("tier", "tier"),
    ("doing", "doing"),
];
/// `who --all`'s columns, from `directory` records (this computer's first).
const WHO_ALL: [(&str, &str); 8] = [
    ("session", "session_id"),
    ("person", "person"),
    ("computer", "computer"),
    ("agent", "agent"),
    ("folder", "cwd"),
    ("status", "status"),
    ("tier", "tier"),
    ("doing", "doing"),
];

/// `who [--all]`: this computer's sessions; with `all`, every teammate's too (`directory`).
pub fn who(as_json: bool, all: bool) -> Result<i32> {
    let mut conn = connect()?;
    let (op, columns) = if all {
        ("directory", &WHO_ALL[..])
    } else {
        ("sessions", &WHO[..])
    };
    let sessions = ask(&mut conn, op, json!({}))?["sessions"].clone();
    if as_json {
        println!("{sessions}");
        return Ok(0);
    }
    let mut rows = vec![columns
        .iter()
        .map(|(h, _)| h.to_string())
        .collect::<Vec<_>>()];
    for s in sessions.as_array().into_iter().flatten() {
        let text = |f: &str| s[f].as_str().map(cell).filter(|t| !t.is_empty());
        rows.push(
            columns
                .iter()
                .map(|(_, f)| match *f {
                    "doing" => text("doing").or(text("title")),
                    // A teammate's session has no folder here: its repo instead.
                    "cwd" => text("cwd").or(text("repo")),
                    f => text(f),
                })
                .map(Option::unwrap_or_default)
                .collect(),
        );
    }
    let widths: Vec<usize> = (0..columns.len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for r in rows {
        let line: Vec<String> = (0..columns.len())
            .map(|i| format!("{:<1$}", r[i], widths[i]))
            .collect();
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
    let identity = session_identity();
    if group && (from.is_some() || identity) {
        return Err("group sends are for people at a terminal".into());
    }
    let mut conn = connect()?;
    let short = conn.member()?;
    let from = if identity {
        // An agent's shell speaks as its own session, never as the terminal or another one.
        calling_session(&mut conn)
            .filter(|own| from.as_ref().is_none_or(|f| f == own))
            .ok_or("send from an agent's shell must name --from <session_id>, your own session")?
    } else {
        from.unwrap_or_else(|| format!("{short}/terminal"))
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

// ---- team -------------------------------------------------------------------------------

/// Prints the text `texts` maps `r`'s error code to (`not_member`'s shared text, else
/// `"{verb}: {code}"`, for any other code);
/// returns the exit code 1.
fn refuse(r: &Value, texts: &[(&str, String)], verb: &str) -> Result<i32> {
    let code = r["error"].as_str().unwrap_or_default();
    match texts.iter().find(|(c, _)| *c == code) {
        Some((_, text)) => eprintln!("{text}"),
        None if code == "not_member" => eprintln!("This computer is not in the network."),
        None => eprintln!("{verb}: {code}"),
    }
    Ok(1)
}

/// `invite [--new|--once|--open]`: this computer's invite line.
pub fn invite(mode: Invite) -> Result<i32> {
    if mode == Invite::Open && session_identity() {
        eprintln!("invite --open is for people at a terminal");
        return Ok(1);
    }
    let r = connect()?.call("team.invite", json!({ "mode": mode }))?;
    if r["ok"] != true {
        let text = "Only the founder and admins can make an open link.".to_string();
        return refuse(&r, &[("not_admin", text)], "invite");
    }
    let line = r["line"].as_str().unwrap_or_default();
    match mode {
        Invite::Once => {
            let at = utc(r["expires_at"].as_i64().unwrap_or_default());
            println!("One-time invite, expires {at}: {line}");
        }
        Invite::Open => {
            println!(
                "Anyone with this link joins without asking. Use it only with people you trust."
            );
            println!("{line}");
        }
        _ if r["replaced"] == true => println!("Old link stopped working. New link: {line}"),
        _ => println!("{line}"),
    }
    Ok(0)
}

/// `join '<link>' [--name <n>]`: joins the network `link` invites to, waiting up to 600 s
/// for its person to allow it.
pub fn join(link: &str, name: Option<String>) -> Result<i32> {
    let name = name.unwrap_or_else(daemon::user_name);
    let mut conn = connect()?;
    if crate::ticket::decode(link).is_ok_and(|t| t.mode == "ask") {
        println!("Waiting for an admin to allow this...");
    }
    conn.wait_up_to(Duration::from_secs(660))?;
    let r = ask(
        &mut conn,
        "team.join",
        json!({"ticket": link, "name": name, "kind": "computer"}),
    )?;
    let team = r["team"].as_str().unwrap_or_default();
    let text = match (r["state"].as_str(), r["reason"].as_str()) {
        (Some("admitted"), _) => {
            let computer = ask(&mut conn, "identity.get", json!({}))?["computer"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let n = r["online"].as_u64().unwrap_or(1);
            println!("Connected as {name} ({computer}). Team \"{team}\": {n} computers online.");
            return Ok(0);
        }
        (Some("waiting"), _) => "No one has answered yet. Run the same command again to keep asking.".to_string(),
        (_, Some(admission::ALREADY_IN_TEAM)) => format!("You are in \"{team}\". Leave it first: lightbulb-network leave"),
        (_, Some(admission::NAME_TAKEN)) => format!(
            "The name \"{name}\" is already used in this network. Try: lightbulb-network join '{link}' --name {name}-2"
        ),
        (_, Some(admission::UNREACHABLE)) => "No one from this network is online right now. Ask them to open their laptop, then run the same command again. Your key is ready; nothing was lost.".to_string(),
        (_, Some(admission::DECLINED)) => "Your request to join was declined.".to_string(),
        (_, Some(admission::BAD_TICKET)) => "This invite link does not work. Ask for a new one.".to_string(),
        (_, Some(admission::EXPIRED_TICKET)) => "This invite link has expired. Ask for a new one.".to_string(),
        (_, reason) => format!("join: {}", reason.unwrap_or("refused")),
    };
    eprintln!("{text}");
    Ok(1)
}

/// `approve J<n>` / `decline J<n>`: answers a join request.
pub fn answer_join(request: &str, allow: bool) -> Result<i32> {
    let r = connect()?.call(
        "team.admit",
        json!({"request": request, "verdict": if allow { "allow" } else { "decline" }}),
    )?;
    if r["ok"] != true {
        let texts = [
            (
                admission::NOT_OPEN,
                format!("{request} was already answered or withdrawn."),
            ),
            (ipc::NOT_FOUND, format!("No join request {request}.")),
        ];
        return refuse(&r, &texts, if allow { "approve" } else { "decline" });
    }
    let name = r["name"].as_str().unwrap_or_default();
    match r["state"].as_str() {
        Some("admitted") => println!("{name} joined the network."),
        Some("declined") => println!("Declined {name}."),
        _ => {
            eprintln!(
                "Could not admit {name}: {}",
                r["reason"].as_str().unwrap_or("refused")
            );
            return Ok(1);
        }
    }
    Ok(0)
}

/// `team`: the team's name and members.
pub fn team() -> Result<i32> {
    let t = ask(&mut connect()?, "team.get", json!({}))?;
    println!("{}", t["name"].as_str().unwrap_or_default());
    for m in t["members"].as_array().into_iter().flatten() {
        let f = |k: &str| m[k].as_str().map(cell).unwrap_or_default();
        println!(
            "  {}  {}  {}  {}",
            f("name"),
            f("computer"),
            f("role"),
            f("short")
        );
    }
    Ok(0)
}

/// `admin <name> [--founder]`.
pub fn admin(name: &str, founder: bool) -> Result<i32> {
    let r = connect()?.call("team.admin", json!({ "member": name, "founder": founder }))?;
    if r["ok"] != true {
        let texts = [
            (
                "not_founder",
                "Only the founder can make admins.".to_string(),
            ),
            ("not_found", format!("No member called {name}.")),
        ];
        return refuse(&r, &texts, "admin");
    }
    let n = r["name"].as_str().unwrap_or(name);
    if founder {
        println!("{n} is now the founder.");
    } else {
        println!("{n} can now remove members.");
    }
    Ok(0)
}

/// `remove <name>`.
pub fn remove(name: &str) -> Result<i32> {
    let r = connect()?.call("team.remove", json!({ "member": name }))?;
    if r["ok"] != true {
        let ask = r["founder"].as_str().unwrap_or("the founder");
        let texts = [
            (
                "not_admin",
                format!("Only the founder and admins can remove computers. Ask {ask}."),
            ),
            ("not_found", format!("No member called {name}.")),
        ];
        return refuse(&r, &texts, "remove");
    }
    let n = r["name"].as_str().unwrap_or(name);
    let line = r["line"].as_str().unwrap_or_default();
    println!("Removed {n}. New connections and new mail from it are blocked. The invite link was replaced: {line}");
    Ok(0)
}

/// `leave`: after a yes, this computer leaves its team and is on its own network again. The
/// service keeps running.
pub fn leave() -> Result<i32> {
    let mut conn = connect()?;
    let name = ask(&mut conn, "team.get", json!({}))?["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    print!(
        "Leave \"{name}\"? Your computer stops connecting. Mail already stored here stays. [y/N] "
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        return Ok(0);
    }
    let r = conn.call("team.leave", json!({}))?;
    if r["ok"] != true {
        let text = "You are the founder. Name a new founder first: lightbulb-network admin <name> --founder";
        return refuse(&r, &[("founder_must_transfer", text.to_string())], "leave");
    }
    println!("Left \"{name}\". This computer is on its own network again.");
    Ok(0)
}

/// `secs` as `YYYY-MM-DD HH:MM UTC`.
fn utc(secs: i64) -> String {
    // Days to a civil date (Howard Hinnant's `civil_from_days`).
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        rem % 3600 / 60
    )
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

/// Puts the running binary at the canonical path and the alias at that, printing the line.
pub fn install_running(home: &Path) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    println!("{}", install_alias(home, &exe)?);
    Ok(())
}

/// `daemon --install`: the canonical binary and alias, then the launchd job running it.
pub fn daemon_install() -> Result<i32> {
    if !cfg!(target_os = "macos") {
        eprintln!("not supported yet");
        return Ok(1);
    }
    let home = home()?;
    install_running(&home)?;
    install_job(&home, &canonical(&home))
}

/// Writes and loads the launchd job that runs `exe daemon`.
pub fn install_job(home: &Path, exe: &Path) -> Result<i32> {
    let path = plist_path(home);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, plist(exe, &paths::state_dir().join("daemon.log")))?;
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

#[cfg(test)]
mod tests {
    #[test]
    fn utc_formats_a_unix_time() {
        assert_eq!(super::utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(super::utc(1_792_728_600), "2026-10-23 04:10 UTC");
        assert_eq!(super::utc(951_782_400), "2000-02-29 00:00 UTC");
    }

    #[test]
    fn a_title_with_control_characters_prints_as_plain_text() {
        let title = "fix\u{1b}[2J\u{1b}]8;;http://x\u{7}\tit\u{85}\u{2066}now\r\nok";
        assert_eq!(super::cell(title), "fix[2J]8;;http://x itnow ok");
    }
}
