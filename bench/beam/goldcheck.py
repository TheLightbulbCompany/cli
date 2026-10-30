"""Is the gold answer on the recall page, and where: in memory, in a raw turn, or nowhere?

    python goldcheck.py results/DIR      per category: questions whose gold is in memory / turn / nowhere

A gold answer is "on" a result when every number in it appears in the result's text, or, for a
gold without numbers, when at least 60% of its content words do. Memory results come first: a
gold found in a fact counts as memory even if a turn has it too.
ponytail: a lexical heuristic (no judge); an LLM check per question is the upgrade if it misleads.
"""

import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

STOP = {"the", "and", "for", "that", "with", "this", "from", "your", "you", "are", "was", "were", "have",
        "has", "had", "but", "not", "they", "their", "there", "what", "when", "which", "will", "would",
        "about", "into", "than", "then", "them", "been", "also", "its", "it's", "user", "llm", "response",
        "should", "contain", "based", "provided", "chat", "information", "related"}


def words(text: str) -> set[str]:
    return {w for w in re.findall(r"[a-z][a-z'-]{2,}", text.lower()) if w not in STOP}


def numbers(text: str) -> list[str]:
    return re.findall(r"\d+(?:[.,:/]\d+)*", text)


def on(gold: str, text: str) -> bool:
    found = numbers(gold)
    if found:
        return all(n in text for n in found)
    wanted = words(gold)
    return bool(wanted) and len(wanted & words(text)) >= 0.6 * len(wanted)


def where(gold: str, results: list[dict]) -> str:
    texts = [(r.get("kind"), (r.get("content") or "") + " " + " ".join(
        e for s in r.get("sources") or [] for e in s.get("excerpts") or [])) for r in results]
    if any(on(gold, t) for kind, t in texts if kind != "turn"):
        return "memory"
    if any(on(gold, t) for kind, t in texts if kind == "turn"):
        return "turn"
    return "nowhere"


def check(rows: list[dict]) -> dict[str, Counter]:
    out: dict[str, Counter] = defaultdict(Counter)
    for row in rows:
        gold = (row.get("goldAnswers") or [""])[0]
        out[row["category"]][where(gold, (row.get("recall") or {}).get("results") or [])] += 1
    return out


def main() -> None:
    parser = argparse.ArgumentParser(description="Where each gold answer is on its recall page.")
    parser.add_argument("run", type=Path)
    args = parser.parse_args()
    rows = [json.loads(l) for l in (args.run / "results.jsonl").read_text("utf-8").splitlines() if l.strip()]
    print(f"  {'category':<26} memory  turn  nowhere")
    for category, c in sorted(check(rows).items()):
        print(f"  {category:<26} {c['memory']:>6} {c['turn']:>5} {c['nowhere']:>8}")
    sys.exit(0)


if __name__ == "__main__":
    main()
