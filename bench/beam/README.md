# BEAM runner

Lightbulb's runner for the BEAM long-term-memory benchmark, used for the numbers in
[docs/benchmarks/beam-100k.md](../../docs/benchmarks/beam-100k.md). It is a fork of past.dev's harness
(`beam/` in https://github.com/pastdotdev/benchmarks), copied from the pinned commit
`a712cd993e249cadd214168c3c3ee674f89bd5e4`. Files copied unchanged except three one-line import
changes (`import prompts as exabase_prompts` in `run.py`, `scoring.py`, `verify.py`), and
`past_api.py`, replaced by one adapter interface (`adapters/base.py`; `run.py --backend NAME`).
The runner trims each recall page to 100 results / 8,000 tokens, counting tokens as past.dev's
`usedEvidenceTokens` does (ceil of content plus excerpt characters over 4 per result; it
reproduces all 400 rows of their published 100K results).

Backends: `free` (this repository's daemon, `adapters/free_cli.py`) and `fake` (in memory, for the
tests). The write-up's workspace-engine column was measured with an adapter for Lightbulb's internal
backend. That adapter is not here: it needs Lightbulb's development environment.

## Prerequisites

- [uv](https://docs.astral.sh/uv/). It installs Python and the locked dependencies (`uv.lock`).
- Docker, for the free engine: the daemons run in a Linux container with no network.
- A model path, one of:
  - **Codex CLI** (`codex`), signed in to a ChatGPT subscription in an isolated home. The published
    numbers used this path. Set `BEAM_LLM=codex` and `BEAM_CODEX_HOME=/path/to/codex-home` (never
    `~/.codex`; sign in there with `CODEX_HOME=/path/to/codex-home codex login`). Optional:
    `BEAM_CODEX_MODEL` (default `gpt-5.6-luna`), `BEAM_CODEX_EFFORT` (default `none`), `BEAM_CODEX_BIN`.
  - **OpenRouter**: leave `BEAM_LLM` unset and set `OPENROUTER_API_KEY`. `llm.py` then calls
    `openai/gpt-5.6-luna` through OpenRouter's API, the original harness's path. This path has no
    call cap and no reply cache, and none of the published numbers used it.
- Network access for the runner itself: it downloads the BEAM dataset from Hugging Face (cached in
  `~/.cache/beam-open`, or `BEAM_OPEN_CACHE`) and ExaBase's prompts (see Licences).

## The bench binary and its container

The free engine needs a daemon built with `--features bench`, which adds the ops the runner uses
(turns stored at the dataset's times, a settle, recall as of a past moment). Build it and start the
`lb-bench` container as in [docs/benchmark.md](../../docs/benchmark.md#the-build-used-for-the-published-run).
The adapter expects the container `lb-bench`, the binary at `/out/release/lightbulb` and the
embedding model at `/models/model`; `BEAM_FREE_CONTAINER`, `BEAM_FREE_BIN` and `BEAM_FREE_MODEL`
override them.

## Running

From `bench/beam/`, with the Codex CLI signed in to its isolated home:

```sh
export BEAM_LLM=codex BEAM_CODEX_HOME=/path/to/codex-home

# Calibrations, on past.dev's published results file: beam/results/100k/results.jsonl (summary.json
# beside it) in github.com/pastdotdev/benchmarks at a712cd993e249cadd214168c3c3ee674f89bd5e4
uv run python calibrate.py rejudge  --past <published>/results.jsonl --output runs/cal-a-rejudge  --budget-calls 1200
uv run python calibrate.py reanswer --past <published>/results.jsonl --output runs/cal-b-reanswer --budget-calls 1600

# Free engine: the daemon's page text, as an agent gets it (container lb-bench running)
BEAM_FREE_SHIPPED=1 uv run python run.py --backend free --split 100k --output runs/free --budget-calls 1500

# Check every number, then print the table
uv run python verify.py runs/free
uv run python table.py cal-a=runs/cal-a-rejudge cal-b=runs/cal-b-reanswer free=runs/free
```

`run.py --conversations N` runs only the first N conversations: a cheap first check.

**Call caps.** On the Codex path, `--budget-calls N` stops a run before model call N+1 (retries
count). Every reply is cached in `<output>/codex-calls.jsonl`, keyed by model, effort, schema and
prompt, so rerunning the same command resumes where it stopped without paying again. For scale: the
published free-engine run made 1,138 paid model calls (note-taker, answerer and judge) and re-read
312 from the cache; calibration (a) made 1,050 calls and (b) 1,454. The answerer, judge and
note-taker are sampled, so a fresh run will not reproduce every answer; it should land inside the
published intervals.

**Checking.** `verify.py DIR` recomputes every question's score from its saved verdicts and the
summary from the scores, and checks that every question of the covered conversations was answered
once. It calls no model (it does download the dataset to list the questions). `verify.py --prompts`
re-downloads ExaBase's file and checks its hash. `table.py` refuses any run that fails `verify.py`.
`goldcheck.py DIR` is a diagnostic: for a run that kept its recall pages, it reports whether each
gold answer was on the page.

## Published results

The stripped results behind the write-up are in [results/100k/](results/100k/README.md); its README
maps each folder to a column. Check any of them with `uv run python verify.py results/100k/<folder>`.

## Licences

- Harness code (`run.py`, `dataset.py`, `llm.py`, `scoring.py`, `verify.py`, `log.py`; `past_api.py` before it was replaced): MIT, Copyright (c) 2026 Past Corp. Notice in `LICENSE-past`; third-party carve-outs in `NOTICE.md` (past.dev's notice, unchanged; it describes their repository, which commits `exabase_prompts.py`, and this fork does not).
- ExaBase's prompts: not committed. `prompts.ensure_exabase_prompts(cache_dir)` downloads ExaBase's published file into `.cache/` (git-ignored) and checks SHA-256 `584b672d957f3e6f75e96f881ec31da22e1039da1df5fbedb53dc6a9145cfc43`; a mismatch raises. The prompts remain ExaBase's work.
- The BEAM dataset and results holding its text: CC BY-SA 4.0 (see `results/100k/README.md`).
- The rest (adapters, note-taker, calibration, table, the Codex model path, tests): Apache-2.0, as this repository.

## Tests

Offline: the fake backend, a fake `codex` binary and stubbed model calls.

    uv run --with pytest pytest -q
