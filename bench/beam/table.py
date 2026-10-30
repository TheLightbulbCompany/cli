"""The results table: per category, each run's mean with a bootstrap 95% interval over questions.

    python table.py NAME=DIR [NAME=DIR ...]

Every DIR must pass verify.py's run check first (scores recomputed from the saved verdicts, the
summary from the scores, every question once); a run that does not is refused, not printed.
"""

import argparse
import json
import random
import sys
from pathlib import Path

import scoring
from verify import verify_run

RESAMPLES = 10_000
SEED = 20261029


def interval(scores: list[float]) -> tuple[float, float]:
    rng = random.Random(SEED)
    means = sorted(sum(rng.choices(scores, k=len(scores))) / len(scores) for _ in range(RESAMPLES))
    return means[int(0.025 * RESAMPLES)], means[int(0.975 * RESAMPLES) - 1]


def column(directory: Path) -> dict[str, tuple[float, float, float]]:
    problems = verify_run(directory)
    if problems:
        raise SystemExit(f"{directory} does not verify: {problems[:3]}")
    rows = [json.loads(l) for l in (directory / "results.jsonl").read_text("utf-8").splitlines() if l.strip()]
    by: dict[str, list[float]] = {}
    for row in rows:
        by.setdefault(row["category"], []).append(float(row["score"]))
    by["overall (micro)"] = [float(row["score"]) for row in rows]
    assert abs(scoring.summarize(rows)["microAverage"] - sum(by["overall (micro)"]) / len(rows)) < 1e-12
    return {c: (sum(s) / len(s), *interval(s)) for c, s in by.items()}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("runs", nargs="+", help="NAME=DIR")
    args = parser.parse_args()
    names, columns = [], []
    for spec in args.runs:
        name, directory = spec.split("=", 1)
        names.append(name)
        columns.append(column(Path(directory)))
    categories = sorted(c for c in columns[0] if c != "overall (micro)") + ["overall (micro)"]
    print("| Category | " + " | ".join(names) + " |")
    print("|---|" + "---:|" * len(names))
    for c in categories:
        cells = [f"{100 * m:.2f} [{100 * lo:.1f}, {100 * hi:.1f}]" for m, lo, hi in (col[c] for col in columns)]
        print(f"| {c} | " + " | ".join(cells) + " |")
    sys.exit(0)


if __name__ == "__main__":
    main()
