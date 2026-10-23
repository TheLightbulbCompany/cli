//! `setup [--remove]`: wires the installed coding agents to this engine. For each agent whose
//! home directory exists it writes the hook entries (`'<executable>' hook <agent> <event>`)
//! and the MCP server entry (`<executable> mcp`). Every file is backed up once, as
//! `<file>.lightbulb-backup`, before its first edit; a backup is never overwritten.
//! `--remove` takes out only entries whose command names our executable, and restores
//! nothing from the backups (they are the person's safety net).
//!
//! An MCP entry under our server name whose command is not our executable is foreign: it is
//! left alone and reported, and no allow rule is written for it. Our allow rule exists only
//! while the MCP entry is ours.
//!
//! A vendor config file that is missing is created, but only when that agent's home
//! directory exists (the agent reads it from there); Cursor's `cli-config.json` is the
//! exception: it is edited only if Cursor made it. Backups, temp files and new files are
//! created with the source file's mode (0600 for a new file), never wider.

mod claude;
mod codex;
mod cursor;
mod grok;

use crate::Result;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Report {
    pub agents: Vec<AgentReport>,
}

/// Where one agent stands after a run (or, for `doctor`, as its files are).
#[derive(Debug, Default, Clone, PartialEq)]
pub enum State {
    #[default]
    NotInstalled,
    /// Hooks and tools, the tools under this name.
    Connected(String),
    /// Hooks and tools, the tools under this name because the first was another program's.
    ClashedTo(String),
    /// Hooks but no tools, for this reason.
    HooksOnly(String),
    /// Wiring it failed (a file it could not read or write); whether its hooks are in.
    Failed { why: String, hooks: bool },
    /// `--remove` ran on it.
    Removed,
}

#[derive(Debug, Default)]
pub struct AgentReport {
    pub name: &'static str,
    pub state: State,
    /// The config files this run changed.
    pub wrote: Vec<PathBuf>,
    pub notes: Vec<String>,
    /// Codex: its hook trust was written for the person this run.
    pub trusted: bool,
}

type Configure = fn(&Ctx, &mut AgentReport) -> Result<()>;

/// What one run writes, and where.
pub(crate) struct Ctx<'a> {
    pub home: &'a Path,
    pub exe: &'a str,
    pub server: &'a str,
    pub remove: bool,
}

/// The MCP server name when `--server-name` is not given, and the one taken instead when an
/// agent has another program's server under it (the Lightbulb app's, say).
pub const DEFAULT_SERVER: &str = "lightbulb";
pub const OTHER_SERVER: &str = "lightbulb-connect";

/// Whether `name` can name the MCP server: it lands in TOML keys and allow rules.
pub fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether `agent`'s hook file under `home` already runs `<some executable> hook <agent> ...`
/// (what `doctor` checks; reads, never writes).
pub fn wired(home: &Path, agent: &str) -> bool {
    let file = match agent {
        "claude" => ".claude/settings.json",
        "codex" => ".codex/hooks.json",
        "cursor" => ".cursor/hooks.json",
        "grok" => ".grok/hooks/lightbulb-connect.json",
        _ => return false,
    };
    fs::read_to_string(home.join(file)).is_ok_and(|t| t.contains(&format!(" hook {agent} ")))
}

/// `agent`'s MCP server entries under `home`, `(name, command)` (reads, never writes).
fn servers(home: &Path, agent: &str) -> Vec<(String, String)> {
    let (file, key) = match agent {
        "claude" => (".claude.json", "mcpServers"),
        "codex" => (".codex/config.toml", "mcp_servers"),
        "cursor" => (".cursor/mcp.json", "mcpServers"),
        "grok" => (".grok/config.toml", "mcp_servers"),
        _ => return vec![],
    };
    let read = || -> Option<Vec<(String, String)>> {
        let text = fs::read_to_string(home.join(file)).ok()?;
        Some(if file.ends_with(".toml") {
            let doc: toml_edit::DocumentMut = text.parse().ok()?;
            let command = |v: &toml_edit::Item| Some(v.get("command")?.as_str()?.to_string());
            doc.get(key)?
                .as_table_like()?
                .iter()
                .filter_map(|(k, v)| Some((k.to_string(), command(v)?)))
                .collect()
        } else {
            let doc: Value = serde_json::from_str(&text).ok()?;
            let command = |v: &Value| Some(v["command"].as_str()?.to_string());
            doc[key]
                .as_object()?
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), command(v)?)))
                .collect()
        })
    };
    read().unwrap_or_default()
}

/// The name of `agent`'s MCP entry under `home` that runs a `lightbulb-connect` binary, if
/// any (what `doctor` checks; reads, never writes).
pub fn tools(home: &Path, agent: &str) -> Option<String> {
    let ours = |c: &str| {
        Path::new(c)
            .file_name()
            .is_some_and(|f| f == "lightbulb-connect")
    };
    servers(home, agent)
        .into_iter()
        .find(|(_, c)| ours(c))
        .map(|(k, _)| k)
}

/// `agent`'s state as its files under `home` are (`doctor`).
pub fn status(home: &Path, agent: &str, dir: &str) -> State {
    match (
        home.join(dir).is_dir(),
        wired(home, agent),
        tools(home, agent),
    ) {
        (false, ..) => State::NotInstalled,
        (_, true, Some(name)) => State::Connected(name),
        (_, true, None) => State::HooksOnly("tools missing (run setup)".into()),
        (_, false, _) => State::Failed {
            why: String::new(),
            hooks: false,
        },
    }
}

/// `setup`'s closing words: what is connected (product names) and what to try next, with
/// `cmd` the command name that works on this computer.
pub fn summary(connected: &[&str], cmd: &str) -> String {
    let list = if connected.is_empty() {
        "none yet. Install Claude Code, Codex, Cursor or Grok CLI, then run setup again".into()
    } else {
        connected.join(", ")
    };
    let first = connected.first().copied().unwrap_or("Claude Code");
    format!(
        "Lightbulb Connect is on.\n  Connected: {list}\n\nTry it: open {first} in any folder and ask \"list my other sessions\".\nAdd your other computer or a teammate: {cmd} invite\nCheck everything: {cmd} doctor"
    )
}

/// The agents setup wires: name, home directory, configure.
pub const AGENTS: [(&str, &str); 4] = [
    ("claude", ".claude"),
    ("codex", ".codex"),
    ("cursor", ".cursor"),
    ("grok", ".grok"),
];

/// Wires (or, `remove`, unwires) every installed agent. `server`: the MCP entry's name; `None`
/// installs under `DEFAULT_SERVER`, or `OTHER_SERVER` where the default is another program's
/// (found by reading the agent's MCP config first), and removes ours under either.
/// ponytail: an agent wired under `OTHER_SERVER` keeps that entry when the default frees up
/// later and a new run adds ours under the default too; clean up when that is seen.
pub fn run(home: &Path, executable: &Path, server: Option<&str>, remove: bool) -> Report {
    let exe = executable.to_string_lossy();
    let names = server.map_or(vec![DEFAULT_SERVER, OTHER_SERVER], |s| vec![s]);
    let configures: [Configure; 4] = [
        claude::configure,
        codex::configure,
        cursor::configure,
        grok::configure,
    ];
    let mut report = Report::default();
    for ((name, dir), configure) in AGENTS.into_iter().zip(configures) {
        let mut r = AgentReport {
            name,
            ..Default::default()
        };
        let ctx = |server| Ctx {
            home,
            exe: &exe,
            server,
            remove,
        };
        r.state = if !home.join(dir).is_dir() {
            State::NotInstalled
        } else if !names.iter().all(|n| valid_server_name(n)) {
            let why = "the server name takes letters, digits, - and _".into();
            State::Failed { why, hooks: false }
        } else if remove {
            let ran = names.iter().try_for_each(|n| configure(&ctx(n), &mut r));
            match ran {
                Ok(()) => State::Removed,
                Err(e) => State::Failed {
                    why: e.to_string(),
                    hooks: wired(home, name),
                },
            }
        } else {
            // The first name that is free here, or ours already.
            let taken = servers(home, name);
            let free = |n: &&&str| taken.iter().all(|(k, c)| k != **n || *c == exe);
            let pick = names.iter().find(free);
            match configure(&ctx(pick.unwrap_or(&names[0])), &mut r) {
                Err(e) => State::Failed {
                    why: e.to_string(),
                    hooks: wired(home, name),
                },
                Ok(()) => match pick {
                    None => State::HooksOnly("another program has the tool server name".into()),
                    Some(n) if *n == names[0] => State::Connected(n.to_string()),
                    Some(n) => State::ClashedTo(n.to_string()),
                },
            }
        };
        report.agents.push(r);
    }
    report
}

impl AgentReport {
    /// Its product name: `Claude Code`, `Grok CLI`.
    pub fn product(&self) -> String {
        crate::frame::agent_name(self.name)
    }

    /// Whether it ended with hooks and tools.
    pub fn connected(&self) -> bool {
        matches!(self.state, State::Connected(_) | State::ClashedTo(_))
    }

    /// One line for the person: connected or not, and why.
    pub fn line(&self) -> String {
        let who = self.product();
        let mut line = match &self.state {
            State::NotInstalled => format!("{who}: not installed"),
            State::Connected(t) if t == DEFAULT_SERVER => format!("{who}: connected (hooks + tools)"),
            State::Connected(t) => format!("{who}: connected (hooks + tools as \"{t}\")"),
            State::ClashedTo(t) => format!(
                "{who}: connected (hooks + tools as \"{t}\" — another \"{DEFAULT_SERVER}\" tool server is already installed)"
            ),
            State::HooksOnly(why) | State::Failed { why, hooks: true } => {
                format!("{who}: hooks added, but not its tools — {why}")
            }
            State::Failed { why, .. } => format!("{who}: not connected — {why}"),
            State::Removed => format!("{who}: removed"),
        };
        if self.trusted {
            line.push_str(". Its hook prompt was answered for you.");
        }
        for note in &self.notes {
            line.push_str(&format!("; {note}"));
        }
        line
    }

    /// What the person does about it when installing left it short, as `cmd` is typed here.
    pub fn fix(&self, cmd: &str) -> Option<String> {
        let who = self.product();
        match self.state {
            State::HooksOnly(_) => Some(format!(
                "{who} has hooks but no tools: another program has the tool server name. Run: {cmd} setup --server-name <a free name>"
            )),
            State::Failed { .. } => Some(format!(
                "{who} is not connected: fix the file named above, then run {cmd} setup again."
            )),
            _ => None,
        }
    }

    /// How `doctor` says it.
    pub fn short(&self) -> &'static str {
        match self.state {
            State::NotInstalled => "not installed",
            State::Connected(_) | State::ClashedTo(_) => "connected",
            State::HooksOnly(_) => "hooks only — tools missing (run setup)",
            State::Failed { .. } | State::Removed => "not connected",
        }
    }
}

/// Who holds the MCP server name in one config.
#[derive(Debug, PartialEq)]
pub(crate) enum Owner {
    Absent,
    Ours,
    Foreign,
}

impl Ctx<'_> {
    pub fn hook_command(&self, agent: &str, event_arg: &str) -> String {
        format!("{} hook {agent} {event_arg}", self.quoted())
    }

    /// Whether a hook command runs our executable.
    pub fn is_ours(&self, command: &str) -> bool {
        command.starts_with(&format!("{} hook ", self.quoted()))
    }

    /// The MCP entry's `args`: `mcp`, plus `--server-name` when it is not the default.
    pub fn mcp_args(&self) -> Vec<&str> {
        if self.server == DEFAULT_SERVER {
            vec!["mcp"]
        } else {
            vec!["mcp", "--server-name", self.server]
        }
    }

    fn quoted(&self) -> String {
        format!("'{}'", self.exe.replace('\'', "'\\''"))
    }

    /// The owner of a server entry, given its command (`None`: the entry has none).
    pub fn owner(&self, exists: bool, command: Option<&str>) -> Owner {
        match (exists, command) {
            (false, _) => Owner::Absent,
            (true, Some(c)) if c == self.exe => Owner::Ours,
            _ => Owner::Foreign,
        }
    }
}

/// Edits one JSON object file: `edit` runs on its contents (`{}` for a missing file when
/// `create`; a missing file is skipped otherwise) and a changed result is written.
pub(crate) fn edit_json(
    path: &Path,
    create: bool,
    r: &mut AgentReport,
    edit: impl FnOnce(&mut Value) -> Result<()>,
) -> Result<()> {
    let Some(text) = read(path, create)? else {
        return Ok(());
    };
    let mut doc: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?
    };
    if !doc.is_object() {
        return Err(format!("{} is not a JSON object", path.display()).into());
    }
    let before = doc.clone();
    edit(&mut doc)?;
    if doc != before {
        write(path, &(serde_json::to_string_pretty(&doc)? + "\n"), true, r)?;
    }
    Ok(())
}

/// `edit_json` for a TOML file; formatting and comments are kept.
pub(crate) fn edit_toml(
    path: &Path,
    create: bool,
    r: &mut AgentReport,
    edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
) -> Result<()> {
    let Some(text) = read(path, create)? else {
        return Ok(());
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    edit(&mut doc)?;
    let after = doc.to_string();
    if after != text {
        write(path, &after, true, r)?;
    }
    Ok(())
}

/// The file's text; `Some("")` for a missing file when `create`, `None` when not.
fn read(path: &Path, create: bool) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(create.then(String::new)),
        Err(e) => Err(format!("{}: {e}", path.display()).into()),
    }
}

/// Replaces `path` atomically (through a symlink), backing it up once first when `backup`.
/// The new contents are written and synced at the target's mode (0600 for a new file).
pub(crate) fn write(path: &Path, text: &str, backup: bool, r: &mut AgentReport) -> Result<()> {
    if backup && path.exists() {
        backup_once(path)?;
    }
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = tmp_path(&target);
    create_with(&tmp, text.as_bytes(), mode_of(&target).unwrap_or(0o600))?;
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    r.wrote.push(path.to_path_buf());
    Ok(())
}

fn mode_of(path: &Path) -> Option<u32> {
    fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

/// `.<name>.<pid>.lightbulb-tmp` beside `path`.
fn tmp_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.{}.lightbulb-tmp", std::process::id()))
}

/// A new file holding `bytes`, created at `mode` and synced; removed again on failure.
pub(crate) fn create_with(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let _ = fs::remove_file(path); // a leftover of a crashed run with this pid
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()));
    if let Err(e) = written {
        let _ = fs::remove_file(path);
        return Err(e.into());
    }
    Ok(())
}

pub(crate) fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lightbulb-backup");
    path.with_file_name(name)
}

/// Copies `path` to its backup unless one exists: complete in a temp file at the source's
/// mode first, then hard-linked into place (which fails if a backup appeared meanwhile), so
/// a crash never leaves a truncated backup behind.
fn backup_once(path: &Path) -> Result<()> {
    let backup = backup_path(path);
    if backup.symlink_metadata().is_ok() {
        return Ok(());
    }
    let tmp = tmp_path(&backup);
    create_with(&tmp, &fs::read(path)?, mode_of(path).unwrap_or(0o600))?;
    let linked = fs::hard_link(&tmp, &backup);
    let _ = fs::remove_file(&tmp);
    match linked {
        Err(e) if e.kind() != ErrorKind::AlreadyExists => Err(e.into()),
        _ => Ok(()),
    }
}

/// The Claude-shaped hooks object (Claude Code, Codex, Grok): `hooks.<Event>[]` groups of
/// `{matcher?, hooks: [{type, command, ...}]}`. Installing puts `group` where our group for
/// `event` already is (an index others' trust may be keyed by), or appends it; removing
/// (`group` = None) takes our hooks out of every group and drops groups left empty.
/// In a group that also holds someone else's hooks only our hook is replaced or removed.
/// Returns the index of our group when the group is only ours (Codex trusts it by index).
pub(crate) fn set_hook_group(
    ctx: &Ctx,
    doc: &mut Value,
    event: &str,
    group: Option<Value>,
) -> Result<Option<usize>> {
    let ours_hook = |h: &Value| h["command"].as_str().is_some_and(|c| ctx.is_ours(c));
    let ours = |g: &Value| {
        g["hooks"]
            .as_array()
            .is_some_and(|hs| hs.iter().any(ours_hook))
    };
    let root = doc.as_object_mut().ok_or("not an object")?;
    if group.is_none() && root.get("hooks").and_then(|h| h.get(event)).is_none() {
        return Ok(None);
    }
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("`hooks` is not an object")?;
    let list = hooks
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| format!("`hooks.{event}` is not a list"))?;
    let at = list.iter().position(ours);
    let only_ours = at.filter(|&i| {
        list[i]["hooks"]
            .as_array()
            .is_some_and(|hs| hs.iter().all(ours_hook))
    });
    let index = match group {
        Some(g) => match at {
            Some(i) if only_ours.is_some() => {
                list[i] = g;
                Some(i)
            }
            Some(i) => {
                let hs = list[i]["hooks"].as_array_mut().expect("holds our hook");
                let j = hs.iter().position(ours_hook).expect("holds our hook");
                hs[j] = g["hooks"][0].clone();
                None
            }
            None => {
                list.push(g);
                Some(list.len() - 1)
            }
        },
        None => {
            for g in list.iter_mut().filter(|g| ours(g)) {
                if let Some(hs) = g["hooks"].as_array_mut() {
                    hs.retain(|h| !h["command"].as_str().is_some_and(|c| ctx.is_ours(c)));
                }
            }
            list.retain(|g| g["hooks"].as_array().is_none_or(|hs| !hs.is_empty()));
            only_ours
        }
    };
    if list.is_empty() {
        hooks.remove(event);
    }
    if hooks.is_empty() {
        root.remove("hooks");
    }
    Ok(index)
}

/// Puts `entry` under `doc[key][server]` or, removing, deletes it; a foreign entry is left.
pub(crate) fn set_json_server(
    ctx: &Ctx,
    doc: &mut Value,
    key: &str,
    entry: Value,
) -> Result<Owner> {
    if !(doc[key].is_null() || doc[key].is_object()) {
        return Err(format!("`{key}` is not an object").into());
    }
    let current = &doc[key][ctx.server];
    let owner = ctx.owner(!current.is_null(), current["command"].as_str());
    match owner {
        Owner::Foreign => return Ok(Owner::Foreign),
        Owner::Ours if ctx.remove => {
            let servers = doc[key].as_object_mut().expect("holds our entry");
            servers.remove(ctx.server);
            if servers.is_empty() {
                doc.as_object_mut().expect("object").remove(key);
            }
            return Ok(Owner::Absent);
        }
        Owner::Absent if ctx.remove => return Ok(Owner::Absent),
        _ => {}
    }
    if doc[key].is_null() {
        doc[key] = json!({});
    }
    doc[key][ctx.server] = entry;
    Ok(Owner::Ours)
}

/// Adds `rule` to a JSON list at `doc[a][b]` (`present`), or removes it.
pub(crate) fn set_json_rule(doc: &mut Value, a: &str, b: &str, rule: &str, present: bool) {
    let has = doc[a][b]
        .as_array()
        .is_some_and(|l| l.iter().any(|v| v == rule));
    if present && !has {
        if !doc[a].is_object() {
            doc[a] = json!({});
        }
        if !doc[a][b].is_array() {
            doc[a][b] = json!([]);
        }
        doc[a][b].as_array_mut().expect("list").push(rule.into());
    } else if !present && has {
        doc[a][b]
            .as_array_mut()
            .expect("list")
            .retain(|v| v != rule);
    }
}

/// `[mcp_servers.<server>]` in a TOML config: `fields` written, or, removing, the table
/// deleted; a foreign one is left.
pub(crate) fn set_toml_server(
    ctx: &Ctx,
    doc: &mut toml_edit::DocumentMut,
    fields: &[(&str, toml_edit::Value)],
) -> Result<Owner> {
    use toml_edit::Item;
    let current = doc.get("mcp_servers").and_then(|s| s.get(ctx.server));
    let owner = ctx.owner(
        current.is_some(),
        current
            .and_then(|t| t.get("command"))
            .and_then(Item::as_str),
    );
    match owner {
        Owner::Foreign => return Ok(Owner::Foreign),
        Owner::Ours if ctx.remove => {
            let servers = doc["mcp_servers"].as_table_like_mut().expect("has ours");
            servers.remove(ctx.server);
            if servers.is_empty() {
                doc.remove("mcp_servers");
            }
            return Ok(Owner::Absent);
        }
        Owner::Absent if ctx.remove => return Ok(Owner::Absent),
        _ => {}
    }
    let table = toml_table(
        toml_table(doc.as_table_mut(), "mcp_servers", true)?,
        ctx.server,
        false,
    )?;
    for (k, v) in fields {
        table[*k] = Item::Value(v.clone());
    }
    Ok(Owner::Ours)
}

/// The table at `parent[key]`, created when missing (`implicit`: with no header of its own).
pub(crate) fn toml_table<'a>(
    parent: &'a mut toml_edit::Table,
    key: &str,
    implicit: bool,
) -> Result<&'a mut toml_edit::Table> {
    use toml_edit::{Item, Table};
    parent
        .entry(key)
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(implicit);
            Item::Table(t)
        })
        .as_table_mut()
        .ok_or_else(|| format!("`{key}` is not a TOML table").into())
}

#[cfg(test)]
mod tests;
