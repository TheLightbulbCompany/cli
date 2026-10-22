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
    let mut chain = vec![];
    let mut pid = std::process::id();
    loop {
        let p = os::parent(pid).map_err(|e| CallerError::Lookup(e.to_string()))?;
        let done = !skipped(&p.executable) || p.pid <= 1;
        pid = p.pid;
        chain.push(p);
        if done {
            break;
        }
    }
    identify_in(agent, &chain)
}

/// `identify` over a given ancestor list, nearest first.
pub fn identify_in(agent: &str, ancestors: &[Process]) -> Result<Process, CallerError> {
    ancestors
        .iter()
        .find(|p| !skipped(&p.executable))
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
    fn identify_accepts_cursor_node_binary_path() {
        let chain = [p(30, "/Users/u/.local/share/cursor-agent/versions/x/node")];
        assert_eq!(identify_in("cursor", &chain), Ok(chain[0].clone()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn identify_walks_the_real_tree() {
        // The test runner is no agent.
        assert_eq!(identify("grok"), Err(CallerError::NotThatAgent));
    }
}
