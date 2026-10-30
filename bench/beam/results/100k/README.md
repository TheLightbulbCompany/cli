# BEAM 100K results (Lightbulb runner, Codex path)

Each folder holds one run's `results.jsonl` and `summary.json`. `uv run python verify.py results/100k/<folder>`
(from `bench/beam/`) recomputes every score and the summary. Method and caveats:
[docs/benchmarks/beam-100k.md](../../../../docs/benchmarks/beam-100k.md).

## Which folder is which number

| Folder | Overall | What |
|---|---:|---|
| cal-a-rejudge | 90.69 | Column (a): our Codex judge on past.dev's saved 100K answers |
| cal-b-reanswer | 88.63 | Column (b): our Codex answerer and judge on past.dev's saved recall pages |
| free-contested-100k | 88.07 | **The free-engine column**: the daemon's page text as shipped in v0.7.2 (measured on branch `loop/contested-line` `94d73ff` of this repository plus the bench ops) |
| free-v071-100k | 87.43 | The v0.7.1 page (source identical to the v0.7.1 tag) |
| free-shipped-100k | 73.82 | The v0.7.0 page (5 turns, 1,600 characters) |
| free-100k | 87.86 | Diagnostic: v0.7.0 with the bench ops, 50 facts and 50 turn excerpts (each the turn's first 240 characters) handed to the answerer as separate results, not the page text |
| free-excerpt-100k | 87.65 | Diagnostic: as free-100k, each excerpt the 240-character window with the most question words (branch `loop/turn-excerpt-window`) |
| free-length-100k | 89.62 | Diagnostic: as free-excerpt-100k with 800-character windows (branch `loop/turn-excerpt-length`) |
| free-dates-100k | 88.61 | Diagnostic: as free-length-100k with dated fact lines, newest first within a subject (branch `loop/fact-dates`) |

The diagnostic runs fed the answerer the recall results directly rather than the page an agent
reads, so they are not comparable with the free-engine column. All free runs held the same 3,164
note-taker entries (296 note-taker calls, re-read from the reply cache after the first run).

The write-up's workspace-engine column is not here: it was measured against Lightbulb's internal
backend and cannot be checked from outside.

## What is in the files

Stripped: per question only ids, category, as-of time, the model's answer, the judge's verdicts
(scores and reasons; no rubric text), the score and the call usage. Calibration (a) rows also keep
past.dev's score for the question. Event-ordering verdicts keep the reference topic lists that
`verify.py` scores against (BEAM's `ordering_tested`: short topic labels). No questions, gold
answers, rubrics or recall pages.

## Licence and attribution

These files contain material derived from the BEAM dataset (the event-ordering reference topic
lists, and model answers written from its conversations), so they are licensed under
[CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/), not this repository's licence.
If you share or adapt them, you must give the same attribution and share your version under
CC BY-SA 4.0.

- Dataset: BEAM, [Mohammadta/BEAM](https://huggingface.co/datasets/Mohammadta/BEAM) on Hugging Face,
  revision `3205395e897e7318c7b094ef4e6047b9b82dbb03`, CC BY-SA 4.0.
- Paper: Mohammad Tavakoli, Alireza Salemi, Carrie Ye, Mohamed Abdalla, Hamed Zamani and J Ross
  Mitchell, "Beyond a Million Tokens: Benchmarking and Enhancing Long-Term Memory in LLMs",
  [arXiv:2510.27246](https://arxiv.org/abs/2510.27246).
- Changes: the runs select questions from the dataset and add model-generated answers and verdicts;
  these are evaluation records, not the dataset files.
