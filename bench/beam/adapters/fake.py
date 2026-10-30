"""A backend for tests and dry runs: it keeps the raw turns and recalls them, newest first."""

from datetime import datetime

from adapters.base import RecallPage, Turn
from dataset import utc_iso


class FakeBackend:
    def __init__(self) -> None:
        self.turns: dict[str, list[Turn]] = {}

    async def create_project(self, name: str) -> str:
        self.turns[name] = []
        return name

    async def register_identities(self, project: str, speakers: list[dict]) -> None:
        pass

    async def ingest_batch(self, project: str, turns: list[Turn]) -> None:
        self.turns[project] += turns

    async def ingestion_status(self, project: str) -> str:
        return "done"

    async def recall(self, project: str, query: str, as_of: datetime | None, identity: str) -> RecallPage:
        seen = [t for t in self.turns[project] if as_of is None or t.at <= as_of]
        results = [{"content": f"{t.speaker}: {t.text}", "occurredAt": utc_iso(t.at)} for t in reversed(seen)]
        return RecallPage(results, as_of)

    async def close(self) -> None:
        pass
