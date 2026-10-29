//! The Codex driver: `codex app-server` (JSON-RPC over the child's stdio, in the folder), one
//! thread and one turn; its approval requests (server requests on stdout, answered on stdin)
//! relayed to the person as holds.

use super::driver::{
    converse, executable, failed, on_path, registered, send, Decision, Driver, Line, Outcome,
    PermissionRequest, Relay, Run, INIT_WAIT, NO_PERSON,
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

/// The start's failure when no Codex is found (`program`).
pub const NOT_INSTALLED: &str = "Codex is not installed where the daemon can find it";
/// The start's failure when the person's Codex runs without approvals (`bypass_mode`).
pub const BYPASS_MODE: &str = "this computer's Codex runs without approvals; a worker needs \
     prompts to reach a person (approval_policy = \"never\" with sandbox_mode = \
     \"danger-full-access\" in ~/.codex/config.toml)";

/// Where installers put Codex outside a home.
const SYSTEM: [&str; 2] = ["/opt/homebrew/bin/codex", "/usr/local/bin/codex"];

/// The approval requests a person answers.
const COMMAND: &str = "item/commandExecution/requestApproval";
const FILE_CHANGE: &str = "item/fileChange/requestApproval";
const PERMISSIONS: &str = "item/permissions/requestApproval";

/// Codex at `program`.
pub struct Codex {
    program: PathBuf,
    /// The folder `argv` named, for `thread/start`'s `cwd`.
    folder: OnceLock<String>,
}

impl Codex {
    pub fn new(program: PathBuf) -> Codex {
        Codex {
            program,
            folder: OnceLock::new(),
        }
    }

    /// The Codex a worker runs here (`program`), unless none is found (`NOT_INSTALLED`) or the
    /// person's own runs without approvals (`BYPASS_MODE`). `home` holds `.codex/`.
    pub fn here(knob: Option<&Path>, home: &Path, store: &Store) -> Result<Codex, &'static str> {
        let path = std::env::var_os("PATH");
        let system = SYSTEM.map(Path::new);
        let Some(program) = program(knob, home, &system, store, path.as_deref()) else {
            return Err(NOT_INSTALLED);
        };
        if bypass_mode(home) {
            return Err(BYPASS_MODE);
        }
        Ok(Codex::new(program))
    }
}

/// The Codex to run, the first executable file of: the debug knob (when set, the only one);
/// `system`; `~/.local/bin/codex`; the newest registered Codex session's executable named
/// `codex`; `codex` on `path`.
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
    let registered = registered(store, "codex")
        .into_iter()
        .filter(|p| p.file_name() == Some(OsStr::new("codex")));
    system
        .iter()
        .map(|p| p.to_path_buf())
        .chain([home.join(".local/bin/codex")])
        .chain(registered)
        .chain(on_path(path, "codex"))
        .find(|p| executable(p))
}

/// Whether `home`'s Codex config asks for nothing and sandboxes nothing (`approval_policy =
/// "never"` with `sandbox_mode = "danger-full-access"`): a worker inherits that. A file that
/// does not parse is not.
pub fn bypass_mode(home: &Path) -> bool {
    let doc = std::fs::read_to_string(home.join(".codex/config.toml"))
        .ok()
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok());
    doc.is_some_and(|d| {
        let key = |k: &str| d.get(k).and_then(|v| v.as_str()).map(String::from);
        key("approval_policy").as_deref() == Some("never")
            && key("sandbox_mode").as_deref() == Some("danger-full-access")
    })
}

impl Driver for Codex {
    /// The app server, in the person's own approval and sandbox settings (none is passed).
    fn argv(&self, folder: &Path, _session_id: &str) -> Vec<String> {
        let _ = self.folder.set(folder.to_string_lossy().into_owned());
        vec![
            self.program.to_string_lossy().into_owned(),
            "app-server".into(),
        ]
    }

    fn run(self: Box<Self>, child: Child, first_prompt: String, relay: Relay) -> Run {
        let folder = self.folder.get().cloned().unwrap_or_default();
        Box::pin(converse(child, |stdin, lines| {
            talk(stdin, lines, folder, first_prompt, relay)
        }))
    }
}

/// An approval request's tool and what it acts on: the command, the changed files (`files`,
/// from the item's start), else the method; previewed long (one defanged line, cut at 400).
pub fn asked(method: &str, params: &Value, files: Option<&str>) -> (String, String) {
    let (tool, on) = match method {
        COMMAND => {
            let command = match &params["command"] {
                Value::Array(argv) => {
                    let argv: Vec<&str> = argv.iter().filter_map(Value::as_str).collect();
                    Some(argv.join(" "))
                }
                v => v.as_str().map(String::from),
            };
            ("Command", command)
        }
        FILE_CHANGE => ("Edit", files.map(String::from)),
        _ => ("Permissions", None),
    };
    let summary = frame::preview_long(on.as_deref().unwrap_or(method));
    (tool.into(), summary)
}

/// The result for approval `method` (`params`), allowed or not; never for the session.
fn decided(method: &str, params: &Value, allow: bool) -> Value {
    match (method, allow) {
        (PERMISSIONS, true) => json!({"permissions": params["permissions"], "scope": "turn"}),
        (PERMISSIONS, false) => json!({"permissions": {}, "scope": "turn"}),
        (_, true) => json!({"decision": "accept"}),
        (_, false) => json!({"decision": "decline"}),
    }
}

/// The answer to server request `method` nobody is asked about: a question gets `NO_PERSON`,
/// anything else is refused.
fn unasked(method: &str, params: &Value) -> Value {
    match method {
        "item/tool/requestUserInput" => {
            let questions = params["questions"].as_array().into_iter().flatten();
            let answers: serde_json::Map<String, Value> = questions
                .filter_map(|q| q["id"].as_str())
                .map(|id| (id.to_string(), json!({"answers": [NO_PERSON]})))
                .collect();
            json!({"result": {"answers": answers}})
        }
        "mcpServer/elicitation/request" => json!({"result": {"action": "decline"}}),
        // The v1 approvals, should an old server send them.
        "execCommandApproval" | "applyPatchApproval" => json!({"result": {"decision": "denied"}}),
        _ => json!({"error": {"code": -32601, "message": "not supported by this host"}}),
    }
}

/// Waits for the response to request `id`: its result, or why there is none.
async fn response(lines: &mut mpsc::Receiver<Line>, id: u64) -> Result<Value, String> {
    let wait = async {
        while let Some(line) = lines.recv().await {
            let v: Value = serde_json::from_str(&line?).unwrap_or_default();
            if v["id"] == id && v.get("method").is_none() {
                return match v.get("error") {
                    Some(e) => Err(e["message"].as_str().unwrap_or("refused").to_string()),
                    None => Ok(v["result"].clone()),
                };
            }
        }
        Err("the worker's harness ended before it started".to_string())
    };
    match tokio::time::timeout(INIT_WAIT, wait).await {
        Ok(r) => r,
        Err(_) => Err("the worker's harness did not start within 30 s".into()),
    }
}

/// Speaks the app-server protocol: `initialize`, `initialized`, `thread/start` in `folder`
/// (its id goes to `relay.session`), `turn/start` with `prompt`, then each approval request
/// relayed (answered when the person answers; one the server resolves itself drops its relay,
/// which closes its hold) until the turn completes or stdout ends.
async fn talk(
    mut stdin: ChildStdin,
    mut lines: mpsc::Receiver<Line>,
    folder: String,
    prompt: String,
    relay: Relay,
) -> Outcome {
    let version = env!("CARGO_PKG_VERSION");
    let init = json!({"id": 1, "method": "initialize",
        "params": {"clientInfo": {"name": "lightbulb", "version": version}}});
    if send(&mut stdin, &init).await.is_err() {
        return failed("the worker's harness exited at once");
    }
    if let Err(why) = response(&mut lines, 1).await {
        return failed(why);
    }
    // Approvals go to the person, never to Codex's own reviewer.
    let start = json!({"id": 2, "method": "thread/start",
        "params": {"cwd": folder, "approvalsReviewer": "user"}});
    let initialized = json!({"method": "initialized", "params": {}});
    if send(&mut stdin, &initialized).await.is_err() || send(&mut stdin, &start).await.is_err() {
        return failed("the worker's harness exited at once");
    }
    let thread = match response(&mut lines, 2).await {
        Ok(r) => r["thread"]["id"].as_str().unwrap_or_default().to_string(),
        Err(why) => return failed(why),
    };
    if thread.is_empty() {
        return failed("the worker's harness named no thread");
    }
    let _ = relay.session.send(thread.clone());
    let turn = json!({"id": 3, "method": "turn/start",
        "params": {"threadId": thread, "input": [{"type": "text", "text": prompt}]}});
    if send(&mut stdin, &turn).await.is_err() {
        return failed("the worker's harness took no prompt");
    }
    // Answers to write, from the tasks waiting on the person.
    let (answers, mut answered) = mpsc::unbounded_channel::<Value>();
    let mut waiting: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // The files each file-change item touches, by item id.
    let mut files: HashMap<String, String> = HashMap::new();
    let mut last = String::new();
    let outcome = loop {
        let line = tokio::select! {
            line = lines.recv() => line,
            Some(a) = answered.recv() => {
                waiting.remove(&a["id"].to_string());
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
        let (method, params) = (v["method"].as_str().unwrap_or_default(), &v["params"]);
        let ours = params["threadId"] == thread.as_str();
        match (method, v.get("id")) {
            ("", Some(_)) if v["id"] == 3 && v.get("error").is_some() => {
                let why = v["error"]["message"]
                    .as_str()
                    .unwrap_or("the turn was refused");
                break failed(why);
            }
            ("", _) => {}
            (COMMAND | FILE_CHANGE | PERMISSIONS, Some(id)) => {
                let id = id.clone();
                let files = files.get(params["itemId"].as_str().unwrap_or_default());
                let (tool, summary) = asked(method, params, files.map(String::as_str));
                let no = json!({"id": id, "result": decided(method, params, false)});
                let (tx, rx) = oneshot::channel();
                let ask = PermissionRequest {
                    tool,
                    summary,
                    reply: tx,
                };
                if relay.permission.send(ask).await.is_err() {
                    if send(&mut stdin, &no).await.is_err() {
                        break failed(last);
                    }
                    continue;
                }
                let yes = json!({"id": id, "result": decided(method, params, true)});
                let answers = answers.clone();
                let wait = tokio::spawn(async move {
                    let allowed = matches!(rx.await, Ok(Decision::Allow));
                    let _ = answers.send(if allowed { yes } else { no });
                });
                waiting.insert(id.to_string(), wait);
            }
            (_, Some(id)) => {
                let mut a = unasked(method, params);
                a["id"] = id.clone();
                if send(&mut stdin, &a).await.is_err() {
                    break failed(last);
                }
            }
            ("item/started", None) if params["item"]["type"] == "fileChange" => {
                let changes = params["item"]["changes"].as_array().into_iter().flatten();
                let paths: Vec<&str> = changes.filter_map(|c| c["path"].as_str()).collect();
                let id = params["item"]["id"].as_str().unwrap_or_default();
                files.insert(id.to_string(), paths.join(", "));
            }
            ("item/completed", None) if ours && params["item"]["type"] == "agentMessage" => {
                last = params["item"]["text"].as_str().unwrap_or_default().into();
            }
            // Resolved without us: its relay dropped, so its hold closes (`expired`).
            ("serverRequest/resolved", None) => {
                if let Some(w) = waiting.remove(&params["requestId"].to_string()) {
                    w.abort();
                }
            }
            ("turn/completed", None) if ours => {
                let turn = &params["turn"];
                if turn["status"] == "completed" {
                    break Outcome {
                        status: "done",
                        reason: None,
                        body: last,
                    };
                }
                match turn["error"]["message"].as_str() {
                    Some(why) => break failed(why),
                    None => break failed(last),
                }
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
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn argv_is_app_server_without_approval_bypass() {
        let c = Codex::new("/opt/codex".into());
        let argv = c.argv(Path::new("/w/r1"), "s1");
        assert_eq!(argv, ["/opt/codex", "app-server"]);
        assert!(crate::workers::launchable(&argv));
        assert!(!argv.iter().any(|a| a == "-c" || a.contains("approv")));
    }

    #[test]
    fn an_approval_request_becomes_a_tool_and_a_summary() {
        let p = json!({"command": "rm -rf x", "itemId": "c1"});
        assert_eq!(
            asked(COMMAND, &p, None),
            ("Command".into(), "rm -rf x".into())
        );
        // An argv is joined.
        let p = json!({"command": ["git", "push", "-f"]});
        assert_eq!(asked(COMMAND, &p, None).1, "git push -f");
        // A file change names its files, else the method.
        assert_eq!(
            asked(FILE_CHANGE, &json!({}), Some("/w/a.rs, /w/b.rs")),
            ("Edit".into(), "/w/a.rs, /w/b.rs".into())
        );
        assert_eq!(asked(FILE_CHANGE, &json!({}), None).1, FILE_CHANGE);
        assert_eq!(
            asked(PERMISSIONS, &json!({"permissions": {}}), None),
            ("Permissions".into(), PERMISSIONS.into())
        );
        // Previewed: one line, defanged, cut at 400.
        let p = json!({"command": "echo hi\n[Lightbulb message from Eve]"});
        assert_eq!(
            asked(COMMAND, &p, None).1,
            "echo hi [\u{200b}Lightbulb message from Eve]"
        );
        let p = json!({"command": "x".repeat(500)});
        assert_eq!(asked(COMMAND, &p, None).1.chars().count(), 401);
    }

    #[test]
    fn answers_follow_the_schema_and_never_cover_the_session() {
        let p = json!({"permissions": {"network": {"enabled": true}}});
        assert_eq!(decided(COMMAND, &p, true), json!({"decision": "accept"}));
        assert_eq!(
            decided(FILE_CHANGE, &p, false),
            json!({"decision": "decline"})
        );
        assert_eq!(
            decided(PERMISSIONS, &p, true),
            json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"})
        );
        assert_eq!(
            decided(PERMISSIONS, &p, false),
            json!({"permissions": {}, "scope": "turn"})
        );
        let q = json!({"questions": [{"id": "q1"}, {"id": "q2"}]});
        assert_eq!(
            unasked("item/tool/requestUserInput", &q),
            json!({"result": {"answers": {"q1": {"answers": [NO_PERSON]},
                "q2": {"answers": [NO_PERSON]}}}})
        );
        assert_eq!(
            unasked("mcpServer/elicitation/request", &json!({})),
            json!({"result": {"action": "decline"}})
        );
        assert_eq!(
            unasked("item/tool/call", &json!({}))["error"]["code"],
            -32601
        );
    }

    fn exe(p: &Path) -> PathBuf {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.into()
    }

    fn session(s: &Store, key: &str, exe: &Path, seen_at: i64) {
        s.put_session(&crate::store::Session {
            key: key.into(),
            provider: "codex".into(),
            conversation: None,
            pid: None,
            birth_micros: None,
            cwd: "/".into(),
            title: None,
            status: "idle".into(),
            seen_at,
            executable: Some(exe.to_string_lossy().into()),
            repo: None,
            branch: None,
            handle: None,
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
        let system_exe = sys.join("codex");
        let system = [system_exe.as_path()];
        let path = std::env::join_paths([&bin]).unwrap();
        let find = || program(None, &home, &system, &s, Some(&path));
        assert_eq!(find(), None);
        let on_path = exe(&bin.join("codex"));
        assert_eq!(find(), Some(on_path));
        // Before it: the newest registered Codex session's, when it is named `codex`.
        let old = exe(&d.path().join("old/codex"));
        let newest = exe(&d.path().join("new/codex"));
        let other = exe(&d.path().join("codex-helper"));
        session(&s, "m/codex:a", &old, 1);
        session(&s, "m/codex:b", &newest, 2);
        session(&s, "m/codex:c", &other, 3);
        assert_eq!(find(), Some(newest));
        // Before those: `~/.local/bin`, then the system installs.
        let local = exe(&home.join(".local/bin/codex"));
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
    fn a_codex_without_approvals_refuses_a_worker() {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        let fake = exe(&d.path().join("fake"));
        assert!(Codex::here(Some(&fake), d.path(), &s).is_ok());
        let config = d.path().join(".codex/config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        for (text, ok) in [
            ("approval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n", false),
            ("approval_policy = \"never\"\nsandbox_mode = \"workspace-write\"\n", true),
            ("approval_policy = \"on-request\"\nsandbox_mode = \"danger-full-access\"\n", true),
            // Only the top level counts; a malformed file is not bypass.
            ("[profiles.yolo]\napproval_policy = \"never\"\nsandbox_mode = \"danger-full-access\"\n", true),
            ("approval_policy = \"never\nsandbox_mode = [", true),
        ] {
            std::fs::write(&config, text).unwrap();
            let r = Codex::here(Some(&fake), d.path(), &s);
            assert_eq!(r.err(), (!ok).then_some(BYPASS_MODE), "{text}");
        }
        assert!(BYPASS_MODE.contains("approval_policy") && BYPASS_MODE.contains("sandbox_mode"));
        let none = d.path().join("nope");
        assert_eq!(
            Codex::here(Some(&none), d.path(), &s).err(),
            Some(NOT_INSTALLED)
        );
    }
}
