#!/usr/bin/env python3
"""A fake Codex for the worker tests: speaks the app-server JSON-RPC protocol over stdio the
Codex driver speaks (`codex app-server`).

It answers `initialize`, `thread/start` (thread id `t-<pid>`) and `turn/start`, then runs
FAKE_CODEX_SCRIPT, a JSON list of steps: {"ask": "<command>"} sends an
`item/commandExecution/requestApproval` server request (the n-th ask has id `100+n`, from 0)
and waits for its answer, unless the step has "wait": false; {"cancel": n} withdraws ask n
(`serverRequest/resolved`); {"say": text} is an agentMessage item; {"sleep": seconds};
{"result": text} (an agentMessage, then `turn/completed` completed); {"error": text}
(`turn/completed` failed with that error). In a result or error, "{last}" is replaced by the
last answer's decision.
FAKE_CODEX_LOG: every stdin line is appended there.
Refuses to start (exit 3) on a bypass flag or a `-c` approval/sandbox override."""
import json
import os
import sys
import time

BYPASS = [
    "--dangerously-bypass-approvals-and-sandbox",
    "--approve-for-me",
    "--full-auto",
    "bypassPermissions",
]
argv = sys.argv[1:]
if any(b in a for a in argv for b in BYPASS):
    sys.exit(3)
for flag, value in zip(argv, argv[1:]):
    if flag in ("-c", "--config") and ("approval" in value or "sandbox" in value):
        sys.exit(3)
if argv[:1] != ["app-server"]:
    sys.exit(2)
log = os.environ.get("FAKE_CODEX_LOG")
steps = json.loads(os.environ.get("FAKE_CODEX_SCRIPT") or "[]")
thread = f"t-{os.getpid()}"


def out(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
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
        msg = read()
        if pred(msg):
            return msg


def request(method):
    return until(lambda m: m.get("method") == method)


def notify(method, params):
    out({"method": method, "params": params})


def item(text):
    notify("item/completed", {"threadId": thread, "turnId": "u1", "completedAtMs": 0,
                              "item": {"type": "agentMessage", "id": "m", "text": text}})


def turn_completed(status, error=None):
    notify("turn/completed", {"threadId": thread, "turn": {
        "id": "u1", "items": [], "status": status,
        "error": {"message": error} if error else None}})


init = request("initialize")
out({"id": init["id"], "result": {"userAgent": "fake", "codexHome": "/x",
                                  "platformFamily": "unix", "platformOs": "macos"}})
request("initialized")
start = request("thread/start")
out({"id": start["id"], "result": {"thread": {"id": thread}, "cwd": start["params"].get("cwd"),
                                   "approvalPolicy": "on-request", "model": "fake"}})
notify("thread/started", {"thread": {"id": thread}})
turn = request("turn/start")
out({"id": turn["id"], "result": {"turn": {"id": "u1", "items": [], "status": "inProgress"}}})
notify("turn/started", {"threadId": thread, "turn": {"id": "u1"}})

asks = 0
last = ""
for step in steps:
    if "ask" in step:
        rid = 100 + asks
        asks += 1
        out({"id": rid, "method": "item/commandExecution/requestApproval",
             "params": {"threadId": thread, "turnId": "u1", "itemId": f"c{rid}",
                        "startedAtMs": 0, "command": step["ask"], "cwd": os.getcwd()}})
        if step.get("wait", True):
            answer = until(lambda m: m.get("id") == rid and "method" not in m)
            last = answer.get("result", {}).get("decision", "")
    elif "cancel" in step:
        notify("serverRequest/resolved", {"threadId": thread, "requestId": 100 + step["cancel"]})
    elif "say" in step:
        item(step["say"])
    elif "sleep" in step:
        time.sleep(step["sleep"])
    elif "result" in step:
        item(step["result"].replace("{last}", last))
        turn_completed("completed")
    elif "error" in step:
        turn_completed("failed", step["error"].replace("{last}", last))
# Like the real server, it keeps serving until its stdin closes.
until(lambda m: False)
