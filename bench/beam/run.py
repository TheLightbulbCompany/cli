"""Run BEAM against a memory backend.

    python run.py --backend free --split 100k

The run has three phases, each over every conversation:

    1. send: create a project, register who takes part, and ingest every turn
    2. wait until the backend has finished processing every project
    3. score: ask each question, answer it from what the backend recalls, and score the answer

Forked from past.dev's harness: past_api.py is replaced by one adapter interface
(adapters/base.py); the evidence budget and the recall JSON's shape are the runner's.

Environment: OPENROUTER_API_KEY.
"""

import argparse
import asyncio
import importlib
import json
import os
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

import prompts as exabase_prompts
import scoring
import adapters
from adapters.base import RECALL_LIMIT, RECALL_MAX_TOKENS, BackendError, MemoryBackend, Turn, recall_json
from dataset import (ASSISTANT_IDENTITY, ASSISTANT_TRAITS, SPLITS, Conversation, Question,
                     build_conversation, load_rows, utc_iso)
from log import duration, log

llm = importlib.import_module("llm_codex" if os.environ.get("BEAM_LLM") == "codex" else "llm")  # BEAM_LLM=codex: the subscription path

HARNESS_VERSION = "lb-1"  # past.dev's 2, with the adapter interface in place of past_api

# How the work is paced: how many conversations are sent and scored at once, how many turns go in
# one push, and how often the harness asks whether the backend has finished.
CONVERSATIONS_SENT_AT_ONCE = 4
CONVERSATIONS_SCORED_AT_ONCE = 4
ITEMS_PER_PUSH = 100
RECALLS_AT_ONCE = 1
SETTLEMENT_POLL_SECONDS = 2
# While the backend processes, a progress line every this many seconds.
PROGRESS_LOG_SECONDS = 30


# --- Phase 1, send: a project, and who takes part -------------------------------------------------------

async def open_project(backend: MemoryBackend, name: str, conversation: Conversation) -> str:
    """A project for one conversation, with the user and the assistant registered in it."""
    project = await backend.create_project(name)
    log(f"project {project} created", conversation.id)
    await backend.register_identities(project, [
        {"identity": conversation.user_identity, "traits": conversation.user_traits},
        {"identity": ASSISTANT_IDENTITY, "traits": ASSISTANT_TRAITS},
    ])
    return project


# --- Phase 1, send: every turn -------------------------------------------------------------------

async def ingest(backend: MemoryBackend, project: str, conversation: Conversation) -> None:
    """Pushes every turn in order, one push after another. Questions and annotations never go."""
    turns = [Turn(m.session_id, m.ordinal, m.author, m.role, m.content, m.timestamp)
             for m in conversation.messages]
    log(f"ingesting {len(turns)} turns", conversation.id)
    for start in range(0, len(turns), ITEMS_PER_PUSH):
        await backend.ingest_batch(project, turns[start:start + ITEMS_PER_PUSH])
        log(f"sent {min(start + ITEMS_PER_PUSH, len(turns))}/{len(turns)} turns", conversation.id)


# --- Phase 2: wait until the backend has processed everything --------------------------------------------

async def wait_until_processed(backend: MemoryBackend, project: str) -> None:
    while await backend.ingestion_status(project) != "done":
        await asyncio.sleep(SETTLEMENT_POLL_SECONDS)


# --- Phase 3, score: ask, answer, judge ------------------------------------------------------------------

async def answer_question(backend: MemoryBackend, project: str, conversation: Conversation, question: Question,
                          recall_slots: asyncio.Semaphore) -> dict:
    usage = llm.start_usage() if hasattr(llm, "start_usage") else None
    try:
        # The user asks, at the question's moment in the conversation.
        async with recall_slots:
            page = await backend.recall(project, question.text, parse_moment(question.as_of),
                                        identity=conversation.user_identity)
        recalled = recall_json(page)
        check_recalled_moment(recalled, question.as_of)

        # The answer model reads the trimmed page, serialised as past.dev's recall JSON.
        context = json.dumps(recalled, ensure_ascii=False, separators=(",", ":"))
        prompt = exabase_prompts.answer_prompt(question.text, context, question.category, question.annotations)
        answer = (await llm.complete_text(prompt)).strip()
    except (BackendError, llm.ModelError, ValueError) as error:
        if llm.stops_the_run(error):
            raise
        # A recall or an answer that failed even after retries scores the question 0, and the
        # summary counts it: a failure is part of the result, never a reason to try again.
        log(f"{question.id}: failed, scored 0 ({str(error)[:160]})", conversation.id)
        return {
            "questionId": question.id,
            "conversationId": conversation.id,
            "category": question.category,
            "question": question.text,
            "asOf": question.as_of,
            "goldAnswers": question.gold_answers,
            "error": str(error)[:500],
            "judge": {"method": "failed"},
            "score": 0.0,
            **({"usage": llm.usage_totals(usage)} if usage is not None else {}),
        }

    score, verdicts = await scoring.score_question(question, answer)
    log(f"{question.id}: {len(recalled.get('results') or [])} results recalled "
        f"({recalled.get('usedEvidenceTokens', '?')} evidence tokens), score {score:.2f}", conversation.id)
    return {
        "questionId": question.id,
        "conversationId": conversation.id,
        "category": question.category,
        "question": question.text,
        "asOf": question.as_of,
        "goldAnswers": question.gold_answers,
        "recall": recalled,
        "answer": answer,
        "judge": verdicts,
        "score": score,
        **({"usage": llm.usage_totals(usage)} if usage is not None else {}),
    }


def parse_moment(text: str) -> datetime:
    return datetime.fromisoformat(text.replace("Z", "+00:00"))


def check_recalled_moment(recalled: dict, asked_at: str) -> None:
    """Recall must answer as of exactly the moment it was asked, or the question is not the one we meant."""
    echoed = str(recalled.get("asOf") or "")
    try:
        echoed_utc = utc_iso(datetime.fromisoformat(echoed.replace("Z", "+00:00")))
    except ValueError:
        echoed_utc = echoed
    if echoed_utc != asked_at:
        raise RuntimeError(f"Recall answered as of {echoed!r}, but the question was asked at {asked_at!r}")


# --- The run -----------------------------------------------------------------------------------

REQUIRED_ENVIRONMENT = getattr(llm, "REQUIRED_ENVIRONMENT", ("OPENROUTER_API_KEY",))


@dataclass
class Sent:
    """A conversation whose turns are all in the backend, with the project that holds them."""
    conversation: Conversation
    project: str


class Run:
    """What every conversation of one run shares."""

    def __init__(self, args: argparse.Namespace) -> None:
        self.split = args.split
        self.backend_name = args.backend
        self.backend: MemoryBackend = adapters.load(args.backend)
        self.output = Path(args.output or f"results/beam-{args.split}-{args.backend}")
        self.results = self.output / "results.jsonl"
        self.tag = datetime.now(timezone.utc).strftime("%m%d-%H%M%S")
        self.sending_slots = asyncio.Semaphore(CONVERSATIONS_SENT_AT_ONCE)
        self.scoring_slots = asyncio.Semaphore(CONVERSATIONS_SCORED_AT_ONCE)
        self.recall_slots = asyncio.Semaphore(RECALLS_AT_ONCE)
        self.write_lock = asyncio.Lock()
        self.processed = 0
        self.scored = 0

    async def send(self, row: dict) -> Sent:
        async with self.sending_slots:
            conversation = build_conversation(row)
            log(f"sending: {len(conversation.messages)} turns, {len(conversation.questions)} questions",
                conversation.id)
            project = await open_project(self.backend, self.project_name(conversation.id), conversation)
            await ingest(self.backend, project, conversation)
            return Sent(conversation, project)

    async def wait(self, sent: Sent, started: float) -> None:
        await wait_until_processed(self.backend, sent.project)
        self.processed += 1
        log(f"processed after {duration(time.monotonic() - started)}", sent.conversation.id)

    async def log_processing_progress(self, total: int, started: float) -> None:
        """A progress line every PROGRESS_LOG_SECONDS, until it is cancelled."""
        while True:
            await asyncio.sleep(PROGRESS_LOG_SECONDS)
            log(f"processing: {self.processed}/{total} conversations done after "
                f"{duration(time.monotonic() - started)}")

    async def score(self, sent: Sent, total: int) -> None:
        async with self.scoring_slots:
            conversation = sent.conversation
            started = time.monotonic()
            rows = await asyncio.gather(*(answer_question(self.backend, sent.project, conversation, question,
                                                          self.recall_slots)
                                          for question in conversation.questions))
            await self.save(rows)
            self.scored += 1
            mean = sum(row["score"] for row in rows) / len(rows) if rows else 0.0
            log(f"scored: mean {mean:.3f} over {len(rows)} questions in {duration(time.monotonic() - started)} "
                f"· {self.scored}/{total} conversations scored", conversation.id)

    async def save(self, rows: list[dict]) -> None:
        """A conversation's rows are written in one go, so a stopped run never leaves half of one."""
        lines = "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in rows)
        async with self.write_lock:
            with self.results.open("a", encoding="utf-8") as file:
                file.write(lines)

    def saved_rows(self) -> list[dict]:
        if not self.results.exists():
            return []
        return [json.loads(line) for line in self.results.read_text("utf-8").splitlines() if line.strip()]

    def project_name(self, conversation_id: str) -> str:
        return f"beam-{self.split}-{self.tag}-{conversation_id}"

    async def close(self) -> None:
        await self.backend.close()
        await llm.close()


async def run_phases(run: Run, to_run: list[dict]) -> None:
    log(f"phase 1/3: sending {len(to_run)} conversation(s)")
    async with asyncio.TaskGroup() as group:
        sending = [group.create_task(run.send(row)) for row in to_run]
    sent = [task.result() for task in sending]
    await wait_for_processing(run, sent)

    log(f"phase 3/3: scoring {len(sent)} conversation(s)")
    async with asyncio.TaskGroup() as group:
        for conversation in sent:
            group.create_task(run.score(conversation, len(sent)))


async def wait_for_processing(run: Run, sent: list[Sent]) -> None:
    log(f"phase 2/3: waiting until the backend has processed {len(sent)} conversation(s)")
    started = time.monotonic()
    progress = asyncio.create_task(run.log_processing_progress(len(sent), started))
    try:
        async with asyncio.TaskGroup() as group:
            for conversation in sent:
                group.create_task(run.wait(conversation, started))
    finally:
        progress.cancel()


async def main_async(args: argparse.Namespace) -> None:
    missing = [name for name in REQUIRED_ENVIRONMENT if not os.environ.get(name, "").strip()]
    if missing:
        raise SystemExit(f"Set {', '.join(missing)} before running (see README.md).")

    run = Run(args)
    run.output.mkdir(parents=True, exist_ok=True)
    if hasattr(llm, "set_budget"):
        llm.set_budget(args.budget_calls)
        llm.set_cache(run.output / "codex-calls.jsonl")
    rows, revision = load_rows(args.split)
    if args.conversations is not None:
        rows = rows[:args.conversations]
    selected = [str(row.get("conversation_id", "")) for row in rows]
    already_scored = {row["conversationId"] for row in run.saved_rows()} & set(selected)
    to_run = [row for row in rows if str(row.get("conversation_id", "")) not in already_scored]
    log(f"BEAM {args.split} (dataset revision {revision[:12]}), run {run.tag}: {len(selected)} conversation(s), "
        f"{len(already_scored)} already scored, {len(to_run)} to run · results in {run.results}")

    # In every phase, a failure cancels the rest: a run finishes or stops, it never scores around
    # a gap. Rerunning resumes after the conversations already scored.
    try:
        if to_run:
            await run_phases(run, to_run)
    finally:
        await run.close()

    scored = [row for row in run.saved_rows() if row["conversationId"] in selected]
    summary = {
        "dataset": "BEAM",
        "split": args.split,
        "datasetRevision": revision,
        "conversations": len(selected),
        **scoring.summarize(scored),
        "answerModel": llm.MODEL,
        "judgeModel": llm.MODEL,
        "reasoningEffort": llm.REASONING_EFFORT,
        "recall": {"limit": RECALL_LIMIT, "maxTokens": RECALL_MAX_TOKENS},
        "prompts": {"source": exabase_prompts.SOURCE_URL, "sha256": exabase_prompts.SOURCE_SHA256},
        "backend": run.backend_name,
        **({"backendStats": run.backend.stats} if hasattr(run.backend, "stats") else {}),
        "projectTag": run.tag,
        "harnessVersion": HARNESS_VERSION,
        "modelPath": "codex-cli" if hasattr(llm, "start_usage") else "openrouter",
        **({"usage": sum_usage(scored)} if hasattr(llm, "start_usage") else {}),
        "finishedAt": utc_iso(datetime.now(timezone.utc)),
    }
    (run.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", "utf-8")
    print_summary(summary)


def sum_usage(rows: list[dict]) -> dict:
    totals: dict[str, int] = {}
    for row in rows:
        for key, value in (row.get("usage") or {}).items():
            totals[key] = totals.get(key, 0) + value
    return totals


def print_summary(summary: dict) -> None:
    print()
    for category, scores in summary["categories"].items():
        print(f"  {category:<26} {scores['mean']:.3f}  ({scores['questions']} questions)")
    if summary["questions"]:
        print(f"\n  {'mean over questions':<26} {summary['microAverage']:.3f}  ({summary['questions']} questions)")
        print(f"  {'mean over categories':<26} {summary['macroAverage']:.3f}")
    print()


def positive_number(text: str) -> int:
    number = int(text)
    if number < 1:
        raise argparse.ArgumentTypeError("must be 1 or more")
    return number


def parse_arguments(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run BEAM against a memory backend.")
    parser.add_argument("--backend", choices=list(adapters.BACKENDS), required=True)
    parser.add_argument("--split", choices=SPLITS, default="100k")
    parser.add_argument("--conversations", type=positive_number, help="only the first N conversations")
    parser.add_argument("--output", help="results directory (default: results/beam-<split>-<backend>)")
    parser.add_argument("--budget-calls", type=int, help="Codex path: stop before model call N+1")
    return parser.parse_args(argv)


def main() -> None:
    try:
        asyncio.run(main_async(parse_arguments()))
    except BaseExceptionGroup as group:
        stop = group.subgroup(lambda e: llm.stops_the_run(e))
        if stop is None:
            raise
        raise SystemExit(f"stopped: {stop.exceptions[0]}") from None


if __name__ == "__main__":
    main()
