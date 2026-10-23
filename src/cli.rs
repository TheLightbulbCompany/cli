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
/// The Lightbulb app's own CLI verbs that this program does not own: `forward` hands them on.
pub const APP_VERBS: [&str; 4] = ["claude", "codex", "share", "continue"];
/// Names a session the invocation speaks as (nothing sets it yet).
const SESSION_ENV: &str = "LIGHTBULB_CONNECT_SESSION";

/// A "(tests)" environment override: read in debug builds only, as `net::Knobs::read` does.
fn test_env(name: &str) -> Option<std::ffi::OsString> {
    if cfg!(debug_assertions) {
        std::env::var_os(name)
    } else {
        None
    }
}

pub fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| "no home directory".into())
}

pub fn canonical(home: &Path) -> PathBuf {
    home.join(".lightbulb/connect/bin/lightbulb")
}

/// The old name beside it (0.2.1's canonical binary), now a link to it: old hooks, launchd
/// jobs and `~/.local/bin/lightbulb-connect` links keep working.
pub fn compat(home: &Path) -> PathBuf {
    home.join(".lightbulb/connect/bin/lightbulb-connect")
}

/// A link to the Lightbulb app's binary, kept when setup takes `~/.local/bin/lightbulb` from
/// it, so `forward` still finds the app wherever it lives.
fn app_link(home: &Path) -> PathBuf {
    home.join(".lightbulb/connect/app")
}

/// The Lightbulb app's binary that `forward` hands the app's verbs to: the one setup kept,
/// else the app's usual place.
fn app_binary(home: &Path) -> Option<PathBuf> {
    let usual = test_env(DESKTOP_ENV).map_or_else(|| PathBuf::from(DESKTOP), PathBuf::from);
    [app_link(home), usual].into_iter().find(|p| p.exists())
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
        if let Ok(mut s) = legacy_socket(home) {
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

/// legacy (Plan A install): a connection to the old daemon, when one is running.
fn legacy_socket(home: &Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(home.join(".lightbulb/network/sock"))
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

/// Who has a command name in `~/.local/bin`.
#[derive(Clone, Copy, PartialEq)]
enum Link {
    Ours,
    App,
    Other,
    Missing,
}

/// `~/.local/bin`'s two command names: `lightbulb` (`short`) and `lightbulb-connect`.
#[derive(Clone, Copy)]
struct Alias {
    short: Link,
    full: Link,
}

impl Alias {
    fn read(home: &Path) -> Alias {
        let link = |name: &str| {
            let path = home.join(".local/bin").join(name);
            match fs::read_link(&path) {
                Ok(t) if t == canonical(home) || t == compat(home) => Link::Ours,
                Ok(t) if is_app(&t) => Link::App,
                _ if path.symlink_metadata().is_ok() => Link::Other,
                _ => Link::Missing,
            }
        };
        Alias {
            short: link("lightbulb"),
            full: link("lightbulb-connect"),
        }
    }

    /// The command name to type: `lightbulb`, unless another program holds that name and
    /// `lightbulb-connect` is ours.
    fn command(self) -> &'static str {
        if matches!(self.short, Link::App | Link::Other) && self.full == Link::Ours {
            "lightbulb-connect"
        } else {
            "lightbulb"
        }
    }

    /// Who has `lightbulb` when it is not ours.
    fn short_owner(self) -> &'static str {
        if self.short == Link::App {
            "the Lightbulb app"
        } else {
            "another program"
        }
    }
}

/// Whether a link target is the Lightbulb app's binary (what the app's own CLI link points at).
fn is_app(target: &Path) -> bool {
    target.to_string_lossy().contains("Lightbulb.app")
        || target.file_name().is_some_and(|f| f == "lightbulb-desktop")
}

/// The command name that runs this program here: `lightbulb`, or `lightbulb-connect` until
/// setup has taken `lightbulb` (see `Alias::command`). Read once per process.
pub fn command() -> &'static str {
    static COMMAND: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    COMMAND.get_or_init(|| home().map_or("lightbulb", |h| Alias::read(&h).command()))
}

/// `addr` as the daemon takes it: a `who` handle (`codex-1`) as its session id (refused when
/// no session has it now), and `<member name>/<rest>` as `<member>/<rest>`; an id or a bare
/// name as given.
fn address(conn: &mut Conn, addr: &str) -> Result<String> {
    let Some((m, rest)) = addr.split_once('/') else {
        let r = conn.call("sessions", json!({ "handle": addr }))?;
        return match (r["error"].as_str(), r["sessions"][0]["key"].as_str()) {
            (_, Some(key)) => Ok(key.into()),
            (Some(ipc::NOT_FOUND), _) => Err(format!(
                "No session \"{addr}\" is open right now. Run {} who to see the current ones.",
                command()
            )
            .into()),
            _ => Ok(addr.into()),
        };
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
    crate::label::plain(&s.replace(['\n', '\t'], " "), usize::MAX)
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
    // (header, cell, also without `--all`)
    type Cell = fn(&Value) -> String;
    fn text(s: &Value, f: &str) -> Option<String> {
        s[f].as_str().map(cell).filter(|t| !t.is_empty())
    }
    let columns: [(&str, Cell, bool); 7] = [
        // A handle, else (a teammate's session, or one not registered yet) the address `send`
        // takes.
        (
            "Name",
            |s| {
                text(s, "handle")
                    .or(text(s, "session_id"))
                    .or(text(s, "key"))
                    .unwrap_or_default()
            },
            true,
        ),
        ("Person", |s| text(s, "person").unwrap_or_default(), false),
        (
            "Computer",
            |s| text(s, "computer").unwrap_or_default(),
            false,
        ),
        (
            "Agent",
            |s| {
                crate::frame::agent_name(
                    &text(s, "agent").or(text(s, "provider")).unwrap_or_default(),
                )
            },
            true,
        ),
        // A teammate's session has no folder here: its repo instead.
        (
            "Folder",
            |s| text(s, "cwd").or(text(s, "repo")).unwrap_or_default(),
            true,
        ),
        (
            "Status",
            |s| {
                let status = if s["status"] == "live" {
                    "busy"
                } else {
                    "idle"
                };
                let gets = match s["tier"].as_str() {
                    Some("push") => "instantly",
                    Some("next_step") => "at its next step",
                    _ => "when it checks",
                };
                let hidden = if s["hidden"] == true {
                    ", hidden from teammates"
                } else {
                    ""
                };
                format!("{status}, gets messages {gets}{hidden}")
            },
            true,
        ),
        (
            "Doing",
            |s| text(s, "doing").or(text(s, "title")).unwrap_or_default(),
            true,
        ),
    ];
    let columns: Vec<_> = columns.into_iter().filter(|c| all || c.2).collect();
    let mut rows = vec![columns.iter().map(|c| c.0.to_string()).collect::<Vec<_>>()];
    rows.extend(
        list.iter()
            .map(|s| columns.iter().map(|c| (c.1)(s)).collect()),
    );
    table(&rows);
    Ok(0)
}

/// Whether this invocation may be a session: the env names one, an agent is above us, or the
/// ancestry could not be walked to a person (fails closed).
fn session_identity() -> bool {
    test_env(SESSION_ENV).is_some()
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
    let from = from.or_else(|| test_env(SESSION_ENV).and_then(|s| s.into_string().ok()));
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
        let line = said(receipt, Some(&to), &rid, true);
        if group {
            println!("{line} ({})", cell(&handle));
        } else {
            println!("{line}");
        }
    }
    Ok(code)
}

/// What receipt `r` of message `rid` (to `to`, when known) means, for the person who sent it.
fn said(r: &Value, to: Option<&str>, rid: &str, check: bool) -> String {
    let p = crate::mail::ForPerson {
        to,
        rid,
        cmd: command(),
        check,
    };
    crate::mail::receipt_note(r, Some(&p)).unwrap_or_default()
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
            // A teammate's text, printed to a terminal: nothing it would act on, and its later
            // lines indented, so none can pass for the Reply line.
            let lines: Vec<String> = at("text").split('\n').map(cell).collect();
            println!("From {}: {}", cell(at("label")), lines.join("\n  "));
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
            println!("{}", said(&r["receipt"], None, request_id, false));
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
        eprintln!("There is no folder at {path}.");
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
    match mode {
        Invite::Once => {
            let at = crate::date::utc(r["expires_at"].as_i64().unwrap_or_default());
            println!("One-time invite, expires {at}.");
        }
        Invite::Open => {
            println!(
                "Anyone with this line joins without asking. Use it only with people you trust."
            );
        }
        _ if r["replaced"] == true => println!("The old invite stopped working."),
        _ => {}
    }
    println!("Send this line to the person (or paste it on your other computer):");
    println!("{line}");
    println!("Replace NAME with the name they go by.");
    println!("{}", r["app_link"].as_str().unwrap_or_default());
    Ok(0)
}

/// `join '<ticket>' [--name <n>]`: joins the network the join line `link` is for, waiting up to 600 s
/// for its person to allow it. Hidden: `--kind cloud --proof <jws>`, a workspace member (the
/// bridge joins in process; this is for tests and ops).
pub fn join(link: &str, name: Option<String>, kind: &str, proof: Option<String>) -> Result<i32> {
    let name = name.unwrap_or_else(daemon::user_name);
    let mut conn = connect()?;
    // Said once a member took an `ask` ticket and is asking its person (`join_asking`), never
    // before a ticket it refuses at once, nor for an open link (it asks no one).
    if crate::ticket::decode(link).is_ok_and(|t| t.mode == "ask") {
        let mut asking = Conn::open()?;
        asking.call("subscribe", json!({}))?;
        asking.wait_up_to(Duration::from_secs(660))?;
        std::thread::spawn(move || {
            while let Ok(e) = asking.next_line() {
                if e["event"] == "join_asking" {
                    println!("Asking whoever sent the link to allow this computer… (they get a prompt; this waits up to 10 minutes)");
                    return;
                }
            }
        });
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
            "Signed in as {name}. To join your network, run the line your inviter sent: {} join '<ticket>' --name <your name>",
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
                match r["answered_in"].as_str() {
                    Some("dialog") => format!("{request} was already answered in the dialog."),
                    Some("phone") => format!("{request} was already answered from your phone."),
                    Some(_) => format!("{request} was already answered at the command line."),
                    None => format!("{request} was already {gone}."),
                },
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
    // "not verified" explained at its first appearance only.
    let mut explained = false;
    for h in holds["holds"].as_array().into_iter().flatten() {
        let (id, mut who) = (f(h, "id"), f(h, "label"));
        if h["verified"] == false && !explained {
            explained = true;
            who = who.replacen(
                " (not verified)",
                " (not verified — they have not signed in)",
                1,
            );
        }
        let (to, text) = (f(h, "target_session"), f(h, "preview"));
        lines.push(format!(
            "{id}  {who} -> {to}  \"{text}\" — {cmd} approve {id}"
        ));
    }
    if lines.is_empty() {
        println!("No messages are waiting for your Allow.");
    }
    for line in lines {
        println!("{line}");
    }
    Ok(0)
}

/// `policy [<person> accept|hold|refuse]`: lists the policy (rows the bridge set marked so),
/// or sets `<person>` (a `ws:<id>` workspace in `cloud_of`, anyone else in `people`).
pub fn policy(person: Option<String>, value: Option<String>) -> Result<i32> {
    if value
        .as_deref()
        .is_some_and(|v| !["accept", "hold", "refuse", "forget"].contains(&v))
    {
        return Err("Policy takes accept, hold or refuse.".into());
    }
    let mut conn = connect()?;
    let mut team = None;
    let p = match (person, value) {
        // An old row, stored under whatever it says: dropped as written.
        (Some(who), Some(v)) if v == "forget" => ask(
            &mut conn,
            "policy.set",
            json!({"person_id": who, "value": v}),
        )?,
        (Some(who), Some(v)) => {
            let (members, mine) = team_members(&mut conn)?;
            let pid = policy_person(&members, &who)?;
            if mine.as_deref() == Some(pid.as_str()) {
                return Err("That is you; your own computers always get through.".into());
            }
            team = Some((members, mine));
            ask(
                &mut conn,
                "policy.set",
                json!({"person_id": pid, "value": v}),
            )?
        }
        (None, None) => ask(&mut conn, "policy.get", json!({}))?,
        _ => {
            let cmd = command();
            eprintln!("Usage: {cmd} policy [<person> accept|hold|refuse]\n  <person> is a teammate's name or short (as {cmd} team lists it), or ws:<id> for a workspace's agents.\n  Example: {cmd} policy aden hold");
            return Ok(2);
        }
    };
    const SCOPES: [&str; 2] = ["people", "cloud_of"];
    // The members' names only when some row needs them.
    let rows = SCOPES
        .iter()
        .any(|s| p[*s].as_object().is_some_and(|o| !o.is_empty()));
    let (members, mine) = match team {
        Some(t) => t,
        None if rows => team_members(&mut conn)?,
        None => (vec![], None),
    };
    let member = |pid: &str| members.iter().find(|m| m["person_id"] == pid);
    let name =
        |pid: &str| member(pid).and_then(|m| Some((m["name"].as_str()?, m["short"].as_str()?)));
    // `aden (qxcrztez36jw)`; a person no member is (a workspace) as stored.
    let name_of = |pid: &str| match name(pid) {
        Some((name, short)) => format!("{} ({short})", cell(name)),
        None => cell(pid),
    };
    let by = |key: String, who: Option<&str>| match p["set_by"][&key].as_str() {
        Some("bridge") => match who {
            Some(pid) => {
                let n = name(pid).map_or(pid, |(n, _)| n);
                format!(" (Allowed {} from your phone)", cell(n))
            }
            None => " (allowed from your phone)".into(),
        },
        _ => String::new(),
    };
    let on = if p["agents_send_without_asking"] == true {
        "yes"
    } else {
        "no"
    };
    println!(
        "Your workspace's agents may message your sessions without asking: {on}.{}",
        by(crate::policy::SWITCH.into(), None)
    );
    for scope in SCOPES {
        for (pid, v) in p[scope].as_object().into_iter().flatten() {
            let v = match v.as_str() {
                Some("accept") => "accepted",
                Some("hold") => "held for your Allow",
                Some("refuse") => "refused",
                _ => "unknown",
            };
            let forget = format!("run: {} policy {} forget", command(), cell(pid));
            // A row for my own person (`decide` never reads it), or a `people` row no member
            // has (an old one, stored under a literal name).
            let note = if mine.as_deref() == Some(pid.as_str()) {
                format!(" (ignored: your own computers always get through — {forget})")
            } else if scope == "people" && member(pid).is_none() {
                format!(" (old entry — {forget})")
            } else {
                String::new()
            };
            println!(
                "Messages from {}: {v}.{note}{}",
                name_of(pid),
                by(format!("{scope}:{pid}"), Some(pid))
            );
        }
    }
    Ok(0)
}

/// The team's members (`team.get`) and this computer's person id among them.
fn team_members(conn: &mut Conn) -> Result<(Vec<Value>, Option<String>)> {
    let me = conn.member()?;
    let team = ask(conn, "team.get", json!({}))?;
    let members = team["members"].as_array().cloned().unwrap_or_default();
    let mine = members
        .iter()
        .find(|m| m["short"] == me.as_str())
        .and_then(|m| m["person_id"].as_str().map(String::from));
    Ok((members, mine))
}

/// The person id `policy <who>` sets: the one member whose name (any case), short, key or
/// person id is `who`; a `ws:` id as given. Several people, or none: a sentence.
fn policy_person(members: &[Value], who: &str) -> Result<String> {
    let field = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let hits: Vec<&Value> = members
        .iter()
        .filter(|m| {
            field(m, "name").eq_ignore_ascii_case(who)
                || ["short", "key", "person_id"]
                    .iter()
                    .any(|k| field(m, k) == who)
        })
        .collect();
    let mut people: Vec<String> = hits.iter().map(|m| field(m, "person_id")).collect();
    people.sort();
    people.dedup();
    match people.len() {
        1 => Ok(people.remove(0)),
        0 if who.starts_with("ws:") => Ok(who.into()),
        0 => {
            let names: Vec<String> = members.iter().map(|v| cell(&field(v, "name"))).collect();
            Err(format!(
                "No teammate called \"{}\". Teammates: {} (see {} team).",
                cell(who),
                names.join(", "),
                command()
            )
            .into())
        }
        _ => {
            let each: Vec<String> = hits
                .iter()
                .map(|m| {
                    format!(
                        "{} on {}: {}",
                        cell(&field(m, "name")),
                        cell(&field(m, "computer")),
                        field(m, "short")
                    )
                })
                .collect();
            Err(format!(
                "More than one teammate is called \"{}\". Use the short of the one you mean: {}.",
                cell(who),
                each.join("; ")
            )
            .into())
        }
    }
}

/// `team`: the team's name and members, this computer first as `You`; a member's address only
/// where its name is shared (what `send` then needs).
pub fn team() -> Result<i32> {
    let mut conn = connect()?;
    let me = conn.member()?;
    let t = ask(&mut conn, "team.get", json!({}))?;
    let mut members = t["members"].as_array().cloned().unwrap_or_default();
    let r = &t["removed"];
    if let (Some(team), Some(by), Some(at)) =
        (r["team"].as_str(), r["by"].as_str(), r["at"].as_i64())
    {
        println!(
            "You were removed from \"{}\" by {} on {}. This computer is on its own network again.",
            cell(team),
            cell(by),
            &crate::date::utc(at)[..10]
        );
    }
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
    println!("Removed {n}. New connections and new mail from it are blocked. The old invite stopped working; send this line instead: {line}");
    Ok(0)
}

/// `leave`: after a yes, this computer leaves its team and is on its own network again. The
/// service keeps running.
pub fn leave() -> Result<i32> {
    let mut conn = connect()?;
    let me = conn.member()?;
    let t = ask(&mut conn, "team.get", json!({}))?;
    let name = t["name"].as_str().unwrap_or_default().to_string();
    // Alone after a removal: there is nothing to leave. (A computer alone by choice may still
    // leave, which starts its network afresh and voids its links.)
    let members = t["members"].as_array().cloned().unwrap_or_default();
    let alone = members.len() == 1 && members[0]["short"] == me.as_str();
    if alone && !t["removed"].is_null() {
        println!("This computer is not in a team with anyone; it is on its own network already.");
        return Ok(0);
    }
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

/// `relay [<url> | --clear] [--local]`: sets this computer's relay to `set` (`""` clears it),
/// and from the founder or an admin the team's unless `local`; alone, prints the relays in
/// effect and where they come from.
pub fn relay(set: Option<String>, local: bool) -> Result<i32> {
    let mut conn = connect()?;
    if let Some(url) = set {
        let r = conn.call("relay.set", json!({ "url": url, "local": local }))?;
        if r["ok"] != true {
            let texts = [("bad_request", format!("Not an https relay URL: {url}"))];
            return refuse(&r, &texts);
        }
        let who = if r["team"] == true {
            "this computer and the team"
        } else {
            "this computer"
        };
        match url.as_str() {
            "" => {
                let l = ask(&mut conn, "relay.get", json!({}))?;
                match l["source"].as_str() {
                    Some("default") => println!("Relay cleared for {who}. Back to the default: the iroh project's public relays."),
                    _ => println!("Relay cleared for {who}."),
                }
            }
            u => {
                let canonical = crate::relay::parse(u).map_or(u.to_string(), |r| r.to_string());
                println!("Relay for {who}: {canonical}")
            }
        }
        return Ok(0);
    }
    let l = ask(&mut conn, "relay.get", json!({}))?;
    let source = l["source"].as_str().unwrap_or("none");
    let urls: Vec<String> = (l["urls"].as_array().into_iter().flatten())
        .map(|u| cell(u.as_str().unwrap_or_default()))
        .collect();
    if source == "default" {
        let hosts: Vec<String> = (urls.iter()).map(|u| relay_host(u)).collect();
        println!(
            "default  the iroh project's public relays ({})",
            hosts.join(", ")
        );
        println!("Set your own: {} relay <url>", command());
    } else if urls.is_empty() {
        println!("none (direct connections only)");
    }
    for u in urls.iter().filter(|_| source != "default") {
        println!("{source}  {u}");
    }
    if let Some(e) = l["error"].as_str() {
        println!("Our relay list was refused: {}", cell(e));
    }
    Ok(0)
}

/// `https://use1-1.relay.n0.iroh.link./` -> `use1-1.relay.n0.iroh.link`.
fn relay_host(url: &str) -> String {
    let host = url.split_once("://").map_or(url, |(_, h)| h);
    host.trim_end_matches('/').trim_end_matches('.').to_string()
}

/// Hands one of the app's verbs to the Lightbulb app's binary as its CLI (`cli` first, as the
/// app reads it), replacing this process. With no app, `claude`/`codex` run the real tool from
/// PATH (the retired app's shell functions call `lightbulb claude`), and `continue`/`share` say
/// they need the app. Anything else is an unknown command.
pub fn forward(args: Vec<OsString>) -> Result<i32> {
    let verb = args
        .first()
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !APP_VERBS.contains(&verb.as_str()) {
        eprintln!(
            "Unknown command: {}. Run {} --help.",
            cell(&verb),
            command()
        );
        return Ok(2);
    }
    let home = home()?;
    if let Some(app) = app_binary(&home) {
        return Err(Command::new(app).arg("cli").args(args).exec().into());
    }
    if verb != "claude" && verb != "codex" {
        eprintln!("{verb} needs the Lightbulb app.");
        return Ok(2);
    }
    // The first `claude`/`codex` on PATH that is not this program or the app's link.
    let me = std::env::current_exe().and_then(fs::canonicalize).ok();
    let skip = [me, fs::canonicalize(app_link(&home)).ok()];
    let real = std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .map(|d| d.join(&verb))
        .find(|p| {
            let c = fs::canonicalize(p).ok();
            c.is_some()
                && !skip.contains(&c)
                && p.metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
        });
    let Some(real) = real else {
        eprintln!("{verb} is not installed.");
        return Ok(127);
    };
    Err(Command::new(real).args(&args[1..]).exec().into())
}

// ---- alias and launchd ------------------------------------------------------------------

/// Puts this executable at the canonical path (unless it is already there with the same
/// bytes), `lightbulb-connect` beside it as a link to it, and `~/.local/bin/lightbulb` and
/// `~/.local/bin/lightbulb-connect` at that. `~/.local/bin/lightbulb` is taken from the
/// Lightbulb app (its target kept as `app_link`, so its verbs still reach it); a regular file
/// or another program's link is left alone. Returns what to type, one or two lines.
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
    // 0.2.1's binary at the old name becomes a link (a running daemon keeps its file).
    if fs::read_link(compat(home)).ok().as_deref() != Some(Path::new("lightbulb")) {
        relink(&compat(home), Path::new("lightbulb"))?;
    }
    let bin = home.join(".local/bin");
    let short = bin.join("lightbulb");
    if Alias::read(home).short == Link::App {
        relink(
            &app_link(home),
            &short.parent().unwrap().join(fs::read_link(&short)?),
        )?;
        relink(&short, &canon)?;
    }
    for name in ["lightbulb-connect", "lightbulb"] {
        link(&bin.join(name), &canon)?;
    }
    let alias = Alias::read(home);
    Ok(match (alias.short, alias.full) {
        (Link::Ours, _) if app_binary(home).is_some() => {
            "Command: lightbulb\nlightbulb claude/codex/continue still open the Lightbulb app.".into()
        }
        (Link::Ours, _) => "Command: lightbulb".into(),
        (_, Link::Ours) => format!(
            "Command: lightbulb-connect (\"lightbulb\" belongs to {})",
            alias.short_owner()
        ),
        _ => format!(
            "Command: {} (lightbulb and lightbulb-connect in ~/.local/bin belong to other programs)",
            canon.display()
        ),
    })
}

/// Makes `path` a symlink to `target`, replacing whatever is there (atomically: a link made
/// beside it is renamed over it).
fn relink(path: &Path, target: &Path) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("lightbulb-tmp");
    let _ = fs::remove_file(&tmp);
    symlink(target, &tmp)?;
    Ok(fs::rename(&tmp, path)?)
}

/// Makes `path` a symlink to `canon` unless something is there already.
fn link(path: &Path, canon: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path.parent().unwrap())?;
            Ok(symlink(canon, path)?)
        }
        Err(e) => Err(e.into()),
        Ok(_) => Ok(()),
    }
}

fn launchctl(args: &[&str]) -> bool {
    Command::new("launchctl")
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

pub(crate) fn use_launchctl() -> bool {
    test_env(NO_LAUNCHCTL).is_none()
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

/// Stops the daemon that holds this state dir's lock: asks it over the socket when it
/// answers, and waits up to 10 s for the lock to be free either way (one still booting does
/// not answer yet). Whether a daemon runs is the lock (`daemon::lock_dir`), never the pid in
/// the file: that pid stays after an exit and may be any process by now. One launchd did not
/// start (a hook's, during an upgrade) survives a bootout and would hold the lock against the
/// new job, so one that does not let go is an error: nothing is installed behind it.
pub fn stop_daemon() -> Result<()> {
    let path = daemon::lock_path(&paths::state_path());
    let Ok(lock) = fs::File::open(&path) else {
        return Ok(());
    };
    // Our probe lock, when it gets one, drops with `lock` on return.
    if lock.try_lock().is_ok() {
        return Ok(());
    }
    if let Ok(mut conn) = Conn::open() {
        let _ = conn.call("shutdown", json!({}));
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        if lock.try_lock().is_ok() {
            return Ok(());
        }
    }
    let pid = fs::read_to_string(&path).unwrap_or_default();
    let (pid, cmd) = (pid.trim(), command());
    Err(format!(
        "The old background service did not stop (pid {pid}). Run: kill {pid}, then {cmd} setup again."
    )
    .into())
}

/// Writes and loads the launchd job that runs `exe daemon`, after stopping the daemon that
/// runs now (`stop_daemon`), so the one that answers after is `exe`'s.
pub fn install_job(home: &Path, exe: &Path) -> Result<i32> {
    let uid = unsafe { libc::getuid() };
    let path = plist_path(home);
    let (domain, service, file) = (
        format!("gui/{uid}"),
        format!("gui/{uid}/{LABEL}"),
        path.to_string_lossy().into_owned(),
    );
    // The loaded job goes first (result ignored): with it loaded, KeepAlive would restart the
    // daemon `stop_daemon` stops, and the new one would hold the lock it waits on.
    if use_launchctl() {
        launchctl(&["bootout", &service]);
    }
    stop_daemon()?;
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, plist(exe, &paths::state_dir().join("daemon.log")))?;
    // Load the job so an update restarts it: bootstrap, and `kickstart -k` if that fails.
    let sequence = format!("bootout {service}; bootstrap {domain} {file}; kickstart -k {service}");
    if use_launchctl() {
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
    stop_daemon()?;
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
    let mut row = |name: &str, ok: bool, good: &str, bad: &str, fix: &str| {
        let (state, fix) = if ok {
            (good, None)
        } else {
            (bad, Some(fix.to_string()))
        };
        rows.push((name.into(), state.into(), fix));
    };
    let mut conn = Conn::open().ok();
    let mut call = |op: &str| conn.as_mut().and_then(|c| c.call(op, json!({})).ok());
    let up = call("ping").is_some_and(|r| r["ok"] == true);
    row("Background service", up, "running", "not running", &setup);
    if let Some(l) = up.then(|| call("relay.get")).flatten() {
        let first = l["urls"][0].as_str().map(cell).unwrap_or_default();
        let state = match l["source"].as_str().unwrap_or("none") {
            "default" => "default".to_string(),
            "config" => format!("yours {first}"),
            "none" => "none".to_string(),
            source => format!("{source} {first}"),
        };
        row("Relay", true, &state, "", "");
    }
    if up && call("status").is_some_and(|r| r["lan"] == "blocked") {
        let fix = "Allow lightbulb in System Settings > Privacy & Security > Local Network";
        row("Local network", false, "", "blocked by macOS", fix);
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
        row("Starts at login", loaded, "yes", "no", &setup);
    }
    let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777).ok();
    let dir = paths::state_path();
    let socket = dir.join(paths::SOCKET);
    if dir.exists() {
        let private =
            mode(&dir) == Some(0o700) && (!socket.exists() || mode(&socket) == Some(0o600));
        let fix = format!("Run: chmod 700 {}", dir.display());
        row(
            "State folder",
            private,
            "private",
            "readable by others",
            &fix,
        );
    }
    for (agent, dot) in setup::AGENTS {
        let r = setup::AgentReport {
            name: agent,
            state: setup::status(&home, agent, dot),
            ..Default::default()
        };
        let ok = matches!(r.state, setup::State::NotInstalled) || r.connected();
        row(&r.product(), ok, r.short(), r.short(), &setup);
    }
    let alias = Alias::read(&home);
    let named = match (alias.short, alias.full) {
        (Link::Ours, _) => Ok("\"lightbulb\""),
        (Link::Other, Link::Ours) => {
            Ok("\"lightbulb\" belongs to another program; type \"lightbulb-connect\"")
        }
        (Link::App, _) => Err("\"lightbulb\" is still the Lightbulb app's (setup takes it)"),
        _ => Err("not installed"),
    };
    row(
        "Command name",
        named.is_ok(),
        named.unwrap_or_default(),
        named.err().unwrap_or_default(),
        &setup,
    );
    if legacy_socket(&home).is_ok() {
        let fix = format!("Run: {cmd} setup --remove, then {cmd} setup");
        row("Old version", false, "", "still running", &fix);
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
    // 0.2.1 wired the old name: its entries go first, so no agent runs two hooks per event.
    let old = setup::run(&home, &compat(&home), server.as_deref(), true);
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
        let legacy = setup::run(&home, &legacy_canonical(&home), server.as_deref(), true);
        let mut said = vec![];
        for a in report
            .agents
            .iter()
            .chain(&old.agents)
            .chain(&legacy.agents)
            .filter(|a| !a.wrote.is_empty())
        {
            if !said.contains(&a.name) {
                println!("{}: removed", a.product());
                said.push(a.name);
            }
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
    let fixes: Vec<String> = report
        .agents
        .iter()
        .filter_map(|a| a.fix(command()))
        .collect();
    for fix in &fixes {
        eprintln!("{fix}");
    }
    if !fixes.is_empty() {
        return Ok(1);
    }
    let connected: Vec<String> = (report.agents.iter())
        .filter(|a| a.connected())
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
