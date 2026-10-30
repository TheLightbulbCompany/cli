"""The note-taker: plays the coding agent that, with the free engine, calls `remember`.

One model call per 20 turns. It reads those turns only (never a question, a gold answer or a
rubric: it is handed Turn objects and nothing else) plus the subjects it already wrote, and
returns remember ops grounded on the turns it just read. The prompt is ours, not ExaBase's.
"""

import importlib
import os

llm = importlib.import_module("llm_codex" if os.environ.get("BEAM_LLM") == "codex" else "llm")

TURNS_PER_CALL = 20
KNOWN_SUBJECTS_SHOWN = 300

PROMPT = """You keep the long-term memory of an assistant. Below are consecutive turns of a conversation between a user and the assistant. Write down what is worth remembering for later: facts about the user and their life, people, projects, plans, dates, deadlines, numbers, decisions, preferences, instructions the user gave for future answers, and any value that changes or contradicts an earlier one.

Rules:
- One op per fact. Each fact stands alone: name the thing and keep numbers, dates and units exactly as stated.
- subject: a short noun phrase naming what the fact is about. When the fact is about one of the known subjects below, reuse that subject exactly.
- updates: true only when this fact replaces an earlier value of the same subject (a change of plan, a corrected number); false otherwise.
- kind: fact, decision, lesson or preference.
- turn: the number in brackets of the turn the fact comes from.
- Only what the turns say. Nothing from general knowledge. If nothing is worth keeping, return no ops.

Known subjects:
{subjects}

Turns:
{turns}"""

SCHEMA = {
    "properties": {
        "ops": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "subject": {"type": "string"},
                    "fact": {"type": "string"},
                    "kind": {"type": "string", "enum": ["fact", "decision", "lesson", "preference"]},
                    "updates": {"type": "boolean"},
                    "turn": {"type": "integer"},
                },
                "required": ["subject", "fact", "kind", "updates", "turn"],
                "additionalProperties": False,
            },
        },
    },
    "required": ["ops"],
}


def prompt_for(turns, subjects: list[str]) -> str:
    lines = [f"[{i + 1}] {t.at:%Y-%m-%d} {t.role} ({t.speaker}): {t.text}" for i, t in enumerate(turns)]
    shown = subjects[-KNOWN_SUBJECTS_SHOWN:]
    return PROMPT.format(subjects="\n".join(f"- {s}" for s in shown) or "(none yet)", turns="\n".join(lines))


async def notes(turns, subjects: list[str]) -> list[dict]:
    """Remember ops for these turns, each with the index of the turn it is grounded on."""
    reply = await llm.complete_json(prompt_for(turns, subjects), SCHEMA)
    ops = []
    for op in reply.get("ops") or []:
        if not str(op.get("subject", "")).strip() or not str(op.get("fact", "")).strip():
            continue
        index = min(max(int(op.get("turn") or len(turns)), 1), len(turns)) - 1
        ops.append({"subject": op["subject"].strip(), "fact": op["fact"].strip(), "kind": op.get("kind", "fact"),
                    "updates": bool(op.get("updates")), "turn": index})
    return ops
