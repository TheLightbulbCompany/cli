#!/usr/bin/env python3
"""A fake OpenCode for the worker tests: `opencode serve --hostname 127.0.0.1 --port <port>`,
an HTTP server speaking what the OpenCode driver uses.

Exit 3 on `--auto`, `--cors`, `--mdns` or a hostname other than 127.0.0.1; exit 2 on any argv
but the exact one; exit 4 without OPENCODE_SERVER_PASSWORD, with OPENCODE_CONFIG,
OPENCODE_CONFIG_CONTENT or OPENCODE_CONFIG_DIR (a config the host did not check), or without
OPENCODE_PERMISSION={"*":"ask"} and OPENCODE_DISABLE_PROJECT_CONFIG=1 (a worker always asks,
and never reads its folder's config).
Every request without Basic auth for OPENCODE_SERVER_USERNAME (default `opencode`) and that
password gets 401. FAKE_OPENCODE_LOG: each request is appended as a JSON line
{method, path, auth (whether it was right), body}.
Routes: GET /global/health; POST /session (session `ses_<pid>`); GET /event (SSE, chunked, each
event split across two chunks); POST /permission/{id}/reply; POST /question/{id}/reject;
POST /session/{id}/message runs FAKE_OPENCODE_SCRIPT, a JSON list of steps:
{"ask": {"type", "pattern", "title"}} emits `permission.asked` (the n-th ask has id `p<n>`, from
0) and waits for its reply, unless the step has "wait": false; with "reply_status": n its
reply is answered HTTP n; {"v2": true} emits `permission.v2.asked`;
{"cancel": n} emits `permission.replied` for `p<n>`; {"question": text} emits `question.asked`
(`q<n>`) and waits for its reject; {"say": text} a text part of the turn; {"sleep": seconds};
{"result": text} the turn's final text; {"error": text} the turn's error; {"drop": true} closes
the event streams. Then it answers with the turn. In a result, "{last}" is replaced by the
last reply (`once`, `reject`)."""
import base64
import json
import os
import queue
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

argv = sys.argv[1:]
if any(a.split("=", 1)[0] in ("--auto", "--cors", "--mdns") for a in argv):
    sys.exit(3)
for i, a in enumerate(argv):
    host = a.split("=", 1)[1] if a.startswith("--hostname=") else (
        argv[i + 1] if a == "--hostname" and i + 1 < len(argv) else None)
    if host is not None and host != "127.0.0.1":
        sys.exit(3)
if argv[:4] != ["serve", "--hostname", "127.0.0.1", "--port"] or len(argv) != 5 \
        or not argv[4].isdigit():
    sys.exit(2)
password = os.environ.get("OPENCODE_SERVER_PASSWORD")
if not password or any(os.environ.get(k) is not None for k in (
        "OPENCODE_CONFIG", "OPENCODE_CONFIG_CONTENT", "OPENCODE_CONFIG_DIR")) \
        or os.environ.get("OPENCODE_PERMISSION") != '{"*":"ask"}' \
        or os.environ.get("OPENCODE_DISABLE_PROJECT_CONFIG") != "1":
    sys.exit(4)

user = os.environ.get("OPENCODE_SERVER_USERNAME", "opencode")
AUTH = "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()
LOG = os.environ.get("FAKE_OPENCODE_LOG")
STEPS = json.loads(os.environ.get("FAKE_OPENCODE_SCRIPT") or "[]")
SESSION = f"ses_{os.getpid()}"
lock = threading.Lock()
streams = []  # one queue per open /event stream; None closes it
waits = {}  # request id -> [threading.Event, the body it was answered with]
statuses = {}  # request id -> the status its reply gets


def log(entry):
    if LOG:
        with lock, open(LOG, "a") as f:
            f.write(json.dumps(entry) + "\n")


def emit(kind, props):
    for q in list(streams):
        q.put({"type": kind, "properties": props})


def ask(rid):
    waits[rid] = [threading.Event(), None]


def part(pid, message, text):
    emit("message.part.updated", {"sessionID": SESSION, "time": 0, "part": {
        "id": pid, "sessionID": SESSION, "messageID": message, "type": "text", "text": text}})


def turn(prompt):
    # The prompt's own part first: it is not what the worker said.
    emit("message.updated", {"sessionID": SESSION, "info": {
        "id": "msg_user", "sessionID": SESSION, "role": "user"}})
    part("prt_user", "msg_user", prompt)
    emit("message.updated", {"sessionID": SESSION, "info": {
        "id": "msg_1", "sessionID": SESSION, "role": "assistant"}})
    asks = questions = says = 0
    last, text, error = "", [], None
    for step in STEPS:
        if "ask" in step:
            rid = f"p{asks}"
            asks += 1
            a = step["ask"]
            ask(rid)
            statuses[rid] = step.get("reply_status", 200)
            emit("permission.asked", {
                "id": rid, "sessionID": SESSION, "permission": a.get("type"),
                "patterns": [a["pattern"]] if a.get("pattern") else [],
                "metadata": {"title": a["title"]} if a.get("title") else {},
                "always": [], "tool": {"messageID": "msg_1", "callID": f"c{rid}"}})
            if step.get("wait", True):
                waits[rid][0].wait()
                last = (waits[rid][1] or {}).get("reply", "")
        elif "v2" in step:
            emit("permission.v2.asked", {"id": "per_v2", "sessionID": SESSION,
                                         "action": "bash", "resources": ["rm -rf x"]})
        elif "cancel" in step:
            emit("permission.replied", {"sessionID": SESSION,
                                        "requestID": f"p{step['cancel']}", "reply": "reject"})
        elif "question" in step:
            rid = f"q{questions}"
            questions += 1
            ask(rid)
            emit("question.asked", {"id": rid, "sessionID": SESSION, "questions": [
                {"question": step["question"], "header": "Q", "options": []}]})
            waits[rid][0].wait()
        elif "say" in step:
            part(f"prt_{says}", "msg_1", step["say"])
            says += 1
        elif "sleep" in step:
            time.sleep(step["sleep"])
        elif "result" in step:
            text.append(step["result"].replace("{last}", last))
        elif "error" in step:
            error = {"name": "UnknownError", "data": {"message": step["error"]}}
        elif "drop" in step:
            for q in list(streams):
                q.put(None)
    info = {"id": "msg_1", "sessionID": SESSION, "role": "assistant", "finish": "stop"}
    if error:
        info["error"] = error
    parts = [{"type": "step-start"}] + [{"type": "text", "text": t} for t in text]
    return {"info": info, "parts": parts}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def answer(self, status, value=None):
        data = b"" if value is None else json.dumps(value).encode()
        self.send_response(status)
        if status == 401:
            self.send_header("www-authenticate", 'Basic realm="Secure Area"')
        if data:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def events(self):
        q = queue.Queue()
        streams.append(q)  # before the head: nothing emitted after it is missed

        def chunk(text):
            data = text.encode()
            self.wfile.write(b"%x\r\n%s\r\n" % (len(data), data))
            self.wfile.flush()

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        try:
            chunk('data: {"type":"server.connected","properties":{}}\n\n')
            while (event := q.get()) is not None:
                text = f"data: {json.dumps(event)}\n\n"
                chunk(text[:len(text) // 2])
                chunk(text[len(text) // 2:])
        except OSError:
            pass
        streams.remove(q)
        self.close_connection = True  # dropped: no last chunk

    def route(self, method):
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            body = json.loads(raw) if raw else None
        except ValueError:
            body = None
        auth = self.headers.get("Authorization") == AUTH
        log({"method": method, "path": self.path, "auth": auth, "body": body})
        if not auth:
            return self.answer(401)
        parts = self.path.strip("/").split("/")
        if (method, self.path) == ("GET", "/global/health"):
            return self.answer(200, {"healthy": True, "version": "fake"})
        if (method, self.path) == ("GET", "/event"):
            return self.events()
        if (method, self.path) == ("POST", "/session"):
            return self.answer(200, {"id": SESSION, "title": (body or {}).get("title")})
        if method == "POST" and parts == ["session", SESSION, "message"]:
            prompt = ((body or {}).get("parts") or [{}])[0].get("text", "")
            return self.answer(200, turn(prompt))
        if method == "POST" and len(parts) == 3 and parts[0] in ("permission", "question") \
                and parts[2] == ("reply" if parts[0] == "permission" else "reject") \
                and parts[1] in waits:
            waits[parts[1]][1] = body
            waits[parts[1]][0].set()
            status = statuses.get(parts[1], 200)
            return self.answer(status, True if status == 200 else {"message": "no"})
        return self.answer(404, {"message": f"no route {method} {self.path}"})

    def do_GET(self):
        self.route("GET")

    def do_POST(self):
        self.route("POST")


server = ThreadingHTTPServer(("127.0.0.1", int(argv[4])), Handler)
server.daemon_threads = True
print(f"opencode server listening on http://127.0.0.1:{argv[4]}", flush=True)
server.serve_forever()
