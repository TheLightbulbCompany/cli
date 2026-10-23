//! The person-facing commands: `who`, `send`, `listen`, `receipt`, `doctor`, `leave`,
//! `daemon --install/--uninstall`, the `lightbulb` alias, and forwarding to the Lightbulb app.
//! Each is a client of the daemon, like the hook.

use crate::daemon::ipc;
use crate::daemon::net_ops::Invite;
use crate::hook::{self, Conn};
use crate::{admission, daemon, http, label, paths, proof, setup, Result};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const LABEL: &str = "company.thelightbulb.connect";
/// Set to skip every `launchctl` call (tests).
const NO_LAUNCHCTL: &str = "LIGHTBULB_CONNECT_NO_LAUNCHCTL";
/// Overrides where the Lightbulb app's binary is looked for (tests).
const DESKTOP_ENV: &str = "LIGHTBULB_CONNECT_DESKTOP";
const DESKTOP: &str = "/Applications/Lightbulb.app/Contents/MacOS/lightbulb-desktop";
/// Names a session the invocation speaks as (nothing sets it yet).
const SESSION_ENV: &str = "LIGHTBULB_CONNECT_SESSION";

pub fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| "no home directory".into())
}

pub fn canonical(home: &Path) -> PathBuf {
    home.join(".lightbulb/connect/bin/lightbulb-connect")
}

/// legacy (Plan A install): the names `setup --remove` still cleans up.
pub const LEGACY_LABEL: &str = "company.thelightbulb.network";

/// legacy (Plan A install): the old canonical binary.
pub fn legacy_canonical(home: &Path) -> PathBuf {
    home.join(".lightbulb/network/bin/lightbulb-network")
}

/// legacy (Plan A install): stops the old daemon (its socket, then its launchd job) and
/// removes the job and the old binary.
pub fn legacy_uninstall(home: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        // A bootout alone leaves a daemon whose job is already gone running.
        if let Ok(mut s) =
            std::os::unix::net::UnixStream::connect(home.join(".lightbulb/network/sock"))
        {
            let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
            let _ = writeln!(s, r#"{{"id":1,"op":"shutdown"}}"#);
            let _ = std::io::Read::read(&mut s, &mut [0; 64]);
        }
        if use_launchctl() {
            let target = format!("gui/{}/{LEGACY_LABEL}", unsafe { libc::getuid() });
            launchctl(&["bootout", &target]);
        }
        let plist = home
            .join("Library/LaunchAgents")
            .join(format!("{LEGACY_LABEL}.plist"));
        for path in [plist, legacy_canonical(home)] {
            match fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
    }
    Ok(())
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

/// `ask` that turns a refusal into an error: a sentence (`refusal`).
fn ask(conn: &mut Conn, op: &str, body: Value) -> Result<Value> {
    let r = conn.call(op, body)?;
    if r["ok"] == true {
        Ok(r)
    } else {
        Err(refusal(&r).into())
    }
}

/// A daemon refusal `r` as a sentence for the person.
fn refusal(r: &Value) -> String {
    let code = r["reason"].as_str().or(r["error"].as_str());
    match code.unwrap_or("failed") {
        "not_member" => "This computer is not in the network.".into(),
        "not_a_person" => "Run this from your own terminal, not from an agent.".into(),
        "not_admin" => "Only the founder and admins can do that.".into(),
        "name_taken" => "That name is taken by your other computer's session.".into(),
        c => format!(
            "Lightbulb Connect could not do that ({}).",
            c.replace('_', " ")
        ),
    }
}

/// The command name that runs this program here: `lightbulb` when that link is ours, else
/// `lightbulb-connect` (the Lightbulb app may own `lightbulb`).
pub fn command() -> &'static str {
    let ours = home().is_ok_and(|h| {
        fs::read_link(h.join(".local/bin/lightbulb")).is_ok_and(|t| t == canonical(&h))
    });
    if ours {
        "lightbulb"
    } else {
        "lightbulb-connect"
    }
}

/// `addr` as the daemon takes it: a `who` handle (`codex-1`) as its session id, and
/// `<member name>/<rest>` as `<member>/<rest>`; an id or a bare name as given.
fn address(conn: &mut Conn, addr: &str) -> Result<String> {
    let Some((m, rest)) = addr.split_once('/') else {
        let handle = addr
            .rsplit_once('-')
            .is_some_and(|(_, n)| n.parse::<u32>().is_ok());
        if handle {
            let all = ask(conn, "directory", json!({}))?["sessions"].clone();
            let hit = all
                .as_array()
                .into_iter()
                .flatten()
                .find(|s| s["handle"] == addr);
            if let Some(id) = hit.and_then(|s| s["session_id"].as_str()) {
                return Ok(id.into());
            }
            // An agent's handle shape is never a name: its session closed.
            let agent = addr.rsplit_once('-').map_or("", |(a, _)| a);
            if crate::sessions::PROVIDERS.contains(&agent) {
                return Err(format!(
                    "No session \"{addr}\" is open right now. Run {} who to see the current ones.",
                    command()
                )
                .into());
            }
        }
        return Ok(addr.into());
    };
    let team = ask(conn, "team.get", json!({}))?;
    let members = team["members"].as_array().cloned().unwrap_or_default();
    let field = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    if members.iter().any(|v| v["short"] == m) {
        return Ok(addr.into());
    }
    let named: Vec<&Value> = members
        .iter()
        .filter(|v| field(v, "name").eq_ignore_ascii_case(m))
        .collect();
    match named[..] {
        [one] => Ok(format!("{}/{rest}", field(one, "short"))),
        [] => {
            let names: Vec<String> = members.iter().map(|v| cell(&field(v, "name"))).collect();
            Err(format!(
                "No teammate called \"{}\". Teammates: {} (see {} team).",
                cell(m),
                names.join(", "),
                command()
            )
            .into())
        }
        _ => {
            let ids: Vec<String> = named
                .iter()
                .map(|v| {
                    format!(
                        "{} on {}: {}/{rest}",
                        cell(m),
                        cell(&field(v, "computer")),
                        field(v, "short")
                    )
                })
                .collect();
            Err(format!(
                "More than one teammate is called \"{}\". Use one of: {}",
                cell(m),
                ids.join("; ")
            )
            .into())
        }
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

/// `rows` (the first the header) as aligned columns.
fn table(rows: &[Vec<String>]) {
    let n = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..n)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for r in rows {
        let line: Vec<String> = (0..n).map(|i| format!("{:<1$}", r[i], widths[i])).collect();
        println!("{}", line.join("  ").trim_end());
    }
}

/// `who [--all]`: this computer's sessions by handle; with `all`, every teammate's too
/// (`directory`). Ids only with `--json`.
pub fn who(as_json: bool, all: bool) -> Result<i32> {
    let mut conn = connect()?;
    let op = if all { "directory" } else { "sessions" };
    let sessions = ask(&mut conn, op, json!({}))?["sessions"].clone();
    if as_json {
        println!("{sessions}");
        return Ok(0);
    }
    let list = sessions.as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("No sessions yet. Open Claude Code, Codex, Cursor or Grok and they appear here.");
        return Ok(0);
    }
    let header: &[&str] = if all {
        &[
            "Name", "Person", "Computer", "Agent", "Folder", "Status", "Doing",
        ]
    } else {
        &["Name", "Agent", "Folder", "Status", "Doing"]
    };
    let mut rows = vec![header.iter().map(|h| h.to_string()).collect::<Vec<_>>()];
    for s in &list {
        let text = |f: &str| s[f].as_str().map(cell).filter(|t| !t.is_empty());
        let agent = text("agent").or(text("provider")).unwrap_or_default();
        let gets = match s["tier"].as_str() {
            Some("push") => "instantly",
            Some("next_step") => "at its next step",
            _ => "when it checks",
        };
        let status = match s["status"].as_str() {
            Some("live") => "busy",
            _ => "idle",
        };
        let mut row = vec![text("handle").unwrap_or_default()];
        if all {
            row.extend([text("person"), text("computer")].map(Option::unwrap_or_default));
        }
        row.extend([
            crate::frame::agent_name(&agent),
            // A teammate's session has no folder here: its repo instead.
            text("cwd").or(text("repo")).unwrap_or_default(),
            format!("{status}, gets messages {gets}"),
            text("doing").or(text("title")).unwrap_or_default(),
        ]);
        rows.push(row);
    }
    table(&rows);
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
    let from = match from {
        Some(f) => Some(address(&mut conn, &f)?),
        None => None,
    };
    let target = &address(&mut conn, target)?;
    let from = if identity {
        // An agent's shell speaks as its own session, never as the terminal or another one.
        let own = calling_session(&mut conn)
            .filter(|own| from.as_ref().is_none_or(|f| f == own))
            .ok_or("send from an agent's shell must name --from <session_id>, your own session")?;
        let provider = own.split_once('/').and_then(|(_, r)| r.split_once(':'));
        let agent = crate::frame::agent_name(provider.map_or("", |(p, _)| p));
        println!("Sent as this {agent} session.");
        own
    } else {
        from.unwrap_or_else(|| format!("{short}/terminal"))
    };
    let rid = request_id.unwrap_or_else(|| ulid::Ulid::generate().to_string());
    // (address, its handle, request id)
    let targets: Vec<(String, String, String)> = if group {
        let sessions = ask(&mut conn, "sessions", json!({}))?["sessions"].clone();
        let prefix = target.trim_end_matches('*');
        sessions
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| {
                s["key"]
                    .as_str()
                    .is_some_and(|k| target == "*" || k.starts_with(prefix))
            })
            .enumerate()
            .map(|(i, s)| {
                let at = |f: &str| s[f].as_str().unwrap_or_default().to_string();
                (at("key"), at("handle"), format!("{rid}.{}", i + 1))
            })
            .collect()
    } else {
        vec![(target.to_string(), String::new(), rid)]
    };
    if targets.is_empty() {
        return Err("No live session matches.".into());
    }
    let mut code = 0;
    for (to, handle, rid) in targets {
        let r = ask(
            &mut conn,
            "send",
            json!({ "from": from, "to": to, "text": text, "request_id": rid,
                "reply_to": reply_to, "wait": true }),
        )?;
        let receipt = &r["receipt"];
        if receipt["state"] == "refused" {
            code = 1;
        }
        let line = said(receipt, Some(&to), &rid);
        if group {
            println!("{line} ({})", cell(&handle));
        } else {
            println!("{line}");
        }
    }
    Ok(code)
}

/// What receipt `r` of message `rid` (to `to`, when known) means, as a sentence for the person
/// who sent it.
fn said(r: &Value, to: Option<&str>, rid: &str) -> String {
    let at = |f: &str| r[f].as_str().map(cell).unwrap_or_default();
    let (person, computer) = (at("person"), at("where"));
    let remote = r["person"].is_string();
    let check = format!(" Check: {} receipt {rid}", command());
    let name = to.map(|t| t.split_once('/').map_or(t, |(_, n)| n));
    match at("state").as_str() {
        "refused" if at("reason") == "loop" => {
            format!(
                "Not sent: {}.",
                crate::mail::receipt_note(r).unwrap_or_default()
            )
        }
        "refused" => format!(
            "Not sent: {}.",
            match at("reason").as_str() {
                "policy" => "they do not take messages from you".to_string(),
                "declined" => "they declined it".into(),
                "not_member" => "that computer is not in your team".into(),
                "request_id_reused" => "that request id was used for another message".into(),
                "unsupported" => "their computer cannot take it".into(),
                other => other.replace('_', " "),
            }
        ),
        "held" => format!("Sent. {person} has to allow it first (they see a prompt).{check}"),
        "queued" if remote => {
            let until = crate::date::local_time(r["until"].as_i64().unwrap_or_default());
            let why = if r["online"] == true {
                "has not confirmed it yet"
            } else {
                "is offline"
            };
            format!("Not delivered yet: {computer} {why}. Lightbulb Connect keeps trying until {until}.{check}")
        }
        "queued" => format!("Not delivered yet.{check}"),
        "stored" if remote => {
            format!("Sent. Stored on {computer} for the session's next step.{check}")
        }
        "stored" if computer == "recipient" => "Sent. The session sees it at its next step.".into(),
        "stored" => match name {
            Some(n) if !n.contains(':') => {
                format!(
                    "Sent. Waiting in the \"{}\" mailbox until someone reads it.",
                    cell(n)
                )
            }
            Some(_) => "Sent. It waits until that session is back.".into(),
            None => "Sent. It waits until it is read.".into(),
        },
        "delivered" if remote => format!("Delivered to {person} on {computer}."),
        "delivered" => "Delivered.".into(),
        "read" if remote => format!("Read by {person} on {computer}."),
        "read" => "Read.".into(),
        "expired" => "Not delivered: the other computer could not be reached for 24 hours.".into(),
        other => format!("Sent ({other})."),
    }
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
            let at = |f: &str| m[f].as_str().unwrap_or_default();
            // A teammate's text, printed to a terminal: line breaks kept, nothing it would act on.
            let text: String = at("text")
                .chars()
                .filter(|c| *c == '\n' || crate::label::printable(*c))
                .collect();
            // Continuation lines indented: no line of the body can pass for the Reply line.
            println!("From {}: {}", cell(at("label")), text.replace('\n', "\n  "));
            println!(
                "Reply: {} send {} \"…\" --reply-to {}",
                command(),
                cell(at("from")),
                cell(at("request_id"))
            );
        }
        if once && !messages.is_empty() {
            return Ok(());
        }
    }
}

/// `receipt <id> [--from <addr>] [--json]`: what became of a message, as a sentence (`said`).
pub fn receipt(request_id: &str, from: Option<String>, as_json: bool) -> Result<i32> {
    let mut conn = connect()?;
    let (from, whence) = match from {
        Some(f) => (address(&mut conn, &f)?, cell(&f)),
        None => (
            format!("{}/terminal", conn.member()?),
            "this terminal".into(),
        ),
    };
    let r = conn.call("receipt", json!({ "from": from, "request_id": request_id }))?;
    if r["ok"] == true {
        if as_json {
            println!("{}", r["receipt"]);
        } else {
            println!("{}", said(&r["receipt"], None, request_id));
        }
        Ok(0)
    } else if r["error"] == "not_found" {
        Err(format!("No message {} was sent from {whence}.", cell(request_id)).into())
    } else {
        Err(refusal(&r).into())
    }
}

/// `folder <path> on|off`: whether the sessions under `path` are shown to teammates
/// (`folders_off` in `config.toml`, canonical as presence compares them), then `reload`.
/// A folder that does not exist cannot be turned off; one turned off before it went away can
/// still be turned on (`presence::canonical` resolves it through its existing parent). `on` for
/// a folder that was not off says so and exits 1.
pub fn folder(path: &str, on: bool) -> Result<i32> {
    let dir = paths::state_dir();
    let full = std::env::current_dir()?.join(crate::config::expand_home(path));
    if !on && !full.is_dir() {
        eprintln!("no such folder: {path}");
        return Ok(1);
    }
    let p = crate::presence::canonical(&full);
    let mut off = crate::config::Config::load(&dir)?.folders_off;
    let before = off.len();
    off.retain(|o| crate::presence::canonical(o) != p);
    if on && off.len() == before {
        eprintln!("{path} was not hidden");
        return Ok(1);
    }
    if !on {
        off.push(p);
    }
    let list: toml_edit::Array = off
        .iter()
        .map(|o| o.to_string_lossy().into_owned())
        .collect();
    // Not locked: the write is atomic, and one person's concurrent `folder` calls are not a case.
    crate::config::Config::set(&dir, "folders_off", &list.to_string())?;
    let r = connect()?.call("reload", json!({}))?;
    if r["ok"] != true {
        let code = r["error"].as_str().unwrap_or("failed");
        eprintln!("Saved to config.toml, but the background service did not reload: {code}");
        return Ok(1);
    }
    if on {
        println!("Sessions in {path} are shown to your teammates.");
    } else {
        println!("Sessions in {path} are hidden from your teammates.");
    }
    Ok(0)
}

// ---- team -------------------------------------------------------------------------------

/// Prints the text `texts` maps `r`'s error code to (else `refusal`'s); returns the exit code 1.
fn refuse(r: &Value, texts: &[(&str, String)]) -> Result<i32> {
    let code = r["error"].as_str().unwrap_or_default();
    match texts.iter().find(|(c, _)| *c == code) {
        Some((_, text)) => eprintln!("{text}"),
        None => eprintln!("{}", refusal(r)),
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
        return refuse(&r, &[("not_admin", text)]);
    }
    let line = r["line"].as_str().unwrap_or_default();
    println!(
        "Send this link to the person (or paste it on your other computer): {} join '<link>'",
        command()
    );
    match mode {
        Invite::Once => {
            let at = crate::date::utc(r["expires_at"].as_i64().unwrap_or_default());
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
/// for its person to allow it. Hidden: `--kind cloud --proof <jws>`, a workspace member (the
/// bridge joins in process; this is for tests and ops).
pub fn join(link: &str, name: Option<String>, kind: &str, proof: Option<String>) -> Result<i32> {
    let name = name.unwrap_or_else(daemon::user_name);
    let mut conn = connect()?;
    if crate::ticket::decode(link).is_ok_and(|t| t.mode == "ask") {
        println!("Asking whoever sent the link to allow this computer… (they get a prompt; this waits up to 10 minutes)");
    }
    conn.wait_up_to(Duration::from_secs(660))?;
    let r = ask(
        &mut conn,
        "team.join",
        json!({"ticket": link, "name": name, "kind": kind, "proof": proof}),
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
            println!(
                "Your agents here can now see and message the team's agents. Try: {} who --all",
                command()
            );
            println!("Until both computers sign in, your other computer counts as another person and its first message needs your Allow.");
            return Ok(0);
        }
        (Some("waiting"), _) => "No one has answered yet. Run the same command again to keep asking.".to_string(),
        (_, Some(admission::ALREADY_IN_TEAM)) => format!("You are in \"{team}\". Leave it first: {} leave", command()),
        (_, Some(admission::NAME_TAKEN)) => format!(
            "The name \"{name}\" is already used in this network. Try: {} join '{link}' --name {name}-2", command()
        ),
        (_, Some(admission::UNREACHABLE)) => "No one from this network is online right now. Ask them to open their laptop, then run the same command again. Your key is ready; nothing was lost.".to_string(),
        (_, Some(admission::DECLINED)) => "Your request to join was declined.".to_string(),
        (_, Some(admission::BAD_NAME)) => "That name has characters Lightbulb Connect cannot show. Use letters, numbers, spaces and punctuation.".to_string(),
        (_, Some(admission::YOUR_COMPUTER_OFFLINE)) => "Open your other computer, then run the same command again.".to_string(),
        (_, Some(admission::BAD_TICKET)) => "This invite link does not work. Ask for a new one.".to_string(),
        (_, Some(admission::EXPIRED_TICKET)) => "This invite link has expired. Ask for a new one.".to_string(),
        (_, reason) => format!(
            "The team refused this computer ({}).",
            reason.unwrap_or("refused").replace('_', " ")
        ),
    };
    eprintln!("{text}");
    Ok(1)
}

/// How long `signin` waits for the person to confirm in the browser.
const SIGNIN_WAIT: Duration = Duration::from_secs(900);

/// `signin`: the issuer's device flow for this computer's key (`device/start`, the browser,
/// then `device/token` until it answers), the proof written to the roster (`identity.proof`).
pub fn signin() -> Result<i32> {
    let issuer = crate::config::Config::load(&paths::state_dir())?.issuer;
    let start = connect()?.call("identity.device_start", json!({}))?;
    if start["ok"] != true {
        match start["reason"].as_str().or(start["error"].as_str()) {
            Some("not_a_person") => eprintln!("Sign in from your own terminal, not from an agent."),
            reason => eprintln!("signin: {}", reason.unwrap_or("failed")),
        }
        return Ok(1);
    }
    let body = json!({"member_key": start["member_key"], "computer": start["computer"],
        "at": start["at"], "sig": start["sig"]});
    let post = |path: &str, body: &Value| http::post_json(&proof::route(&issuer, path), body);
    let (status, s) = post("device/start", &body)?;
    let (Some(code), Some(uri)) = (
        s["device_code"].as_str(),
        s["verification_uri_complete"].as_str(),
    ) else {
        eprintln!(
            "Sign-in is not available right now (the issuer answered {status}). Try again later."
        );
        return Ok(1);
    };
    // The issuer's words, as plain text: `config.toml` (its URL) is the agent's to write.
    let user_code = label::plain(s["user_code"].as_str().unwrap_or_default(), 32);
    println!(
        "Confirm the code {user_code} in your browser: {}",
        label::plain(uri, 512)
    );
    // A person at a terminal gets the browser (an https page only; debug: the tests' http);
    // a script reads the line above.
    let web = uri.starts_with("https://") || (cfg!(debug_assertions) && uri.starts_with("http://"));
    if web
        && uri.chars().all(label::printable)
        && std::io::IsTerminal::is_terminal(&std::io::stdout())
    {
        let _ = Command::new("open").arg(uri).status();
    }
    let every = Duration::from_secs(s["interval"].as_u64().unwrap_or(5).clamp(1, 60));
    let deadline = std::time::Instant::now() + SIGNIN_WAIT;
    let mut token = None;
    while token.is_none() && std::time::Instant::now() < deadline {
        let (status, t) = post("device/token", &json!({"device_code": code}))?;
        match status {
            200 if t["pending"] == true => std::thread::sleep(every),
            // A proof, or (neither) nothing more to wait for.
            200 => match t["proof"].as_str() {
                Some(p) => token = Some(p.to_string()),
                None => break,
            },
            410 => break,
            _ => {
                eprintln!("Sign-in failed (the issuer answered {status}). Try again later.");
                return Ok(1);
            }
        }
    }
    let Some(token) = token else {
        eprintln!("This sign-in code expired. Run the same command again.");
        return Ok(1);
    };
    // A fresh connection: the first may have sat idle through the wait.
    let r = connect()?.call("identity.proof", json!({ "proof": token }))?;
    if r["reason"] == "other_person" {
        eprintln!("This sign-in is for a different person than this computer already belongs to.");
        return Ok(1);
    }
    if r["ok"] != true {
        eprintln!("The issuer's answer did not verify for this computer. Try again later.");
        return Ok(1);
    }
    // `proof::parse` let only plain text in.
    let name = r["name"].as_str().unwrap_or_default();
    if r["solo"] == true {
        println!(
            "Signed in as {name}. To join your network, paste its join line: {} join '<link>'",
            command()
        );
    } else {
        println!("Signed in as {name}.");
    }
    Ok(0)
}

/// `approve <id> [--always]` / `decline <id>`: answers join request `J<n>` or held message
/// `H<n>`. `--always` (a held message from a cloud member) covers the sender's workspace.
pub fn answer(request: &str, allow: bool, always: bool) -> Result<i32> {
    let held = request.starts_with('H');
    let verdict = if allow { "allow" } else { "decline" };
    let (op, body, what, gone) = if held {
        let scope = always.then_some("always");
        let body = json!({"hold": request, "verdict": verdict, "scope": scope});
        (
            "hold.answer",
            body,
            "held message",
            "answered or has expired",
        )
    } else {
        let body = json!({"request": request, "verdict": verdict});
        ("team.admit", body, "join request", "answered or withdrawn")
    };
    let r = connect()?.call(op, body)?;
    if r["ok"] != true {
        let always_text = "--always is only for messages from a cloud member's agents.";
        let texts = [
            (
                admission::NOT_OPEN,
                format!("{request} was already {gone}."),
            ),
            (ipc::NOT_FOUND, format!("No {what} {request}.")),
            (ipc::BAD_REQUEST, always_text.into()),
        ];
        let texts = &texts[..if held { 3 } else { 2 }];
        return refuse(&r, texts);
    }
    if held {
        println!("{} {request}.", if allow { "Allowed" } else { "Declined" });
        return Ok(0);
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

/// `held`: the join requests and held messages waiting for an Allow, one per line with the
/// command that allows it; "not verified" is explained once.
pub fn held() -> Result<i32> {
    let mut conn = connect()?;
    let joins = ask(&mut conn, "team.join_requests", json!({}))?;
    let holds = ask(&mut conn, "holds", json!({}))?;
    let cmd = command();
    let f = |v: &Value, k: &str| v[k].as_str().map(cell).unwrap_or_default();
    let mut lines: Vec<String> = joins["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| {
            let id = f(j, "id");
            let (name, computer) = (f(j, "name"), f(j, "computer"));
            format!("{id}  {name} ({computer}) wants to join — {cmd} approve {id}")
        })
        .collect();
    for h in holds["holds"].as_array().into_iter().flatten() {
        let id = f(h, "id");
        let (who, to, text) = (f(h, "label"), f(h, "target_session"), f(h, "preview"));
        lines.push(format!(
            "{id}  {who} -> {to}  \"{text}\" — {cmd} approve {id}"
        ));
    }
    if lines.is_empty() {
        println!("No messages are waiting for your Allow.");
    }
    let mut explained = false;
    for line in lines {
        let unverified = " (not verified)";
        if !explained && line.contains(unverified) {
            explained = true;
            println!(
                "{}",
                line.replacen(unverified, " (not verified — they have not signed in)", 1)
            );
        } else {
            println!("{line}");
        }
    }
    Ok(0)
}

/// `policy [<person> accept|hold|refuse]`: lists the policy (rows the bridge set marked so),
/// or sets `<person>` (a `ws:<id>` workspace in `cloud_of`, anyone else in `people`).
pub fn policy(person: Option<String>, value: Option<String>) -> Result<i32> {
    if value
        .as_deref()
        .is_some_and(|v| !["accept", "hold", "refuse"].contains(&v))
    {
        return Err("policy takes accept, hold or refuse.".into());
    }
    let mut conn = connect()?;
    let p = match (person, value) {
        (Some(pid), Some(v)) => ask(
            &mut conn,
            "policy.set",
            json!({"person_id": pid, "value": v}),
        )?,
        (None, None) => ask(&mut conn, "policy.get", json!({}))?,
        _ => {
            let cmd = command();
            eprintln!("Usage: {cmd} policy [<person> accept|hold|refuse]\n  <person> is a person id as {cmd} policy lists it (ws:<id> for a workspace's agents).\n  Example: {cmd} policy ws:acme accept");
            return Ok(2);
        }
    };
    let by = |key: String| match p["set_by"][&key].as_str() {
        Some("bridge") => " (set by the Lightbulb app)",
        _ => "",
    };
    let on = if p["agents_send_without_asking"] == true {
        "yes"
    } else {
        "no"
    };
    println!(
        "Agents may message teammates without asking you: {on}.{}",
        by(crate::policy::SWITCH.into())
    );
    for scope in ["people", "cloud_of"] {
        for (pid, v) in p[scope].as_object().into_iter().flatten() {
            let v = match v.as_str() {
                Some("accept") => "accepted",
                Some("hold") => "held for your Allow",
                Some("refuse") => "refused",
                _ => "unknown",
            };
            println!(
                "Messages from {}: {v}.{}",
                cell(pid),
                by(format!("{scope}:{pid}"))
            );
        }
    }
    Ok(0)
}

/// `team`: the team's name and members, this computer first as `You`; a member's address only
/// where its name is shared (what `send` then needs).
pub fn team() -> Result<i32> {
    let mut conn = connect()?;
    let me = conn.member()?;
    let t = ask(&mut conn, "team.get", json!({}))?;
    let mut members = t["members"].as_array().cloned().unwrap_or_default();
    members.sort_by_key(|m| m["short"] != me.as_str());
    let n = members.len();
    let s = if n == 1 { "" } else { "s" };
    println!(
        "Team \"{}\" — {n} computer{s}",
        cell(t["name"].as_str().unwrap_or_default())
    );
    for m in &members {
        let f = |k: &str| m[k].as_str().map(cell).unwrap_or_default();
        let name = f("name");
        let mut line = if m["short"] == me.as_str() {
            format!("  You ({name}) · {} · {}", f("computer"), f("role"))
        } else {
            format!("  {name} · {} · {}", f("computer"), f("role"))
        };
        if m["online"] == false {
            line.push_str(" · offline");
        }
        if members.iter().filter(|o| o["name"] == m["name"]).count() > 1 {
            line.push_str(&format!(" · address: {}", f("short")));
        }
        println!("{line}");
    }
    println!("Invite someone: {} invite", command());
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
        return refuse(&r, &texts);
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
        return refuse(&r, &texts);
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
        let text = format!(
            "You are the founder. Name a new founder first: {} admin <name> --founder",
            command()
        );
        return refuse(&r, &[("founder_must_transfer", text)]);
    }
    println!("Left \"{name}\". This computer is on its own network again.");
    if r["told"] == false {
        println!("Could not tell the team; they will see you gone when you are removed.");
    }
    Ok(0)
}

/// The Lightbulb app's own CLI verbs that this program does not own: `forward` hands them on.
const APP_VERBS: [&str; 3] = ["claude", "codex", "share"];

/// Hands one of the app's verbs to the Lightbulb app's binary as its CLI (`cli` first, as the
/// app reads it), replacing this process; anything else, or no app, is an unknown command.
pub fn forward(args: Vec<OsString>) -> Result<i32> {
    let app = std::env::var_os(DESKTOP_ENV).map_or_else(|| PathBuf::from(DESKTOP), PathBuf::from);
    let verb = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !APP_VERBS.contains(&verb.as_str()) || !app.exists() {
        eprintln!(
            "Unknown command: {}. Run {} --help.",
            cell(&verb),
            command()
        );
        return Ok(2);
    }
    Err(Command::new(app).arg("cli").args(args).exec().into())
}

// ---- alias and launchd ------------------------------------------------------------------

/// Puts this executable at the canonical path (unless it is already there with the same
/// bytes), the `~/.local/bin/lightbulb-connect` symlink at that, and `~/.local/bin/lightbulb`
/// when that name is free. Returns the command name to type, one line. A regular file or a
/// foreign symlink at either path is left alone.
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
    let full = link(&home.join(".local/bin/lightbulb-connect"), &canon)?;
    Ok(match (link(&home.join(".local/bin/lightbulb"), &canon)?, full) {
        (true, _) => "Command: lightbulb (or lightbulb-connect)".into(),
        (false, true) => format!(
            "Command: lightbulb-connect (\"lightbulb\" belongs to {})",
            link_owner(&home.join(".local/bin/lightbulb"))
        ),
        (false, false) => format!(
            "Command: {} (lightbulb and lightbulb-connect in ~/.local/bin belong to other programs)",
            canon.display()
        ),
    })
}

/// Who has the command name at `path` that is not ours: the Lightbulb app when it links to the
/// app's binary, else another program.
fn link_owner(path: &Path) -> &'static str {
    match fs::read_link(path) {
        Ok(t) if t.to_string_lossy().contains("Lightbulb.app") => "the Lightbulb app",
        _ => "another program",
    }
}

/// Makes `path` a symlink to `canon` unless something else is there; whether it is ours.
fn link(path: &Path, canon: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(m.is_symlink() && fs::read_link(path)? == canon),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path.parent().unwrap())?;
            symlink(canon, path)?;
            Ok(true)
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
            println!(
                "Background service: could not start it ({}).",
                path.display()
            );
            return Ok(1);
        }
        println!("Background service: running (starts at login).");
    } else {
        println!("Background service: launchctl skipped (would run: {sequence})");
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
    let state = paths::state_path();
    let shown = home()
        .ok()
        .and_then(|h| state.strip_prefix(h).ok().map(|p| Path::new("~").join(p)))
        .unwrap_or(state);
    println!(
        "Lightbulb Connect is off. Your settings and messages are kept in {}.",
        shown.display()
    );
    Ok(0)
}

// ---- doctor -----------------------------------------------------------------------------

/// `doctor`: one line per check, then `All good.` or the command that fixes the first that
/// failed. (Whether macOS let it read a folder is not something it can ask.)
pub fn doctor() -> Result<i32> {
    let home = home()?;
    let cmd = command();
    let setup = format!("Run: {cmd} setup");
    // (check, what it found, the fix when it failed)
    let mut rows: Vec<(String, String, Option<String>)> = vec![];
    let up = Conn::open()
        .and_then(|mut c| c.call("ping", json!({})))
        .is_ok_and(|r| r["ok"] == true);
    let (state, fix) = if up {
        ("running", None)
    } else {
        ("not running", Some(setup.clone()))
    };
    rows.push(("Background service".into(), state.into(), fix));
    if cfg!(target_os = "macos") {
        let loaded = if use_launchctl() {
            launchctl(&[
                "print",
                &format!("gui/{}/{LABEL}", unsafe { libc::getuid() }),
            ])
        } else {
            plist_path(&home).exists()
        };
        let (state, fix) = if loaded {
            ("yes", None)
        } else {
            ("no", Some(setup.clone()))
        };
        rows.push(("Starts at login".into(), state.into(), fix));
    }
    let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777).ok();
    let dir = paths::state_path();
    let socket = dir.join(paths::SOCKET);
    let private = mode(&dir) == Some(0o700) && (!socket.exists() || mode(&socket) == Some(0o600));
    if dir.exists() {
        let (state, fix) = if private {
            ("private", None)
        } else {
            (
                "readable by others",
                Some(format!("Run: chmod 700 {}", dir.display())),
            )
        };
        rows.push(("State folder".into(), state.into(), fix));
    }
    for (agent, dot) in [
        ("claude", ".claude"),
        ("codex", ".codex"),
        ("cursor", ".cursor"),
        ("grok", ".grok"),
    ] {
        let state = match (
            home.join(dot).is_dir(),
            setup::wired(&home, agent),
            setup::tools(&home, agent),
        ) {
            (false, ..) => Ok("not installed"),
            (_, true, Some(_)) => Ok("connected"),
            (_, true, None) => Err("hooks only — tools missing (run setup)"),
            (_, false, _) => Err("not connected"),
        };
        let name = crate::frame::agent_name(agent);
        match state {
            Ok(s) => rows.push((name, s.into(), None)),
            Err(s) => rows.push((name, s.into(), Some(setup.clone()))),
        }
    }
    let ours = |p: &Path| fs::read_link(p).is_ok_and(|t| t == canonical(&home));
    let (short, full) = (
        home.join(".local/bin/lightbulb"),
        home.join(".local/bin/lightbulb-connect"),
    );
    let owner = link_owner(&short);
    let (state, fix) = if ours(&short) {
        ("\"lightbulb\"".to_string(), None)
    } else if ours(&full) && short.symlink_metadata().is_ok() {
        (
            format!("\"lightbulb\" belongs to {owner}; type \"lightbulb-connect\""),
            None,
        )
    } else if ours(&full) {
        ("\"lightbulb-connect\"".to_string(), None)
    } else {
        ("not installed".to_string(), Some(setup.clone()))
    };
    rows.push(("Command name".into(), state, fix));
    // legacy (Plan A install): its daemon on its old socket.
    if std::os::unix::net::UnixStream::connect(home.join(".lightbulb/network/sock")).is_ok() {
        let fix = format!("Run: {cmd} setup --remove, then {cmd} setup");
        rows.push(("Old version".into(), "still running".into(), Some(fix)));
    }
    for (name, state, _) in &rows {
        println!("{name:<20} {state}");
    }
    match rows.into_iter().find_map(|(.., fix)| fix) {
        Some(fix) => {
            println!("{fix}");
            Ok(1)
        }
        None => {
            println!("All good.");
            Ok(0)
        }
    }
}

/// `setup [--remove]`: wires the installed agents to the canonical binary (or `executable`)
/// and starts the background service; prints one line per agent, then what to try next. Exits
/// 1 when an agent ends with hooks but no tools. `--remove` is the off switch: our entries,
/// the old version's, the job and the running service go.
pub fn setup(
    remove: bool,
    executable: Option<PathBuf>,
    server: Option<String>,
    verbose: bool,
) -> Result<i32> {
    if server
        .as_deref()
        .is_some_and(|s| !setup::valid_server_name(s))
    {
        return Err("--server-name takes letters, digits, - and _.".into());
    }
    if let Some(e) = executable.as_ref().filter(|e| !remove && !e.exists()) {
        return Err(format!("--executable {} does not exist.", e.display()).into());
    }
    let home = home()?;
    if !remove {
        install_running(&home)?;
    }
    // Wired to the canonical copy, never to wherever this binary was built.
    let executable = executable.unwrap_or_else(|| canonical(&home));
    let report = setup::run(&home, &executable, server.as_deref(), remove);
    let files = |a: &setup::AgentReport| {
        if verbose {
            for f in &a.wrote {
                println!("  wrote {}", f.display());
            }
        }
    };
    if remove {
        // legacy (Plan A install): its hook and MCP entries name the old binary.
        let old = setup::run(&home, &legacy_canonical(&home), server.as_deref(), true);
        for a in report
            .agents
            .iter()
            .chain(&old.agents)
            .filter(|a| !a.wrote.is_empty())
        {
            println!("{}: removed", a.product());
            files(a);
        }
        legacy_uninstall(&home)?;
        return if cfg!(target_os = "macos") {
            daemon_uninstall()
        } else {
            Ok(0)
        };
    }
    for a in &report.agents {
        println!("{}", a.line());
        files(a);
    }
    if cfg!(target_os = "macos") {
        let code = install_job(&home, &executable)?;
        if code != 0 {
            return Ok(code);
        }
    }
    let names = |f: fn(&setup::AgentReport) -> bool| -> Vec<String> {
        report
            .agents
            .iter()
            .filter(|a| f(a))
            .map(|a| a.product())
            .collect()
    };
    let (clash, failed) = (names(|a| a.clash()), names(|a| a.error.is_some()));
    let verb = |n: &[String], one: &'static str, many: &'static str| {
        if n.len() == 1 {
            one
        } else {
            many
        }
    };
    if !clash.is_empty() {
        eprintln!(
            "{} {} hooks but no tools: another program has the tool server name. Run: {} setup --server-name <a free name>",
            clash.join(", "),
            verb(&clash, "has", "have"),
            command()
        );
    }
    if !failed.is_empty() {
        eprintln!(
            "{} {} not connected: fix the file named above, then run {} setup again.",
            failed.join(", "),
            verb(&failed, "is", "are"),
            command()
        );
    }
    if !clash.is_empty() || !failed.is_empty() {
        return Ok(1);
    }
    let connected: Vec<String> = report
        .agents
        .iter()
        .filter(|a| a.tools.is_some())
        .map(|a| a.product())
        .collect();
    let connected: Vec<&str> = connected.iter().map(String::as_str).collect();
    println!("\n{}", setup::summary(&connected, command()));
    Ok(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_title_with_control_characters_prints_as_plain_text() {
        let title = "fix\u{1b}[2J\u{1b}]8;;http://x\u{7}\tit\u{85}\u{2066}now\r\nok";
        assert_eq!(super::cell(title), "fix[2J]8;;http://x itnow ok");
    }
}
