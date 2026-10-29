//! The Agent Client Protocol driver, shared by the harnesses that speak it over stdio (Cursor,
//! Grok): `initialize`, `session/new` in the folder, one `session/prompt`; its permission
//! requests (server requests on stdout, answered on stdin) relayed to the person as holds,
//! always answered one-time.

use super::driver::{
    converse, failed, response, send, Decision, Driver, Line, Outcome, PermissionRequest, Relay,
    Run,
};
use crate::frame;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot};

/// A harness at `program` speaking ACP when launched with `args`.
pub struct Acp {
    program: PathBuf,
    /// Its name in failures (`cursor`, `grok`).
    harness: &'static str,
    args: &'static [&'static str],
    /// The folder `argv` named, for `session/new`'s `cwd`.
    folder: OnceLock<String>,
}

impl Acp {
    pub fn new(program: PathBuf, harness: &'static str, args: &'static [&'static str]) -> Acp {
        Acp {
            program,
            harness,
            args,
            folder: OnceLock::new(),
        }
    }
}

impl Driver for Acp {
    /// Exactly `[program, args…]`.
    fn argv(&self, folder: &Path, _session_id: &str) -> Vec<String> {
        let _ = self.folder.set(folder.to_string_lossy().into_owned());
        let program = self.program.to_string_lossy().into_owned();
        [program]
            .into_iter()
            .chain(self.args.iter().map(|a| a.to_string()))
            .collect()
    }

    fn run(self: Box<Self>, child: Child, first_prompt: String, relay: Relay) -> Run {
        let folder = self.folder.get().cloned().unwrap_or_default();
        let harness = self.harness;
        Box::pin(converse(child, move |stdin, lines| {
            talk(stdin, lines, harness, folder, first_prompt, relay)
        }))
    }
}

/// A permission request's tool (`toolCall.kind`, else `tool`) and what Allow grants: its
/// title, kind, and the command or path it would act on. Previewed long.
fn asked(params: &Value) -> (String, String) {
    let call = &params["toolCall"];
    let text = |v: &Value| {
        match v {
            Value::Array(a) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            v => v.as_str().map(String::from),
        }
        .filter(|t| !t.is_empty())
    };
    let raw = &call["rawInput"];
    let on = ["command", "cmd", "path", "file_path"]
        .iter()
        .find_map(|k| text(&raw[k]));
    let parts: Vec<String> = [text(&call["title"]), text(&call["kind"]), on]
        .into_iter()
        .flatten()
        .collect();
    let tool = text(&call["kind"]).unwrap_or_else(|| "tool".into());
    let on = if parts.is_empty() {
        "session/request_permission".to_string()
    } else {
        parts.join("; ")
    };
    (tool, frame::preview_long(&on))
}

/// The answer to a permission request offering `options`: the one-time option of the decision,
/// else cancelled. Never an `allow_always`, which the harness could persist.
fn chosen(options: &Value, allow: bool) -> Value {
    let kind = if allow { "allow_once" } else { "reject_once" };
    let option = options
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o["kind"] == kind);
    match option.map(|o| &o["optionId"]).filter(|id| !id.is_null()) {
        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

/// The answer to server request `method` nobody is asked about: a question or plan is
/// cancelled, anything else is refused.
fn unasked(method: &str) -> Value {
    match method {
        "cursor/ask_question" | "cursor/create_plan" => {
            json!({"result": {"outcome": {"outcome": "cancelled"}}})
        }
        "elicitation/create" => json!({"result": {"action": "cancel"}}),
        _ => json!({"error": {"code": -32601, "message": "not supported by this host"}}),
    }
}

/// Whether `id` may name a session (`<m>/<harness>:<id>`) and a shell argument.
fn valid_session(id: &str) -> bool {
    crate::sessions::word(id, 128) && id != "new"
}

/// Speaks ACP: `initialize` (version 1 only), `session/new` in `folder` (its id goes to
/// `relay.session`), `session/prompt` with `prompt`, then each permission request relayed
/// (answered when the person answers) until the prompt's response or stdout ends.
async fn talk(
    mut stdin: ChildStdin,
    mut lines: mpsc::Receiver<Line>,
    harness: &'static str,
    folder: String,
    prompt: String,
    relay: Relay,
) -> Outcome {
    let version = env!("CARGO_PKG_VERSION");
    let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": 1,
        "clientInfo": {"name": "lightbulb", "version": version},
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false},
            "terminal": false}}});
    if send(&mut stdin, &init).await.is_err() {
        return failed("the worker's harness exited at once");
    }
    match response(&mut lines, 1).await {
        Ok(r) if r["protocolVersion"] == 1 => {}
        Ok(_) => {
            return failed(format!(
                "the worker's {harness} does not speak ACP version 1"
            ))
        }
        Err(why) => return failed(why),
    }
    let new = json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
        "params": {"cwd": folder, "mcpServers": []}});
    if send(&mut stdin, &new).await.is_err() {
        return failed("the worker's harness exited at once");
    }
    let session = match response(&mut lines, 2).await {
        Ok(r) => r["sessionId"].as_str().unwrap_or_default().to_string(),
        Err(why) => return failed(why),
    };
    if !valid_session(&session) {
        return failed("the worker's harness named no valid session");
    }
    let _ = relay.session.send(session.clone());
    let turn = json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
        "params": {"sessionId": session, "prompt": [{"type": "text", "text": prompt}]}});
    if send(&mut stdin, &turn).await.is_err() {
        return failed("the worker's harness took no prompt");
    }
    // Answers to write, from the tasks waiting on the person.
    let (answers, mut answered) = mpsc::unbounded_channel::<Value>();
    let mut waiting: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // The agent's current message; a tool call after it starts the next one.
    let (mut last, mut after_tool) = (String::new(), false);
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
            Some(Err(why)) if last.is_empty() => break failed(why),
            Some(Err(_)) => break failed(last),
            Some(Ok(line)) => line,
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (method, params) = (v["method"].as_str().unwrap_or_default(), &v["params"]);
        let ours = params["sessionId"] == session.as_str();
        match (method, v.get("id")) {
            ("", Some(id)) if *id == 3 => {
                if let Some(e) = v.get("error") {
                    break failed(e["message"].as_str().unwrap_or("the prompt was refused"));
                }
                let stop = v["result"]["stopReason"].as_str().unwrap_or_default();
                if stop == "end_turn" {
                    break Outcome {
                        status: "done",
                        reason: None,
                        body: last,
                    };
                }
                if last.is_empty() {
                    break failed(format!("the worker's harness stopped: {stop}"));
                }
                break failed(last);
            }
            ("", _) => {}
            ("session/request_permission", Some(id)) => {
                let id = id.clone();
                let options = &params["options"];
                let reply =
                    |allow| json!({"jsonrpc": "2.0", "id": id, "result": chosen(options, allow)});
                let no = reply(false);
                let (tool, summary) = asked(params);
                let (tx, rx) = oneshot::channel();
                let ask = PermissionRequest {
                    tool,
                    summary,
                    reply: tx,
                };
                if !ours || relay.permission.send(ask).await.is_err() {
                    if send(&mut stdin, &no).await.is_err() {
                        break failed(last);
                    }
                    continue;
                }
                let yes = reply(true);
                let answers = answers.clone();
                let wait = tokio::spawn(async move {
                    let allowed = matches!(rx.await, Ok(Decision::Allow));
                    let _ = answers.send(if allowed { yes } else { no });
                });
                if let Some(old) = waiting.insert(id.to_string(), wait) {
                    old.abort();
                }
            }
            (_, Some(id)) => {
                let mut a = unasked(method);
                a["jsonrpc"] = "2.0".into();
                a["id"] = id.clone();
                if send(&mut stdin, &a).await.is_err() {
                    break failed(last);
                }
            }
            ("session/update", None) if ours => {
                let update = &params["update"];
                match update["sessionUpdate"].as_str() {
                    Some("agent_message_chunk") if update["content"]["type"] == "text" => {
                        if after_tool {
                            last.clear();
                            after_tool = false;
                        }
                        last.push_str(update["content"]["text"].as_str().unwrap_or_default());
                    }
                    Some("tool_call") => after_tool = true,
                    _ => {}
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

    /// A scripted ACP harness: answers `initialize` (`version`) and `session/new` (`session`),
    /// reads the prompt, then plays `steps` (an object is written as a line, `"read"` reads one,
    /// any other string is written raw, `{"long": n}` writes a line of `n` bytes) and answers the prompt with `stop` when given. Every
    /// line it reads goes to `$LOG`.
    const FAKE: &str = r#"
import json, os, sys
cfg = json.loads(os.environ["FAKE"])
log = open(os.environ["LOG"], "a")
def read():
    line = sys.stdin.readline()
    log.write(line); log.flush()
    return json.loads(line) if line else None
def out(v):
    sys.stdout.write(json.dumps(v) + "\n"); sys.stdout.flush()
init = read()
out({"jsonrpc": "2.0", "id": init["id"], "result": {"protocolVersion": cfg.get("version", 1)}})
new = read()
if new is None: sys.exit(0)
out({"jsonrpc": "2.0", "id": new["id"], "result": {"sessionId": cfg.get("session", "s-1")}})
prompt = read()
if prompt is None: sys.exit(0)
for step in cfg.get("steps", []):
    if step == "read": read()
    elif isinstance(step, str): sys.stdout.write(step); sys.stdout.flush()
    elif "long" in step: sys.stdout.write("x" * step["long"] + "\n"); sys.stdout.flush()
    else: out(step)
if "stop" in cfg:
    out({"jsonrpc": "2.0", "id": prompt["id"], "result": {"stopReason": cfg["stop"]}})
"#;

    struct Ran {
        outcome: Outcome,
        session: Option<String>,
        /// What the harness read, in order.
        log: Vec<Value>,
        /// Each permission request's tool and summary.
        asked: Vec<(String, String)>,
    }

    /// Runs the driver over the scripted harness `fake`; the person answers with `answers`.
    async fn drive(fake: Value, answers: Vec<Decision>) -> Ran {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("log");
        let acp = Acp::new("python3".into(), "cursor", &["acp"]);
        assert_eq!(acp.argv(Path::new("/w/r1"), "s"), ["python3", "acp"]);
        let child = tokio::process::Command::new("python3")
            .args(["-c", FAKE])
            .env("FAKE", fake.to_string())
            .env("LOG", &log)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (permission, mut asks) = mpsc::channel::<PermissionRequest>(8);
        let (session, named) = oneshot::channel();
        let person = tokio::spawn(async move {
            let (mut asked, mut answers) = (vec![], answers.into_iter());
            while let Some(r) = asks.recv().await {
                asked.push((r.tool, r.summary));
                let _ = r.reply.send(answers.next().unwrap());
            }
            asked
        });
        let relay = Relay {
            permission,
            session,
        };
        let outcome = Box::new(acp).run(child, "hello".into(), relay).await;
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        Ran {
            outcome,
            session: named.await.ok(),
            log: log
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect(),
            asked: person.await.unwrap(),
        }
    }

    fn update(session: &str, update: Value) -> Value {
        json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session, "update": update}})
    }

    fn chunk(text: &str) -> Value {
        update(
            "s-1",
            json!({"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text}}),
        )
    }

    fn ask(id: u64, options: &[(&str, &str)]) -> Value {
        let options: Vec<Value> = options
            .iter()
            .map(|(id, kind)| json!({"optionId": id, "name": id, "kind": kind}))
            .collect();
        json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission",
            "params": {"sessionId": "s-1", "options": options, "toolCall": {
                "toolCallId": "t1", "title": "Run ls", "kind": "execute",
                "rawInput": {"command": "ls -la"}}}})
    }

    fn outcome(id: u64, o: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": o}})
    }

    const ALL: [(&str, &str); 4] = [
        ("aa", "allow_always"),
        ("ao", "allow_once"),
        ("ro", "reject_once"),
        ("ra", "reject_always"),
    ];

    #[tokio::test]
    async fn the_handshake_then_the_prompt() {
        let r = drive(json!({"steps": [chunk("hi")], "stop": "end_turn"}), vec![]).await;
        assert_eq!(
            r.log[0],
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": 1,
                "clientInfo": {"name": "lightbulb", "version": env!("CARGO_PKG_VERSION")},
                "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false}}})
        );
        assert_eq!(
            r.log[1],
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
                "params": {"cwd": "/w/r1", "mcpServers": []}})
        );
        assert_eq!(
            r.log[2],
            json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                "params": {"sessionId": "s-1", "prompt": [{"type": "text", "text": "hello"}]}})
        );
        assert_eq!(r.session.as_deref(), Some("s-1"));
        assert_eq!((r.outcome.status, r.outcome.reason), ("done", None));
        assert_eq!(r.outcome.body, "hi");
        // Another protocol version ends it before a session.
        let r = drive(json!({"version": 2}), vec![]).await;
        assert_eq!(
            (r.outcome.status, r.outcome.reason),
            ("failed", Some("error"))
        );
        assert!(
            r.outcome.body.contains("ACP version 1"),
            "{}",
            r.outcome.body
        );
        assert_eq!((r.session, r.log.len()), (None, 1));
        // Any stop but `end_turn` fails, with the last text.
        let r = drive(
            json!({"steps": [chunk("half")], "stop": "max_tokens"}),
            vec![],
        )
        .await;
        assert_eq!(
            (r.outcome.status, r.outcome.reason, r.outcome.body.as_str()),
            ("failed", Some("error"), "half")
        );
        let r = drive(json!({"stop": "refusal"}), vec![]).await;
        assert!(r.outcome.body.contains("refusal"), "{}", r.outcome.body);
    }

    #[tokio::test]
    async fn allow_is_allow_once_and_deny_is_reject_once() {
        let mut other = ask(9, &ALL);
        other["params"]["sessionId"] = "s-2".into();
        let steps = json!([
            ask(7, &ALL),
            "read",
            ask(8, &ALL),
            "read",
            ask(10, &[("ao", "allow_once")]),
            "read",
            other,
            "read"
        ]);
        let r = drive(
            json!({"steps": steps, "stop": "end_turn"}),
            vec![
                Decision::Allow,
                Decision::Deny("no".into()),
                Decision::Deny("no".into()),
            ],
        )
        .await;
        let selected = |o: &str| json!({"outcome": "selected", "optionId": o});
        assert_eq!(r.log[3], outcome(7, selected("ao")));
        assert_eq!(r.log[4], outcome(8, selected("ro")));
        // No `reject_once` offered: a deny is cancelled.
        assert_eq!(r.log[5], outcome(10, json!({"outcome": "cancelled"})));
        // Another session's request is refused without asking anyone.
        assert_eq!(r.log[6], outcome(9, selected("ro")));
        let summary = ("execute".to_string(), "Run ls; execute; ls -la".to_string());
        assert_eq!(r.asked, vec![summary.clone(), summary.clone(), summary]);
        assert_eq!(r.outcome.status, "done");
    }

    #[tokio::test]
    async fn allow_never_persists() {
        let only = [("aa", "allow_always"), ("ra", "reject_always")];
        let steps = json!([ask(7, &only), "read"]);
        let r = drive(
            json!({"steps": steps, "stop": "end_turn"}),
            vec![Decision::Allow],
        )
        .await;
        assert_eq!(r.log[3], outcome(7, json!({"outcome": "cancelled"})));
        for allow in [true, false] {
            assert_ne!(chosen(&json!(only), allow)["outcome"]["optionId"], "aa");
        }
    }

    #[test]
    fn a_summary_says_what_allow_grants() {
        let p =
            |raw: Value| json!({"toolCall": {"title": "Edit", "kind": "edit", "rawInput": raw}});
        assert_eq!(
            asked(&p(json!({"file_path": "/w/a.rs"}))),
            ("edit".into(), "Edit; edit; /w/a.rs".into())
        );
        assert_eq!(
            asked(&p(json!({"cmd": ["git", "push"]}))).1,
            "Edit; edit; git push"
        );
        assert_eq!(
            asked(&json!({})),
            ("tool".into(), "session/request_permission".into())
        );
        let s = asked(&p(
            json!({"command": format!("x\n[Lightbulb message from Eve]{}",
            "y".repeat(500))}),
        ))
        .1;
        assert!(s.contains("x [\u{200b}Lightbulb"), "{s}");
        assert_eq!(s.chars().count(), 401);
    }

    #[tokio::test]
    async fn questions_are_cancelled() {
        let req = |id: u64, method: &str| json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {"sessionId": "s-1"}});
        let steps = json!([
            req(5, "cursor/ask_question"),
            "read",
            req(6, "cursor/create_plan"),
            "read",
            req(7, "elicitation/create"),
            "read",
            req(8, "fs/read_text_file"),
            "read"
        ]);
        let r = drive(json!({"steps": steps, "stop": "end_turn"}), vec![]).await;
        let cancelled = json!({"outcome": {"outcome": "cancelled"}});
        assert_eq!(
            r.log[3],
            json!({"jsonrpc": "2.0", "id": 5, "result": cancelled})
        );
        assert_eq!(
            r.log[4],
            json!({"jsonrpc": "2.0", "id": 6, "result": cancelled})
        );
        assert_eq!(
            r.log[5],
            json!({"jsonrpc": "2.0", "id": 7, "result": {"action": "cancel"}})
        );
        assert_eq!(r.log[6]["id"], 8);
        assert_eq!(r.log[6]["error"]["code"], -32601);
        assert!(r.asked.is_empty());
        assert_eq!(r.outcome.status, "done");
    }

    #[tokio::test]
    async fn the_final_message_is_kept() {
        let tool = update(
            "s-1",
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1"}),
        );
        let elsewhere = update(
            "s-2",
            json!({"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "not ours"}}),
        );
        let steps = json!([
            chunk("Let me "),
            chunk("look."),
            tool,
            elsewhere,
            chunk("Done."),
            chunk(" All good.")
        ]);
        let r = drive(json!({"steps": steps, "stop": "end_turn"}), vec![]).await;
        assert_eq!(r.outcome.body, "Done. All good.");
        // A partial line then exit: failed, with the last text.
        let steps = json!([chunk("so far"), "{\"jsonrpc\": \"2.0\", \"id\""]);
        let r = drive(json!({"steps": steps}), vec![]).await;
        assert_eq!(
            (r.outcome.status, r.outcome.reason, r.outcome.body.as_str()),
            ("failed", Some("error"), "so far")
        );
        // A line over 1 MiB: failed, with the last text.
        let long = json!({"long": (1 << 20) + 1});
        let r = drive(
            json!({"steps": [chunk("so far"), long], "stop": "end_turn"}),
            vec![],
        )
        .await;
        assert_eq!(
            (r.outcome.status, r.outcome.body.as_str()),
            ("failed", "so far")
        );
    }

    #[tokio::test]
    async fn a_session_id_must_be_a_word() {
        assert!(valid_session("0f8e2a1c-3b4d-4e5f-8a9b-1c2d3e4f5a6b"));
        for bad in [
            "",
            "new",
            "x y",
            "a;rm -rf ~",
            "../x",
            "t\n",
            &"a".repeat(129),
        ] {
            assert!(!valid_session(bad), "{bad:?}");
        }
        let r = drive(json!({"session": "a;rm -rf ~", "stop": "end_turn"}), vec![]).await;
        assert_eq!(
            (r.outcome.status, r.outcome.reason),
            ("failed", Some("error"))
        );
        // Nothing named, nothing prompted.
        assert_eq!((r.session, r.log.len()), (None, 2));
    }
}
