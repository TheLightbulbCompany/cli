//! The Cursor driver: Cursor Agent's ACP server (`cursor-agent acp`) over the child's stdio,
//! spoken by `Acp`.

use super::acp::Acp;
use super::driver::find;
use crate::store::Store;
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The start's failure when no Cursor Agent is found (`program`).
pub const NOT_INSTALLED: &str = "Cursor Agent is not installed where the daemon can find it";
/// The start's failure when the person's Cursor Agent allows everything (`bypass_mode`).
pub const BYPASS_MODE: &str = "this computer's Cursor Agent runs without approvals; a worker \
     needs prompts to reach a person (a wildcard such as \"Shell(*)\" in permissions.allow of \
     ~/.cursor/cli-config.json)";

/// Exactly what follows the program.
pub const ARGS: &[&str] = &["acp"];

/// Where installers put Cursor Agent outside a home.
const SYSTEM: [&str; 2] = [
    "/opt/homebrew/bin/cursor-agent",
    "/usr/local/bin/cursor-agent",
];

/// `permissions.allow` rules that allow every command or write without a prompt.
const WILDCARDS: [&str; 4] = ["*", "Shell(*)", "Shell(**)", "Write(*)"];

/// The Cursor Agent a worker runs here, unless none is found (`NOT_INSTALLED`) or the person's
/// own allows everything (`BYPASS_MODE`). `home` holds `.cursor/`.
pub fn here(knob: Option<&Path>, home: &Path, store: &Store) -> Result<Acp, &'static str> {
    let path = std::env::var_os("PATH");
    let system = SYSTEM.map(Path::new);
    let Some(program) = program(knob, home, &system, store, path.as_deref()) else {
        return Err(NOT_INSTALLED);
    };
    if bypass_mode(home) {
        return Err(BYPASS_MODE);
    }
    Ok(Acp::new(program, "cursor", ARGS))
}

/// The Cursor Agent to run, the first executable file of: the debug knob (when set, the only
/// one); `~/.local/bin/cursor-agent`; `system`; the newest registered Cursor session's
/// executable named `cursor-agent` or `agent`; `cursor-agent` on `path`.
pub fn program(
    knob: Option<&Path>,
    home: &Path,
    system: &[&Path],
    store: &Store,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    let known = [home.join(".local/bin/cursor-agent")]
        .into_iter()
        .chain(system.iter().map(|p| p.to_path_buf()))
        .collect();
    find(
        knob,
        known,
        store,
        "cursor",
        &["cursor-agent", "agent"],
        path,
    )
}

/// Whether `home`'s Cursor CLI config allows a wildcard rule (`WILDCARDS`) in
/// `permissions.allow`: a worker inherits that. A file that does not parse is not.
pub fn bypass_mode(home: &Path) -> bool {
    let doc = std::fs::read_to_string(home.join(".cursor/cli-config.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    doc.is_some_and(|d| {
        d["permissions"]["allow"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r.as_str().is_some_and(|r| WILDCARDS.contains(&r.trim())))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workers::driver::tests::{exe, session};
    use crate::workers::Driver;

    #[test]
    fn the_argv_is_exactly_acp() {
        let d = Acp::new("/x/cursor-agent".into(), "cursor", ARGS);
        assert_eq!(d.argv(Path::new("/w/r1"), "s1"), ["/x/cursor-agent", "acp"]);
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
        let system_exe = sys.join("cursor-agent");
        let system = [system_exe.as_path()];
        let path = std::env::join_paths([&bin]).unwrap();
        let find = || program(None, &home, &system, &s, Some(&path));
        assert_eq!(find(), None);
        // `agent` on PATH is too generic a name; `cursor-agent` is it.
        exe(&bin.join("agent"));
        assert_eq!(find(), None);
        let on_path = exe(&bin.join("cursor-agent"));
        assert_eq!(find(), Some(on_path));
        // Before it: the newest registered Cursor session's, named `agent` or `cursor-agent`.
        let old = exe(&d.path().join("old/cursor-agent"));
        let newest = exe(&d.path().join("new/agent"));
        let other = exe(&d.path().join("cursor-helper"));
        session(&s, "cursor", "m/cursor:a", &old, 1);
        session(&s, "cursor", "m/cursor:b", &newest, 2);
        session(&s, "cursor", "m/cursor:c", &other, 3);
        assert_eq!(find(), Some(newest));
        // Before those: the system installs, then `~/.local/bin`.
        let installed = exe(&system_exe);
        assert_eq!(find(), Some(installed));
        let local = exe(&home.join(".local/bin/cursor-agent"));
        assert_eq!(find(), Some(local));
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
    fn a_cursor_that_allows_everything_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        assert!(here(Some(&fake), d.path(), &s).is_ok());
        let config = d.path().join(".cursor/cli-config.json");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        for (text, ok) in [
            (r#"{"permissions": {"allow": ["*"], "deny": []}}"#, false),
            (
                r#"{"permissions": {"allow": ["Shell(ls)", "Shell(*)"]}}"#,
                false,
            ),
            (r#"{"permissions": {"allow": ["Shell(**)"]}}"#, false),
            (r#"{"permissions": {"allow": ["Write(*)"]}}"#, false),
            (
                r#"{"permissions": {"allow": ["Shell(ls)", "Mcp(lightbulb:*)"]}}"#,
                true,
            ),
            (r#"{"permissions": {"allow": ["Read(*)"]}}"#, true),
            (r#"{"permissions": {"deny": ["*"]}}"#, true),
            // A malformed file is not bypass.
            (r#"{"permissions": {"allow": ["*"]"#, true),
        ] {
            std::fs::write(&config, text).unwrap();
            let r = here(Some(&fake), d.path(), &s);
            assert_eq!(r.err(), (!ok).then_some(BYPASS_MODE), "{text}");
        }
        let none = d.path().join("nope");
        assert_eq!(here(Some(&none), d.path(), &s).err(), Some(NOT_INSTALLED));
    }
}
