//! The Grok driver: Grok CLI's ACP mode (`grok agent --no-leader stdio`, off any shared leader)
//! over the child's stdio, spoken by `Acp`.

use super::acp::Acp;
use super::driver::find;
use crate::store::Store;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The start's failure when no Grok CLI is found (`program`).
pub const NOT_INSTALLED: &str = "Grok CLI is not installed where the daemon can find it";
/// The start's failure when the person's Grok CLI runs without approvals (`bypass_mode`).
pub const BYPASS_MODE: &str = "this computer's Grok CLI runs without approvals; a worker needs \
     prompts to reach a person (permission-mode = \"bypassPermissions\", \"dontAsk\" or \
     \"auto\", or allow = [\"*\"] under [permission], in ~/.grok/config.toml)";

/// Exactly what follows the program: ACP over stdio, never joining a shared leader.
pub const ARGS: &[&str] = &["agent", "--no-leader", "stdio"];

/// Where installers put Grok CLI outside a home.
const SYSTEM: [&str; 2] = ["/opt/homebrew/bin/grok", "/usr/local/bin/grok"];

/// `permission-mode` values that skip the prompts.
const BYPASS: [&str; 3] = ["bypassPermissions", "dontAsk", "auto"];

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
    Ok(Acp::new(program, "grok", ARGS))
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

/// Whether `home`'s Grok config skips the prompts: a top-level `permission-mode` (or
/// `permission_mode`) in `BYPASS`, or `"*"` in `[permission] allow`. A worker inherits that. A
/// file that does not parse is not.
pub fn bypass_mode(home: &Path) -> bool {
    let doc = std::fs::read_to_string(home.join(".grok/config.toml"))
        .ok()
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok());
    doc.is_some_and(|d| {
        let mode = ["permission-mode", "permission_mode"]
            .iter()
            .filter_map(|k| d.get(k).and_then(|v| v.as_str()))
            .any(|m| BYPASS.contains(&m));
        let allow = d
            .get("permission")
            .and_then(|p| p.get("allow"))
            .and_then(|a| a.as_array())
            .is_some_and(|a| a.iter().any(|r| r.as_str().map(str::trim) == Some("*")));
        mode || allow
    })
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
        let config = d.path().join(".grok/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        for (text, ok) in [
            ("permission-mode = \"bypassPermissions\"\n", false),
            ("permission_mode = \"dontAsk\"\n", false),
            ("permission-mode = \"auto\"\n", false),
            ("[permission]\nallow = [\"Bash(ls)\", \"*\"]\n", false),
            ("permission-mode = \"default\"\n", true),
            ("[permission]\nallow = [\"MCPTool(lightbulb__*)\"]\n", true),
            // Only the top level counts for the mode; a malformed file is not bypass.
            (
                "[profile.x]\npermission-mode = \"bypassPermissions\"\n",
                true,
            ),
            ("permission-mode = \"bypassPermissions\n[permission", true),
        ] {
            std::fs::write(&config, text).unwrap();
            let r = here(Some(&fake), d.path(), &s);
            assert_eq!(r.err(), (!ok).then_some(BYPASS_MODE), "{text}");
        }
        let none = d.path().join("nope");
        assert_eq!(here(Some(&none), d.path(), &s).err(), Some(NOT_INSTALLED));
    }
}
