import hashlib
from pathlib import Path

import pytest

import prompts

GOOD = b"def answer_prompt(): ...\n"


def test_verify_prompts_rejects_a_changed_file(tmp_path, monkeypatch):
    monkeypatch.setattr(prompts, "SOURCE_SHA256", hashlib.sha256(GOOD).hexdigest())
    assert prompts.ensure_exabase_prompts(tmp_path, fetch=lambda: GOOD).read_bytes() == GOOD
    tampered = bytearray(GOOD)
    tampered[0] ^= 1
    (tmp_path / prompts.CACHED_NAME).write_bytes(bytes(tampered))
    with pytest.raises(prompts.PromptsHashMismatch):
        prompts.ensure_exabase_prompts(tmp_path, fetch=lambda: GOOD)
    with pytest.raises(prompts.PromptsHashMismatch):
        prompts.ensure_exabase_prompts(tmp_path / "fresh", fetch=lambda: bytes(tampered))


def test_fork_pins_a_commit():
    import re
    readme = (Path(__file__).parent.parent / "README.md").read_text()
    assert re.search(r"pastdotdev/benchmarks.*\b[0-9a-f]{40}\b", readme, re.S)


FAKE = '''
from ._cache import dataset_cache_dir
from ..llm.base import LLM, Schema

class BEAMDataset:
    def build_rag_prompt(self, query, context, task_type, split, category=None, meta=None):
        return f"ANSWER[{category}] {query} | {context} | {sorted((meta or {}))}"

    @staticmethod
    def _llm_equivalence(ref_item, sys_item, llm):
        schema = Schema(
            properties={"answer": {"type": "string", "description": "YES or NO"}},
            required=["answer"],
        )
        llm.generate(f"EQUIV {ref_item} {sys_item}", schema)

    @staticmethod
    def _rubric_item_score(query, answer, rubric_item, llm):
        schema = Schema(
            properties={"score": {"type": "number", "description": "Score: 0.0, 0.5, or 1.0"}, "reason": {"type": "string"}},
            required=["score", "reason"],
        )
        llm.generate(f"RUBRIC {query} {answer} {rubric_item}", schema)
'''.encode()


@pytest.fixture
def fake_cache(tmp_path, monkeypatch):
    monkeypatch.setattr(prompts, "SOURCE_SHA256", hashlib.sha256(FAKE).hexdigest())
    monkeypatch.setattr(prompts, "_cache_dir", tmp_path)
    monkeypatch.setattr(prompts, "_loaded", None)
    return tmp_path


def test_prompts_resolve_from_a_verified_cache(fake_cache):
    prompts.ensure_exabase_prompts(fake_cache, fetch=lambda: FAKE)
    assert prompts.answer_prompt("Q", "C", "abstention", {"k": 1}) == "ANSWER[abstention] Q | C | ['k']"
    assert prompts.rubric_item_prompt("q", "a", "r") == "RUBRIC q a r"
    assert prompts.equivalence_prompt("x", "y") == "EQUIV x y"
    assert prompts.EQUIVALENCE_SCHEMA["required"] == ["answer"]
    assert prompts.RUBRIC_SCHEMA["required"] == ["score", "reason"]


def test_prompts_without_a_cache_raise_a_clear_error(fake_cache):
    with pytest.raises(RuntimeError, match=r"ensure_exabase_prompts.*" ) as e:
        prompts.answer_prompt
    assert str(fake_cache) in str(e.value)


def test_verify_prompts_passes_against_the_fake(fake_cache, monkeypatch):
    import verify
    monkeypatch.setattr(verify, "download_exabase_adapter", lambda: FAKE)
    assert verify.verify_prompts() == []
