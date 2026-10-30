//! `lightbulb app`: build, check, preview, publish and undo the apps you build (not the hosted
//! assistants, which are `agents`). Every command is one call to the builder routes (`api`).
//! `app.yaml` is the app's settings; its parser is hand-rolled for exactly six keys.

pub mod api;
pub mod check;
pub(crate) mod evidence;
pub mod migrations;
mod ship;
pub mod starter;

pub use ship::{archive, has_passing_review};

use crate::Result;
use clap::{Subcommand, ValueEnum};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// What an app is (spec §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Kind {
    Site,
    ContentSite,
    Form,
    InternalTool,
    Dashboard,
    Portal,
    Payments,
    Automation,
}

/// Who can open it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OpenTo {
    Room,
    Workspace,
    Invited,
    Public,
}

/// What the browser pass proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum EvidenceKind {
    Browser,
    Review,
}

/// What the pass or reviewer found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Verdict {
    Pass,
    Fail,
}

/// What asked for the app: `kind` (`idea`, `issue`, ...) and its `ref`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Origin {
    pub kind: String,
    pub r#ref: Option<String>,
}

#[derive(Subcommand)]
pub enum AppCommand {
    /// Start an app in a new folder.
    New {
        /// The app's name: a DNS label, lowercase letters, digits and dashes.
        name: String,
        /// The room it is built for.
        #[arg(long)]
        room: Option<String>,
        #[arg(long, value_enum, default_value = "site")]
        kind: Kind,
    },
    /// Build the preview and say when it is done.
    Up {
        /// An approved card for a destructive migration.
        #[arg(long, alias = "approved-destructive")]
        approved: Option<String>,
        /// Who asked for the app (sent with the first `up`).
        #[arg(long)]
        requested_by: Option<String>,
        /// The room it is built for (saved in .lightbulb/app.json).
        #[arg(long)]
        room: Option<String>,
    },
    /// Put the preview live.
    Publish {
        #[arg(long)]
        reason: Option<String>,
        /// An approved card for a destructive migration.
        #[arg(long, alias = "approved-destructive")]
        approved: Option<String>,
        /// The room it is built for (saved in .lightbulb/app.json).
        #[arg(long)]
        room: Option<String>,
    },
    /// Go back to an earlier version (code only; data is unchanged).
    Rollback { version: Option<u32> },
    /// Where the app is.
    Status {
        /// Also the last errors from the app.
        #[arg(long)]
        errors: bool,
    },
    /// Remove the app (the owner confirms).
    Destroy,
    /// Type check, lint and design lint (pnpm check); --publish adds the dependency audit.
    Check {
        /// Also check what publishing needs.
        #[arg(long)]
        publish: bool,
    },
    /// Note a tool that failed you in .lightbulb/lessons.md (once per `up`).
    ReportBrokenTool {
        /// The tool that broke.
        tool: String,
        /// What happened.
        what: String,
    },
    /// A short-lived link past the preview's door, for your own check. Never post it.
    TestLink,
    /// Record a browser pass for the preview.
    Evidence {
        #[arg(long, value_enum)]
        kind: EvidenceKind,
        /// The recording or screenshot; it stays on this computer.
        #[arg(long)]
        file: PathBuf,
        #[arg(long, default_value = "")]
        note: String,
        /// Required for a review; a browser pass defaults to pass.
        #[arg(long, value_enum)]
        verdict: Option<Verdict>,
    },
}

/// Prints a failure and returns the exit code 1: `{"code","message","next"}` on stdout in
/// JSON mode, else the message and a `Next:` line on stderr.
pub fn fail(json: bool, code: &str, message: &str, next: &str) -> i32 {
    fail_with(json, code, message, next, &[])
}

/// `fail` with `details` (one line each): a JSON list, or indented lines under the message.
pub fn fail_with(json: bool, code: &str, message: &str, next: &str, details: &[String]) -> i32 {
    if json {
        let mut v = json!({"code": code, "message": message, "next": next});
        if !details.is_empty() {
            v["details"] = json!(details);
        }
        println!("{v}");
    } else {
        for l in failure_lines(message, next, details) {
            eprintln!("{l}");
        }
    }
    1
}

/// A failure for a person: the message, the details indented, then `Next:` when there is one.
fn failure_lines(message: &str, next: &str, details: &[String]) -> Vec<String> {
    let mut out = vec![message.to_string()];
    out.extend(details.iter().map(|d| format!("  {d}")));
    if !next.trim().is_empty() {
        out.push(format!("Next: {next}"));
    }
    out
}

/// A DNS label: `^[a-z][a-z0-9-]{1,38}[a-z0-9]$`, not ending in `-preview` (the preview's
/// address is `{name}-preview.<apps domain>`).
pub fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    !n.ends_with("-preview")
        && (3..=40).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[b.len() - 1] != b'-'
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn run(cmd: AppCommand, json: bool) -> Result<i32> {
    match cmd {
        AppCommand::New { name, room, kind } => {
            if name.ends_with("-preview") {
                return Ok(fail(
                    json,
                    "bad_name",
                    &format!("An app's name cannot end in -preview: Lightbulb gives every app a preview at <name>-preview, so {name} would clash with another app's preview."),
                    "pick a name that does not end in -preview",
                ));
            }
            if !valid_name(&name) {
                return Ok(fail(
                    json,
                    "bad_name",
                    "An app's name is 3 to 40 lowercase letters, digits and dashes, starting with a letter.",
                    "pick another name",
                ));
            }
            new(&name, room.as_deref(), kind, json)
        }
        AppCommand::Status { errors } => status(errors, json),
        AppCommand::Check { publish } => check::check(publish, json),
        AppCommand::Up {
            approved,
            requested_by,
            room,
        } => {
            let (root, config, auth, approved) = match gated(approved, json) {
                Ok(g) => g,
                Err(code) => return Ok(code),
            };
            ship::up(&root, &config, &auth, approved, requested_by, room, json)
        }
        AppCommand::Publish {
            reason,
            approved,
            room,
        } => {
            let (root, config, auth, approved) = match gated(approved, json) {
                Ok(g) => g,
                Err(code) => return Ok(code),
            };
            ship::publish(&root, &config, &auth, reason, approved, room, json)
        }
        AppCommand::Rollback { version } => {
            let (root, _, auth) = match preflight(json) {
                Ok(p) => p,
                Err(code) => return Ok(code),
            };
            ship::rollback(&root, &auth, version, json)
        }
        AppCommand::Destroy => {
            let (root, _, auth) = match preflight(json) {
                Ok(p) => p,
                Err(code) => return Ok(code),
            };
            ship::destroy(&root, &auth, json)
        }
        AppCommand::TestLink => {
            let (root, _, auth) = match preflight(json) {
                Ok(p) => p,
                Err(code) => return Ok(code),
            };
            ship::test_link(&root, &auth, json)
        }
        AppCommand::ReportBrokenTool { tool, what } => {
            let (root, _, _) = match preflight(json) {
                Ok(p) => p,
                Err(code) => return Ok(code),
            };
            report_broken_tool(&root, &tool, &what, json)
        }
        AppCommand::Evidence {
            kind,
            file,
            note,
            verdict,
        } => {
            let (root, _, auth) = match preflight(json) {
                Ok(p) => p,
                Err(code) => return Ok(code),
            };
            evidence::record(&root, &auth, kind, verdict, &file, &note, json)
        }
    }
}

/// Appends `- DATE tool: what` under `## Broken tools` in lessons.md; once per `up` cycle.
fn report_broken_tool(root: &Path, tool: &str, what: &str, json: bool) -> Result<i32> {
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut progress = match check::read_progress(root) {
        Ok(p) => p,
        Err(e) => return Ok(fail(json, &e.code, &e.message, &e.next)),
    };
    if evidence::holds_pass(tool) || evidence::holds_pass(what) {
        return Ok(fail(
            json,
            "note_has_pass",
            "That note holds a test link; lessons are committed and uploaded.",
            "say what broke, without the link",
        ));
    }
    let cycle = progress["up_cycle"].as_u64().unwrap_or(0);
    if progress["broken_tool_reported_cycle"].as_u64() == Some(cycle) {
        return Ok(fail(
            json,
            "already_reported",
            "A broken tool was already reported since the last `up`.",
            "run lightbulb app up first; one report per cycle",
        ));
    }
    let path = root.join(".lightbulb/lessons.md");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    let line = format!(
        "- {} {}: {}\n",
        &crate::date::utc(crate::daemon::ipc::now() as i64)[..10],
        one_line(tool),
        one_line(what)
    );
    let head = "## Broken tools\n";
    let found = if text.starts_with(head) {
        Some(0)
    } else {
        text.find(&format!("\n{head}")).map(|i| i + 1)
    };
    if let Some(at) = found {
        // After the section's last entry, before the next heading.
        let start = at + head.len();
        let end = text[start..].find("\n#").map_or(text.len(), |i| start + i);
        let at = start + text[start..end].trim_end().len();
        text.insert_str(at, &format!("\n{}", line.trim_end()));
    } else {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("\n{head}{line}"));
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(&path, text)?;
    progress["broken_tool_reported_cycle"] = cycle.into();
    if let Err(e) = check::write_progress(root, &progress) {
        return Ok(fail(json, &e.code, &e.message, &e.next));
    }
    if json {
        println!("{}", json!({"ok": true}));
    } else {
        println!("Noted in .lightbulb/lessons.md.");
    }
    Ok(0)
}

/// The app, and the approved card id once the migration gate has passed.
fn gated(
    approved: Option<String>,
    json: bool,
) -> std::result::Result<(PathBuf, AppConfig, api::Auth, Option<String>), i32> {
    let (root, config, auth) = preflight(json)?;
    let approved = destructive_gate(&root, approved, json)?;
    Ok((root, config, auth, approved))
}

/// Stops on a destructive migration unless `approved` names the card; returns that id.
fn destructive_gate(
    root: &Path,
    approved: Option<String>,
    json: bool,
) -> std::result::Result<Option<String>, i32> {
    if approved.as_deref().is_some_and(|a| a.trim().is_empty()) {
        return Err(fail(
            json,
            "bad_args",
            "--approved needs the id of an approved card.",
            "pass --approved <card id>, or leave it out",
        ));
    }
    let g = migrations::gate(root).map_err(|e| fail(json, &e.code, &e.message, &e.next))?;
    if g.findings.is_empty() || approved.is_some() {
        return Ok(approved);
    }
    let mut words: Vec<String> = g
        .findings
        .iter()
        .map(|(_, f)| format!("This change would {}.", migrations::describe(f)))
        .collect();
    words.dedup();
    words.push("Nothing was applied.".into());
    let sql: Vec<String> = g
        .findings
        .iter()
        .map(|(file, f)| format!("{file}: {}", f.statement))
        .collect();
    Err(fail_with(
        json,
        "destructive_migration",
        &words.join("\n"),
        migrations::NEXT,
        &sql,
    ))
}

/// `./{name}`: the starter, `app.yaml`, the plan, and one git commit.
pub fn new(name: &str, room: Option<&str>, kind: Kind, json: bool) -> Result<i32> {
    let dir = std::env::current_dir()?.join(name);
    if dir.exists() && (!dir.is_dir() || std::fs::read_dir(&dir)?.next().is_some()) {
        return Ok(fail(
            json,
            "dir_not_empty",
            &format!("{name} already exists and is not empty."),
            "pick another name or an empty folder",
        ));
    }
    let content = matches!(kind, Kind::ContentSite | Kind::Site);
    let mut files = 0;
    for (path, bytes) in app_files(content) {
        let to = dir.join(path);
        std::fs::create_dir_all(to.parent().unwrap())?;
        // Binary files pass through untouched.
        match std::str::from_utf8(bytes) {
            Ok(t) => std::fs::write(to, t.replace("{{name}}", name))?,
            Err(_) => std::fs::write(to, bytes)?,
        }
        files += 1;
    }
    // The starter's own app.yaml is the base; only what was asked for changes.
    let mut config = AppConfig::load(&dir)?;
    config.kind = kind;
    if content {
        config.services.push("content".into());
    }
    config.open_to = if room.is_some() {
        OpenTo::Room
    } else {
        OpenTo::Workspace
    };
    config.origin = Origin {
        kind: "ask".into(),
        r#ref: None,
    };
    config.write(&dir)?;
    // The playbook is the plan's first section.
    let plan = dir.join(".lightbulb/plan.md");
    let playbook = std::fs::read_to_string(dir.join(format!("playbooks/{}.md", word(&kind))))?;
    let headings = std::fs::read_to_string(&plan)?;
    std::fs::write(&plan, format!("{}\n\n{headings}", playbook.trim_end()))?;
    if let Some(room) = room {
        std::fs::write(
            dir.join(".lightbulb/app.json"),
            json!({"room_id": room}).to_string(),
        )?;
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(&dir)
            .args(args)
            .output()
    };
    let msg = format!("lightbulb app new {name}");
    let git_failed = |what: &str, o: Option<std::process::Output>| {
        let why = o.map_or("git is not installed".into(), |o| {
            String::from_utf8_lossy(&o.stderr).trim().to_string()
        });
        fail(
            json,
            "git_failed",
            &format!("The app's files are written, but git {what} failed: {why}"),
            &format!("install git, delete {name}, then run lightbulb app new again"),
        )
    };
    for args in [["init", "-q"], ["add", "-A"]] {
        match git(&args) {
            Ok(o) if o.status.success() => {}
            o => return Ok(git_failed(args[0], o.ok())),
        }
    }
    if !git(&["commit", "-qm", &msg]).is_ok_and(|o| o.status.success()) {
        // No git identity on this computer: commit as Lightbulb.
        match git(&[
            "-c",
            "user.name=Lightbulb",
            "-c",
            "user.email=app@thelightbulb.company",
            "commit",
            "-qm",
            &msg,
        ]) {
            Ok(o) if o.status.success() => {}
            o => return Ok(git_failed("commit", o.ok())),
        }
    }
    let next = format!("cd {name} && pnpm install, then `lightbulb app up` to get a preview");
    if json {
        println!(
            "{}",
            json!({"name": name, "dir": dir, "files": files, "next": next})
        );
    } else {
        println!("Started {name} in {}.", dir.display());
        println!(
            "Who can open it: {}",
            if room.is_some() {
                "the people in its room"
            } else {
                "everyone in your workspace"
            }
        );
        println!("Next: {next}");
    }
    Ok(0)
}

const CONTENT: &str = "addons/content/";
/// Moved into the `(site)` route group by the content switch: /admin brings its own root layout.
const SITE_GROUP: [&str; 3] = [
    "src/app/layout.tsx",
    "src/app/page.tsx",
    "src/app/%5Flightbulb/",
];

/// The starter's files as the app's paths. `addons/` never lands in an app. The content switch
/// (Payload) moves the site's root layout and pages into `src/app/(site)/`, then lays
/// `addons/content/` over the result: its files mirror the app's tree, and its `package.json`,
/// `pnpm-lock.yaml` and `pnpm-workspace.yaml` are whole copies of the base ones plus Payload. It
/// ships its first migration, so nothing runs here. Turning content on later by editing
/// `services` in app.yaml is not built yet.
fn app_files(content: bool) -> Vec<(String, &'static [u8])> {
    let base = starter::FILES
        .iter()
        .filter(|(p, _)| !p.starts_with("addons/"));
    let base = base.map(|(p, b)| {
        let moved = content
            && SITE_GROUP
                .iter()
                .any(|g| p == g || (g.ends_with('/') && p.starts_with(g)));
        let path = if moved {
            p.replacen("src/app/", "src/app/(site)/", 1)
        } else {
            p.to_string()
        };
        (path, *b)
    });
    let addon = starter::FILES
        .iter()
        .filter(|_| content)
        .filter_map(|(p, b)| Some((p.strip_prefix(CONTENT)?.to_string(), *b)));
    // Later entries win: the addon replaces base files of the same path.
    let mut all: std::collections::BTreeMap<String, &'static [u8]> = base.collect();
    all.extend(addon);
    all.into_iter().collect()
}

/// The app's folder (the git root), its `app.yaml`, and how to call the backend. Writes nothing.
pub fn preflight(json: bool) -> std::result::Result<(PathBuf, AppConfig, api::Auth), i32> {
    let top = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    let Some(root) = top.filter(|r| r.join("app.yaml").is_file()) else {
        return Err(fail(
            json,
            "not_an_app",
            "This folder is not an app: there is no app.yaml at the top of the git repo.",
            "run lightbulb app new <name>, then work inside it",
        ));
    };
    match AppConfig::load(&root) {
        Ok(c) => Ok((root, c, api::Auth::detect())),
        Err(e) => Err(fail(json, "bad_config", &e.to_string(), "fix app.yaml")),
    }
}

/// `GET /{app_id}/status`, and with `errors` also `/errors`.
pub fn status(errors: bool, json: bool) -> Result<i32> {
    let (root, _, auth) = match preflight(json) {
        Ok(p) => p,
        Err(code) => return Ok(code),
    };
    let id = std::fs::read(root.join(".lightbulb/app.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["app_id"].as_str().map(String::from));
    let Some(id) = id else {
        return Ok(fail(
            json,
            "not_up_yet",
            "This app has not been built yet, so it has no status.",
            "run lightbulb app up",
        ));
    };
    let get = |what: &str| api::call(&auth, "GET", &format!("/{id}/{what}"), None);
    let head = match ship::git_ok(&root, &["rev-parse", "HEAD"]) {
        Ok(h) => h,
        Err(e) => return Ok(fail(json, &e.code, &e.message, &e.next)),
    };
    let out = get("status").and_then(|s| {
        // Whether the preview is done, so a builder need not run `up` again to learn it.
        let d = evidence::done_with(&root, &s, &head)?;
        let mut v = if errors {
            json!({"status": ship::status_contract(&s),
                "errors": ship::errors_contract(&get("errors")?)})
        } else {
            ship::status_contract(&s)
        };
        v["done"] = d.done.into();
        v["missing"] = json!(d.missing);
        v["next"] = d.next.into();
        Ok(v)
    });
    match out {
        Ok(v) if json => {
            println!("{v}");
            Ok(0)
        }
        Ok(v) => {
            for l in status_lines(&v, errors) {
                println!("{l}");
            }
            Ok(0)
        }
        Err(e) => Ok(fail(json, &e.code, &e.message, &e.next)),
    }
}

/// `status` for a person: preview / live / done, never a JSON key or enum.
pub(crate) fn status_lines(v: &Value, errors: bool) -> Vec<String> {
    let s = if errors { &v["status"] } else { v };
    let url = |k: &str| s[k].as_str().filter(|u| !u.is_empty());
    let mut out = vec![match s["state"].as_str() {
        Some("awaiting_approval") => {
            "Preview: waiting for the owner to approve the cards in Inbox".to_string()
        }
        Some("provisioning" | "building") => "Preview: building".into(),
        Some("failed") => "Preview: the last build failed".into(),
        _ => format!("Preview: {}", url("preview_url").unwrap_or("none yet")),
    }];
    if let Some(u) = url("live_url") {
        let version = s["live_version"]
            .as_u64()
            .map_or(String::new(), |n| format!(" (version {n})"));
        out.push(format!("Live: {u}{version}"));
    }
    if let Some(u) = url("canvas_url") {
        out.push(format!("Canvas: {u}"));
    }
    if errors {
        for side in ["client", "server"] {
            let word = if side == "client" {
                "browser"
            } else {
                "server"
            };
            for e in v["errors"][side].as_array().into_iter().flatten() {
                out.push(format!(
                    "Error in the {word} at {}: {}",
                    e["at"].as_str().unwrap_or("?"),
                    e["message"].as_str().unwrap_or("")
                ));
            }
        }
    }
    out.extend(ship::done_lines(v));
    out
}

/// The six keys of `app.yaml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    pub name: String,
    /// Stripe catalog names.
    pub services: Vec<String>,
    pub open_to: OpenTo,
    pub budget_usd: u32,
    pub kind: Kind,
    pub origin: Origin,
}

fn word<T: ValueEnum>(v: &T) -> String {
    v.to_possible_value().unwrap().get_name().to_string()
}

impl AppConfig {
    /// `dir/app.yaml`.
    pub fn load(dir: &Path) -> Result<AppConfig> {
        let text = std::fs::read_to_string(dir.join("app.yaml"))
            .map_err(|e| format!("Could not read app.yaml: {e}"))?;
        let mut m = std::collections::HashMap::new();
        for line in text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        {
            let (k, v) = line
                .split_once(':')
                .ok_or(format!("app.yaml: not a key: {line}"))?;
            let known = [
                "name",
                "services",
                "open_to",
                "budget_usd",
                "kind",
                "origin",
            ];
            if !known.contains(&k) {
                return Err(format!("app.yaml: unknown key {k}").into());
            }
            if m.insert(k, v.trim()).is_some() {
                return Err(format!("app.yaml: {k} twice").into());
            }
        }
        let get = |k: &str| m.get(k).copied().ok_or(format!("app.yaml: {k} is missing"));
        let name = get("name")?.to_string();
        if !valid_name(&name) {
            return Err(format!("app.yaml: {name} is not a valid app name").into());
        }
        let services: Vec<String> = serde_json::from_str(get("services")?)
            .map_err(|_| "app.yaml: services is a list of names, like [\"a\", \"b\"]")?;
        let origin: Value = serde_json::from_str(get("origin")?)
            .map_err(|_| "app.yaml: origin is like {\"kind\": \"idea\"}")?;
        Ok(AppConfig {
            name,
            services,
            open_to: OpenTo::from_str(get("open_to")?, false)
                .map_err(|_| "app.yaml: open_to is room, workspace, invited or public")?,
            budget_usd: get("budget_usd")?
                .parse()
                .map_err(|_| "app.yaml: budget_usd is a whole number of dollars")?,
            kind: Kind::from_str(get("kind")?, false).map_err(|_| "app.yaml: unknown kind")?,
            origin: Origin {
                kind: origin["kind"]
                    .as_str()
                    .ok_or("app.yaml: origin needs a kind")?
                    .into(),
                r#ref: origin["ref"].as_str().map(String::from),
            },
        })
    }

    /// `dir/app.yaml`, in the format `load` reads (lists and maps are JSON flow style).
    pub fn write(&self, dir: &Path) -> Result<()> {
        let mut origin = json!({"kind": self.origin.kind});
        if let Some(r) = &self.origin.r#ref {
            origin["ref"] = r.as_str().into();
        }
        let text = format!(
            "name: {}\nservices: {}\nopen_to: {}\nbudget_usd: {}\nkind: {}\norigin: {origin}\n",
            self.name,
            json!(self.services),
            word(&self.open_to),
            self.budget_usd,
            word(&self.kind),
        );
        std::fs::write(dir.join("app.yaml"), text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_for_a_person_has_no_empty_next() {
        assert_eq!(failure_lines("It broke.", "", &[]), ["It broke."]);
        assert_eq!(
            failure_lines("Stopped.", "run again", &["a.sql: DROP TABLE t".into()]),
            ["Stopped.", "  a.sql: DROP TABLE t", "Next: run again"]
        );
    }

    #[test]
    fn status_in_words() {
        let v = json!({"status": {"state": "ready", "preview_url": "https://p", "live_url": "https://l",
            "live_version": 2, "canvas_url": "https://c"},
            "errors": {"client": [{"at": "t1", "message": "boom"}], "server": []},
            "done": true, "missing": [], "next": "tell the room"});
        assert_eq!(
            status_lines(&v, true),
            [
                "Preview: https://p",
                "Live: https://l (version 2)",
                "Canvas: https://c",
                "Error in the browser at t1: boom",
                "Done: yes",
                "Next: tell the room",
            ]
        );
        let building =
            json!({"state": "building", "done": false, "missing": ["ready"], "next": ""});
        assert_eq!(
            status_lines(&building, false),
            [
                "Preview: building",
                "Done: no",
                "Missing: a finished preview"
            ]
        );
    }

    #[test]
    fn a_name_ending_in_preview_is_not_valid() {
        assert!(valid_name("team-lunch"));
        assert!(!valid_name("team-lunch-preview"));
    }
}
