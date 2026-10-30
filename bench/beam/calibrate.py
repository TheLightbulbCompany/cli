"""The model-path offset: our answerer and judge against past.dev's, on past.dev's own pages.

    BEAM_LLM=codex python calibrate.py rejudge  --past PAST/results.jsonl --output DIR --budget-calls N
    BEAM_LLM=codex python calibrate.py reanswer --past PAST/results.jsonl --output DIR --budget-calls N

rejudge (a): our judge scores past.dev's saved answers. Reported per category: the exact-verdict
agreement (rubric items: our rounded score equals theirs; event ordering: the question's score
equals theirs) and the mean score delta, ours minus theirs.

reanswer (b): our answerer answers from past.dev's saved recall pages, our judge scores it.
Reported per category: our mean minus their published mean.

Both write results.jsonl and summary.json in run.py's format (verify.py recomputes them) and
calibration.json. Rerunning resumes: finished questions are skipped and every paid reply is
re-read from codex-calls.jsonl. The call cap stops the run before the next call.
"""

import argparse
import asyncio
import json
import math
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

import prompts as exabase_prompts
import scoring
from dataset import build_conversation, load_rows, utc_iso
from log import log
from scoring import llm

QUESTIONS_AT_ONCE = 8


def load_jsonl(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text("utf-8").splitlines() if line.strip()]


async def calibrate_one(mode: str, past: dict, question) -> dict:
    usage = llm.start_usage() if hasattr(llm, "start_usage") else None
    row = {"questionId": question.id, "conversationId": past["conversationId"], "category": question.category,
           "question": question.text, "asOf": question.as_of, "goldAnswers": question.gold_answers}
    answer = past["answer"]
    if mode == "reanswer":
        context = json.dumps(past["recall"], ensure_ascii=False, separators=(",", ":"))
        prompt = exabase_prompts.answer_prompt(question.text, context, question.category, question.annotations)
        try:
            answer = (await llm.complete_text(prompt)).strip()
        except (llm.ModelError, ValueError) as error:
            if llm.stops_the_run(error):
                raise
            row |= {"error": str(error)[:500], "judge": {"method": "failed"}, "score": 0.0}
            answer = None
    if answer is not None:
        score, verdicts = await scoring.score_question(question, answer)
        row |= {"answer": answer, "judge": verdicts, "score": score}
    row["pastScore"] = past["score"]
    if usage is not None:
        row["usage"] = llm.usage_totals(usage)
    return row


def verdict_units(row: dict) -> list:
    judge = row.get("judge") or {}
    if judge.get("method") == "rubric" and "items" in judge:
        return [item["score"] for item in judge["items"]]
    return [round(row["score"], 9)]


def compare(mode: str, ours: list[dict], past: dict[str, dict], published: dict) -> dict:
    by_category = defaultdict(lambda: {"agree": 0, "units": 0, "ours": [], "past": []})
    for row in ours:
        their = past[row["questionId"]]
        c = by_category[row["category"]]
        c["ours"].append(row["score"])
        c["past"].append(their["score"])
        if mode == "rejudge":
            a, b = verdict_units(row), verdict_units(their)
            c["units"] += max(len(a), len(b))
            c["agree"] += sum(x == y for x, y in zip(a, b)) if len(a) == len(b) else 0
    result = {"mode": mode, "categories": {}}
    for category, c in sorted(by_category.items()):
        ours_mean, past_mean = sum(c["ours"]) / len(c["ours"]), sum(c["past"]) / len(c["past"])
        entry = {"questions": len(c["ours"]), "ours": ours_mean, "past": past_mean, "delta": ours_mean - past_mean,
                 "pastPublished": published["categories"][category]["mean"]}
        if mode == "rejudge":
            entry |= {"agreement": c["agree"] / c["units"], "verdicts": c["units"]}
        result["categories"][category] = entry
    all_ours = [row["score"] for row in ours]
    all_past = [past[row["questionId"]]["score"] for row in ours]
    result["overall"] = {"questions": len(ours), "ours": sum(all_ours) / len(ours), "past": sum(all_past) / len(ours),
                         "delta": (sum(all_ours) - sum(all_past)) / len(ours)}
    if mode == "rejudge":
        units = sum(c["units"] for c in by_category.values())
        result["overall"] |= {"agreement": sum(c["agree"] for c in by_category.values()) / units, "verdicts": units}
    return result


async def main_async(args) -> None:
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    results = output / "results.jsonl"
    if hasattr(llm, "set_budget"):
        llm.set_budget(args.budget_calls)
        llm.set_cache(output / "codex-calls.jsonl")

    past_rows = load_jsonl(Path(args.past))
    published = json.loads((Path(args.past).parent / "summary.json").read_text())
    rows, revision = load_rows(published["split"])
    if revision != published["datasetRevision"]:
        raise SystemExit(f"dataset revision {revision} is not past.dev's {published['datasetRevision']}")
    questions = {q.id: q for row in rows[:published["conversations"]] for q in build_conversation(row).questions}
    done = {row["questionId"] for row in load_jsonl(results)}
    todo = [p for p in past_rows if p["questionId"] not in done]
    log(f"calibration {args.mode}: {len(past_rows)} questions, {len(done)} done, {len(todo)} to go")

    slots, lock = asyncio.Semaphore(QUESTIONS_AT_ONCE), asyncio.Lock()
    started = datetime.now(timezone.utc)

    async def one(past: dict) -> None:
        async with slots:
            row = await calibrate_one(args.mode, past, questions[past["questionId"]])
            async with lock:
                with results.open("a", encoding="utf-8") as file:
                    file.write(json.dumps(row, ensure_ascii=False) + "\n")
            log(f"{row['questionId']}: score {row['score']:.2f} (past.dev {row['pastScore']:.2f})")

    try:
        async with asyncio.TaskGroup() as group:
            for past in todo:
                group.create_task(one(past))
    finally:
        ours = load_jsonl(results)
        log(f"{len(ours)}/{len(past_rows)} questions done; {llm.calls_made() if hasattr(llm, 'calls_made') else '?'} "
            f"calls this session")

    usage: dict[str, int] = {}
    for row in ours:
        for key, value in (row.get("usage") or {}).items():
            usage[key] = usage.get(key, 0) + value
    summary = {"dataset": "BEAM", "split": published["split"], "datasetRevision": revision,
               "conversations": published["conversations"], **scoring.summarize(ours),
               "answerModel": llm.MODEL, "judgeModel": llm.MODEL, "reasoningEffort": llm.REASONING_EFFORT,
               "prompts": {"source": exabase_prompts.SOURCE_URL, "sha256": exabase_prompts.SOURCE_SHA256},
               "calibration": args.mode, "pastResults": str(args.past), "usage": usage,
               "finishedAt": utc_iso(datetime.now(timezone.utc))}
    if hasattr(llm, "memories_are_empty"):
        summary["codexMemoriesEmpty"] = llm.memories_are_empty()
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", "utf-8")
    report = compare(args.mode, ours, {p["questionId"]: p for p in past_rows}, published)
    (output / "calibration.json").write_text(json.dumps(report, indent=2) + "\n", "utf-8")
    for category, c in report["categories"].items():
        extra = f"  agreement {c['agreement']:.3f}" if "agreement" in c else ""
        print(f"  {category:<26} ours {c['ours']:.4f}  past {c['past']:.4f}  delta {c['delta']:+.4f}{extra}")
    o = report["overall"]
    print(f"  {'overall':<26} ours {o['ours']:.4f}  past {o['past']:.4f}  delta {o['delta']:+.4f}"
          + (f"  agreement {o['agreement']:.3f}" if "agreement" in o else ""))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("mode", choices=("rejudge", "reanswer"))
    parser.add_argument("--past", required=True, help="past.dev's results.jsonl (summary.json beside it)")
    parser.add_argument("--output", required=True)
    parser.add_argument("--budget-calls", type=int)
    exabase_prompts.ensure_exabase_prompts(exabase_prompts._cache_dir)
    try:
        asyncio.run(main_async(parser.parse_args()))
    except BaseExceptionGroup as group:
        stop = group.subgroup(lambda e: llm.stops_the_run(e))
        if stop is None:
            raise
        raise SystemExit(f"stopped: {stop.exceptions[0]}") from None


if __name__ == "__main__":
    main()
