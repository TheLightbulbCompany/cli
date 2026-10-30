import asyncio
import json
from datetime import datetime, timedelta, timezone

import pytest

import notetaker
import run
from adapters.base import Turn
from adapters.free_cli import FreeCliBackend, seconds
from dataset import build_conversation

ROW = {
    "conversation_id": "t1",
    "user_profile": {"user_info": "Name: Ann Lee"},
    "chat": [[{"role": "user" if i % 2 == 0 else "assistant", "content": f"turn {i} about the garden",
               "id": i, "time_anchor": "March-15-2024" if i == 0 else None} for i in range(100)]],
    "probing_questions": str({"information_extraction": [
        {"question": "ZZQ-UNIQUE-QUESTION what grows?", "answer": "GOLD-XYZ-ANSWER", "rubric": ["RUBRIC-QQ"]}]}),
}


class Fake(FreeCliBackend):
    def __init__(self):
        super().__init__("none")
        self.sent: list[dict] = []
        self.prompts: list[str] = []
        self.entries = 0

    async def start_daemon(self, project):
        pass

    async def ask(self, project, body):
        self.sent.append(body)
        if body["op"] == "memory.remember":
            self.entries += 1
            return {"ok": True, "stored": True, "entry_id": f"e{self.entries}"}
        if body["op"] == "memory.recall":
            return {"ok": True, "as_of": body.get("as_of"), "turns": [{"role": "user", "excerpt": "x", "at": 5}],
                    "facts": [{"subject": "garden", "value": "roses", "kind": "fact", "when": 5, "status": "current",
                               "history": [{"value": "tulips", "when": 1}]}]}
        return {"ok": True}


@pytest.fixture
def backend(monkeypatch):
    fake = Fake()

    async def complete_json(prompt, schema):
        fake.prompts.append(prompt)
        return {"ops": [{"subject": "garden", "fact": "Ann grows roses", "kind": "fact", "updates": True, "turn": 3}]}
    monkeypatch.setattr(notetaker.llm, "complete_json", complete_json)
    return fake


def test_the_note_taker_never_sees_a_question_or_an_answer(backend):
    conversation = build_conversation(ROW)
    assert conversation.questions[0].text.startswith("ZZQ")

    async def go():
        project = await backend.create_project("beam-100k-t1")
        await run.ingest(backend, project, conversation)
    asyncio.run(go())
    assert len(backend.prompts) == 5  # 100 turns, 20 per call
    for prompt in backend.prompts:
        assert "ZZQ" not in prompt and "GOLD-XYZ" not in prompt and "RUBRIC-QQ" not in prompt
        assert "about the garden" in prompt


def test_a_100_turn_session_ingests_in_order_and_ops_carry_when(backend):
    conversation = build_conversation(ROW)
    backend.moments["p"], backend.subjects["p"] = {}, {}
    asyncio.run(run.ingest(backend, "p", conversation))
    ingested = [b for b in backend.sent if b["op"] == "bench.ingest_turns"]
    ats = [t["at"] for b in ingested for t in b["turns"]]
    texts = [t["text"] for b in ingested for t in b["turns"]]
    assert texts == [f"turn {i} about the garden" for i in range(100)]
    assert ats == sorted(ats) and len(set(ats)) == 100
    remembers = [b for b in backend.sent if b["op"] == "memory.remember"]
    # each op is dated by the turn it cites (turn 3 of each 20), and an update replaces the last entry
    assert remembers[0]["when"] == ats[2] and remembers[1]["when"] == ats[22]
    assert remembers[0]["replaces"] == [] and remembers[1]["replaces"] == ["e1"]


def test_recall_sends_as_of_and_echoes_the_dataset_moment(backend):
    at = datetime(2024, 3, 15, 12, tzinfo=timezone.utc) + timedelta(microseconds=7)
    backend.moments["p"], backend.subjects["p"] = {}, {}
    asyncio.run(backend.ingest_batch("p", [Turn("s0", 7, "Ann", "user", "hi", at)]))
    page = asyncio.run(backend.recall("p", "what grows?", at, "ann"))
    assert backend.sent[-1]["as_of"] == seconds(at) == int(at.replace(microsecond=0).timestamp()) + 7
    assert page.as_of == at
    assert page.results[0]["content"].startswith("garden: roses (was tulips, 1970-01-01)")
    assert page.results[1] == {"kind": "turn", "content": "user: x", "occurredAt": "1970-01-01T00:00:05Z"}


def test_the_shipped_page_is_one_result_with_the_release_knobs(backend, monkeypatch):
    import adapters.free_cli as free
    monkeypatch.setattr(free, "SHIPPED", True)
    backend.moments["p"], backend.subjects["p"] = {}, {}
    real = backend.ask

    async def ask(project, body):
        answer = await real(project, body)
        return {**answer, "page": "Current:\ngarden: roses (Ann, 2024-03-15)"} if body["op"] == "memory.recall" else answer
    backend.ask = ask
    page = asyncio.run(backend.recall("p", "what grows?", None, "ann"))
    assert "turn_limit" not in backend.sent[-1] and "budget_chars" not in backend.sent[-1]
    assert page.results == [{"kind": "page", "content": "Current:\ngarden: roses (Ann, 2024-03-15)"}]
