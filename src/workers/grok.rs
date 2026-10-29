//! The Grok driver: Grok CLI's ACP mode (`grok agent --no-leader stdio`, off any shared leader)
//! over the child's stdio, spoken by `Acp`.

use super::acp::Acp;
use super::driver::find;
use crate::store::Store;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The start's failure when no Grok CLI is found (`program`).
pub const NOT_INSTALLED: &str = "Grok CLI is not installed where the daemon can find it";
/// The start's failure when a config Grok CLI reads skips approvals (`bypass_mode`).
pub const BYPASS_MODE: &str = "this computer's Grok CLI runs without approvals; a worker needs \
     prompts to reach a person (permission-mode such as \"bypassPermissions\", \"dontAsk\", \
     \"auto\" or \"yolo\", at the top or under [ui], or allow = [\"*\"] under [permission], \
     in ~/.grok/config.toml; a config.toml it cannot read; or bypassPermissions in \
     ~/.claude/settings.json, which Grok reads too)";

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
    Ok(Acp::new(program, "grok", ARGS).checking(folder_bypass))
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

/// Whether a config `home`'s Grok reads skips the prompts: its `config.toml` (`bypass`), or
/// Claude Code's `settings.json` in bypass mode (Grok reads it too).
pub fn bypass_mode(home: &Path) -> bool {
    bypass(&home.join(CONFIG)) || super::claude::bypass_mode(home)
}

/// Why a worker may not run in `folder`: a `.grok/config.toml` or `.claude/settings.json` of
/// its own that skips the prompts.
pub fn folder_bypass(folder: &Path) -> Option<String> {
    let found = if bypass(&folder.join(CONFIG)) {
        folder.join(CONFIG)
    } else if super::claude::bypass_mode(folder) {
        folder.join(".claude/settings.json")
    } else {
        return None;
    };
    Some(format!("{BYPASS_MODE}: {}", found.display()))
}

/// Whether the Grok config at `path` skips the prompts: a `permission-mode` (or
/// `permission_mode`), at the top level or under `[ui]`, in `BYPASS`, or `"*"` in
/// `[permission] allow`. Only a missing file is clean: one that cannot be read or parsed
/// refuses.
fn bypass(path: &Path) -> bool {
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
    let allow = d
        .get("permission")
        .and_then(|p| p.get("allow"))
        .and_then(|a| a.as_array())
        .is_some_and(|a| a.iter().any(|r| r.as_str().map(str::trim) == Some("*")));
    mode || allow
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
        ];
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
        // Claude Code's settings in bypass mode: Grok reads them too.
        let claude = d.path().join(".claude/settings.json");
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(&claude, r#"{"permissions": {"defaultMode": "default"}}"#).unwrap();
        assert!(here(Some(&fake), d.path(), &s).is_ok());
        std::fs::write(
            &claude,
            r#"{"permissions": {"defaultMode": "bypassPermissions"}}"#,
        )
        .unwrap();
        assert_eq!(here(Some(&fake), d.path(), &s).err(), Some(BYPASS_MODE));
        let none = d.path().join("nope");
        assert_eq!(here(Some(&none), d.path(), &s).err(), Some(NOT_INSTALLED));
    }

    #[test]
    fn a_folder_that_skips_approvals_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let folder = d.path();
        assert_eq!(folder_bypass(folder), None);
        let config = folder.join(CONFIG);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "[ui]\npermission-mode = \"default\"\n").unwrap();
        assert_eq!(folder_bypass(folder), None);
        std::fs::write(&config, "[ui]\npermission-mode = \"yolo\"\n").unwrap();
        let why = folder_bypass(folder).unwrap();
        assert!(why.starts_with(BYPASS_MODE) && why.ends_with(&*config.to_string_lossy()));
        let d = Acp::new("/x/grok".into(), "grok", ARGS).checking(folder_bypass);
        assert_eq!(d.refuses(folder), Some(why));
        std::fs::remove_file(&config).unwrap();
        let claude = folder.join(".claude/settings.json");
        std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
        std::fs::write(
            &claude,
            r#"{"permissions": {"defaultMode": "bypassPermissions"}}"#,
        )
        .unwrap();
        let why = folder_bypass(folder).unwrap();
        assert!(why.ends_with(&*claude.to_string_lossy()), "{why}");
    }
}
