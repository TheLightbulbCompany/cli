"""The one interface every memory backend implements, in place of past_api.py.

The runner, not the backend, holds the evidence budget: recall_json() trims a backend's page to
past.dev's 100 results / 8,000 tokens and serialises it the way past.dev's recall JSON reads
(asOf, usedEvidenceTokens, results), so the answer model sees the same shape from every backend.
"""

import math
from dataclasses import dataclass
from datetime import datetime
from typing import Literal, Protocol

from dataset import utc_iso

# One recall page: at most 100 results, within 8,000 tokens of evidence (past_api.py's constants).
RECALL_LIMIT = 100
RECALL_MAX_TOKENS = 8_000


@dataclass
class Turn:
    session_id: str
    ordinal: int
    speaker: str
    role: str
    text: str
    at: datetime


@dataclass
class RecallPage:
    results: list[dict]  # in rank order, each in past.dev's result shape: content, sources[].excerpts, ...
    as_of: datetime | None  # the moment the backend answered as of, echoed as asOf


class MemoryBackend(Protocol):
    async def create_project(self, name: str) -> str: ...
    async def register_identities(self, project: str, speakers: list[dict]) -> None: ...
    async def ingest_batch(self, project: str, turns: list[Turn]) -> None: ...
    async def ingestion_status(self, project: str) -> Literal["done", "pending"]: ...
    async def recall(self, project: str, query: str, as_of: datetime | None, identity: str) -> RecallPage: ...
    async def close(self) -> None: ...


def evidence_tokens(result: dict) -> int:
    """past.dev's count: ceil((content + excerpt characters) / 4) per result. It reproduces the
    usedEvidenceTokens of all 400 rows of their published 100K results."""
    chars = len(result.get("content") or "")
    chars += sum(len(e) for s in result.get("sources") or [] for e in s.get("excerpts") or [])
    return math.ceil(chars / 4)


def recall_json(page: RecallPage) -> dict:
    """The page as the answer model reads it: results in rank order while they fit."""
    if [r.get("kind") for r in page.results] == ["page"]:
        # A backend's own page, already trimmed by it (free engine v0.7.1: its trim_evidence is
        # the page's only cutter): the answerer reads it exactly as an agent gets it.
        used = evidence_tokens(page.results[0])
        return {"asOf": utc_iso(page.as_of) if page.as_of else None, "usedEvidenceTokens": used,
                "results": page.results}
    kept, used = [], 0
    for result in page.results[:RECALL_LIMIT]:
        tokens = evidence_tokens(result)
        if used + tokens > RECALL_MAX_TOKENS:
            break  # ponytail: stops at the first result that overflows; past.dev's server rule is unpublished
        kept.append(result)
        used += tokens
    return {"asOf": utc_iso(page.as_of) if page.as_of else None, "usedEvidenceTokens": used, "results": kept}


class BackendError(RuntimeError):
    """A recall the backend could not give: the question scores 0, as a failed Past call did."""
