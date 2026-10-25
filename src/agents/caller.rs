//! Which agent ran this hook. The hook's ancestors are walked past shells to the first
//! real program; that program must be the agent the hook was configured for.

use crate::os::{self, Process};
use std::path::Path;

#[derive(Debug, PartialEq)]
pub enum CallerError {
    /// The first program above the hook is not the named agent.
    NotThatAgent,
    Lookup(String),
}

const SKIPPED: [&str; 6] = ["sh", "bash", "zsh", "fish", "dash", "env"];

fn skipped(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| SKIPPED.contains(&n))
}

/// The agent an executable is, by file name or install path.
pub fn classify(path: &Path) -> Option<&'static str> {
    let name = path.file_name()?.to_str()?;
    let full = path.to_string_lossy();
    match name {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "cursor-agent" => Some("cursor"),
        "grok" | "grok-native" => Some("grok"),
        // The npm install's native binary is `.../opencode-ai/bin/opencode.exe`, on macOS too.
        "opencode" | "opencode.exe" => Some("opencode"),
        // Cursor's CLI runs as `.../cursor-agent/versions/<v>/node`.
        "node" if full.contains("/cursor-agent/") => Some("cursor"),
        // Claude's native binary is named by its version: `.../claude/versions/2.1.307`.
        _ if full.contains("/claude/versions/") => Some("claude"),
        _ if full.contains("/app-server-daemon/") => Some("codex"),
        _ => None,
    }
}

/// The process of `agent` that ran this hook.
pub fn identify(agent: &str) -> Result<Process, CallerError> {
    identify_in(agent, &ancestors()?)
}

/// The agent process above this one, whichever agent it is (the MCP server's caller).
pub fn identify_any() -> Result<Process, CallerError> {
    first_program(&ancestors()?)
        .filter(|p| classify(&p.executable).is_some())
        .cloned()
        .ok_or(CallerError::NotThatAgent)
}

/// Whether the walk up from here ended at a real program that is not an agent, and not at
/// pid 1 (a detached command reparented to launchd shows nothing about who started it) or a
/// failed lookup. Only then is a person at a terminal the likely caller.
// ponytail: this gate stops accidents, not a hostile agent (same-uid ceiling: it can detach,
// rename a wrapper, or edit this binary's environment).
pub fn walked_to_a_person() -> bool {
    for step in ancestors_from(std::process::id()).skip(1) {
        let Ok((p, agent)) = step else { return false };
        if agent || p.pid <= 1 {
            return false;
        }
        if !skipped(&p.executable) {
            return true;
        }
    }
    false
}

/// This process's ancestors, nearest first, up to and including the first non-shell.
fn ancestors() -> Result<Vec<Process>, CallerError> {
    let mut chain = vec![];
    for step in ancestors_from(std::process::id()).skip(1) {
        let (p, _) = step?;
        let done = !skipped(&p.executable);
        chain.push(p);
        if done {
            break;
        }
    }
    Ok(chain)
}

/// `pid`, then each of its ancestors up to pid 1, nearest first, each with whether it is an
/// agent. A failed lookup ends the walk. In debug builds the process `CALLER_ENV` pins is an
/// agent too.
pub fn ancestors_from(pid: u32) -> impl Iterator<Item = Result<(Process, bool), CallerError>> {
    ancestors_pinned(pid, crate::hook::pinned_caller().and_then(Result::ok))
}

/// `ancestors_from`, with `pinned` (pid + birth) the process that counts as an agent.
pub(crate) fn ancestors_pinned(
    pid: u32,
    pinned: Option<Process>,
) -> impl Iterator<Item = Result<(Process, bool), CallerError>> {
    let mut next = Some(os::process(pid));
    std::iter::from_fn(move || {
        let p = match next.take()? {
            Ok(p) => p,
            Err(e) => return Some(Err(CallerError::Lookup(e.to_string()))),
        };
        if p.pid > 1 {
            next = Some(os::parent(p.pid));
        }
        let agent = classify(&p.executable).is_some()
            || pinned
                .as_ref()
                .is_some_and(|a| a.pid == p.pid && a.birth_micros == p.birth_micros);
        Some(Ok((p, agent)))
    })
}

/// Whether `path` is Cursor: its CLI (`classify`) or the editor app and its helpers.
pub fn is_cursor(path: &Path) -> bool {
    classify(path) == Some("cursor") || path.to_string_lossy().contains("/Cursor.app/")
}

/// Whether the program that ran this hook is Cursor, which also runs `~/.claude/settings.json`
/// hooks: the first program above it, or (debug builds) the process `CALLER_ENV` pins.
pub fn fired_by_cursor() -> bool {
    let first = match crate::hook::pinned_caller() {
        Some(pinned) => pinned.ok().and_then(|p| os::process(p.pid).ok()),
        None => ancestors().ok().and_then(|a| first_program(&a).cloned()),
    };
    first.is_some_and(|p| is_cursor(&p.executable))
}

fn first_program(ancestors: &[Process]) -> Option<&Process> {
    ancestors.iter().find(|p| !skipped(&p.executable))
}

/// `identify` over a given ancestor list, nearest first.
pub fn identify_in(agent: &str, ancestors: &[Process]) -> Result<Process, CallerError> {
    first_program(ancestors)
        .filter(|p| classify(&p.executable) == Some(agent))
        .cloned()
        .ok_or(CallerError::NotThatAgent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(pid: u32, exe: &str) -> Process {
        Process {
            pid,
            birth_micros: pid.into(),
            executable: PathBuf::from(exe),
        }
    }

    #[test]
    fn cursor_is_its_cli_or_its_app() {
        for path in [
            "/Users/u/.local/bin/cursor-agent",
            "/Users/u/.local/share/cursor-agent/versions/x/node",
            "/Applications/Cursor.app/Contents/MacOS/Cursor",
            "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper (Plugin).app/Contents/MacOS/Cursor Helper (Plugin)",
        ] {
            assert!(is_cursor(Path::new(path)), "{path}");
        }
        for path in [
            "/Users/u/.local/bin/claude",
            "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
        ] {
            assert!(!is_cursor(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn classify_table() {
        for (path, agent) in [
            ("/opt/homebrew/bin/claude", Some("claude")),
            (
                "/Users/u/.local/share/claude/versions/2.1.307",
                Some("claude"),
            ),
            ("/usr/local/bin/codex", Some("codex")),
            (
                "/Users/u/.codex/app-server-daemon/bin/codex-app-server",
                Some("codex"),
            ),
            ("/Users/u/.local/bin/cursor-agent", Some("cursor")),
            (
                "/Users/u/.local/share/cursor-agent/versions/2026.10.1/node",
                Some("cursor"),
            ),
            ("/usr/local/bin/node", None),
            ("/Users/u/.grok/bin/grok", Some("grok")),
            ("/Users/u/.grok/bin/grok-native", Some("grok")),
            (
                "/opt/homebrew/lib/node_modules/opencode-ai/bin/opencode.exe",
                Some("opencode"),
            ),
            ("/Users/u/.opencode/bin/opencode", Some("opencode")),
            ("/bin/zsh", None),
            ("/usr/bin/python3", None),
            ("/Applications/Cursor.app/Contents/MacOS/Cursor", None),
        ] {
            assert_eq!(classify(Path::new(path)), agent, "{path}");
        }
    }

    #[test]
    fn identify_in_skips_shells_and_stops_at_the_first_program() {
        let chain = [
            p(10, "/bin/sh"),
            p(11, "/usr/bin/env"),
            p(12, "/opt/homebrew/bin/fish"),
            p(13, "/Users/u/.grok/bin/grok"),
            p(14, "/opt/homebrew/bin/claude"),
        ];
        assert_eq!(identify_in("grok", &chain), Ok(chain[3].clone()));
        assert_eq!(
            identify_in("claude", &chain),
            Err(CallerError::NotThatAgent)
        );
        // The first program is not an agent: stop there, never look past it.
        let chain = [p(10, "/bin/bash"), p(11, "/usr/bin/make"), p(12, "/x/grok")];
        assert_eq!(identify_in("grok", &chain), Err(CallerError::NotThatAgent));
        assert_eq!(
            identify_in("grok", &[p(1, "/bin/zsh")]),
            Err(CallerError::NotThatAgent)
        );
    }

    #[test]
    fn identify_refuses_a_hook_run_under_the_wrong_agent() {
        // Run by hand from a shell inside a Claude session, asked for grok.
        let chain = [
            p(20, "/bin/zsh"),
            p(21, "/Users/u/.local/share/claude/versions/2.1.307"),
        ];
        assert_eq!(identify_in("grok", &chain), Err(CallerError::NotThatAgent));
        assert_eq!(identify_in("claude", &chain), Ok(chain[1].clone()));
    }

    #[test]
    fn identify_accepts_opencode_and_refuses_it_for_another_agent() {
        let chain = [
            p(40, "/bin/zsh"),
            p(
                41,
                "/opt/homebrew/lib/node_modules/opencode-ai/bin/opencode.exe",
            ),
        ];
        assert_eq!(identify_in("opencode", &chain), Ok(chain[1].clone()));
        assert_eq!(identify_in("grok", &chain), Err(CallerError::NotThatAgent));
        let claude = [p(50, "/opt/homebrew/bin/claude")];
        assert_eq!(
            identify_in("opencode", &claude),
            Err(CallerError::NotThatAgent)
        );
    }

    #[test]
    fn identify_accepts_cursor_node_binary_path() {
        let chain = [p(30, "/Users/u/.local/share/cursor-agent/versions/x/node")];
        assert_eq!(identify_in("cursor", &chain), Ok(chain[0].clone()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn identify_walks_the_real_tree() {
        // The test runner is no agent.
        assert_eq!(identify("grok"), Err(CallerError::NotThatAgent));
        assert_eq!(identify_any(), Err(CallerError::NotThatAgent));
    }
}
