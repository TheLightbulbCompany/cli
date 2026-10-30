import asyncio
import json

import pytest

import llm_codex
import prompts
import run
import scoring
from test_free_adapter import ROW
from test_llm_codex import fake  # noqa: F401  (the fake codex binary)
from test_verify import fake_cache  # noqa: F401


@pytest.fixture
def harness(fake, fake_cache, tmp_path, monkeypatch):  # noqa: F811
    prompts.ensure_exabase_prompts(fake_cache, fetch=lambda: __import__("test_verify").FAKE)
    monkeypatch.setenv("FAKE_REPLY", '{"score": 1, "reason": "ok"}')
    monkeypatch.setattr(run, "llm", llm_codex)
    monkeypatch.setattr(scoring, "llm", llm_codex)
    monkeypatch.setattr(run, "load_rows", lambda split: ([ROW], "rev"))
    monkeypatch.setattr(run, "REQUIRED_ENVIRONMENT", ())
    output = tmp_path / "out"

    def go(budget):
        args = run.parse_arguments(["--backend", "fake", "--output", str(output), "--budget-calls", str(budget)])
        asyncio.run(run.main_async(args))
    return go, output, fake


def test_the_cap_stops_before_the_next_paid_call_and_a_rerun_does_not_pay_twice(harness):
    go, output, calls = harness
    with pytest.raises(BaseExceptionGroup) as stopped:  # answer paid, then the judge call is refused
        go(1)
    assert stopped.group_contains(llm_codex.BudgetExhausted)
    assert len(calls()) == 1
    assert not (output / "results.jsonl").exists()

    go(1)  # resumes: the answer is re-read from the call log, only the judge is paid
    assert len(calls()) == 2
    [row] = [json.loads(line) for line in (output / "results.jsonl").read_text().splitlines()]
    assert row["score"] == 1.0 and row["usage"]["calls"] == 1 and row["usage"]["cacheHits"] == 1
    summary = json.loads((output / "summary.json").read_text())
    assert summary["modelPath"] == "codex-cli" and summary["usage"]["calls"] == 1

    go(0)  # finished: nothing left to call
    assert len(calls()) == 2
