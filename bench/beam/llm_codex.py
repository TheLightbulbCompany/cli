"""The answer and judge model, called through the Codex CLI under a ChatGPT subscription.

Selected with BEAM_LLM=codex; same interface as llm.py (complete_text, complete_json, close,
stops_the_run, ModelError, MODEL, REASONING_EFFORT).

Each call is one `codex exec --ephemeral -s read-only` in a fresh empty directory, with CODEX_HOME
set to an isolated home (BEAM_CODEX_HOME; never ~/.codex) and the ExaBase prompt as the sole
prompt argument. The measured deviation from past.dev's bare OpenRouter call is Codex's wrapper:
its instructions are replaced by WRAPPER_INSTRUCTIONS and its optional tools and context are
switched off (WRAPPER_FLAGS), which leaves about 7.4k tokens of wrapper per call (17.2k with
Codex's defaults). Codex's memories are off (`--disable memories`).

Pacing and safety:
- at most CONCURRENCY calls at once;
- a rate limit (429) or transport failure is retried with exponential backoff; a rate limit still
  failing after the retries, a usage-limit refusal, an auth refusal, or the call cap stops the run
  (resume later from the cache) instead of scoring questions 0;
- set_budget(n) caps the run's calls: the cap is checked BEFORE each call, retries included;
- set_cache(path) keeps every reply keyed by model, effort, schema and prompt: a resumed run
  re-reads them instead of paying again. The same file is the run's call log (usage per call).
"""

import asyncio
import contextvars
import hashlib
import json
import os
import re
import shutil
import tempfile
import time
from pathlib import Path

from log import log

MODEL = os.environ.get("BEAM_CODEX_MODEL", "gpt-5.6-luna")
REASONING_EFFORT = os.environ.get("BEAM_CODEX_EFFORT", "none")
REQUIRED_ENVIRONMENT = ("BEAM_CODEX_HOME",)
CONCURRENCY = 4
RETRY_DELAYS_SECONDS = (2, 4, 8, 16, 32)
TIMEOUT_SECONDS = 300

WRAPPER_INSTRUCTIONS = "Follow the user's instructions.\n"
DISABLED_FEATURES = ("memories", "plugins", "apps", "multi_agent", "image_generation", "browser_use",
                     "browser_use_external", "computer_use", "goals", "skill_search", "tool_suggest",
                     "shell_tool", "unified_exec", "view_image", "sleep_tool", "in_app_browser", "hooks")
WRAPPER_FLAGS = [arg for name in DISABLED_FEATURES for arg in ("--disable", name)] + [
    "-c", "include_permissions_instructions=false",
    "-c", "include_environment_context=false",
    "-c", 'web_search="disabled"',
    "-c", "skills.bundled.enabled=false",
]


class ModelError(RuntimeError):
    def __init__(self, message: str, *, retryable: bool, status: int | None = None, stop: bool = False) -> None:
        super().__init__(message)
        self.retryable = retryable
        self.status = status
        self.stop = stop


class BudgetExhausted(RuntimeError):
    stop = True


def stops_the_run(error: BaseException) -> bool:
    return bool(getattr(error, "stop", False)) or getattr(error, "status", None) in (401, 402, 403)


# --- Run state: the cap, the cache, the usage of the question being scored ---------------------

_cap: int | None = None
_calls = 0
_inflight = 0
_cache_path: Path | None = None
_cache: dict[str, dict] = {}
_slots = asyncio.Semaphore(CONCURRENCY)
_usage: contextvars.ContextVar[list | None] = contextvars.ContextVar("codex_usage", default=None)


def set_budget(calls: int | None) -> None:
    global _cap, _calls
    _cap, _calls = calls, 0


def calls_made() -> int:
    return _calls


def set_cache(path: Path | None) -> None:
    global _cache_path, _cache
    _cache_path, _cache = (Path(path) if path else None), {}
    if _cache_path and _cache_path.exists():
        for line in _cache_path.read_text("utf-8").splitlines():
            if line.strip():
                entry = json.loads(line)
                _cache[entry["key"]] = entry


def start_usage() -> list:
    """Collect the usage of every call made from here on in this task (one question)."""
    calls: list = []
    _usage.set(calls)
    return calls


def usage_totals(calls: list[dict]) -> dict:
    keys = ("input_tokens", "cached_input_tokens", "output_tokens", "reasoning_output_tokens")
    totals = {key: sum(c.get(key, 0) for c in calls if not c.get("cacheHit")) for key in keys}
    return {"calls": sum(1 for c in calls if not c.get("cacheHit")),
            "cacheHits": sum(1 for c in calls if c.get("cacheHit")), **totals}


# --- The interface -----------------------------------------------------------------------------

async def complete_text(prompt: str) -> str:
    text = await _complete(prompt, None)
    if not text.strip():
        raise ValueError("The model returned an empty answer")
    return text


async def complete_json(prompt: str, schema: dict) -> dict:
    strict = {"type": "object", "properties": schema["properties"], "required": schema["required"],
              "additionalProperties": False}
    text = await _complete(prompt, strict)
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        start, end = text.find("{"), text.rfind("}")
        if start >= 0 and end > start:
            return json.loads(text[start:end + 1])
        raise


async def close() -> None:
    pass


# --- Calls -------------------------------------------------------------------------------------

def _key(prompt: str, schema: dict | None) -> str:
    return hashlib.sha256(json.dumps([MODEL, REASONING_EFFORT, schema, prompt]).encode()).hexdigest()


def _record(entry: dict) -> None:
    sink = _usage.get()
    if sink is not None:
        sink.append(entry["usage"] | ({"cacheHit": True} if entry.get("cacheHit") else {}))


async def _complete(prompt: str, schema: dict | None) -> str:
    key = _key(prompt, schema)
    if key in _cache:
        _record(_cache[key] | {"cacheHit": True})
        return _cache[key]["reply"]
    async with _slots:
        for attempt in range(len(RETRY_DELAYS_SECONDS) + 1):
            await _take_call()
            try:
                reply, usage = await _exec_once(prompt, schema)
                break
            except ModelError as error:
                last = attempt == len(RETRY_DELAYS_SECONDS)
                if not error.retryable or error.stop or last:
                    if error.status == 429:
                        error.stop = True  # still rate-limited after the retries: stop, resume later
                    raise
                delay = RETRY_DELAYS_SECONDS[attempt]
                log(f"codex call failed ({str(error)[:120]}); retrying in {delay}s")
                await asyncio.sleep(delay)
    entry = {"key": key, "model": MODEL, "effort": REASONING_EFFORT, "json": schema is not None,
             "reply": reply, "usage": usage, "at": time.time()}
    _cache[key] = entry
    if _cache_path:
        with _cache_path.open("a", encoding="utf-8") as file:
            file.write(json.dumps(entry, ensure_ascii=False) + "\n")
    _record(entry)
    return reply


async def _take_call() -> None:
    """The cap is checked before the call is made, never after. At the cap, the calls already
    under way finish (and land in the cache) before the run is stopped, so none is paid for twice."""
    global _calls
    if _cap is not None and _calls >= _cap:
        while _inflight:
            await asyncio.sleep(0.2)
        raise BudgetExhausted(f"call cap of {_cap} reached; rerun to resume")
    _calls += 1


def _home() -> str:
    home = os.environ.get("BEAM_CODEX_HOME", "").strip()
    if not home:
        raise RuntimeError("BEAM_CODEX_HOME (an isolated Codex home, not ~/.codex) is required")
    if Path(home).resolve() == (Path.home() / ".codex").resolve():
        raise RuntimeError("BEAM_CODEX_HOME must not be ~/.codex")
    return home


async def _exec_once(prompt: str, schema: dict | None) -> tuple[str, dict]:
    global _inflight
    _inflight += 1
    try:
        return await _exec_in_scratch(prompt, schema)
    finally:
        _inflight -= 1


async def _exec_in_scratch(prompt: str, schema: dict | None) -> tuple[str, dict]:
    work = Path(tempfile.mkdtemp(prefix="beam-codex-", dir=os.environ.get("BEAM_CODEX_WORK") or None))
    try:
        cwd, io = work / "cwd", work / "io"
        cwd.mkdir()
        io.mkdir()
        (io / "instructions.md").write_text(WRAPPER_INSTRUCTIONS)
        argv = [os.environ.get("BEAM_CODEX_BIN", "codex"), "exec", "--json", "--skip-git-repo-check",
                "--ephemeral", "-s", "read-only", "-m", MODEL, "-c", f"model_reasoning_effort={REASONING_EFFORT}",
                "-c", f'model_instructions_file="{io / "instructions.md"}"', *WRAPPER_FLAGS,
                "-o", str(io / "last.txt")]
        if schema is not None:
            (io / "schema.json").write_text(json.dumps(schema))
            argv += ["--output-schema", str(io / "schema.json")]
        argv += ["--", prompt]
        env = {k: v for k, v in os.environ.items() if k not in ("OPENAI_API_KEY", "CODEX_API_KEY")}
        env["CODEX_HOME"] = _home()
        try:
            process = await asyncio.create_subprocess_exec(
                *argv, cwd=cwd, env=env, stdin=asyncio.subprocess.DEVNULL,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            stdout, stderr = await asyncio.wait_for(process.communicate(), TIMEOUT_SECONDS)
        except asyncio.TimeoutError:
            process.kill()
            raise ModelError(f"codex timed out after {TIMEOUT_SECONDS}s", retryable=True)
        except asyncio.CancelledError:
            process.kill()
            raise
        events = [json.loads(line) for line in stdout.decode("utf-8", "replace").splitlines()
                  if line.startswith("{")]
        usage = next((e["usage"] for e in events if e.get("type") == "turn.completed"), None)
        # An "error" event alone can be Codex's own reconnect notice; a completed turn wins over it.
        failure = next((e for e in events if e.get("type") == "turn.failed"), None) or (
            None if usage is not None else next((e for e in events if e.get("type") == "error"), None))
        if process.returncode != 0 or usage is None or failure is not None:
            message = json.dumps(failure) if failure else stderr.decode("utf-8", "replace")[-1000:]
            raise _classify(message, process.returncode)
        out = io / "last.txt"
        reply = out.read_text("utf-8") if out.exists() else ""
        if not reply:
            reply = next((e["item"]["text"] for e in reversed(events)
                          if e.get("type") == "item.completed" and e["item"].get("type") == "agent_message"), "")
        return reply, usage
    finally:
        shutil.rmtree(work, ignore_errors=True)


def _classify(message: str, returncode: int | None) -> ModelError:
    found = re.search(r'status\\*"?\s*:\s*(\d{3})', message)
    status = int(found.group(1)) if found else None
    lowered = message.lower()
    if "usage_limit" in lowered or "usage limit" in lowered:
        return ModelError(f"codex usage limit: {message[:300]}", retryable=False, status=429, stop=True)
    if status == 429 or "rate limit" in lowered or "too many requests" in lowered:
        return ModelError(f"codex rate-limited: {message[:300]}", retryable=True, status=429)
    if status in (401, 402, 403):
        return ModelError(f"codex refused the account: {message[:300]}", retryable=False, status=status)
    if status is not None and status < 500:
        return ModelError(f"codex returned {status}: {message[:300]}", retryable=False, status=status)
    return ModelError(f"codex failed (exit {returncode}): {message[:300]}", retryable=True, status=status)


def memories_are_empty(home: str | None = None) -> bool:
    """Codex's own memory store in the isolated home holds nothing (or does not exist)."""
    import sqlite3
    path = Path(home or _home()) / "memories_1.sqlite"
    if not path.exists():
        return True
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
        tables = [r[0] for r in db.execute("select name from sqlite_master where type='table'")]
        return all(db.execute(f'select count(*) from "{t}"').fetchone()[0] == 0
                   for t in tables if t in ("jobs", "stage1_outputs"))
