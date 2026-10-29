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
/// The start's failure when a Cursor Agent config it reads allows everything (`bypass_mode`).
pub const BYPASS_MODE: &str = "this computer's Cursor Agent runs without approvals; a worker \
     needs prompts to reach a person (approvalMode \"unrestricted\" or \"auto-review\", or a \
     wildcard such as \"Shell(*)\" in permissions.allow, in ~/.cursor/cli-config.json; or a \
     cli-config.json it cannot read)";

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

/// The CLI config under a home or a folder.
const CONFIG: &str = ".cursor/cli-config.json";

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
    Ok(Acp::new(program, "cursor", ARGS).checking(folder_bypass))
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

/// Whether `home`'s Cursor CLI config skips the prompts (`bypass`).
pub fn bypass_mode(home: &Path) -> bool {
    bypass(&home.join(CONFIG))
}

/// Why a worker may not run in `folder`: its own `.cursor/cli-config.json` skips the prompts.
pub fn folder_bypass(folder: &Path) -> Option<String> {
    let config = folder.join(CONFIG);
    bypass(&config).then(|| format!("{BYPASS_MODE}: {}", config.display()))
}

/// Whether the Cursor CLI config at `path` skips the prompts: `approvalMode` in `MODES`, or a
/// `WILDCARDS` rule in `permissions.allow`. Only a missing file is clean: one that cannot be
/// read, is not UTF-8, or does not parse (comments allowed) refuses.
fn bypass(path: &Path) -> bool {
    let text = match std::fs::read_to_string(path) {
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
        Ok(t) => t,
    };
    let Ok(d) = serde_json::from_str::<Value>(&uncommented(&text)) else {
        return true;
    };
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

/// `text` without its `//` line and `/* */` block comments outside strings (a URL's `//`
/// stays), each comment replaced by a space.
fn uncommented(text: &str) -> String {
    let (mut out, mut chars) = (String::with_capacity(text.len()), text.chars().peekable());
    let (mut string, mut escaped) = (false, false);
    while let Some(c) = chars.next() {
        if string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                while chars.next_if(|&n| n != '\n').is_some() {}
                out.push(' ');
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
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
    fn comments_go_but_strings_stay() {
        let text = "{\"u\": \"https://x/*y*/\\\"//z\", // a\n/* b\n */ \"v\": 1}";
        let v: Value = serde_json::from_str(&uncommented(text)).unwrap();
        assert_eq!(v["u"], "https://x/*y*/\"//z");
        assert_eq!(v["v"], 1);
    }

    #[test]
    fn a_cursor_that_allows_everything_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        assert!(here(Some(&fake), d.path(), &s).is_ok());
        let config = d.path().join(CONFIG);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
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
            let r = here(Some(&fake), d.path(), &s);
            assert_eq!(r.err(), (!ok).then_some(BYPASS_MODE), "{text}");
        }
        // Nor one that is not UTF-8.
        std::fs::write(&config, b"{\"a\": \"\xff\"}").unwrap();
        assert_eq!(here(Some(&fake), d.path(), &s).err(), Some(BYPASS_MODE));
        let none = d.path().join("nope");
        assert_eq!(here(Some(&none), d.path(), &s).err(), Some(NOT_INSTALLED));
    }

    #[test]
    fn a_folder_that_allows_everything_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(folder_bypass(d.path()), None);
        let config = d.path().join(CONFIG);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, r#"{"permissions": {"allow": ["Shell(ls)"]}}"#).unwrap();
        assert_eq!(folder_bypass(d.path()), None);
        std::fs::write(&config, r#"{"approvalMode": "unrestricted"}"#).unwrap();
        let why = folder_bypass(d.path()).unwrap();
        assert!(why.starts_with(BYPASS_MODE) && why.ends_with(&*config.to_string_lossy()));
        let d = Acp::new("/x/cursor-agent".into(), "cursor", ARGS).checking(folder_bypass);
        assert_eq!(
            d.refuses(config.parent().unwrap().parent().unwrap()),
            Some(why)
        );
    }
}
