//! The person-facing commands: `who`, `send`, `listen`, `receipt`, `doctor`, `leave`,
//! `daemon --install/--uninstall`, and the `lightbulb` alias.
//! Each is a client of the daemon, like the hook.

use crate::code::Via;
use crate::daemon::ipc;
use crate::daemon::net_ops::Invite;
use crate::hook::{self, Conn};
use crate::{admission, daemon, paths, setup, Result};
use serde_json::{json, Value};
use sha2::Digest;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const LABEL: &str = "company.thelightbulb.connect";
/// What `daemon --install/--uninstall` says off macOS (launchd is the only service it writes).
const NO_SERVICE: &str = "Lightbulb's background service is macOS-only; run lightbulb daemon.";
/// Set to skip every `launchctl` call (tests).
const NO_LAUNCHCTL: &str = "LIGHTBULB_CONNECT_NO_LAUNCHCTL";
/// Names a session the invocation speaks as (nothing sets it yet).
const SESSION_ENV: &str = "LIGHTBULB_CONNECT_SESSION";
/// (tests) Treat stdout as a terminal: a person at it (`install_running`).
const TTY_ENV: &str = "LIGHTBULB_CONNECT_DEBUG_TTY";

/// (tests) The Lightbulb app bundle to use instead of the installed one.
const APP_ENV: &str = "LIGHTBULB_CONNECT_DEBUG_APP";
/// The words the Lightbulb app answers (`lightbulb claude …`); none is one of ours.
pub const APP_WORDS: [&str; 5] = ["claude", "codex", "continue", "share", "pair"];

/// Runs `lightbulb <word> …` as the Lightbulb app's `cli <word> …` in this process (stdin,
/// stdout and the exit code are the app's); without the app, says where to get it and exits 1.
pub fn app_command(args: &[std::ffi::OsString]) -> Result<i32> {
    let bundles = match test_env(APP_ENV) {
        Some(p) => vec![PathBuf::from(p)],
        None if cfg!(target_os = "macos") => vec![
            PathBuf::from("/Applications/Lightbulb.app"),
            home()?.join("Applications/Lightbulb.app"),
        ],
        None => vec![],
    };
    let exe = bundles
        .iter()
        .map(|b| b.join("Contents/MacOS/lightbulb-desktop"))
        .find(|e| e.is_file());
    let Some(exe) = exe else {
        eprintln!(
            "\"lightbulb {}\" needs the Lightbulb app: https://thelightbulb.company/download",
            args[0].to_string_lossy()
        );
        return Ok(1);
    };
    use std::os::unix::process::CommandExt;
    let e = Command::new(&exe).arg("cli").args(args).exec();
    Err(format!("{}: {e}", exe.display()).into())
}

/// A "(tests)" environment override: read in debug builds only, as `net::Knobs::read` does.
pub(crate) fn test_env(name: &str) -> Option<std::ffi::OsString> {
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
        c => format!("Lightbulb could not do that ({}).", c.replace('_', " ")),
    }
}

/// Who has a command name in `~/.local/bin`.
#[derive(Clone, Copy, PartialEq)]
enum Link {
    Ours,
    /// A link to something else.
    Other,
    /// A regular file (or anything not a link).
    File,
    Missing,
}

/// Who has `~/.local/bin/lightbulb`.
fn short_link(home: &Path) -> Link {
    let path = home.join(".local/bin/lightbulb");
    match fs::read_link(&path) {
        Ok(t) if t == canonical(home) => Link::Ours,
        Ok(_) => Link::Other,
        _ if path.symlink_metadata().is_ok() => Link::File,
        _ => Link::Missing,
    }
}

/// The command name to type.
pub fn command() -> &'static str {
    "lightbulb"
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
        println!("No sessions yet. Open Claude Code, Codex, Cursor, Grok or OpenCode and they appear here.");
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
    println!("{}", setup::UPSELL);
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
        // `*`: this computer's sessions; `<member>/*`: theirs as `who --all` sees them.
        let op = if target == "*" {
            "sessions"
        } else {
            "directory"
        };
        let sessions = ask(&mut conn, op, json!({}))?["sessions"].clone();
        let prefix = target.trim_end_matches('*');
        sessions
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["hidden"] != true && s["status"] != "ended")
            .filter_map(|s| {
                let key = s["key"].as_str().or(s["session_id"].as_str())?;
                (target == "*" || key.starts_with(prefix)).then_some((key, s))
            })
            .enumerate()
            .map(|(i, (key, s))| {
                let handle = s["handle"].as_str().filter(|h| !h.is_empty());
                let agent = || crate::frame::agent_name(s["agent"].as_str().unwrap_or_default());
                let handle = handle.map_or_else(agent, String::from);
                (key.to_string(), handle, format!("{rid}.{}", i + 1))
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

/// A `memory` subcommand.
pub enum Memory {
    List {
        team: bool,
        me: bool,
        repo: Option<String>,
    },
    Show(String),
    Forget(String),
    Share(String),
    Export(bool),
    Last,
    Team(bool),
    Drafts,
    Approve(String, bool),
    Dismiss(String),
}

/// `memory ...`: a person's view of what is remembered, through the daemon.
pub fn memory(cmd: Memory) -> Result<i32> {
    if matches!(
        cmd,
        Memory::Drafts | Memory::Approve(..) | Memory::Dismiss(_)
    ) {
        return drafts(cmd);
    }
    let texts = [
        (
            "not_found",
            "No entry starts with that id. `lightbulb memory list` shows them.".to_string(),
        ),
        (
            "team_off",
            "Team memory is off on this computer. Turn it on with `lightbulb memory team on`."
                .to_string(),
        ),
        (
            "not_mine",
            "Only an entry of yours that is not shared yet can be shared.".to_string(),
        ),
    ];
    let day = |v: &Value| crate::date::day(v.as_i64().unwrap_or_default());
    let s = |v: &Value| cell(v.as_str().unwrap_or_default());
    let mut conn = connect()?;
    let (op, body) = match &cmd {
        Memory::List { team, me, repo } => (
            "memory.list",
            json!({ "scope": team.then_some("team").or(me.then_some("me")), "repo": repo }),
        ),
        Memory::Show(id) => ("memory.show", json!({ "entry": id })),
        Memory::Forget(id) => ("memory.forget", json!({ "entry": id })),
        Memory::Share(id) => ("memory.share", json!({ "entry": id })),
        Memory::Export(_) => ("memory.export", json!({})),
        Memory::Last => (
            "memory.last",
            json!({ "session": calling_session(&mut conn) }),
        ),
        Memory::Team(on) => ("memory.team", json!({ "on": on })),
        Memory::Drafts | Memory::Approve(..) | Memory::Dismiss(_) => unreachable!("handled above"),
    };
    let r = conn.call(op, body)?;
    if r["error"] == "replaced" {
        let now = r["current"]
            .as_str()
            .map_or(String::new(), |c| format!(": {c}"));
        eprintln!("This entry was replaced; share the current one{now}");
        return Ok(1);
    }
    if r["ok"] != true {
        return refuse(&r, &texts);
    }
    match cmd {
        Memory::List { .. } => {
            let facts = r["facts"].as_array().cloned().unwrap_or_default();
            if facts.is_empty() {
                println!("Nothing is remembered yet.");
                return Ok(0);
            }
            let mut rows = vec![["Id", "Scope", "By", "Status", "Subject", "Value"]
                .map(String::from)
                .to_vec()];
            for f in &facts {
                let id: String = s(&f["id"]).chars().take(8).collect();
                rows.push(vec![
                    id,
                    s(&f["scope"]),
                    s(&f["author"]),
                    s(&f["status"]),
                    s(&f["subject"]),
                    s(&f["value"]),
                ]);
            }
            table(&rows);
        }
        Memory::Show(_) => {
            let (f, src) = (&r["fact"], &r["source"]);
            println!("{}: {}", s(&f["subject"]), s(&f["value"]));
            println!("Id      {}", s(&f["id"]));
            println!(
                "Scope   {} ({}, {})",
                s(&f["scope"]),
                s(&f["kind"]),
                s(&f["status"])
            );
            println!("By      {} on {}", s(&f["author"]), day(&f["when"]));
            println!(
                "From    {} in {}",
                s(&src["harness"]),
                src["repo"].as_str().map_or("no repository".into(), cell)
            );
            if let Some(from) = src["shared_from"].as_str() {
                println!("Shared  from your entry {}", cell(from));
            }
            for h in f["history"].as_array().into_iter().flatten() {
                println!("Was     {} ({})", s(&h["value"]), day(&h["when"]));
            }
        }
        Memory::Forget(_) => println!("Forgotten. Every computer that comes online drops it."),
        Memory::Share(_) => println!("Shared with the team. Your own entry stays."),
        Memory::Export(as_json) => {
            let entries = r["entries"].as_array().cloned().unwrap_or_default();
            if as_json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                entries.iter().for_each(|e| println!("{e}"));
            }
        }
        Memory::Last => match r["block"].as_str().filter(|b| !b.is_empty()) {
            Some(b) => println!("{b}"),
            None => println!("No session has been handed a memory block yet."),
        },
        Memory::Team(on) => println!(
            "Team memory is {} on this computer.",
            if on { "on" } else { "off" }
        ),
        Memory::Drafts | Memory::Approve(..) | Memory::Dismiss(_) => {}
    }
    Ok(0)
}

/// `memory drafts | approve <id> [--team] | dismiss <id>`: the lessons Lightbulb's evaluator
/// suggests from sessions you uploaded. A person's terminal only; signed calls to the backend.
fn drafts(cmd: Memory) -> Result<i32> {
    use crate::memory::Source;
    let mut conn = connect()?;
    if let Err(e) = ask(&mut conn, "person", json!({})) {
        eprintln!("{e}");
        return Ok(1);
    }
    // Before anything is settled on the server: a `team` entry cannot be stored here while
    // team memory is off, and a retry would find the draft already approved.
    if matches!(cmd, Memory::Approve(_, true))
        && !crate::config::Config::load(&paths::state_dir())?.memory_team
    {
        eprintln!(
            "Team memory is off on this computer. Turn it on with `lightbulb memory team on`, then approve again."
        );
        return Ok(1);
    }
    let call = |method: &str, path: &str, body: Option<Value>| {
        crate::mcp::rest(method, path, body.as_ref(), "memory").map_err(|e| {
            match (e["status"].as_u64(), e["error"].as_str()) {
                (Some(409), _) => "That draft was settled somewhere else already.".to_string(),
                (_, Some("not_signed_in")) => {
                    format!(
                        "Sign in first: lightbulb login ({})",
                        e["hint"].as_str().unwrap_or("")
                    )
                }
                (_, Some("unreachable")) => {
                    "Could not reach Lightbulb. Check your connection and try again.".into()
                }
                (_, Some(m)) => m.to_string(),
                _ => "Lightbulb could not do that.".into(),
            }
        })
    };
    let all = match call("GET", "/memory/drafts", None) {
        Ok(v) => v["drafts"].as_array().cloned().unwrap_or_default(),
        Err(e) => {
            eprintln!("{e}");
            return Ok(1);
        }
    };
    let (Memory::Approve(prefix, _) | Memory::Dismiss(prefix)) = &cmd else {
        if all.is_empty() {
            println!("No suggested lessons are waiting.");
            return Ok(0);
        }
        let mut rows = vec![["Id", "Kind", "Suggestion", "Sessions"]
            .map(String::from)
            .to_vec()];
        for d in &all {
            let first = d["text"]
                .as_str()
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default();
            rows.push(vec![
                cell(d["id"].as_str().unwrap_or_default())
                    .chars()
                    .take(8)
                    .collect(),
                cell(d["kind"].as_str().unwrap_or_default()),
                cell(first),
                d["evidence"].as_array().map_or(0, Vec::len).to_string(),
            ]);
        }
        table(&rows);
        return Ok(0);
    };
    let hits: Vec<&Value> = all
        .iter()
        .filter(|d| {
            !prefix.is_empty()
                && d["id"]
                    .as_str()
                    .is_some_and(|i| i.starts_with(prefix.as_str()))
        })
        .collect();
    let [d] = hits[..] else {
        let why = if hits.is_empty() {
            "No suggestion starts with that id. `lightbulb memory drafts` shows them."
        } else {
            "More than one suggestion starts with that id. Give more of it."
        };
        eprintln!("{why}");
        return Ok(1);
    };
    let path = format!("/memory/drafts/{}", d["id"].as_str().unwrap_or_default());
    if matches!(cmd, Memory::Dismiss(_)) {
        return match call("POST", &format!("{path}/dismiss"), None) {
            Ok(_) => {
                println!("Dismissed.");
                Ok(0)
            }
            Err(e) => {
                eprintln!("{e}");
                Ok(1)
            }
        };
    }
    let team = matches!(cmd, Memory::Approve(_, true));
    let r = match call(
        "POST",
        &format!("{path}/approve"),
        Some(json!({ "scope": team.then_some("team") })),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return Ok(1);
        }
    };
    let (Some(subject), Some(fact)) = (r["subject"].as_str(), r["fact"].as_str()) else {
        // In a workspace the entry arrives through the network.
        println!("Approved; the workspace will remember it");
        return Ok(0);
    };
    let source = Source {
        harness: "cloud-evaluator".into(),
        repo: None,
        session: d["evidence"][0].as_str().map(String::from),
        on_behalf_of: None,
        shared_from: None,
    };
    let kept = ask(
        &mut conn,
        "memory.remember",
        json!({"subject": subject, "fact": fact, "kind": r["kind"], "scope": r["scope"], "source": source}),
    )?;
    if kept["stored"] == true {
        println!("Approved and remembered on this computer.");
        return Ok(0);
    }
    // The draft is approved on the server now: print the lesson so nothing is lost.
    let reason = kept["refused"].as_str().unwrap_or("it could not be stored");
    eprintln!("Approved, but not stored locally: {reason}; the lesson is:\n{fact}");
    Ok(1)
}

/// A `sessions` subcommand.
pub enum Sessions {
    List,
    Show(String),
    Forget(String),
    Export,
    Upload(Option<String>),
    /// `Some(on)` sets whether turns are kept; `None` says which.
    Turns(Option<bool>),
}

/// `sessions ...`: what was captured of your coding sessions on this computer.
pub fn sessions(cmd: Sessions) -> Result<i32> {
    if let Sessions::Upload(mode) = &cmd {
        let dir = paths::state_dir();
        let now = match mode {
            // Through the daemon, which refuses an agent's shell: uploading is a person's choice.
            Some(m) => {
                let r = connect()?.call("sessions.upload", json!({ "mode": m }))?;
                if r["ok"] != true {
                    return refuse(&r, &[]);
                }
                m.clone()
            }
            None => crate::config::Config::load(&dir)?.capture_upload,
        };
        println!("Uploads are {now}. {}", crate::upload::describe(&now));
        let now_s = crate::daemon::ipc::now() as i64;
        let signed_out =
            crate::login::stored_proof(&dir).is_none() || crate::login::ended(&dir, now_s);
        if now != "off" && signed_out {
            println!("Nothing is sent until you sign in: lightbulb login");
        }
        return Ok(0);
    }
    if let Sessions::Turns(on) = cmd {
        let dir = paths::state_dir();
        if let Some(on) = on {
            // Through the daemon, which refuses an agent's shell: what is kept is a person's choice.
            let r = connect()?.call("sessions.turns", json!({ "on": on }))?;
            if r["ok"] != true {
                return refuse(&r, &[]);
            }
        }
        let on = crate::config::Config::load(&dir)?.capture_turns;
        println!(
            "{}",
            if on {
                "Your prompts and the agent's replies are kept on this computer only, never uploaded. Turn off: lightbulb sessions turns off"
            } else {
                "Your prompts and the agent's replies are not kept. Turn on: lightbulb sessions turns on"
            }
        );
        return Ok(0);
    }
    let (op, body) = match &cmd {
        Sessions::List => ("sessions.list", json!({})),
        Sessions::Show(id) => ("sessions.show", json!({ "session": id })),
        Sessions::Forget(id) => ("sessions.forget", json!({ "session": id })),
        Sessions::Export => ("sessions.export", json!({})),
        Sessions::Upload(_) | Sessions::Turns(_) => unreachable!("handled above"),
    };
    let r = connect()?.call(op, body)?;
    if r["ok"] != true {
        let none = "No session starts with that id. `lightbulb sessions list` shows them.";
        let many = "More than one session starts with that id. Give more of it.";
        return refuse(
            &r,
            &[
                ("not_found", none.to_string()),
                ("ambiguous", many.to_string()),
            ],
        );
    }
    let day = |v: &Value| crate::date::day(v.as_i64().unwrap_or_default());
    let s = |v: &Value| cell(v.as_str().unwrap_or_default());
    let tokens = |v: &Value| v.as_i64().map_or("-".to_string(), |n| n.to_string());
    match cmd {
        Sessions::List => {
            let rows = r["sessions"].as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("No sessions captured yet.");
                return Ok(0);
            }
            let mut t = vec![["Id", "Agent", "Repo", "Started", "In", "Out"]
                .map(String::from)
                .to_vec()];
            for x in rows.iter().rev() {
                t.push(vec![
                    s(&x["id"]),
                    s(&x["harness"]),
                    s(&x["repo"]),
                    day(&x["started"]),
                    tokens(&x["prompt_tokens"]),
                    tokens(&x["completion_tokens"]),
                ]);
            }
            table(&t);
        }
        Sessions::Show(_) => {
            let x = &r["session"];
            println!("Session {}", s(&x["id"]));
            println!("Agent   {} on {}", s(&x["harness"]), s(&x["computer"]));
            println!("Repo    {} {}", s(&x["repo"]), s(&x["branch"]));
            println!("Started {}", day(&x["started"]));
            println!(
                "Tokens  in {} out {}",
                tokens(&x["prompt_tokens"]),
                tokens(&x["completion_tokens"])
            );
            for c in r["tool_calls"].as_array().into_iter().flatten() {
                let ok = if c["ok"] == true { "" } else { "  (failed)" };
                let text = c["summary"]
                    .as_str()
                    .unwrap_or(c["tool"].as_str().unwrap_or_default());
                println!("{:>4}  {}{ok}", c["seq"], cell(text));
            }
            for t in r["turns"].as_array().into_iter().flatten() {
                let text = t["text"].as_str().unwrap_or_default();
                let line = cell(&format!("{} {}: {text}", t["seq"], s(&t["role"])));
                println!("{}", line.chars().take(200).collect::<String>());
            }
        }
        Sessions::Forget(_) => {
            println!("Forgotten. The session, its tool calls and its turns are deleted.")
        }
        Sessions::Export => println!("{}", serde_json::to_string_pretty(&r["sessions"])?),
        Sessions::Upload(_) | Sessions::Turns(_) => {}
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

/// `invite --agents`: a once link for the workspace to join with (`login::invite_agents`).
pub fn invite_agents() -> Result<i32> {
    let r = connect()?.call(
        "team.invite",
        json!({ "mode": Invite::Once, "code": false }),
    )?;
    match r["ticket"].as_str() {
        Some(t) if r["ok"] == true => crate::login::invite_agents(t),
        _ => refuse(&r, &[]),
    }
}

/// `invite [--new|--once|--open]`: this computer's invite line.
pub fn invite(mode: Invite) -> Result<i32> {
    let r = connect()?.call("team.invite", json!({ "mode": mode }))?;
    if r["ok"] != true {
        return refuse(&r, &[]);
    }
    crate::telemetry::send(
        &paths::state_dir(),
        crate::telemetry::Event::InviteCreated(crate::telemetry::Via::Line),
    );
    let line = r["line"].as_str().unwrap_or_default();
    match mode {
        Invite::Once => {
            let at = crate::date::utc(r["expires_at"].as_i64().unwrap_or_default());
            println!("One-time invite, expires {at}.");
        }
        _ if r["replaced"] == true => {
            println!("Every earlier invite from this computer stopped working.")
        }
        _ => {}
    }
    // Code first (the easy way), then the link, then the full line.
    // The form the invite leads with, for usage counts.
    let _via = if r["code"].is_string() {
        Via::Code
    } else {
        Via::Link
    };
    if let Some(code) = r["code"].as_str() {
        println!(
            "Tell them to run:  {} join {code}   (the code works for {} minutes)",
            command(),
            (r["code_secs"].as_i64().unwrap_or(600) + 59) / 60
        );
    }
    println!(
        "Or send this link:  {}",
        r["link"].as_str().unwrap_or_default()
    );
    println!("If the code doesn't work, send this line (or paste it on your other computer):");
    println!("{line}");
    Ok(0)
}

/// `join '<ticket>' [--name <n>]`: joins the network the join line `link` is for, waiting up to 600 s
/// (the invite is the approval).
pub fn join(link: &str, name: Option<String>) -> Result<i32> {
    // Which form the person was given; usage counts read it.
    let (code, _via) = match crate::code::normalize(link) {
        Some(c) => (Some(c), Via::Code),
        None if link.trim().starts_with("https://") => (None, Via::Link),
        None => (None, Via::Line),
    };
    let name = name.unwrap_or_else(daemon::user_name);
    let mut conn = connect()?;
    // Said once the daemon rang a parked workspace's doorbell (`join_waking`); never before a
    // ticket it refuses at once.
    let ticket = crate::ticket::decode(link).ok();
    if code.is_some() || ticket.is_some_and(|t| t.wake.is_some()) {
        let mut asking = Conn::open()?;
        asking.call("subscribe", json!({}))?;
        asking.wait_up_to(Duration::from_secs(660))?;
        std::thread::spawn(move || {
            while let Ok(e) = asking.next_line() {
                if e["event"] == "join_waking" {
                    println!("Waking your workspace… this takes about a minute.");
                }
            }
        });
    }
    conn.wait_up_to(Duration::from_secs(660))?;
    let body = match &code {
        Some(c) => json!({"code": c, "name": name}),
        None => json!({"ticket": link, "name": name}),
    };
    let r = ask(&mut conn, "team.join", body)?;
    let team = r["team"].as_str().unwrap_or_default();
    // The ticket named a workspace (for a code, the daemon saw the ticket, not this process).
    let has_wake = r["wake"] == true;
    let text = match (r["state"].as_str(), r["reason"].as_str()) {
        (Some("admitted"), _) => {
            let computer = ask(&mut conn, "identity.get", json!({}))?["computer"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let n = r["online"].as_u64().unwrap_or(1);
            crate::telemetry::send(
                &paths::state_dir(),
                crate::telemetry::Event::NetworkJoined(crate::telemetry::Via::of_join(link)),
            );
            println!("Connected as {name} ({computer}). Team \"{team}\": {n} computers online.");
            println!(
                "Your agents here can now see and message the team's agents. Try: {} who --all",
                command()
            );
            return Ok(0);
        }
        (Some("waiting"), _) => "No one has answered yet. Run the same command again to keep asking.".to_string(),
        (_, Some(admission::ALREADY_IN_TEAM)) => format!("You are in \"{team}\". Leave it first: {} leave", command()),
        (_, Some(admission::NAME_TAKEN)) => format!(
            "The name \"{name}\" is already used in this network. Run the same command again with: --name {name}-2"
        ),
        (_, Some(admission::NO_CODE)) => "That code did not work. Check the words. Ask them to open their computer, or send the full line instead. A code works once.".to_string(),
        (_, Some(admission::UNREACHABLE)) if has_wake => "Your workspace did not answer. Run the same command again in a minute. Your key is ready; nothing was lost.".to_string(),
        (_, Some(admission::UNREACHABLE)) => "No one from this network is online right now. Ask them to open their laptop, then run the same command again. Your key is ready; nothing was lost.".to_string(),
        (_, Some(admission::DECLINED)) => "Your request to join was declined.".to_string(),
        (_, Some(admission::BAD_NAME)) => "That name has characters Lightbulb cannot show. Use letters, numbers, spaces and punctuation.".to_string(),
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

/// `signin`: verified names need the issuer's real keys (the key ceremony), so this refuses.
pub fn signin() -> Result<i32> {
    eprintln!("Verified names are not available yet.");
    Ok(2)
}

/// `approve <id>` / `decline <id>`: answers permission prompt `H<n>`.
pub fn answer(request: &str, allow: bool) -> Result<i32> {
    let verdict = if allow { "allow" } else { "decline" };
    let r = connect()?.call("hold.answer", json!({"hold": request, "verdict": verdict}))?;
    if r["ok"] != true {
        let texts = [
            (
                admission::NOT_OPEN,
                match r["answered_in"].as_str() {
                    Some("dialog") => format!("{request} was already answered in the dialog."),
                    Some("phone") => format!("{request} was already answered from your phone."),
                    Some(_) => format!("{request} was already answered at the command line."),
                    None => format!("{request} was already answered or has expired."),
                },
            ),
            (ipc::NOT_FOUND, format!("No prompt {request}.")),
        ];
        return refuse(&r, &texts);
    }
    println!("{} {request}.", if allow { "Allowed" } else { "Declined" });
    Ok(0)
}

/// `held`: the permission prompts waiting for an answer, one per line with the command that
/// answers it.
pub fn held() -> Result<i32> {
    let holds = ask(&mut connect()?, "holds", json!({}))?;
    let cmd = command();
    let f = |v: &Value, k: &str| v[k].as_str().map(cell).unwrap_or_default();
    let mut any = false;
    for h in holds["holds"].as_array().into_iter().flatten() {
        let (id, to, text) = (f(h, "id"), f(h, "target_session"), f(h, "preview"));
        println!("{id}  {to}  \"{text}\" — {cmd} approve {id}");
        any = true;
    }
    if !any {
        println!("Nothing is waiting for your answer.");
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
        if m["by"].is_string() {
            line.push_str(&format!(" · admitted by {}", f("by")));
        }
        if members.iter().filter(|o| o["name"] == m["name"]).count() > 1 {
            line.push_str(&format!(" · address: {}", f("short")));
        }
        println!("{line}");
    }
    println!("Invite someone: {} invite", command());
    println!("{}", setup::UPSELL);
    Ok(0)
}

/// `remove <name>`.
pub fn remove(name: &str) -> Result<i32> {
    let r = connect()?.call("team.remove", json!({ "member": name }))?;
    if r["ok"] != true {
        let texts = [
            ("not_found", format!("No member called {name}.")),
            (
                crate::daemon::net_ops::FOUNDER,
                format!(
                    "{name} started this team and cannot be removed. It can leave the team itself."
                ),
            ),
        ];
        return refuse(&r, &texts);
    }
    let n = r["name"].as_str().unwrap_or(name);
    let line = r["line"].as_str().unwrap_or_default();
    println!("Removed {n}. New connections and new messages from it are blocked. The old invite stopped working; send this line instead: {line}");
    Ok(0)
}

/// `leave` when `identity.key` no longer matches the team record (the daemon will not start):
/// after a yes, the team record goes from `state.db`, as a removal would leave it; the next
/// start makes this computer its own network with its new key. Nothing can be signed for the
/// team, so it is not told.
fn leave_unsigned(dir: &std::path::Path) -> Result<i32> {
    // A start that is about to refuse (launchd's respawn) holds the lock a moment: wait it out.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let _lock =
        loop {
            match daemon::lock_dir(dir, false) {
                Ok(l) => break l,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                Err(_) => return Err(
                    "The background service holds the state folder. Stop it, then run leave again."
                        .into(),
                ),
            }
        };
    let store = crate::store::Store::open(&dir.join("state.db"))?;
    let name = crate::roster::replay(&crate::roster::entries(&store)?).team_name;
    print!("Leave \"{name}\"? This computer's key changed, so the team cannot be told. Messages already stored here stay. [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        return Ok(0);
    }
    crate::outbox::refuse_all(&store, daemon::ipc::now(), "not_member")?;
    store.clear_roster()?;
    println!("Left \"{name}\". This computer is on its own network again.");
    Ok(0)
}

/// `leave`: after a yes, this computer leaves its team and is on its own network again. The
/// service keeps running.
pub fn leave() -> Result<i32> {
    let dir = paths::state_dir();
    if crate::embed::key_mismatch(&dir)? {
        return leave_unsigned(&dir);
    }
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
        "Leave \"{name}\"? Your computer stops connecting. Messages already stored here stay. [y/N] "
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes") {
        return Ok(0);
    }
    let r = conn.call("team.leave", json!({}))?;
    if r["ok"] != true {
        return refuse(&r, &[]);
    }
    println!("Left \"{name}\". This computer is on its own network again.");
    if r["told"] == false {
        println!("Could not tell the team; they will see you gone when you are removed.");
    }
    Ok(0)
}

/// `relay [<url> | --clear] [--local]`: sets this computer's relay to `set` (`""` clears it),
/// and the team's unless `local`; alone, prints the relays in
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
    Ok(0)
}

/// `https://use1-1.relay.n0.iroh.link./` -> `use1-1.relay.n0.iroh.link`.
fn relay_host(url: &str) -> String {
    let host = url.split_once("://").map_or(url, |(_, h)| h);
    host.trim_end_matches('/').trim_end_matches('.').to_string()
}

// ---- alias and launchd ------------------------------------------------------------------

/// Puts this executable at the canonical path (unless it is already there with the same
/// bytes) and `~/.local/bin/lightbulb` at that, replacing any link there; a regular file there
/// is left alone. Returns what to say; only a person's own run (`person`) edits their shell
/// profile (`command_lines`).
pub fn install_alias(home: &Path, exe: &Path, person: bool) -> Result<String> {
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
    let short = home.join(".local/bin/lightbulb");
    match short_link(home) {
        Link::File => Ok("~/.local/bin/lightbulb belongs to another program and was left alone; put ~/.lightbulb/connect/bin on your PATH to type lightbulb.".into()),
        Link::Ours => Ok(command_lines(home, person)),
        _ => {
            relink(&short, &canon)?;
            Ok(command_lines(home, person))
        }
    }
}

/// The release this binary is: `Cargo.toml`'s version.
const VERSION: &str = env!("CARGO_PKG_VERSION");
const RELEASES: &str = "https://github.com/TheLightbulbCompany/cli/releases";
/// This build's release asset (`install.sh` picks the same one).
pub const ASSET: &str = if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
    "lightbulb-linux-arm64.tar.gz"
} else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "lightbulb-linux-x86_64.tar.gz"
} else {
    "lightbulb-macos-arm64.tar.gz"
};
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "aarch64", target_arch = "x86_64"))
))]
compile_error!("no release asset for this Linux architecture (arm64 and x86_64 only)");
/// The oldest release `update` can install: earlier ones shipped a differently named binary.
const FIRST_UPDATABLE: (u64, u64, u64) = (0, 2, 2);
/// "(tests)" A release folder (`file://…`) used in place of GitHub: it holds the asset, its
/// `.sha256`, and `latest`, a file naming the latest tag (GitHub tells it by a redirect).
const RELEASE_BASE: &str = "LIGHTBULB_RELEASE_BASE";

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.strip_prefix('v').unwrap_or(v).split('.').map(str::parse);
    let ver = (it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?);
    it.next().is_none().then_some(ver)
}

/// `curl -fsSL url` (with `-I`: the headers only); its stdout, or an error naming the URL.
pub(crate) fn curl(url: &str, head: bool) -> Result<Vec<u8>> {
    curl_for(url, head, 600)
}

/// `curl` that gives up after `secs` seconds.
pub(crate) fn curl_for(url: &str, head: bool, secs: u64) -> Result<Vec<u8>> {
    let mut c = Command::new("curl");
    // -q first: ignore ~/.curlrc. Bounded, so a stalled download cannot hang the daemon's daily check.
    c.arg("-q");
    c.args(["--connect-timeout", "30", "--max-time", &secs.to_string()]);
    c.arg(if head { "-fsSI" } else { "-fsSL" }).arg(url);
    let out = c.output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("Could not download {url}; nothing changed.").into());
    }
    Ok(out.stdout)
}

/// The latest release's tag: the last part of the redirect GitHub answers `releases/latest` with.
fn latest_tag(base: Option<&str>) -> Result<String> {
    if let Some(base) = base {
        return Ok(
            String::from_utf8_lossy(&curl(&format!("{base}/latest"), false)?)
                .trim()
                .to_string(),
        );
    }
    let head = String::from_utf8_lossy(&curl(&format!("{RELEASES}/latest"), true)?).into_owned();
    head.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("location")
                .then(|| v.trim().rsplit('/').next().unwrap_or("").to_string())
        })
        .ok_or_else(|| "Could not learn the latest version; nothing changed.".into())
}

/// The code requirement every release binary must meet: Apple-anchored, our Developer ID team.
#[cfg(target_os = "macos")]
const REQUIREMENT: &str = "=anchor apple generic and certificate leaf[subject.OU] = \"WZX4U3C22Y\"";

/// Refuses a binary that is not validly signed by our team; the old binary is untouched.
#[cfg(target_os = "macos")]
fn codesign_args(bin: &Path) -> Vec<std::ffi::OsString> {
    ["--verify", "--strict", "-R", REQUIREMENT]
        .into_iter()
        .map(Into::into)
        .chain([bin.as_os_str().to_owned()])
        .collect()
}

#[cfg(target_os = "macos")]
fn verify_signature(bin: &Path) -> Result<()> {
    let ok = Command::new("codesign")
        .args(codesign_args(bin))
        .output()
        .is_ok_and(|o| o.status.success());
    if ok {
        Ok(())
    } else {
        Err("The download is not signed by Lightbulb; nothing changed.".into())
    }
}

/// The release-signing public key (Ed25519, 32 bytes). Its private half lives only on the
/// signing Mac, which signs every Linux asset's `.sha256` (`docs/releasing.md`); a Linux or WSL
/// update installs nothing else.
const RELEASE_KEY: [u8; 32] = [
    0x5f, 0x56, 0xf6, 0x7c, 0xef, 0x03, 0x59, 0x00, 0xbf, 0xae, 0x7b, 0xca, 0x33, 0xe1, 0xee, 0x56,
    0x12, 0xda, 0xb8, 0x2f, 0xb8, 0xd9, 0xae, 0x3e, 0x31, 0x76, 0x82, 0xd0, 0x9a, 0x6a, 0x78, 0xdf,
];

/// The key a release's `.sha256.sig` must verify under, or `None` where none is checked: macOS
/// pins the binary's codesign team instead. "(tests)" debug builds take a hex key from
/// `LIGHTBULB_RELEASE_KEY`, on macOS too.
fn release_key() -> Option<[u8; 32]> {
    if let Some(k) = test_env("LIGHTBULB_RELEASE_KEY") {
        return data_encoding::HEXLOWER
            .decode(k.to_string_lossy().as_bytes())
            .ok()?
            .try_into()
            .ok();
    }
    (!cfg!(target_os = "macos")).then_some(RELEASE_KEY)
}

/// Whether `sig` (64 raw bytes, as `openssl pkeyutl -sign -rawin` writes it) is `key`'s
/// Ed25519 signature over `sum`, the `.sha256` file's exact bytes.
fn signed_by(key: &[u8; 32], sum: &[u8], sig: &[u8]) -> bool {
    let key = ed25519_dalek::VerifyingKey::from_bytes(key);
    let sig = ed25519_dalek::Signature::from_slice(sig);
    matches!((key, sig), (Ok(k), Ok(s)) if k.verify_strict(sum, &s).is_ok())
}

/// Downloads release `tag` from `base`, checks the release's `.sha256` against its signature
/// (`release_key`), then the tarball against that `.sha256`, and only then makes it the
/// canonical binary (`install_alias`). `Ok(false)`: the checksum did not
/// match and nothing changed. Shared by `update` and the daemon's daily check.
fn install_release(home: &Path, base: &str, tag: &str) -> Result<bool> {
    let canon = canonical(home);
    let bin = canon.parent().unwrap();
    fs::create_dir_all(bin)?;
    // Unpacked beside the canonical binary, so the final rename stays on one filesystem.
    let dir = bin.join(format!(".update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let installed = (|| -> Result<bool> {
        let sum = curl(&format!("{base}/{ASSET}.sha256"), false)?;
        if let Some(key) = release_key() {
            let sig = curl(&format!("{base}/{ASSET}.sha256.sig"), false)?;
            if !signed_by(&key, &sum, &sig) {
                return Err("The release is not signed by Lightbulb; nothing changed.".into());
            }
        }
        let sum = String::from_utf8_lossy(&sum)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let tarball = curl(&format!("{base}/{ASSET}"), false)?;
        if data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&tarball)) != sum {
            return Ok(false);
        }
        fs::write(dir.join(ASSET), &tarball)?;
        let ok = Command::new("tar")
            .arg("-xzf")
            .arg(dir.join(ASSET))
            .arg("-C")
            .arg(&dir)
            .status()
            .is_ok_and(|s| s.success());
        let new = dir.join("lightbulb");
        if !ok || !fs::symlink_metadata(&new).is_ok_and(|m| m.is_file()) {
            return Err(
                format!("The {tag} download holds no lightbulb binary; nothing changed.").into(),
            );
        }
        // "(tests)" debug builds only: a stub binary has no signature.
        #[cfg(target_os = "macos")]
        {
            #[cfg(debug_assertions)]
            let skip = std::env::var_os("LIGHTBULB_ALLOW_UNSIGNED").is_some();
            #[cfg(not(debug_assertions))]
            let skip = false;
            if !skip {
                verify_signature(&new)?;
            }
        }
        install_alias(home, &new, false)?;
        Ok(true)
    })();
    let _ = fs::remove_dir_all(&dir);
    installed
}

/// Set to `1` to turn the daemon's daily update off (with `auto_update = false` in config).
const NO_AUTO_UPDATE: &str = "LIGHTBULB_NO_AUTO_UPDATE";
/// The tag the last auto-update installed, in the state dir: a release that does not report the
/// version it is tagged with is installed once, not on every restart.
const AUTO_UPDATE_LAST: &str = "auto-update.last";

/// Whether the daily update is on: the config setting, unless the environment turns it off.
pub fn auto_update_on(config: &crate::config::Config) -> bool {
    config.auto_update && std::env::var(NO_AUTO_UPDATE).as_deref() != Ok("1")
}

/// Whether `latest` is to be installed over `current`: strictly newer (never the same, never a
/// downgrade, never something that is not a version) and not the one installed last time.
fn should_auto_update(current: &str, latest: &str, last: Option<&str>) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(c), Some(l)) => l > c && last.and_then(parse_version) != Some(l),
        _ => false,
    }
}

/// The version whose `setup` last ran, in the state dir.
const SETUP_VERSION: &str = "setup-version";

/// Whether `setup` must run again: the binary is not the version that last ran it.
fn setup_due(last: Option<&str>, current: &str) -> bool {
    last.map(str::trim) != Some(current)
}

/// At a daemon's start on the real install: after a version change (an auto-update), runs the
/// canonical binary's `setup` once so hooks, MCP entries and the skill follow it. The marker is
/// written first, so a failing setup is not retried on every start.
pub fn refresh_setup(dir: &Path) {
    let Ok(home) = home() else { return };
    if !is_installed_exe(&home) {
        return;
    }
    let last = fs::read_to_string(dir.join(SETUP_VERSION)).ok();
    if !setup_due(last.as_deref(), VERSION) || fs::write(dir.join(SETUP_VERSION), VERSION).is_err()
    {
        return;
    }
    let child = Command::new(canonical(&home))
        .args(["setup", "--refresh"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut c) = child {
        std::thread::spawn(move || c.wait());
    }
}

/// Whether this process is the real install's canonical binary (not a side-by-side state dir,
/// not a dev or `cargo run` build).
fn is_installed_exe(home: &Path) -> bool {
    std::env::var_os(paths::HOME_ENV).is_none()
        && std::env::current_exe()
            .and_then(|e| e.canonicalize())
            .ok()
            .zip(canonical(home).canonicalize().ok())
            .is_some_and(|(a, b)| a == b)
}

/// The daemon's daily check: installs the latest release when it is newer than this binary, by
/// the same checksum-verified path as `update`. `Ok(true)`: installed, the daemon should exit so
/// its service manager starts the new binary. Only the real install is touched (not a
/// side-by-side state dir, not a binary run from elsewhere); a Linux install has no service to
/// restart, so it only says a release exists.
pub fn auto_update(dir: &Path) -> Result<bool> {
    let home = home()?;
    if !is_installed_exe(&home) {
        return Ok(false);
    }
    let base = test_env(RELEASE_BASE).map(|b| b.to_string_lossy().into_owned());
    let tag = latest_tag(base.as_deref())?;
    let last = fs::read_to_string(dir.join(AUTO_UPDATE_LAST)).ok();
    if !should_auto_update(VERSION, &tag, last.as_deref().map(str::trim)) {
        return Ok(false);
    }
    let want = parse_version(&tag).ok_or("not a version")?;
    let tag = format!("v{}.{}.{}", want.0, want.1, want.2);
    if !cfg!(target_os = "macos") {
        eprintln!("auto-update: {tag} is available; run: lightbulb update");
        return Ok(false);
    }
    let base = base.unwrap_or_else(|| format!("{RELEASES}/download/{tag}"));
    if !install_release(&home, &base, &tag)? {
        return Err("checksum mismatch; nothing changed".into());
    }
    eprintln!("auto-update: installed {tag}; restarting");
    let _ = fs::write(dir.join(AUTO_UPDATE_LAST), &tag);
    Ok(true)
}

/// `update [--version vX.Y.Z]`: installs that release (default: the latest) over the canonical
/// binary, then runs the new binary's `setup`, which moves the hooks, MCP entries and the
/// background service to it.
pub fn update(version: Option<String>) -> Result<i32> {
    let base = test_env(RELEASE_BASE).map(|b| b.to_string_lossy().into_owned());
    let tag = match version {
        Some(v) => v,
        None => latest_tag(base.as_deref())?,
    };
    let Some(want) = parse_version(&tag) else {
        eprintln!("Not a version: {tag}. Use the form v0.2.2.");
        return Ok(2);
    };
    if want < FIRST_UPDATABLE {
        eprintln!("Versions before 0.2.2 cannot be installed.");
        return Ok(2);
    }
    if parse_version(VERSION) == Some(want) {
        println!("Already on {VERSION}.");
        return Ok(0);
    }
    let tag = format!("v{}.{}.{}", want.0, want.1, want.2);
    let base = base.unwrap_or_else(|| format!("{RELEASES}/download/{tag}"));
    println!("Updating {VERSION} → {}…", &tag[1..]);

    let home = home()?;
    let canon = canonical(&home);
    if !install_release(&home, &base, &tag)? {
        eprintln!("Checksum mismatch; nothing changed.");
        return Ok(1);
    }
    use std::os::unix::process::CommandExt;
    Err(format!(
        "{}: {}",
        canon.display(),
        Command::new(&canon).arg("setup").exec()
    )
    .into())
}

/// `Command: lightbulb`. When `~/.local/bin` is not on this shell's PATH (a fresh Mac: the curl
/// install lands there and nothing else puts it on PATH), the PATH line is added to the login
/// shell's profile once, in a marked block, and the person is told to open a new terminal.
/// Only on a `person`'s own run: an update or the daemon only says which line to add.
fn command_lines(home: &Path, person: bool) -> String {
    let bin = home.join(".local/bin");
    let on_path =
        std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == bin));
    if on_path {
        return "Command: lightbulb".into();
    }
    let profile = shell_profile(home);
    let shown = format!(
        "~/{}",
        profile.strip_prefix(home).unwrap_or(&profile).display()
    );
    if person && add_to_path(&profile).is_ok() {
        format!(
            "Command: lightbulb (added ~/.local/bin to your PATH in {shown}; open a new terminal, or run {} now)",
            canonical(home).display()
        )
    } else {
        format!(
            "Command: lightbulb\nAdd this line to {shown}, then open a new terminal: {PATH_LINE}"
        )
    }
}

const PATH_LINE: &str = "export PATH=\"$HOME/.local/bin:$PATH\"";
const PATH_MARK: &str = "# Added by Lightbulb";

/// The login shell's profile: `~/.zshrc` for zsh (the Mac default), `~/.bash_profile` on a Mac
/// and `~/.bashrc` on Linux for bash, else `~/.profile`.
fn shell_profile(home: &Path) -> PathBuf {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let name = match shell.rsplit('/').next().unwrap_or_default() {
        "zsh" => ".zshrc",
        "bash" if cfg!(target_os = "macos") => ".bash_profile",
        "bash" => ".bashrc",
        _ => ".profile",
    };
    home.join(name)
}

/// Appends the marked PATH block to `profile` unless it already mentions `.local/bin`; never
/// through a symlink (a dotfiles repo's file is the person's to edit).
fn add_to_path(profile: &Path) -> Result<()> {
    if fs::symlink_metadata(profile).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("a symlinked profile".into());
    }
    let had = fs::read_to_string(profile).unwrap_or_default();
    if had.contains(PATH_MARK) || had.contains(".local/bin") {
        return Ok(());
    }
    let sep = if had.is_empty() || had.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(profile)?;
    use std::io::Write;
    write!(f, "{sep}\n{PATH_MARK}\n{PATH_LINE}\n")?;
    Ok(())
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

fn launchctl(args: &[&str]) -> bool {
    Command::new("launchctl")
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// (tests, debug builds) Let launchctl run from a temporary HOME (the macOS shim test).
#[cfg(debug_assertions)]
const LAUNCHCTL_ANYWHERE: &str = "LIGHTBULB_CONNECT_DEBUG_LAUNCHCTL_ANYWHERE";

/// Whether HOME is the account's own home directory (the passwd entry) and no
/// `LIGHTBULB_CONNECT_HOME` moves the state: anywhere else is a test or a sandbox, and the
/// launchd label is fixed, so touching launchd there would replace the real background service.
pub(crate) fn real_home() -> bool {
    use std::os::unix::ffi::OsStrExt;
    if std::env::var_os(paths::HOME_ENV).is_some() {
        return false;
    }
    let pw = unsafe { libc::getpwuid(libc::getuid()) };
    if pw.is_null() || unsafe { (*pw).pw_dir.is_null() } {
        return false;
    }
    let passwd = PathBuf::from(std::ffi::OsStr::from_bytes(
        unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) }.to_bytes(),
    ));
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(&home) == canon(&passwd)
}

pub(crate) fn use_launchctl() -> bool {
    #[cfg(debug_assertions)]
    let anywhere = test_env(LAUNCHCTL_ANYWHERE).is_some();
    #[cfg(not(debug_assertions))]
    let anywhere = false;
    test_env(NO_LAUNCHCTL).is_none() && (real_home() || anywhere)
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

/// Puts the running binary at the canonical path and the alias at that, printing the line; a
/// person at a terminal may get the PATH line added to their profile.
pub fn install_running(home: &Path) -> Result<()> {
    use std::io::IsTerminal;
    let exe = std::env::current_exe()?.canonicalize()?;
    let person = std::io::stdout().is_terminal() || test_env(TTY_ENV).is_some();
    println!("{}", install_alias(home, &exe, person)?);
    Ok(())
}

/// The running daemon's version, when one answers.
fn running_version() -> Option<String> {
    let s = Conn::open().ok()?.call("status", json!({})).ok()?;
    s["version"].as_str().map(String::from)
}

/// Off macOS no service manager restarts the daemon, so one started before an update (by
/// hand, in tmux, by a hook) kept running the old binary. Restart it on this one.
fn restart_if_old() -> Result<()> {
    if running_version().is_some_and(|v| v != VERSION) {
        stop_daemon()?;
        daemon::ensure_running()?;
        println!("Background service: restarted on {VERSION}.");
    }
    Ok(())
}

/// `daemon --install`: the canonical binary and alias, then the launchd job running it.
pub fn daemon_install() -> Result<i32> {
    if !cfg!(target_os = "macos") {
        eprintln!("{NO_SERVICE}");
        return Ok(2);
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

/// The job half of `setup --refresh`, which only the daemon runs, as its own child, after an
/// auto-update (`refresh_setup`). It writes the plist when it changed and never touches
/// launchctl: `install_job`'s bootout would kill this process along with the daemon it runs
/// under, before its bootstrap, leaving no job loaded. launchd already runs the new binary (the
/// plist names the canonical path, which an update overwrites), and a changed plist is read at
/// the next load: login, or a person's `setup`.
fn refresh_job(home: &Path, exe: &Path) -> Result<i32> {
    let path = plist_path(home);
    let want = plist(exe, &paths::state_dir().join("daemon.log"));
    if fs::read_to_string(&path).ok().as_deref() != Some(want.as_str()) {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(&path, want)?;
    }
    println!("Background service: left running (starts at login).");
    Ok(0)
}

/// Whether `launchctl print-disabled` output marks `label` disabled (`=> disabled`, or
/// `=> true` on older macOS). A label it does not list is enabled.
fn disabled_in(listing: &str, label: &str) -> bool {
    let key = format!("\"{label}\"");
    listing.lines().any(|l| {
        let mut parts = l.trim().splitn(2, "=>");
        parts.next().map(str::trim) == Some(key.as_str())
            && matches!(parts.next().map(str::trim), Some("disabled" | "true"))
    })
}

pub fn daemon_uninstall() -> Result<i32> {
    if !cfg!(target_os = "macos") {
        eprintln!("{NO_SERVICE}");
        return Ok(2);
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
        "Lightbulb is off. Your settings and messages are kept in {}.",
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
    // A daemon refuses to start under a mismatched key (`embed::KEY_MISMATCH`): name that,
    // not setup, which cannot fix it.
    let dir = paths::state_path();
    let mismatch = !up && crate::embed::key_mismatch(&dir).unwrap_or(false);
    let fix = if mismatch {
        crate::embed::KEY_MISMATCH
    } else {
        &setup
    };
    // Running, but an older binary than this one (started before an update): it lacks what
    // this version added.
    let old = up
        .then(|| call("status"))
        .flatten()
        .and_then(|s| s["version"].as_str().map(String::from))
        .filter(|v| v != VERSION);
    match old {
        Some(v) => row(
            "Background service",
            false,
            "",
            &format!("running {v}, not {VERSION}"),
            &setup,
        ),
        None => row("Background service", up, "running", "not running", fix),
    }
    // The daemon refuses to start without its key (`identity::keychain_error`): say why.
    if crate::identity::exists(&dir).is_err() {
        let fix = "Unlock your login keychain (open Keychain Access), then run: lightbulb setup";
        row("Keychain", false, "", "cannot be read", fix);
    }
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
    if cfg!(target_os = "macos") && up && call("status").is_some_and(|r| r["lan"] == "blocked") {
        let fix = "Allow lightbulb in System Settings > Privacy & Security > Local Network";
        row("Local network", false, "", "blocked by macOS", fix);
    }
    if cfg!(target_os = "macos") {
        // Installed to start at login: the plist is there and launchd has not disabled it.
        // Loaded now is a separate fact, shown so a job that is not running under launchd
        // (no KeepAlive until the next login) is visible.
        let installed = plist_path(&home).exists();
        let (enabled, loaded) = if use_launchctl() {
            let uid = unsafe { libc::getuid() };
            let listing = Command::new("launchctl")
                .args(["print-disabled", &format!("gui/{uid}")])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let loaded = launchctl(&["print", &format!("gui/{uid}/{LABEL}")]);
            (!disabled_in(&listing, LABEL), loaded)
        } else {
            (true, true)
        };
        let good = if loaded {
            "yes"
        } else {
            "yes (not loaded now; run lightbulb setup to load it)"
        };
        let bad = if installed { "no (disabled)" } else { "no" };
        row("Starts at login", installed && enabled, good, bad, &setup);
    }
    if crate::login::stored(&dir).is_some() {
        let now = crate::daemon::ipc::now() as i64;
        let ended = crate::login::ended(&dir, now);
        let fix = "Sign in again: lightbulb login";
        row(
            "Lightbulb sign-in",
            !ended,
            "signed in",
            "ended: run lightbulb login",
            fix,
        );
    }
    let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777).ok();
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
        let short = r.short();
        row(&r.product(), ok, &short, &short, &setup);
    }
    let named = match short_link(&home) {
        Link::Ours => Ok("\"lightbulb\""),
        Link::File => Ok("~/.local/bin/lightbulb is another program's; put ~/.lightbulb/connect/bin on your PATH"),
        Link::Other => Err("\"lightbulb\" is a link to another program (run setup)"),
        Link::Missing => Err("not installed"),
    };
    row(
        "Command name",
        named.is_ok(),
        named.unwrap_or_default(),
        named.err().unwrap_or_default(),
        &setup,
    );
    let model = if crate::embed_model::installed() {
        "installed"
    } else {
        "not installed (lightbulb setup downloads it; --no-model keeps it off)"
    };
    // Keyword-only memory still works: a missing model is said, not failed.
    row("Memory model", true, model, "", "");
    let on = crate::config::Config::load(&dir).is_ok_and(|c| auto_update_on(&c));
    row("Auto-update", true, if on { "on" } else { "off" }, "", "");
    crate::telemetry::sync_env_optout(&dir);
    let counts = crate::telemetry::active(&dir);
    row(
        "Usage counts",
        true,
        if counts { "on" } else { "off" },
        "",
        "",
    );
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

/// The words `setup` opens with for a person at a terminal who is not signed in.
const SIGN_IN_OFFER: &str = "Sign in (free): your memory and sessions follow you across computers; the Claude app reaches your agents; the evaluator can read sessions you upload. Or stay local.\n  [Enter] Sign in    [l] Stay local";

/// `setup`'s first screen: the free sign-in, with staying local one key away. Staying local
/// records that uploads are off. Skipped with nobody at a terminal (an update, the daemon's
/// refresh) and when this computer is already signed in.
/// (`login::login` asks what to upload once signed in, so this does not.)
fn first_screen() -> Result<()> {
    use std::io::IsTerminal;
    let dir = paths::state_dir();
    let person = std::io::stdin().is_terminal() || test_env(TTY_ENV).is_some();
    if !person || crate::login::stored_proof(&dir).is_some() {
        return Ok(());
    }
    println!("{SIGN_IN_OFFER}");
    let mut answer = String::new();
    // Only Enter or `y` signs in; anything else, and end of input, stays local.
    let n = std::io::stdin().read_line(&mut answer)?;
    let a = answer.trim();
    if n > 0 && (a.is_empty() || a.eq_ignore_ascii_case("y")) {
        match crate::login::login() {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) => eprintln!("{e}"),
        }
        println!("Staying local.");
    }
    crate::config::Config::set(&dir, "capture_upload", "off")
}

/// What `lightbulb login` asks right after signing in, to a person at a terminal: what to
/// upload of the sessions captured here. Enter, end of input and anything unknown are `off`.
pub fn ask_upload_mode() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() && test_env(TTY_ENV).is_none() {
        return Ok(());
    }
    println!(
        "Send your coding sessions to Lightbulb? The evaluator reads only what you send, and suggests lessons for you to approve.\n  off       Nothing leaves this computer. [default]\n  metadata  The agent, repository, times, token counts and tool names, signed as you; never what the tools did.\n  full      Also a short summary of each tool call (never file contents, diffs, command output or prompts).\nChange it any time: lightbulb sessions upload off|metadata|full\nUpload [off/metadata/full]:"
    );
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    let mode = answer.trim().to_lowercase();
    let mode = crate::upload::MODES
        .iter()
        .find(|m| **m == mode)
        .unwrap_or(&"off");
    crate::upload::set_mode(&paths::state_dir(), mode)?;
    println!("Uploads are {mode}. {}", crate::upload::describe(mode));
    Ok(())
}

/// `setup [--remove]`: wires the installed agents to the canonical binary (or `executable`)
/// and starts the background service; prints one line per agent, then what to try next. Exits
/// 1 when an agent ends with hooks but no tools. `--remove` is the off switch: our entries,
/// the old version's, the job and the running service go. After the hooks it downloads the
/// memory model once; a failed download is one line, and setup goes on. `model`: the person's
/// choice (`--no-model` is `Some(false)`), kept in `memory_model`; `None` (the automatic setup
/// after an update) keeps the choice already made.
pub fn setup(
    remove: bool,
    executable: Option<PathBuf>,
    server: Option<String>,
    verbose: bool,
    model: Option<bool>,
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
        first_screen()?;
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
        for a in report.agents.iter().filter(|a| !a.wrote.is_empty()) {
            println!("{}: removed", a.product());
            files(a);
        }
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
    let dir = paths::state_dir();
    let refresh = model.is_none();
    let model = match model {
        Some(on) => {
            // A config.toml that will not take the key is a line, not a failed setup.
            if let Err(e) = crate::config::Config::set(&dir, "memory_model", &on.to_string()) {
                eprintln!("setup: the model choice was not kept: {e}");
            }
            on
        }
        None => crate::config::Config::load(&dir).map_or(true, |c| c.memory_model),
    };
    if model {
        match crate::embed_model::install(&dir) {
            Ok(()) => println!(
                "Memory model installed ({} MB)",
                crate::embed_model::size_mb()
            ),
            Err(e) => println!("Memory model not installed: {e} Memory search is keyword-only."),
        }
    }
    if cfg!(target_os = "macos") {
        let code = if refresh {
            refresh_job(&home, &executable)?
        } else {
            install_job(&home, &executable)?
        };
        if code != 0 {
            return Ok(code);
        }
    } else {
        restart_if_old()?;
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
    if crate::telemetry::on_setup(&paths::state_dir()) {
        println!("{}\n", crate::telemetry::NOTICE);
    }
    let connected: Vec<String> = (report.agents.iter())
        .filter(|a| a.connected())
        .map(|a| a.product())
        .collect();
    let connected: Vec<&str> = connected.iter().map(String::as_str).collect();
    println!("\n{}", setup::summary(&connected, command()));
    let _ = fs::write(paths::state_path().join(SETUP_VERSION), VERSION);
    Ok(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn print_disabled_marks_only_the_disabled_label() {
        let l = "\tdisabled services = {\n\t\t\"a.b\" => disabled\n\t\t\"c.d\" => enabled\n\t\t\"e.f\" => true\n\t}\n";
        assert!(super::disabled_in(l, "a.b"));
        assert!(super::disabled_in(l, "e.f"));
        assert!(!super::disabled_in(l, "c.d"));
        assert!(!super::disabled_in(l, "missing"));
        assert!(!super::disabled_in(l, "a"));
    }

    #[test]
    fn a_checksum_verifies_only_under_its_signing_key() {
        use ed25519_dalek::{Signer, SigningKey};
        let k = SigningKey::from_bytes(&[7; 32]);
        let sum = b"ab12  lightbulb-linux-x86_64.tar.gz\n";
        let sig = k.sign(sum).to_bytes();
        assert!(super::signed_by(k.verifying_key().as_bytes(), sum, &sig));
        assert!(!super::signed_by(
            k.verifying_key().as_bytes(),
            b"other",
            &sig
        ));
        assert!(!super::signed_by(
            k.verifying_key().as_bytes(),
            sum,
            &sig[..63]
        ));
        // Not the release key's signature.
        assert!(!super::signed_by(&super::RELEASE_KEY, sum, &sig));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_signature_requirement_is_pinned_to_our_team() {
        let bin = std::path::Path::new("/x/lightbulb");
        assert_eq!(
            super::REQUIREMENT,
            "=anchor apple generic and certificate leaf[subject.OU] = \"WZX4U3C22Y\""
        );
        let want: Vec<std::ffi::OsString> = [
            "--verify",
            "--strict",
            "-R",
            super::REQUIREMENT,
            "/x/lightbulb",
        ]
        .map(Into::into)
        .to_vec();
        assert_eq!(super::codesign_args(bin), want);
        // Apple-signed, but not by our team.
        assert!(super::verify_signature(std::path::Path::new("/bin/ls")).is_err());
    }

    #[test]
    fn auto_update_installs_only_a_strictly_newer_release_once() {
        use super::should_auto_update as go;
        assert!(go("0.3.2", "v0.3.3", None));
        assert!(go("0.3.2", "v0.10.0", None), "versions compare as numbers");
        assert!(!go("0.3.3", "v0.3.3", None), "same version");
        assert!(!go("0.3.3", "v0.3.2", None), "never a downgrade");
        assert!(!go("0.3.2", "latest", None) && !go("0.3.2", "v0.3.3; rm", None));
        assert!(
            !go("0.3.2", "v0.3.3", Some("v0.3.3")),
            "installed last time"
        );
        assert!(go("0.3.2", "v0.3.4", Some("v0.3.3")));
    }

    #[test]
    fn setup_reruns_after_a_version_change() {
        assert!(super::setup_due(None, "0.3.3"));
        assert!(super::setup_due(Some("0.3.2\n"), "0.3.3"));
        assert!(!super::setup_due(Some("0.3.3\n"), "0.3.3"));
    }

    #[test]
    fn auto_update_is_off_by_config() {
        let mut c = crate::config::Config::default();
        std::env::remove_var(super::NO_AUTO_UPDATE);
        assert!(super::auto_update_on(&c));
        c.auto_update = false;
        assert!(!super::auto_update_on(&c));
    }

    #[test]
    fn a_title_with_control_characters_prints_as_plain_text() {
        let title = "fix\u{1b}[2J\u{1b}]8;;http://x\u{7}\tit\u{85}\u{2066}now\r\nok";
        assert_eq!(super::cell(title), "fix[2J]8;;http://x itnow ok");
    }
}
