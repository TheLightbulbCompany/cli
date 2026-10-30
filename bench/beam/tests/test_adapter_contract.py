import asyncio
import json
import math
from datetime import datetime, timezone

import pytest

import adapters
from adapters.base import RECALL_LIMIT, RECALL_MAX_TOKENS, RecallPage, Turn, evidence_tokens, recall_json
from adapters.fake import FakeBackend

AT = datetime(2024, 4, 25, 12, 0, 0, 187, tzinfo=timezone.utc)


def result(chars: int, excerpt: int = 0) -> dict:
    return {"content": "x" * chars, "sources": [{"excerpts": ["y" * excerpt]}] if excerpt else []}


def test_tokens_are_counted_as_past_counts_them():
    # past.dev's usedEvidenceTokens is ceil((content + excerpts) / 4) per result: 400/400 of
    # their published 100K rows match this rule.
    assert evidence_tokens(result(5, 3)) == 2
    assert evidence_tokens(result(4)) == 1


def test_recall_page_is_trimmed_to_100_results_and_8000_tokens():
    many = recall_json(RecallPage([result(4) for _ in range(150)], AT))
    assert len(many["results"]) == RECALL_LIMIT
    assert many["usedEvidenceTokens"] == 100

    big = recall_json(RecallPage([result(4000) for _ in range(20)], AT))  # 1,000 tokens each
    assert len(big["results"]) == 8
    assert big["usedEvidenceTokens"] == RECALL_MAX_TOKENS

    few = recall_json(RecallPage([result(40, 40)], AT))
    assert few["usedEvidenceTokens"] == 20 and len(few["results"]) == 1


def test_as_of_is_echoed_as_asOf():
    page = recall_json(RecallPage([], AT))
    assert list(page) == ["asOf", "usedEvidenceTokens", "results"]  # past.dev's key order
    assert page["asOf"] == "2024-04-25T12:00:00.000187Z"
    assert recall_json(RecallPage([], None))["asOf"] is None


def test_fake_backend_recalls_turns_up_to_as_of():
    async def go():
        backend = FakeBackend()
        project = await backend.create_project("p")
        await backend.ingest_batch(project, [
            Turn("s0", 0, "Ann", "user", "early", datetime(2024, 1, 1, tzinfo=timezone.utc)),
            Turn("s1", 1, "Ann", "user", "late", datetime(2024, 2, 1, tzinfo=timezone.utc)),
        ])
        assert await backend.ingestion_status(project) == "done"
        page = await backend.recall(project, "q", datetime(2024, 1, 15, tzinfo=timezone.utc), "ann")
        return page
    page = asyncio.run(go())
    assert [r["content"] for r in page.results] == ["Ann: early"]
    assert page.as_of == datetime(2024, 1, 15, tzinfo=timezone.utc)


def test_run_py_uses_the_named_backend(monkeypatch):
    import run
    assert isinstance(adapters.load("fake"), FakeBackend)
    with pytest.raises(SystemExit):
        adapters.load("nope")
    args = run.parse_arguments(["--backend", "fake", "--split", "100k"])
    assert args.backend == "fake"


def test_a_backends_own_page_is_read_as_it_came():
    page = recall_json(RecallPage([{"kind": "page", "content": "x" * 36_000}], AT))
    assert len(page["results"]) == 1 and page["usedEvidenceTokens"] == 9_000
