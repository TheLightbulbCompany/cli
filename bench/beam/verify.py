"""Check a run's numbers, and check that the prompts are ExaBase's.

    python verify.py results/beam-100k    recompute every score and the summary from the run's own records
    python verify.py --prompts            compare exabase_prompts.py with ExaBase's published file

Checking a run calls neither Past nor a model. Every question's score is recomputed from the judge
verdicts saved beside it, and the summary from those scores, so anyone holding a run's two files
can confirm the published numbers add up and that no question is missing.
"""

import argparse
import hashlib
import json
import math
import re
import sys
from pathlib import Path

import httpx

import prompts as exabase_prompts
import scoring
from dataset import build_conversation, load_rows

# --- Checking a run ----------------------------------------------------------------------------

def verify_run(directory: Path) -> list[str]:
    """Every problem found in a run directory; an empty list means the run checks out."""
    rows = [json.loads(line) for line in (directory / "results.jsonl").read_text("utf-8").splitlines() if line.strip()]
    summary = json.loads((directory / "summary.json").read_text("utf-8"))
    problems = []
    problems += check_every_question_once(rows, summary)
    problems += check_question_scores(rows)
    problems += check_summary(rows, summary)
    return problems


def check_every_question_once(rows: list[dict], summary: dict) -> list[str]:
    """The run answered exactly the questions of the conversations it covers, each once."""
    problems = []
    ids = [row["questionId"] for row in rows]
    duplicates = sorted({question for question in ids if ids.count(question) > 1})
    if duplicates:
        problems.append(f"questions answered more than once: {', '.join(duplicates)}")

    dataset_rows, revision = load_rows(summary["split"])
    if revision != summary["datasetRevision"]:
        problems.append(f"the run used dataset revision {summary['datasetRevision']}, "
                        f"but the local copy is {revision}; question lists may differ")
    covered = dataset_rows[:summary["conversations"]]
    expected = {question.id for row in covered for question in build_conversation(row).questions}
    missing = sorted(expected - set(ids))
    extra = sorted(set(ids) - expected)
    if missing:
        problems.append(f"{len(missing)} questions have no answer, for example {missing[0]}")
    if extra:
        problems.append(f"{len(extra)} answers belong to no covered conversation, for example {extra[0]}")
    return problems


def check_question_scores(rows: list[dict]) -> list[str]:
    """Each question's score follows from the judge verdicts recorded with it."""
    problems = []
    for row in rows:
        judge = row["judge"]
        if row.get("error") or "note" in judge:  # a failed recall or answer, or nothing to score against
            recomputed = 0.0
        elif judge["method"] == "rubric":
            item_scores = [item["score"] for item in judge["items"]]
            for item in judge["items"]:
                # Runs from before raw scores were saved can only be checked from the rounded ones.
                if "rawScore" in item and item["score"] != scoring.round_rubric_score(item["rawScore"]):
                    problems.append(f"{row['questionId']}: judge said {item['rawScore']}, "
                                    f"recorded as {item['score']}")
            recomputed = sum(item_scores) / len(item_scores)
        else:  # event ordering: compare the reference order with the answer's, as the judge matched them
            recomputed = scoring.ordering_score(judge["reference"], judge["answer"]) if judge["answer"] else 0.0
        if not same(recomputed, row["score"]):
            problems.append(f"{row['questionId']}: recorded score {row['score']}, "
                            f"but its verdicts give {recomputed}")
    return problems


def check_summary(rows: list[dict], summary: dict) -> list[str]:
    """The summary's numbers are the means of the questions' scores."""
    problems = []
    recomputed = scoring.summarize(rows)
    for key in ("questions", "microAverage", "macroAverage"):
        if not same(recomputed[key], summary[key]):
            problems.append(f"summary {key} is {summary[key]}, but the questions give {recomputed[key]}")
    if set(recomputed["categories"]) != set(summary["categories"]):
        problems.append("summary lists different categories from the questions")
    for category, numbers in recomputed["categories"].items():
        published = summary["categories"].get(category, {})
        for key in ("questions", "mean"):
            if not same(numbers[key], published.get(key)):
                problems.append(f"summary {category} {key} is {published.get(key)}, "
                                f"but the questions give {numbers[key]}")
    return problems


def same(a, b) -> bool:
    if a is None or b is None:
        return a is b
    return math.isclose(float(a), float(b), abs_tol=1e-9)


# --- Checking the prompts ----------------------------------------------------------------------

def verify_prompts() -> list[str]:
    """Download ExaBase's adapter, check its hash, and find each of our prompts in it unchanged."""
    exabase_prompts.ensure_exabase_prompts(exabase_prompts._cache_dir, fetch=download_exabase_adapter)
    published = download_exabase_adapter()
    source = published.decode("utf-8")
    problems = []
    digest = hashlib.sha256(published).hexdigest()
    if digest != exabase_prompts.SOURCE_SHA256:
        problems.append(f"ExaBase's file now hashes to {digest}, not {exabase_prompts.SOURCE_SHA256}")

    # The fork's prompts.py derives the prompt functions by calling ExaBase's own code in the
    # verified file, so there is no copy of their bodies to compare; the hash and schemas remain.

    for schema in (exabase_prompts.RUBRIC_SCHEMA, exabase_prompts.EQUIVALENCE_SCHEMA):
        for name, spec in schema["properties"].items():
            if f'"{name}": {json.dumps(spec)}' not in source:
                problems.append(f"the judge's {name!r} field differs from ExaBase's schema")
    return problems


def download_exabase_adapter() -> bytes:
    """The published page links the file through a signed download URL; follow it."""
    page = httpx.get(exabase_prompts.SOURCE_URL, follow_redirects=True, timeout=30).text.replace("\\u0026", "&")
    link = re.search(r'https://cdn\.fabric\.so/[^"\s]+?\.py\?token=[^"\s\\]+', page)
    if link is None:
        raise SystemExit(f"Could not find the adapter's download link on {exabase_prompts.SOURCE_URL}")
    return httpx.get(link.group(0), follow_redirects=True, timeout=30).content


# --- Command line ------------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(description="Check a run's numbers, or check the prompts.")
    parser.add_argument("run", nargs="?", type=Path, help="a run directory holding results.jsonl and summary.json")
    parser.add_argument("--prompts", action="store_true", help="compare the prompts with ExaBase's published file")
    args = parser.parse_args()
    if not args.run and not args.prompts:
        parser.error("give a run directory, --prompts, or both")

    failed = False
    if args.run:
        problems = verify_run(args.run)
        report(f"run {args.run}", problems)
        failed |= bool(problems)
    if args.prompts:
        problems = verify_prompts()
        report("prompts", problems)
        failed |= bool(problems)
    sys.exit(1 if failed else 0)


def report(subject: str, problems: list[str]) -> None:
    for problem in problems:
        print(f"  ✗ {problem}")
    print(f"✓ {subject}: everything checks out" if not problems else f"✗ {subject}: {len(problems)} problem(s)")


if __name__ == "__main__":
    main()
