//! The Grok driver: Grok CLI's ACP mode (`grok agent --no-leader stdio`, off any shared leader)
//! over the child's stdio, spoken by `Acp`.

use super::acp::Acp;
use super::driver::{find, uncommented};
use crate::store::Store;
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The start's failure when no Grok CLI is found (`program`).
pub const NOT_INSTALLED: &str = "Grok CLI is not installed where the daemon can find it";
/// The start's failure when a config Grok CLI reads skips approvals (`bypass_mode`).
pub const BYPASS_MODE: &str = "this computer's Grok CLI runs without approvals; a worker needs \
     prompts to reach a person (permission-mode such as \"bypassPermissions\", \"dontAsk\", \
     \"auto\" or \"yolo\", at the top or under [ui], or a rule allowing every command or edit \
     under [permission], in ~/.grok/config.toml; defaultMode \"bypassPermissions\", \
     \"acceptEdits\", \"auto\" or \"dontAsk\", or an allow rule such as \"Bash\" or \"Edit(**)\", \
     in ~/.grok/settings.json or ~/.claude/settings.json, which Grok reads too; or one of \
     them it cannot read)";
/// The start's failure when the worker's folder has a Grok config of its own that allows
/// (`folder_bypass`).
pub const FOLDER_CONFIG: &str = "this worker's folder has a Grok CLI config that could run \
     without approvals; a worker needs prompts to reach a person (a skipping permission mode \
     or any allow rule in .grok/config.toml, .grok/settings.json, .claude/settings.json or \
     .claude/settings.local.json; or one of them it cannot read)";

/// Exactly what follows the program: ACP over stdio, never joining a shared leader.
pub const ARGS: &[&str] = &["agent", "--no-leader", "stdio"];

/// Where installers put Grok CLI outside a home.
const SYSTEM: [&str; 2] = ["/opt/homebrew/bin/grok", "/usr/local/bin/grok"];

/// `permission-mode` values that skip the prompts.
const BYPASS: [&str; 6] = [
    "bypassPermissions",
    "dontAsk",
    "auto",
    "always-approve",
    "always_approve",
    "yolo",
];

/// The config under a home or a folder.
const CONFIG: &str = ".grok/config.toml";
/// The Claude-style settings Grok reads too, under a home.
const SETTINGS: [&str; 2] = [".grok/settings.json", ".claude/settings.json"];
/// The same in a folder, with its local ones.
const FOLDER_SETTINGS: [&str; 3] = [
    ".grok/settings.json",
    ".claude/settings.json",
    ".claude/settings.local.json",
];
/// `defaultMode` values that skip the prompts.
const MODES: [&str; 4] = ["bypassPermissions", "acceptEdits", "auto", "dontAsk"];
/// Allow rules that allow every command or edit without a prompt.
const WILDCARDS: [&str; 7] = [
    "*",
    "Bash",
    "Bash(*)",
    "Edit",
    "Edit(**)",
    "Write",
    "Write(**)",
];
/// Variables that would hand Grok a config other than the one checked: removed from the
/// child's environment.
const CONFIG_ENV: [&str; 2] = ["GROK_CONFIG", "GROK_CONFIG_PATH"];

/// The Grok CLI a worker runs here, unless none is found (`NOT_INSTALLED`) or the person's own
/// runs without approvals (`BYPASS_MODE`). `home` holds `.grok/`.
pub fn here(knob: Option<&Path>, home: &Path, store: &Store) -> Result<Acp, &'static str> {
    let path = std::env::var_os("PATH");
    let system = SYSTEM.map(Path::new);
    let Some(program) = program(knob, home, &system, store, path.as_deref()) else {
        return Err(NOT_INSTALLED);
    };
    if bypass_mode(home) {
        return Err(BYPASS_MODE);
    }
    Ok(Acp::new(program, "grok", ARGS)
        .checking(folder_bypass)
        .with_env(CONFIG_ENV.map(|k| (k, None)).to_vec()))
}

/// The Grok CLI to run, the first executable file of: the debug knob (when set, the only one);
/// `system`; `~/.local/bin/grok`; the newest registered Grok session's executable named `grok`
/// or `grok-native`; `grok` on `path`.
pub fn program(
    knob: Option<&Path>,
    home: &Path,
    system: &[&Path],
    store: &Store,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    let known = system
        .iter()
        .map(|p| p.to_path_buf())
        .chain([home.join(".local/bin/grok")])
        .collect();
    find(knob, known, store, "grok", &["grok", "grok-native"], path)
}

/// Whether a config `home`'s Grok reads skips the prompts: its `config.toml` (`bypass`), or a
/// settings file (`settings_bypass`), Grok's own or Claude Code's (Grok reads it too).
pub fn bypass_mode(home: &Path) -> bool {
    bypass(&home.join(CONFIG), false)
        || SETTINGS
            .iter()
            .any(|f| settings_bypass(&home.join(f), false))
}

/// Why a worker may not run in `folder`: a `.grok/config.toml` or settings file of its own
/// that skips the prompts or has any allow rule.
pub fn folder_bypass(folder: &Path) -> Option<String> {
    let found = if bypass(&folder.join(CONFIG), true) {
        folder.join(CONFIG)
    } else {
        let settings = FOLDER_SETTINGS.iter().map(|f| folder.join(f));
        settings.into_iter().find(|p| settings_bypass(p, true))?
    };
    Some(format!("{FOLDER_CONFIG}: {}", found.display()))
}

/// Whether allow rule `rule` (`None`: not a string) allows: any rule when `any`, else a
/// `WILDCARDS` one.
fn allowing(rule: Option<&str>, any: bool) -> bool {
    any || rule.is_none_or(|r| WILDCARDS.contains(&r.trim()))
}

/// Whether the Claude-style settings at `path` skip the prompts: `defaultMode`, at the top or
/// under `permissions`, in `MODES`, or an allow rule in `permissions.allow` (`allowing`). Only
/// a missing file is clean: one that cannot be read or parsed refuses.
fn settings_bypass(path: &Path, any: bool) -> bool {
    let text = match std::fs::read_to_string(path) {
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
        Ok(t) => t,
    };
    let Ok(d) = serde_json::from_str::<Value>(&uncommented(&text)) else {
        return true;
    };
    let mode = [&d["defaultMode"], &d["permissions"]["defaultMode"]]
        .iter()
        .any(|m| m.as_str().is_some_and(|m| MODES.contains(&m.trim())));
    let allow = match &d["permissions"]["allow"] {
        Value::Null => false,
        Value::Array(rules) => rules.iter().any(|r| allowing(r.as_str(), any)),
        _ => true,
    };
    mode || allow
}

/// Whether the Grok config at `path` skips the prompts: a `permission-mode` (or
/// `permission_mode`), at the top level or under `[ui]`, in `BYPASS`; a rule in
/// `[permission] allow` (`allowing`); or a `[permission] rules` entry whose `action` is
/// `allow` with no `pattern` or `*` (any, when `any`). Only a missing file is clean: one that
/// cannot be read or parsed refuses, as does a `rules` or `allow` of another shape.
fn bypass(path: &Path, any: bool) -> bool {
    let text = match std::fs::read_to_string(path) {
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
        Ok(t) => t,
    };
    let Ok(d) = text.parse::<toml_edit::DocumentMut>() else {
        return true;
    };
    let tables = [Some(d.as_item()), d.get("ui")];
    let mode = tables.into_iter().flatten().any(|t| {
        ["permission-mode", "permission_mode"]
            .iter()
            .filter_map(|k| t.get(k).and_then(|v| v.as_str()))
            .any(|m| BYPASS.contains(&m.trim()))
    });
    let permission = d.get("permission");
    let allow = match permission.and_then(|p| p.get("allow")) {
        None => false,
        Some(a) => a
            .as_array()
            .is_none_or(|a| a.iter().any(|r| allowing(r.as_str(), any))),
    };
    // Each rule's (action, pattern); a rule of another shape counts as allowing everything.
    let rule = |action: Option<&str>, pattern: Option<&str>| {
        action.is_none_or(|a| a.trim() == "allow")
            && (any || pattern.is_none_or(|p| ["", "*"].contains(&p.trim())))
    };
    let rules = match permission.and_then(|p| p.get("rules")) {
        None => false,
        Some(r) => match (r.as_array_of_tables(), r.as_array()) {
            (Some(t), _) => t.iter().any(|t| {
                let get = |k| t.get(k).and_then(|v| v.as_str());
                rule(get("action"), get("pattern"))
            }),
            (None, Some(a)) => a.iter().any(|v| match v.as_inline_table() {
                Some(t) => {
                    let get = |k| t.get(k).and_then(|v| v.as_str());
                    rule(get("action"), get("pattern"))
                }
                None => true,
            }),
            (None, None) => true,
        },
    };
    mode || allow || rules
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workers::driver::tests::{exe, session};
    use crate::workers::Driver;

    #[test]
    fn the_argv_is_exactly_agent_no_leader_stdio() {
        let d = Acp::new("/x/grok".into(), "grok", ARGS);
        assert_eq!(
            d.argv(Path::new("/w/r1"), "s1"),
            ["/x/grok", "agent", "--no-leader", "stdio"]
        );
    }

    #[test]
    fn the_program_is_found_without_a_login_path() {
        let d = tempfile::tempdir().unwrap();
        let (home, sys, bin) = (
            d.path().join("home"),
            d.path().join("sys"),
            d.path().join("bin"),
        );
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let system_exe = sys.join("grok");
        let system = [system_exe.as_path()];
        let path = std::env::join_paths([&bin]).unwrap();
        let find = || program(None, &home, &system, &s, Some(&path));
        assert_eq!(find(), None);
        let on_path = exe(&bin.join("grok"));
        assert_eq!(find(), Some(on_path));
        // Before it: the newest registered Grok session's, named `grok` or `grok-native`.
        let old = exe(&d.path().join("old/grok"));
        let newest = exe(&d.path().join("new/grok-native"));
        let other = exe(&d.path().join("grok-helper"));
        session(&s, "grok", "m/grok:a", &old, 1);
        session(&s, "grok", "m/grok:b", &newest, 2);
        session(&s, "grok", "m/grok:c", &other, 3);
        assert_eq!(find(), Some(newest));
        // Before those: `~/.local/bin`, then the system installs.
        let local = exe(&home.join(".local/bin/grok"));
        assert_eq!(find(), Some(local));
        let installed = exe(&system_exe);
        assert_eq!(find(), Some(installed));
        // The debug knob, when set, is the only one.
        let fake = exe(&d.path().join("fake"));
        assert_eq!(
            program(Some(&fake), &home, &system, &s, Some(&path)),
            Some(fake)
        );
        let none = d.path().join("nope");
        assert_eq!(program(Some(&none), &home, &system, &s, Some(&path)), None);
    }

    #[test]
    fn a_grok_without_approvals_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        assert!(here(Some(&fake), d.path(), &s).is_ok());
        let config = d.path().join(CONFIG);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        let mut rows = vec![
            (
                "[permission]\nallow = [\"Bash(ls)\", \"*\"]\n".to_string(),
                false,
            ),
            ("permission-mode = \"default\"\n".into(), true),
            ("[ui]\npermission_mode = \"default\"\n".into(), true),
            (
                "[permission]\nallow = [\"MCPTool(lightbulb__*)\"]\n".into(),
                true,
            ),
            // Another table's mode is not Grok's.
            (
                "[profile.x]\npermission-mode = \"bypassPermissions\"\n".into(),
                true,
            ),
            // A file that does not parse refuses.
            (
                "permission-mode = \"bypassPermissions\n[permission".into(),
                false,
            ),
            ("garbage".into(), false),
            // Compact rules that allow every command or edit.
            (
                "[permission]\nallow = [\"Bash(git status)\"]\n".into(),
                true,
            ),
            ("[permission]\nallow = \"Bash\"\n".into(), false),
            // Rules: an allow with no pattern, or `*`.
            (
                "[[permission.rules]]\ntool = \"Bash\"\naction = \"allow\"\n".into(),
                false,
            ),
            (
                "[[permission.rules]]\naction = \"allow\"\npattern = \" * \"\n".into(),
                false,
            ),
            (
                "[[permission.rules]]\naction = \"allow\"\npattern = \"git status\"\n".into(),
                true,
            ),
            ("[[permission.rules]]\naction = \"deny\"\n".into(), true),
            (
                "[permission]\nrules = [{action = \"allow\"}]\n".into(),
                false,
            ),
            (
                "[permission]\nrules = [{action = \"ask\", pattern = \"*\"}]\n".into(),
                true,
            ),
            ("[permission]\nrules = \"allow\"\n".into(), false),
            ("[permission]\nrules = [\"allow\"]\n".into(), false),
        ];
        for rule in ["*", "Bash", "Bash(*)", "Edit", "Edit(**)"] {
            rows.push((format!("[permission]\nallow = [\"{rule}\"]\n"), false));
        }
        for mode in BYPASS {
            for key in ["permission-mode", "permission_mode"] {
                rows.push((format!("{key} = \"{mode}\"\n"), false));
                rows.push((format!("[ui]\n{key} = \"{mode}\"\n"), false));
            }
        }
        for (text, ok) in rows {
            std::fs::write(&config, &text).unwrap();
            let r = here(Some(&fake), d.path(), &s);
            assert_eq!(r.err(), (!ok).then_some(BYPASS_MODE), "{text}");
        }
        // Nor one that cannot be read.
        std::fs::write(&config, b"a = \"\xff\"\n").unwrap();
        assert_eq!(here(Some(&fake), d.path(), &s).err(), Some(BYPASS_MODE));
        std::fs::remove_file(&config).unwrap();
        std::fs::create_dir(&config).unwrap();
        assert_eq!(here(Some(&fake), d.path(), &s).err(), Some(BYPASS_MODE));
        std::fs::remove_dir(&config).unwrap();
        let none = d.path().join("nope");
        assert_eq!(here(Some(&none), d.path(), &s).err(), Some(NOT_INSTALLED));
    }

    /// Settings rows: (text, whether a worker may start).
    fn settings_rows() -> Vec<(String, bool)> {
        let mut rows: Vec<(String, bool)> = vec![
            (
                r#"{"permissions": {"defaultMode": "default"}}"#.into(),
                true,
            ),
            (r#"{"defaultMode": "plan"}"#.into(), true),
            (r#"{"permissions": {"deny": ["Bash"]}}"#.into(), true),
            (r#"{"permissions": {"allow": "Bash"}}"#.into(), false),
            ("garbage".into(), false),
            ("{\"permissions\": {".into(), false),
        ];
        for mode in MODES {
            rows.push((format!(r#"{{"defaultMode": "{mode}"}}"#), false));
            rows.push((
                format!(r#"{{"permissions": {{"defaultMode": "{mode}"}}}}"#),
                false,
            ));
        }
        for rule in WILDCARDS {
            let text = format!(r#"{{"permissions": {{"allow": ["Read", "{rule}"]}}}}"#);
            rows.push((text, false));
        }
        rows
    }

    #[test]
    fn settings_grok_reads_that_skip_approvals_refuse_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        for file in SETTINGS {
            let path = d.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut rows = settings_rows();
            // A person's own narrow rules are theirs.
            let narrow = r#"{"permissions": {"allow": ["Bash(git status)", "mcp__lightbulb"]}}"#;
            rows.push((narrow.into(), true));
            for (text, ok) in rows {
                std::fs::write(&path, &text).unwrap();
                let r = here(Some(&fake), d.path(), &s);
                assert_eq!(r.err(), (!ok).then_some(BYPASS_MODE), "{file}: {text}");
            }
            std::fs::write(&path, b"{\"a\": \"\xff\"}").unwrap();
            assert_eq!(here(Some(&fake), d.path(), &s).err(), Some(BYPASS_MODE));
            std::fs::remove_file(&path).unwrap();
        }
        assert!(here(Some(&fake), d.path(), &s).is_ok());
    }

    #[test]
    fn a_config_named_in_the_environment_is_not_the_childs() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        let g = here(Some(&fake), d.path(), &s).unwrap();
        assert_eq!(g.env(), [("GROK_CONFIG", None), ("GROK_CONFIG_PATH", None)]);
    }

    #[test]
    fn a_folder_that_skips_approvals_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path();
        assert_eq!(folder_bypass(folder), None);
        let config = folder.join(CONFIG);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        for (text, ok) in [
            ("[ui]\npermission-mode = \"default\"\n", true),
            ("[[permission.rules]]\naction = \"deny\"\n", true),
            ("[ui]\npermission-mode = \"yolo\"\n", false),
            // Any allow rule at all, in a folder.
            ("[permission]\nallow = [\"Bash(ls)\"]\n", false),
            (
                "[[permission.rules]]\naction = \"allow\"\npattern = \"git status\"\n",
                false,
            ),
            ("garbage", false),
        ] {
            std::fs::write(&config, text).unwrap();
            let why = folder_bypass(folder);
            assert_eq!(why.is_none(), ok, "{text}");
            if let Some(why) = why {
                assert_eq!(why, format!("{FOLDER_CONFIG}: {}", config.display()));
            }
        }
        let a = Acp::new("/x/grok".into(), "grok", ARGS).checking(folder_bypass);
        assert!(a.refuses(folder).is_some());
        std::fs::remove_file(&config).unwrap();
        for file in FOLDER_SETTINGS {
            let path = folder.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut rows = settings_rows();
            // Any allow rule at all, in a folder.
            rows.push((r#"{"permissions": {"allow": ["Bash(ls)"]}}"#.into(), false));
            for (text, ok) in rows {
                std::fs::write(&path, &text).unwrap();
                let why = folder_bypass(folder);
                assert_eq!(why.is_none(), ok, "{file}: {text}");
                if let Some(why) = why {
                    assert_eq!(why, format!("{FOLDER_CONFIG}: {}", path.display()));
                }
            }
            std::fs::remove_file(&path).unwrap();
        }
        assert_eq!(folder_bypass(folder), None);
    }
}
