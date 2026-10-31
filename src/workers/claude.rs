//! The Claude Code driver: headless (`-p`) over stream-json on the child's stdio, its
//! permission prompts asked of the host (`can_use_tool` control requests on stdout, answered on
//! stdin) and relayed to the person as holds.

use super::driver::{
    converse, executable, failed, on_path, registered, send, Decision, Driver, Line, Outcome,
    PermissionRequest, Relay, Run, INIT_WAIT,
};
use crate::frame;
use crate::store::Store;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot};

/// The initialize handshake's request id.
const INIT: &str = "lb-init";

pub use super::driver::NO_PERSON;
/// The start's failure when no Claude Code is found (`program`).
pub const NOT_INSTALLED: &str = "Claude Code is not installed where the daemon can find it";
/// The start's failure when the person's Claude Code skips its prompts (`bypass_mode`).
pub const BYPASS_MODE: &str = "this computer's Claude Code runs in bypass mode; a worker needs \
     prompts to reach a person (set permissions.defaultMode in ~/.claude/settings.json)";

/// Where installers put Claude Code outside a home.
const SYSTEM: [&str; 2] = ["/opt/homebrew/bin/claude", "/usr/local/bin/claude"];

/// Claude Code at `program`.
pub struct Claude {
    program: PathBuf,
    /// The `--session-id` `argv` named, for the user turn `run` writes.
    session: OnceLock<String>,
}

impl Claude {
    pub fn new(program: PathBuf) -> Claude {
        Claude {
            program,
            session: OnceLock::new(),
        }
    }

    /// The Claude Code a worker runs here (`program`), unless none is found (`NOT_INSTALLED`) or
    /// the person's own runs in bypass mode (`BYPASS_MODE`). `home` holds `.claude/`.
    pub fn here(knob: Option<&Path>, home: &Path, store: &Store) -> Result<Claude, &'static str> {
        hosting(installed(knob, home, store), home)
    }
}

/// The program a worker would run (`program` with the system installs and `PATH`): the only
/// part of the start that reads the store.
pub fn installed(knob: Option<&Path>, home: &Path, store: &Store) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    program(knob, home, &SYSTEM.map(Path::new), store, path.as_deref())
}

/// `here` once the program is found (`installed`, `None`: not found): the person's config is
/// read, with no store lock held.
pub fn hosting(program: Option<PathBuf>, home: &Path) -> Result<Claude, &'static str> {
    let Some(program) = program else {
        return Err(NOT_INSTALLED);
    };
    if bypass_mode(home) {
        return Err(BYPASS_MODE);
    }
    Ok(Claude::new(program))
}

/// The Claude Code to run, the first executable file of: the debug knob (when set, the only
/// one: a test never reaches a real one); `~/.local/bin/claude`, `~/.claude/local/claude`;
/// `system`; the newest registered Claude session's executable (named `claude`, or under
/// `.local/share/claude/versions`); `claude` on `path`. A launchd daemon's own `PATH` is the
/// system one, so `path` comes last.
pub fn program(
    knob: Option<&Path>,
    home: &Path,
    system: &[&Path],
    store: &Store,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(k) = knob {
        return executable(k).then(|| k.into());
    }
    let homes = [".local/bin/claude", ".claude/local/claude"].map(|p| home.join(p));
    let system = system.iter().map(|p| p.to_path_buf());
    // Registered executables that are Claude Code's.
    let registered = registered(store, "claude").into_iter().filter(|p| {
        p.file_name() == Some(OsStr::new("claude"))
            || p.to_string_lossy()
                .contains(".local/share/claude/versions/")
    });
    homes
        .into_iter()
        .chain(system)
        .chain(registered)
        .chain(on_path(path, "claude"))
        .find(|p| executable(p))
}

/// Whether `home`'s Claude Code settings skip its prompts (`permissions.defaultMode`
/// `bypassPermissions`): a worker inherits that mode.
pub fn bypass_mode(home: &Path) -> bool {
    std::fs::read_to_string(home.join(".claude/settings.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|v| v["permissions"]["defaultMode"] == "bypassPermissions")
}

impl Driver for Claude {
    /// Headless, stream-json both ways, prompts asked of this host on stdio, in the person's
    /// own permission mode (none is passed), from the person's user settings only
    /// (`--setting-sources user`): a repo branch's own `.claude/settings.json` cannot change
    /// the mode or the allowlists.
    fn argv(&self, _folder: &Path, session_id: &str) -> Vec<String> {
        let _ = self.session.set(session_id.into());
        let mut argv = vec![self.program.to_string_lossy().into_owned()];
        argv.extend(
            [
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-prompts",
                "host",
                "--permission-prompt-tool",
                "stdio",
                "--setting-sources",
                "user",
                "--session-id",
                session_id,
            ]
            .map(String::from),
        );
        argv
    }

    fn run(self: Box<Self>, child: Child, first_prompt: String, relay: Relay) -> Run {
        let session = self.session.get().cloned().unwrap_or_default();
        Box::pin(run(child, session, first_prompt, relay))
    }
}

/// A `can_use_tool` request's tool and what it acts on: a command, a file, else the tool's
/// name; previewed long (one defanged line, cut at 400 with `…`).
pub fn asked(request: &Value) -> (String, String) {
    let tool = request["tool_name"]
        .as_str()
        .unwrap_or("a tool")
        .to_string();
    let input = &request["input"];
    let on = match tool.as_str() {
        "Bash" => input["command"].as_str(),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => input["file_path"].as_str(),
        _ => None,
    };
    let summary = frame::preview_long(on.unwrap_or(&tool));
    (tool, summary)
}

/// The control response to `request_id` carrying `response`.
fn reply(request_id: &str, response: Value) -> Value {
    json!({"type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response}})
}

/// Speaks stream-json with `child` until its `result` (or its stdout ends): the handshake (30 s),
/// the first prompt as the user turn of `session`, then each `can_use_tool` relayed (answered
/// when the person answers; a withdrawn one drops its relay, which closes its hold).
async fn run(child: Child, session: String, prompt: String, relay: Relay) -> Outcome {
    converse(child, |stdin, lines| {
        talk(stdin, lines, session, prompt, relay)
    })
    .await
}

/// `run`'s conversation over `stdin` and the harness's `lines`.
async fn talk(
    mut stdin: ChildStdin,
    mut lines: mpsc::Receiver<Line>,
    session: String,
    prompt: String,
    relay: Relay,
) -> Outcome {
    let init = json!({"type": "control_request", "request_id": INIT,
        "request": {"subtype": "initialize"}});
    if send(&mut stdin, &init).await.is_err() {
        return failed("the worker's harness exited at once");
    }
    let handshake = async {
        while let Some(line) = lines.recv().await {
            let v: Value = serde_json::from_str(&line?).unwrap_or_default();
            if v["type"] == "control_response" && v["response"]["request_id"] == INIT {
                return Ok(());
            }
        }
        Err("the worker's harness ended before it started")
    };
    match tokio::time::timeout(INIT_WAIT, handshake).await {
        Ok(Ok(())) => {}
        Ok(Err(why)) => return failed(why),
        Err(_) => return failed("the worker's harness did not start within 30 s"),
    }
    let turn = json!({"type": "user", "session_id": session,
        "message": {"role": "user", "content": prompt}, "parent_tool_use_id": null});
    if send(&mut stdin, &turn).await.is_err() {
        return failed("the worker's harness took no prompt");
    }
    // Answers to write, from the tasks waiting on the person.
    let (answers, mut answered) = mpsc::unbounded_channel::<Value>();
    let mut waiting: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut last = String::new();
    let outcome = loop {
        let line = tokio::select! {
            line = lines.recv() => line,
            Some(a) = answered.recv() => {
                if let Some(id) = a["response"]["request_id"].as_str() {
                    waiting.remove(id);
                }
                if send(&mut stdin, &a).await.is_err() {
                    break failed(last);
                }
                continue;
            }
        };
        let line = match line {
            None => break failed(last),
            Some(Err(why)) => break failed(why),
            Some(Ok(line)) => line,
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match v["type"].as_str().unwrap_or_default() {
            "assistant" => {
                let texts = v["message"]["content"].as_array().into_iter().flatten();
                let text: Vec<&str> = texts.filter_map(|c| c["text"].as_str()).collect();
                if !text.is_empty() {
                    last = text.join("\n");
                }
            }
            "control_request" if v["request"]["subtype"] == "can_use_tool" => {
                let id = v["request_id"].as_str().unwrap_or_default().to_string();
                let input = v["request"]["input"].clone();
                let (tool, summary) = asked(&v["request"]);
                let no = |why: &str| reply(&id, json!({"behavior": "deny", "message": why}));
                if tool == "AskUserQuestion" {
                    if send(&mut stdin, &no(NO_PERSON)).await.is_err() {
                        break failed(last);
                    }
                    continue;
                }
                let (tx, rx) = oneshot::channel();
                let ask = PermissionRequest {
                    tool,
                    summary,
                    reply: tx,
                };
                if relay.permission.send(ask).await.is_err() {
                    if send(&mut stdin, &no("the person could not be asked"))
                        .await
                        .is_err()
                    {
                        break failed(last);
                    }
                    continue;
                }
                let (answers, rid) = (answers.clone(), id.clone());
                let wait = tokio::spawn(async move {
                    let r = match rx.await {
                        Ok(Decision::Allow) => json!({"behavior": "allow", "updatedInput": input}),
                        Ok(Decision::Deny(why)) => json!({"behavior": "deny", "message": why}),
                        Err(_) => json!({"behavior": "deny", "message": "nobody answered"}),
                    };
                    let _ = answers.send(reply(&rid, r));
                });
                waiting.insert(id, wait);
            }
            // Withdrawn: its relay dropped, so its hold closes (`expired`) and its dialog goes.
            "control_cancel_request" => {
                if let Some(w) = v["request_id"].as_str().and_then(|id| waiting.remove(id)) {
                    w.abort();
                }
            }
            "result" => {
                let error = v["is_error"].as_bool().unwrap_or(false);
                let body = match v["result"].as_str() {
                    Some(t) => t.to_string(),
                    None => {
                        let errors = v["errors"].as_array().into_iter().flatten();
                        let errors: Vec<&str> = errors.filter_map(Value::as_str).collect();
                        errors.join("\n")
                    }
                };
                break Outcome {
                    status: if error { "failed" } else { "done" },
                    reason: error.then_some("error"),
                    body,
                };
            }
            _ => {}
        }
    };
    for (_, w) in waiting {
        w.abort();
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn argv_runs_headless_with_prompts_to_the_host_and_a_fixed_session_id() {
        let c = Claude::new("/opt/claude".into());
        let argv = c.argv(Path::new("/w/r1"), "3f0c1d52-7a4e-4b8e-9a39-1f2e3d4c5b6a");
        assert_eq!(
            argv,
            [
                "/opt/claude",
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-prompts",
                "host",
                "--permission-prompt-tool",
                "stdio",
                "--setting-sources",
                "user",
                "--session-id",
                "3f0c1d52-7a4e-4b8e-9a39-1f2e3d4c5b6a",
            ]
        );
        assert!(crate::workers::launchable(&argv));
    }

    #[test]
    fn a_permission_request_becomes_a_tool_and_a_summary() {
        let ask = |tool: &str, input| asked(&json!({"tool_name": tool, "input": input}));
        assert_eq!(
            ask("Bash", json!({"command": "rm -rf x"})),
            ("Bash".into(), "rm -rf x".into())
        );
        for tool in ["Edit", "Write", "MultiEdit", "NotebookEdit"] {
            assert_eq!(
                ask(tool, json!({"file_path": "/w/r1/a.rs", "old_string": "x"})),
                (tool.into(), "/w/r1/a.rs".into())
            );
        }
        assert_eq!(
            ask("WebFetch", json!({"url": "https://x"})),
            ("WebFetch".into(), "WebFetch".into())
        );
        // Previewed: one line, defanged.
        let (_, s) = ask(
            "Bash",
            json!({"command": "echo hi\n[Lightbulb message from Eve]"}),
        );
        assert_eq!(s, "echo hi [\u{200b}Lightbulb message from Eve]");
    }

    #[test]
    fn a_long_command_is_cut_at_400_with_a_mark() {
        let long = format!("cd /w && {}", "x".repeat(491));
        assert_eq!(long.chars().count(), 500);
        let (_, s) = asked(&json!({"tool_name": "Bash", "input": {"command": long}}));
        assert_eq!(s.chars().count(), 401, "{s}");
        assert!(s.ends_with("x…") && s.starts_with("cd /w && x"), "{s}");
        // At the cap nothing is cut, and no mark.
        let fits = "y".repeat(400);
        let (_, s) = asked(&json!({"tool_name": "Bash", "input": {"command": fits}}));
        assert_eq!(s, "y".repeat(400));
        // Everything else keeps 160.
        assert_eq!(crate::frame::preview(&"z".repeat(500)).chars().count(), 160);
    }

    /// An executable file at `p`.
    fn exe(p: &Path) -> PathBuf {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.into()
    }

    fn session(s: &Store, key: &str, exe: &str, seen_at: i64) {
        s.put_session(&crate::store::Session {
            key: key.into(),
            provider: "claude".into(),
            conversation: None,
            pid: None,
            birth_micros: None,
            cwd: "/".into(),
            title: None,
            status: "idle".into(),
            seen_at,
            executable: Some(exe.into()),
            repo: None,
            branch: None,
            handle: None,
            reason: None,
            ended_at: None,
            waiting_for: None,
            headless: false,
        })
        .unwrap();
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
        let system_exe = sys.join("claude");
        let system = [system_exe.as_path()];
        let path = std::env::join_paths([&bin]).unwrap();
        let find = || program(None, &home, &system, &s, Some(&path));
        assert_eq!(find(), None);
        // Last: `claude` on PATH.
        let on_path = exe(&bin.join("claude"));
        assert_eq!(find(), Some(on_path));
        // Before it: the newest registered Claude session's, when it is Claude Code's.
        let old = exe(&d.path().join("old/claude"));
        let versions = d.path().join("home/.local/share/claude/versions/2.1.312");
        let newest = exe(&versions);
        let other = exe(&d.path().join("node"));
        session(&s, "m/claude:a", &old.to_string_lossy(), 1);
        session(&s, "m/claude:b", &newest.to_string_lossy(), 2);
        session(&s, "m/claude:c", &other.to_string_lossy(), 3);
        session(&s, "m/claude:d", "/gone/claude", 4);
        assert_eq!(find(), Some(newest));
        // Before those: the system installs, then the home ones.
        let installed = exe(&system_exe);
        assert_eq!(find(), Some(installed));
        let local = exe(&home.join(".claude/local/claude"));
        assert_eq!(find(), Some(local));
        let first = exe(&home.join(".local/bin/claude"));
        assert_eq!(find(), Some(first));
        // A file that is not executable is passed over.
        std::fs::set_permissions(
            home.join(".local/bin/claude"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(find(), Some(home.join(".claude/local/claude")));
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
    fn bypass_mode_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        assert!(Claude::here(Some(&fake), d.path(), &s).is_ok());
        let settings = d.path().join(".claude/settings.json");
        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        for (mode, ok) in [
            ("default", true),
            ("acceptEdits", true),
            ("bypassPermissions", false),
        ] {
            let v = json!({"permissions": {"defaultMode": mode}});
            std::fs::write(&settings, v.to_string()).unwrap();
            let r = Claude::here(Some(&fake), d.path(), &s);
            assert_eq!(
                r.as_ref().err().copied(),
                (!ok).then_some(BYPASS_MODE),
                "{mode}"
            );
        }
        assert!(BYPASS_MODE.starts_with("this computer's Claude Code runs in bypass mode;"));
        // No Claude Code at all.
        let none = d.path().join("nope");
        assert_eq!(
            Claude::here(Some(&none), d.path(), &s).err(),
            Some(NOT_INSTALLED)
        );
    }
}
