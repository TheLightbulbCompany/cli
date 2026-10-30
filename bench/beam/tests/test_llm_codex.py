import asyncio
import json
import os
import stat
import sys

import pytest

import llm_codex

FAKE_CODEX = r'''#!PYTHON
import json, os, sys
from pathlib import Path
state = Path(os.environ["FAKE_STATE"])
calls = json.loads(state.read_text()) if state.exists() else []
argv = sys.argv[1:]
calls.append({"argv": argv, "cwd": os.getcwd(), "cwd_files": os.listdir("."),
              "home": os.environ.get("CODEX_HOME"), "openai_key": "OPENAI_API_KEY" in os.environ,
              "schema": json.loads(Path(argv[argv.index("--output-schema") + 1]).read_text())
                        if "--output-schema" in argv else None})
state.write_text(json.dumps(calls))
fail = int(os.environ.get("FAKE_FAIL", "0"))
print(json.dumps({"type": "thread.started", "thread_id": "t"}))
if len(calls) <= fail:
    print(json.dumps({"type": "error", "message": json.dumps({"status": 429, "error": {"message": "Too Many Requests"}})}))
    print(json.dumps({"type": "turn.failed", "error": {"message": "rate limited"}}))
    sys.exit(1)
reply = os.environ.get("FAKE_REPLY", "the answer")
if os.environ.get("FAKE_RECONNECT"):
    print(json.dumps({"type": "error", "message": "Reconnecting... 2/5 (unexpected status 403 Forbidden)"}))
Path(argv[argv.index("-o") + 1]).write_text(reply)
print(json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": reply}}))
print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 7400, "cached_input_tokens": 100,
                  "output_tokens": 12, "reasoning_output_tokens": 0}}))
'''


@pytest.fixture
def fake(tmp_path, monkeypatch):
    binary = tmp_path / "bin" / "codex"
    binary.parent.mkdir()
    binary.write_text(FAKE_CODEX.replace("#!PYTHON", "#!" + sys.executable))
    binary.chmod(binary.stat().st_mode | stat.S_IEXEC)
    monkeypatch.setenv("PATH", f"{binary.parent}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.setenv("BEAM_CODEX_HOME", str(tmp_path / "home"))
    monkeypatch.setenv("BEAM_CODEX_WORK", str(tmp_path))
    monkeypatch.setenv("FAKE_STATE", str(tmp_path / "calls.json"))
    monkeypatch.setenv("OPENAI_API_KEY", "must-not-reach-codex")
    monkeypatch.setattr(llm_codex, "RETRY_DELAYS_SECONDS", (0, 0, 0))
    llm_codex.set_budget(None)
    llm_codex.set_cache(None)

    def calls():
        path = tmp_path / "calls.json"
        return json.loads(path.read_text()) if path.exists() else []
    return calls


def test_the_backend_is_called_with_the_exact_prompt(fake):
    prompt = "-starts with a dash\nand has\nlines {\"x\": 1}"
    assert asyncio.run(llm_codex.complete_text(prompt)) == "the answer"
    [call] = fake()
    argv = call["argv"]
    assert argv[-2:] == ["--", prompt]  # the prompt is the sole positional argument, verbatim
    assert argv[:2] == ["exec", "--json"]
    for flag in ("--ephemeral", "--skip-git-repo-check"):
        assert flag in argv
    assert argv[argv.index("-s") + 1] == "read-only"
    assert argv[argv.index("-m") + 1] == "gpt-5.6-luna"
    assert "model_reasoning_effort=none" in argv
    assert "memories" in [argv[i + 1] for i, a in enumerate(argv) if a == "--disable"]
    assert call["cwd_files"] == []  # an empty scratch directory
    assert call["home"].endswith("home") and not call["openai_key"]


def test_json_output_is_parsed(fake, monkeypatch):
    monkeypatch.setenv("FAKE_REPLY", '{"score": 0.5, "reason": "partly"}')
    schema = {"properties": {"score": {"type": "number"}, "reason": {"type": "string"}}, "required": ["score", "reason"]}
    assert asyncio.run(llm_codex.complete_json("judge this", schema)) == {"score": 0.5, "reason": "partly"}
    assert fake()[0]["schema"]["additionalProperties"] is False


def test_a_rate_limit_is_retried_then_surfaced(fake, monkeypatch):
    monkeypatch.setenv("FAKE_FAIL", "1")
    assert asyncio.run(llm_codex.complete_text("q1")) == "the answer"
    assert len(fake()) == 2

    monkeypatch.setenv("FAKE_FAIL", "99")
    with pytest.raises(llm_codex.ModelError) as error:
        asyncio.run(llm_codex.complete_text("q2"))
    assert error.value.status == 429 and llm_codex.stops_the_run(error.value)
    assert len(fake()) == 2 + 4  # four attempts in all


def test_the_cap_stops_before_the_call(fake):
    llm_codex.set_budget(1)
    asyncio.run(llm_codex.complete_text("first"))
    with pytest.raises(llm_codex.BudgetExhausted) as error:
        asyncio.run(llm_codex.complete_text("second"))
    assert llm_codex.stops_the_run(error.value)
    assert len(fake()) == 1  # the second call was never made


def test_usage_is_recorded_and_a_cached_reply_is_not_paid_again(fake, tmp_path):
    llm_codex.set_cache(tmp_path / "calls.jsonl")

    async def question():
        calls = llm_codex.start_usage()
        await llm_codex.complete_text("same prompt")
        return llm_codex.usage_totals(calls)
    assert asyncio.run(question())["input_tokens"] == 7400
    llm_codex.set_cache(tmp_path / "calls.jsonl")  # a resumed run re-reads the file
    llm_codex.set_budget(0)
    again = asyncio.run(question())
    assert again["calls"] == 0 and again["cacheHits"] == 1 and len(fake()) == 1


def test_a_reconnect_notice_in_a_completed_turn_is_not_a_failure(fake, monkeypatch):
    monkeypatch.setenv("FAKE_RECONNECT", "1")
    assert asyncio.run(llm_codex.complete_text("q")) == "the answer"
    assert len(fake()) == 1
