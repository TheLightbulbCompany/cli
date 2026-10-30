//! `lightbulb app`: build, check, preview, publish and undo the apps you build (not the hosted
//! assistants, which are `agents`). Every command is one call to the builder routes (`api`).
//! `app.yaml` is the app's settings; its parser is hand-rolled for exactly six keys.

pub mod api;

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
    Up,
    /// Put the preview live.
    Publish {
        #[arg(long)]
        reason: Option<String>,
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
    /// Type check and build, with errors parsed.
    Check {
        /// Also check what publishing needs.
        #[arg(long)]
        publish: bool,
    },
    /// Record a browser pass for the preview.
    Evidence {
        #[arg(long, value_enum)]
        kind: EvidenceKind,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long, default_value = "")]
        note: String,
    },
}

/// Prints a failure and returns the exit code 1: `{"code","message","next"}` on stdout in
/// JSON mode, else a sentence on stderr.
pub fn fail(json: bool, code: &str, message: &str, next: &str) -> i32 {
    if json {
        println!(
            "{}",
            json!({"code": code, "message": message, "next": next})
        );
    } else {
        eprintln!("{message} ({next})");
    }
    1
}

/// A DNS label: `^[a-z][a-z0-9-]{1,38}[a-z0-9]$`.
pub fn valid_name(n: &str) -> bool {
    let b = n.as_bytes();
    (3..=40).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[b.len() - 1] != b'-'
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

pub fn run(cmd: AppCommand, json: bool) -> Result<i32> {
    if let AppCommand::New { name, .. } = &cmd {
        if !valid_name(name) {
            return Ok(fail(
                json,
                "bad_name",
                "An app's name is 3 to 40 lowercase letters, digits and dashes, starting with a letter.",
                "pick another name",
            ));
        }
    }
    Ok(fail(
        json,
        "not_implemented",
        "That app command is not built yet.",
        "update lightbulb",
    ))
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
