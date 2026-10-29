//! The Cursor driver: Cursor Agent's ACP server (`cursor-agent acp`) over the child's stdio,
//! spoken by `Acp`.

use super::acp::Acp;
use super::driver::{find, uncommented};
use crate::store::Store;
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The start's failure when no Cursor Agent is found (`program`).
pub const NOT_INSTALLED: &str = "Cursor Agent is not installed where the daemon can find it";
/// The start's failure when a Cursor Agent config it reads allows everything (`bypass_mode`).
pub const BYPASS_MODE: &str = "this computer's Cursor Agent runs without approvals; a worker \
     needs prompts to reach a person (approvalMode \"unrestricted\" or \"auto-review\", or a \
     wildcard such as \"Shell(*)\" in permissions.allow, in cli-config.json in its config \
     folder: $CURSOR_CONFIG_DIR, else $XDG_CONFIG_HOME/cursor, else ~/.cursor; or a \
     cli-config.json it cannot read)";
/// The start's failure when the worker's folder has a Cursor config of its own
/// (`folder_bypass`).
pub const FOLDER_CONFIG: &str = "this worker's folder has a Cursor Agent config that could run \
     without approvals; a worker needs prompts to reach a person (approvalMode or any \
     permissions.allow rule in .cursor/cli.json, or a .cursor/hooks.json or .cursor/mcp.json; \
     or a cli.json it cannot read)";

/// Exactly what follows the program.
pub const ARGS: &[&str] = &["acp"];

/// Where installers put Cursor Agent outside a home.
const SYSTEM: [&str; 2] = [
    "/opt/homebrew/bin/cursor-agent",
    "/usr/local/bin/cursor-agent",
];

/// `permissions.allow` rules that allow every command or write without a prompt.
const WILDCARDS: [&str; 6] = [
    "*",
    "Shell(*)",
    "Shell(**)",
    "Shell(*:*)",
    "Write(*)",
    "Write(**)",
];

/// `approvalMode` values that skip the prompts.
const MODES: [&str; 2] = ["unrestricted", "auto-review"];

/// The person's CLI config, in the config folder (`config_dir`).
const CONFIG: &str = "cli-config.json";
/// A folder's own CLI config.
const FOLDER_CLI: &str = ".cursor/cli.json";
/// A folder's own files that run commands or tools beside the prompts: refused when present.
const FOLDER_OTHER: [&str; 2] = [".cursor/hooks.json", ".cursor/mcp.json"];

/// The Cursor Agent a worker runs here, unless none is found (`NOT_INSTALLED`) or the person's
/// own allows everything (`BYPASS_MODE`). `home` holds `.cursor/`; the config folder is read
/// from the daemon's environment (`config_dir`).
pub fn here(knob: Option<&Path>, home: &Path, store: &Store) -> Result<Acp, &'static str> {
    let (dir, xdg) = (
        std::env::var_os("CURSOR_CONFIG_DIR"),
        std::env::var_os("XDG_CONFIG_HOME"),
    );
    here_in(knob, home, store, config_dir(home, dir.as_deref(), xdg.as_deref()))
}

/// `here` with the person's config folder `dir`, which the child is pinned to
/// (`CURSOR_CONFIG_DIR`), so it reads the file checked.
pub fn here_in(
    knob: Option<&Path>,
    home: &Path,
    store: &Store,
    dir: PathBuf,
) -> Result<Acp, &'static str> {
    let path = std::env::var_os("PATH");
    let system = SYSTEM.map(Path::new);
    let Some(program) = program(knob, home, &system, store, path.as_deref()) else {
        return Err(NOT_INSTALLED);
    };
    if bypass(&dir.join(CONFIG)) {
        return Err(BYPASS_MODE);
    }
    let env = vec![("CURSOR_CONFIG_DIR", Some(dir.to_string_lossy().into_owned()))];
    Ok(Acp::new(program, "cursor", ARGS)
        .checking(folder_bypass)
        .with_env(env))
}

/// Cursor Agent's config folder, as it resolves it: `cursor_dir` (`$CURSOR_CONFIG_DIR`), else
/// `xdg/cursor` (`$XDG_CONFIG_HOME`), else `home/.cursor`; a blank value is unset.
pub fn config_dir(home: &Path, cursor_dir: Option<&OsStr>, xdg: Option<&OsStr>) -> PathBuf {
    let set = |v: Option<&OsStr>| v.is_some_and(|v| !v.to_string_lossy().trim().is_empty());
    match (cursor_dir.filter(|_| set(cursor_dir)), xdg.filter(|_| set(xdg))) {

        (Some(d), _) => d.into(),
        (None, Some(x)) => Path::new(x).join("cursor"),
        (None, None) => home.join(".cursor"),
    }
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

/// Why a worker may not run in `folder`: a `.cursor/cli.json` of its own with any allow rule or
/// a skipping `approvalMode` (`read`), or a `.cursor/hooks.json` or `.cursor/mcp.json`.
pub fn folder_bypass(folder: &Path) -> Option<String> {
    let cli = folder.join(FOLDER_CLI);
    let allows = |d: &Value| {
        let rules = &d["permissions"]["allow"];
        bypass_in(d) || !(rules.is_null() || rules.as_array().is_some_and(|r| r.is_empty()))
    };
    let present = |p: &PathBuf| {
        !matches!(p.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    };
    let found = if read(&cli).is_none_or(|d| allows(&d)) {
        cli
    } else {
        let other = FOLDER_OTHER.iter().map(|f| folder.join(f));
        other.into_iter().find(present)?
    };
    Some(format!("{FOLDER_CONFIG}: {}", found.display()))
}

/// The Cursor CLI config at `path`: `null` when missing; `None` when it cannot be read, is not

/// UTF-8, or does not parse (comments allowed).
fn read(path: &Path) -> Option<Value> {
    let text = match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Value::Null),
        Err(_) => return None,
        Ok(t) => t,
    };
    serde_json::from_str::<Value>(&uncommented(&text)).ok()
}

/// Whether the Cursor CLI config at `path` skips the prompts (`bypass_in`); one that cannot be
/// read or parsed does.
fn bypass(path: &Path) -> bool {
    read(path).is_none_or(|d| bypass_in(&d))
}

/// Whether CLI config `d` skips the prompts: `approvalMode` in `MODES`, or a `WILDCARDS` rule
/// in `permissions.allow`.
fn bypass_in(d: &Value) -> bool {
    let mode = d["approvalMode"]
        .as_str()
        .is_some_and(|m| MODES.contains(&m.trim()));
    let allow = d["permissions"]["allow"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| r.as_str().is_some_and(|r| WILDCARDS.contains(&r.trim())));
    mode || allow
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
    fn the_config_folder_resolves_as_cursor_resolves_it() {
        let (home, os) = (Path::new("/h"), |s: &str| Some(OsStr::new(s).to_owned()));
        let dir = |c: Option<std::ffi::OsString>, x: Option<std::ffi::OsString>| {
            config_dir(home, c.as_deref(), x.as_deref())
        };
        assert_eq!(dir(None, None), Path::new("/h/.cursor"));
        assert_eq!(dir(None, os("/x")), Path::new("/x/cursor"));
        assert_eq!(dir(os("/c"), os("/x")), Path::new("/c"));
        // Blank is unset.
        assert_eq!(dir(os(" "), os("")), Path::new("/h/.cursor"));
        assert_eq!(dir(os(""), os("/x")), Path::new("/x/cursor"));
    }

    #[test]
    fn the_config_checked_is_the_one_the_child_reads() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        let (home, other) = (d.path().join("home"), d.path().join("xdg"));
        // An allow-all in ~/.cursor does not matter when the folder is elsewhere, and the
        // other folder's does.
        let write = |dir: &Path, text: &str| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(CONFIG), text).unwrap();
        };
        write(&home.join(".cursor"), r#"{"approvalMode": "unrestricted"}"#);
        write(&other.join("cursor"), r#"{"approvalMode": "allowlist"}"#);
        let x = Some(other.as_os_str());
        let a = here_in(Some(&fake), &home, &s, config_dir(&home, None, x)).unwrap();
        // The child is pinned to the folder checked.
        let pinned = other.join("cursor").to_string_lossy().into_owned();
        assert_eq!(a.env(), [("CURSOR_CONFIG_DIR", Some(pinned))]);
        write(&other.join("cursor"), r#"{"permissions": {"allow": ["Shell(*)"]}}"#);
        let r = here_in(Some(&fake), &home, &s, config_dir(&home, None, x));
        assert_eq!(r.err(), Some(BYPASS_MODE));
        let r = here_in(Some(&fake), &home, &s, config_dir(&home, None, None));
        assert_eq!(r.err(), Some(BYPASS_MODE));
    }

    #[test]
    fn a_cursor_that_allows_everything_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        let dir = d.path().join(".cursor");
        let here = |knob: &Path| here_in(Some(knob), d.path(), &s, dir.clone());
        assert!(here(&fake).is_ok());
        let config = dir.join(CONFIG);
        std::fs::create_dir_all(&dir).unwrap();
        for (text, ok) in [
            (r#"{"permissions": {"allow": ["*"], "deny": []}}"#, false),
            (r#"{"permissions": {"allow": ["Shell(ls)", "Shell(*)"]}}"#, false),
            (r#"{"permissions": {"allow": ["Shell(**)"]}}"#, false),
            (r#"{"permissions": {"allow": ["Shell(*:*)"]}}"#, false),
            (r#"{"permissions": {"allow": ["Write(*)"]}}"#, false),
            (r#"{"permissions": {"allow": ["Write(**)"]}}"#, false),
            (r#"{"approvalMode": "unrestricted"}"#, false),
            (r#"{"approvalMode": "auto-review"}"#, false),
            ("// mine\n{\"permissions\": {/* all */ \"allow\": [\"*\"]}}", false),
            (r#"{"approvalMode": "default", "permissions": {"allow": ["Shell(ls)", "Mcp(lightbulb:*)"]}}"#, true),
            ("{\"docs\": \"https://cursor.com/docs\", // see\n\"permissions\": {\"allow\": [\"Read(*)\"]}}", true),
            (r#"{"permissions": {"deny": ["*"]}}"#, true),
            // A file that does not parse refuses.
            (r#"{"permissions": {"allow": ["Shell(ls)"]"#, false),
            ("garbage", false),
        ] {
            std::fs::write(&config, text).unwrap();
            let r = here(&fake);
            assert_eq!(r.err(), (!ok).then_some(BYPASS_MODE), "{text}");
        }
        // Nor one that is not UTF-8.
        std::fs::write(&config, b"{\"a\": \"\xff\"}").unwrap();
        assert_eq!(here(&fake).err(), Some(BYPASS_MODE));
        let none = d.path().join("nope");
        assert_eq!(here(&none).err(), Some(NOT_INSTALLED));
    }

    #[test]
    fn a_folder_with_its_own_cursor_config_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path();
        assert_eq!(folder_bypass(f), None);
        let cli = f.join(FOLDER_CLI);
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        // The person's own file name is not the folder's: only `cli.json` is read there.
        std::fs::write(f.join(".cursor/cli-config.json"), "garbage").unwrap();
        assert_eq!(folder_bypass(f), None);
        for (text, ok) in [
            (r#"{"permissions": {"deny": ["Shell(rm)"]}}"#, true),
            (r#"{"permissions": {"allow": []}}"#, true),
            ("// none\n{}", true),
            // Any allow rule at all.
            (r#"{"permissions": {"allow": ["Shell(ls)"]}}"#, false),
            (r#"{"permissions": {"allow": ["Mcp(lightbulb:*)"]}}"#, false),
            (r#"{"permissions": {"allow": "Shell(*)"}}"#, false),
            (r#"{"approvalMode": "unrestricted"}"#, false),
            ("garbage", false),
        ] {
            std::fs::write(&cli, text).unwrap();
            let why = folder_bypass(f);
            assert_eq!(why.is_none(), ok, "{text}");
            if let Some(why) = why {
                assert_eq!(why, format!("{FOLDER_CONFIG}: {}", cli.display()));
            }
        }
        std::fs::remove_file(&cli).unwrap();
        // Hooks or MCP servers of its own: present at all.
        for file in FOLDER_OTHER {
            let p = f.join(file);
            std::fs::write(&p, "{}").unwrap();
            let why = format!("{FOLDER_CONFIG}: {}", p.display());
            assert_eq!(folder_bypass(f), Some(why.clone()));
            let a = Acp::new("/x/cursor-agent".into(), "cursor", ARGS).checking(folder_bypass);
            assert_eq!(a.refuses(f), Some(why));
            std::fs::remove_file(&p).unwrap();
        }
        assert_eq!(folder_bypass(f), None);
    }

}
