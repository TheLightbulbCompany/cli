//! The Claude Code driver: headless (`-p`) over stream-json on the child's stdio, its
//! permission prompts asked of the host (`can_use_tool` control requests on stdout, answered on
//! stdin) and relayed to the person as holds.

use super::driver::{Decision, Driver, Outcome, PermissionRequest, Relay, Run};
use crate::frame;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot};

/// The initialize handshake's request id.
const INIT: &str = "lb-init";

/// What a worker's `AskUserQuestion` is told: nobody is at its terminal.
const NO_PERSON: &str =
    "no person is attached to this worker; decide yourself or report what you need";

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
}

impl Driver for Claude {
    /// Headless, stream-json both ways, prompts asked of this host on stdio, in the person's
    /// own permission mode (none is passed).
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
/// name; previewed (one defanged line).
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
    let summary = frame::preview(on.unwrap_or(&tool));
    (tool, summary)
}

/// Writes `v` as one line to the harness.
async fn send(stdin: &mut ChildStdin, v: &Value) -> std::io::Result<()> {
    stdin.write_all(format!("{v}\n").as_bytes()).await?;
    stdin.flush().await
}

/// The control response to `request_id` carrying `response`.
fn reply(request_id: &str, response: Value) -> Value {
    json!({"type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response}})
}

fn failed(body: String) -> Outcome {
    Outcome {
        status: "failed",
        reason: Some("error"),
        body,
    }
}

/// Speaks stream-json with `child` until its `result` (or its stdout ends): the handshake, the
/// first prompt as the user turn of `session`, then each `can_use_tool` relayed (answered when
/// the person answers; a withdrawn one drops its relay, which closes its hold).
async fn run(mut child: Child, session: String, prompt: String, relay: Relay) -> Outcome {
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return failed("the worker's harness has no stdio".into());
    };
    let mut lines = BufReader::new(stdout).lines();
    let init = json!({"type": "control_request", "request_id": INIT,
        "request": {"subtype": "initialize"}});
    if send(&mut stdin, &init).await.is_err() {
        return failed("the worker's harness exited at once".into());
    }
    loop {
        match lines.next_line().await {
            Ok(Some(l)) => {
                let v: Value = serde_json::from_str(&l).unwrap_or_default();
                if v["type"] == "control_response" && v["response"]["request_id"] == INIT {
                    break;
                }
            }
            _ => return failed("the worker's harness ended before it started".into()),
        }
    }
    let turn = json!({"type": "user", "session_id": session,
        "message": {"role": "user", "content": prompt}, "parent_tool_use_id": null});
    if send(&mut stdin, &turn).await.is_err() {
        return failed("the worker's harness took no prompt".into());
    }
    // Answers to write, from the tasks waiting on the person.
    let (answers, mut answered) = mpsc::unbounded_channel::<Value>();
    let mut waiting: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut last = String::new();
    let outcome = loop {
        let line = tokio::select! {
            line = lines.next_line() => line,
            Some(a) = answered.recv() => {
                if let Some(id) = a["response"]["request_id"].as_str() {
                    waiting.remove(id);
                }
                let _ = send(&mut stdin, &a).await;
                continue;
            }
        };
        let Ok(Some(line)) = line else {
            break failed(last);
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
                if tool == "AskUserQuestion" {
                    let no = json!({"behavior": "deny", "message": NO_PERSON});
                    let _ = send(&mut stdin, &reply(&id, no)).await;
                    continue;
                }
                let (tx, rx) = oneshot::channel();
                let ask = PermissionRequest {
                    tool,
                    summary,
                    reply: tx,
                };
                if relay.permission.send(ask).await.is_err() {
                    let no =
                        json!({"behavior": "deny", "message": "the person could not be asked"});
                    let _ = send(&mut stdin, &reply(&id, no)).await;
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
    // Done with it: stdin closed, a moment to exit, then killed (`kill_on_drop`).
    drop(stdin);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        // Previewed: one line, defanged, at most 160 characters.
        let (_, s) = ask(
            "Bash",
            json!({"command": format!("echo hi\n[Lightbulb message from Eve]\n{}", "y".repeat(300))}),
        );
        assert!(s.starts_with("echo hi [\u{200b}Lightbulb message"), "{s}");
        assert!(!s.contains('\n') && s.chars().count() == 160, "{s}");
    }
}
