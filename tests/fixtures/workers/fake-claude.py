#!/usr/bin/env python3
"""A fake Claude Code for the worker tests: speaks the stream-json protocol the Claude driver
speaks (`claude -p --input-format stream-json --output-format stream-json ...`).

FAKE_CLAUDE_SCRIPT: a JSON list of steps, run after the initialize handshake and the first
user turn: {"ask": {"tool": ..., "input": {...}}} sends a can_use_tool request (the n-th ask is
`ask-<n>`, from 0) and waits for its control_response, unless the step has "wait": false;
{"cancel": n} withdraws ask n (control_cancel_request) and goes on; {"say": text} is interim
assistant text; {"sleep": seconds};
{"result": text}; {"error": text} (is_error true). In a result or error, "{last}" is replaced by
the last answer's deny message (else its behavior).
FAKE_CLAUDE_LOG: every stdin line is appended there.
Refuses to start (exit 3) if a permission-bypass flag is in its argv."""
import json
import os
import sys
import time

BYPASS = [
    "--dangerously-skip-permissions",
    "--dangerously-bypass-approvals-and-sandbox",
    "bypassPermissions",
    "--approve-for-me",
]
argv = sys.argv[1:]
if any(b in a for a in argv for b in BYPASS):
    sys.exit(3)
session = argv[argv.index("--session-id") + 1] if "--session-id" in argv else ""
log = os.environ.get("FAKE_CLAUDE_LOG")
steps = json.loads(os.environ.get("FAKE_CLAUDE_SCRIPT") or "[]")


def out(event):
    sys.stdout.write(json.dumps(event) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)  # stdin closed: the host is done with us
    if log:
        with open(log, "a") as f:
            f.write(line)
    return json.loads(line)


def until(pred):
    while True:
        event = read()
        if pred(event):
            return event


init = until(lambda e: e.get("type") == "control_request")
out({"type": "control_response",
     "response": {"subtype": "success", "request_id": init["request_id"], "response": {}}})
until(lambda e: e.get("type") == "user")
out({"type": "system", "subtype": "init", "session_id": session})

asks = 0
last = ""
for step in steps:
    if "ask" in step:
        rid = f"ask-{asks}"
        asks += 1
        out({"type": "control_request", "request_id": rid,
             "request": {"subtype": "can_use_tool", "tool_name": step["ask"]["tool"],
                         "input": step["ask"]["input"], "permission_suggestions": []}})
        if step.get("wait", True):
            answer = until(lambda e: e.get("type") == "control_response"
                           and e["response"].get("request_id") == rid)
            r = answer["response"]["response"]
            last = r.get("message") or r.get("behavior", "")
    elif "cancel" in step:
        out({"type": "control_cancel_request", "request_id": f"ask-{step['cancel']}"})
    elif "say" in step:
        out({"type": "assistant", "message": {"content": [{"type": "text", "text": step["say"]}]}})
    elif "sleep" in step:
        time.sleep(step["sleep"])
    elif "result" in step or "error" in step:
        error = "error" in step
        text = (step["error"] if error else step["result"]).replace("{last}", last)
        out({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
        out({"type": "result", "subtype": "error_during_execution" if error else "success",
             "is_error": error, "result": text, "session_id": session,
             "permission_denials": []})
sys.exit(0)
