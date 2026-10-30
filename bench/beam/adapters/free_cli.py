"""The free CLI engine: a `lightbulb daemon` (a `--features bench` build) per conversation, in a
Linux container with no network, driven over its IPC socket.

- Turns go in through `bench.ingest_turns` with the dataset's times; the note-taker (notetaker.py)
  reads them 20 at a time and its ops go in through the real `memory.remember`, with `when` the
  time of the turn each op is grounded on and `replaces` the entry this run last wrote for the
  same subject when the op says it updates it.
- `ingestion_status` is `bench.settle`: every vector embedded before a question is asked.
- `recall` is `memory.recall` with `as_of` and 50 turns; results are its facts, in its order,
  then its turn excerpts, in the fork's result shape.

Times: the daemon keeps unix seconds; the dataset places turns at a session's moment plus the
ordinal in microseconds. Each turn goes in at that moment's second plus the ordinal in seconds,
which keeps the order and the day; the adapter maps an echoed `as_of` back to the dataset moment.

Transport: one `docker exec -i` bridge per run (perl, in the base image) carries JSON lines to
each daemon's socket. Environment: BEAM_FREE_CONTAINER (default lb-bench), BEAM_FREE_BIN
(default /out/release/lightbulb), BEAM_FREE_MODEL (default /models/model).
"""

import asyncio
import json
import os
import re
from datetime import datetime, timedelta, timezone

import notetaker
from adapters.base import BackendError, RecallPage, Turn
from dataset import utc_iso
from log import log

BRIDGE = r'''use IO::Socket::UNIX; $| = 1;
while (my $l = <STDIN>) { chomp $l; my ($p, $j) = split /\t/, $l, 2;
  my $s = IO::Socket::UNIX->new(Type => SOCK_STREAM(), Peer => $p);
  if (!$s) { print "{\"ok\":false,\"error\":\"connect\"}\n"; next; }
  print $s "$j\n"; my $r = <$s>; close $s;
  print(defined $r ? $r : "{\"ok\":false,\"error\":\"eof\"}\n"); }'''

TURN_LIMIT = 50
# BEAM_FREE_SHIPPED=1: what an agent is handed, the page text of a release's recall (5 turns,
# 1,600 characters), as one result, instead of 50 facts and 50 excerpts.
SHIPPED = os.environ.get("BEAM_FREE_SHIPPED") == "1"
# BEAM_FREE_EXCERPT_CHARS=N: ask recall for N-character turn excerpts (a loop/turn-excerpt-length build).
EXCERPT_CHARS = int(os.environ.get("BEAM_FREE_EXCERPT_CHARS") or 0)
CONFIG = "lan = false\ncapture_turns_days = 36500\n"


def seconds(at: datetime) -> int:
    """The dataset's moment as the daemon's second: ordinals go from microseconds to seconds."""
    base = at.replace(microsecond=0)
    if at.microsecond >= 3600:
        raise BackendError("over 3,600 turns in a conversation: the second-per-turn mapping would cross sessions")
    return int(base.timestamp()) + at.microsecond


class FreeCliBackend:
    def __init__(self, container: str | None = None) -> None:
        self.container = container or os.environ.get("BEAM_FREE_CONTAINER", "lb-bench")
        self.binary = os.environ.get("BEAM_FREE_BIN", "/out/release/lightbulb")
        self.model = os.environ.get("BEAM_FREE_MODEL", "/models/model")
        self._bridge: asyncio.subprocess.Process | None = None
        self._lock = asyncio.Lock()
        self.moments: dict[str, dict[int, datetime]] = {}  # project -> second -> dataset moment
        self.subjects: dict[str, dict[str, str]] = {}  # project -> subject -> last entry id
        self.stats = {"turns": 0, "noteCalls": 0, "noteFailures": 0, "remembered": 0, "refused": 0}

    # --- Transport (tests replace these two) ---------------------------------------------------

    async def ask(self, project: str, body: dict) -> dict:
        async with self._lock:
            if self._bridge is None or self._bridge.returncode is not None:
                self._bridge = await asyncio.create_subprocess_exec(
                    "docker", "exec", "-i", self.container, "perl", "-e", BRIDGE,
                    stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                    limit=1 << 24)  # one JSON line per answer: an export can pass 64 KiB
            line = f"/bench/{project}/sock\t{json.dumps({'id': 1, **body})}\n"
            self._bridge.stdin.write(line.encode())
            await self._bridge.stdin.drain()
            reply = await self._bridge.stdout.readline()
        if not reply:
            raise BackendError("the docker bridge closed")
        answer = json.loads(reply)
        if not answer.get("ok"):
            raise BackendError(f"{body.get('op')} failed: {answer}")
        return answer

    async def start_daemon(self, project: str) -> None:
        home = f"/bench/{project}"
        script = (f"mkdir -p {home} && chmod 700 {home} && printf '{CONFIG}' > {home}/config.toml && "
                  f"ln -sfn {self.model} {home}/model && "
                  f"LIGHTBULB_CONNECT_HOME={home} HOME={home} USER=bench LIGHTBULB_CONNECT_NO_LAUNCHCTL=1 "
                  f"LIGHTBULB_CONNECT_DEBUG_NO_DEFAULT_RELAYS=1 LIGHTBULB_CONNECT_DEBUG_NO_TELEMETRY=1 "
                  f"LIGHTBULB_NO_TELEMETRY=1 DO_NOT_TRACK=1 LIGHTBULB_API=http://127.0.0.1:9 "
                  f"nohup {self.binary} daemon </dev/null >{home}/daemon.log 2>&1 &")
        process = await asyncio.create_subprocess_exec("docker", "exec", self.container, "sh", "-c", script)
        if await process.wait() != 0:
            raise BackendError(f"could not start the daemon for {project}")
        for _ in range(100):
            try:
                await self.ask(project, {"op": "ping"})
                return
            except BackendError:
                await asyncio.sleep(0.2)
        raise BackendError(f"the daemon for {project} did not answer")

    # --- The interface -------------------------------------------------------------------------

    async def create_project(self, name: str) -> str:
        project = re.sub(r"[^A-Za-z0-9_-]", "-", name)[-40:]  # the socket path stays short
        self.moments[project], self.subjects[project] = {}, {}
        await self.start_daemon(project)
        return project

    async def register_identities(self, project: str, speakers: list[dict]) -> None:
        pass  # one person per daemon: the user's turns and the assistant's are told apart by role

    async def ingest_batch(self, project: str, turns: list[Turn]) -> None:
        for turn in turns:
            self.moments[project][seconds(turn.at)] = turn.at
        await self.ask(project, {"op": "bench.ingest_turns", "session": f"bench:{project}",
                                 "turns": [{"role": t.role, "text": t.text, "at": seconds(t.at)} for t in turns]})
        self.stats["turns"] += len(turns)
        for start in range(0, len(turns), notetaker.TURNS_PER_CALL):
            await self.take_notes(project, turns[start:start + notetaker.TURNS_PER_CALL])

    async def take_notes(self, project: str, turns: list[Turn]) -> None:
        known = self.subjects[project]
        self.stats["noteCalls"] += 1
        try:
            ops = await notetaker.notes(turns, list(known))
        except (notetaker.llm.ModelError, ValueError) as error:
            if notetaker.llm.stops_the_run(error):
                raise
            self.stats["noteFailures"] += 1
            log(f"note-taker failed on {len(turns)} turns ({str(error)[:120]}); they go in as turns only", project)
            return
        for op in ops:
            replaces = [known[op["subject"].lower()]] if op["updates"] and op["subject"].lower() in known else []
            stored = await self.ask(project, {"op": "memory.remember", "subject": op["subject"], "fact": op["fact"],
                                              "kind": op["kind"], "when": seconds(turns[op["turn"]].at),
                                              "replaces": replaces})
            if stored.get("stored"):
                known[op["subject"].lower()] = stored["entry_id"]
                self.stats["remembered"] += 1
            else:
                self.stats["refused"] += 1

    async def ingestion_status(self, project: str) -> str:
        await self.ask(project, {"op": "bench.settle"})
        return "done"

    async def recall(self, project: str, query: str, as_of: datetime | None, identity: str) -> RecallPage:
        body = {"op": "memory.recall", "question": query} if SHIPPED else \
            {"op": "memory.recall", "question": query, "turn_limit": TURN_LIMIT}
        if as_of is not None:
            body["as_of"] = seconds(as_of)
        if EXCERPT_CHARS and not SHIPPED:
            body["excerpt_chars"] = EXCERPT_CHARS
        page = await self.ask(project, body)
        echoed = self.moments[project].get(page["as_of"]) if page.get("as_of") is not None else None
        known = self.moments[project]

        def moment(second: int) -> str:  # the dataset's moment where the second is a turn's
            return utc_iso(known.get(second) or datetime.fromtimestamp(second, timezone.utc))
        if SHIPPED:
            results = [{"kind": "page", "content": page["page"]}] if page.get("page") else []
            return RecallPage(results, echoed)
        results = [fact_result(f, moment) for f in page.get("facts") or []]
        results += [turn_result(t, moment) for t in page.get("turns") or []]
        return RecallPage(results, echoed)

    async def close(self) -> None:
        log(f"free engine: {json.dumps(self.stats)}")
        if self._bridge is not None and self._bridge.returncode is None:
            self._bridge.stdin.close()
            await self._bridge.wait()


def fact_result(fact: dict, moment) -> dict:
    # A build with loop/fact-dates gives the fact's own line, dated; older builds do not.
    content = fact.get("line") or f"{fact['subject']}: {fact['value']}"
    if fact.get("history"):
        was = fact["history"][-1]
        content += f" (was {was['value']}, {moment(was['when'])[:10]})"
    result = {"kind": fact["kind"], "content": content, "validFrom": moment(fact["when"])}
    if fact.get("status") == "contested":
        result["status"] = "contested"
    return result


def turn_result(turn: dict, moment) -> dict:
    return {"kind": "turn", "content": f"{turn['role']}: {turn['excerpt']}", "occurredAt": moment(turn["at"])}
