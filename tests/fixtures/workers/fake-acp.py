#!/usr/bin/env python3
"""A fake Cursor Agent or Grok CLI for the worker tests: speaks the Agent Client Protocol over
stdio as the ACP driver does (`cursor-agent acp`, `grok agent --no-leader stdio`).

FAKE_ACP_HARNESS (cursor | grok) names the exact argv it requires: exit 3 on any bypass word,
and for grok without `--no-leader`; exit 2 on any other argv.
It answers `initialize` (protocolVersion 1) and `session/new` (session `s-<pid>`), then on
`session/prompt` runs FAKE_ACP_SCRIPT, a JSON list of steps:
{"ask": {"kind", "title", "rawInput", "options"}} sends `session/request_permission` (the n-th
ask has id `100+n`, from 0; options default to allow_once/allow_always/reject_once/
reject_always) and waits for its answer, unless the step has "wait": false;
{"cancel": n} gives up on ask n and moves on (ACP has no withdrawal notification);
{"say": text} is an `agent_message_chunk`; {"sleep": seconds};
{"result": text} (a final chunk, then the prompt's response, stopReason `end_turn`);
{"error": text} (the prompt's response, stopReason `refusal`). In a result or error, "{last}"
is replaced by the last answer: the chosen option's kind, or `cancelled`.
FAKE_ACP_LOG: every stdin line is appended there."""
import json
import os
import sys
import time

ARGV = {"cursor": ["acp"], "grok": ["agent", "--no-leader", "stdio"]}
# Whole elements (or either side of `=`), and words that mean a bypass anywhere.
BYPASS = ["--force", "--auto-review", "--approve-mcps", "--trust", "--auto", "dontAsk",
          "--approve-for-me", "bypassPermissions"]
DISTINCTIVE = ["dangerously", "bypassPermissions", "always-approve", "yolo"]
harness = os.environ.get("FAKE_ACP_HARNESS", "")
argv = sys.argv[1:]
if any({a, *a.split("=", 1)} & set(BYPASS) or any(d in a for d in DISTINCTIVE) for a in argv):
    sys.exit(3)
if harness == "grok" and "--no-leader" not in argv:
    sys.exit(3)
if argv != ARGV.get(harness):
    sys.exit(2)
log = os.environ.get("FAKE_ACP_LOG")
steps = json.loads(os.environ.get("FAKE_ACP_SCRIPT") or "[]")
session = f"s-{os.getpid()}"
OPTIONS = [{"optionId": k.replace("_", "-"), "name": k, "kind": k}
           for k in ["allow_once", "allow_always", "reject_once", "reject_always"]]


def out(msg):
    msg["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)  # stdin closed: the client is done with us
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


def chunk(text):
    out({"method": "session/update", "params": {"sessionId": session, "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})


init = request("initialize")
out({"id": init["id"], "result": {"protocolVersion": 1, "agentCapabilities": {},
                                  "authMethods": []}})
new = request("session/new")
out({"id": new["id"], "result": {"sessionId": session}})
prompt = request("session/prompt")

asks = 0
last = ""
for step in steps:
    if "ask" in step:
        rid = 100 + asks
        asks += 1
        ask = step["ask"]
        options = ask.get("options", OPTIONS)
        call = {"toolCallId": f"c{rid}", "title": ask.get("title"), "kind": ask.get("kind"),
                "rawInput": ask.get("rawInput"), "status": "pending"}
        out({"id": rid, "method": "session/request_permission",
             "params": {"sessionId": session, "toolCall": call, "options": options}})
        if step.get("wait", True):
            answer = until(lambda m: m.get("id") == rid and "method" not in m)
            outcome = answer.get("result", {}).get("outcome", {})
            chosen = [o["kind"] for o in options if o["optionId"] == outcome.get("optionId")]
            last = chosen[0] if outcome.get("outcome") == "selected" and chosen else "cancelled"
    elif "cancel" in step:
        pass  # no ACP withdrawal: the fake just stops waiting for ask n
    elif "say" in step:
        chunk(step["say"])
    elif "sleep" in step:
        time.sleep(step["sleep"])
    elif "result" in step:
        chunk(step["result"].replace("{last}", last))
        out({"id": prompt["id"], "result": {"stopReason": "end_turn"}})
    elif "error" in step:
        out({"id": prompt["id"], "result": {"stopReason": "refusal"}})
# Like the real servers, it keeps serving until its stdin closes.
until(lambda m: False)
