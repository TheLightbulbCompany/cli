# BEAM 100K: Lightbulb's memory, first run

This is one run of the BEAM long-term-memory benchmark at its smallest size, 100K tokens per
conversation, against Lightbulb's two memory engines. It is a first measurement, not a leaderboard
entry. Read the caveats before the numbers.

## What BEAM is

BEAM ("Beyond a Million Tokens: Benchmarking and Enhancing Long-Term Memory in LLMs", Tavakoli et
al., [arXiv:2510.27246](https://arxiv.org/abs/2510.27246)) gives a system long user-assistant
conversations and then asks questions about them at a set moment in each conversation. The 100K
split has 20 conversations and 400 questions, 40 in each of ten categories (the rows of the table
below). The dataset is [Mohammadta/BEAM](https://huggingface.co/datasets/Mohammadta/BEAM), CC BY-SA
4.0. We used revision `3205395e897e7318c7b094ef4e6047b9b82dbb03`.

## Method

- **Harness.** Our runner is a fork of the MIT-licensed BEAM harness in the repository cited under
  the results table, copied at commit `a712cd993e249cadd214168c3c3ee674f89bd5e4`. The harness files
  are unchanged except three one-line imports. Its memory client is replaced by an adapter interface
  with one adapter per engine. Its scoring code is unchanged.
- **Prompts.** The answer and judge prompts are ExaBase's, from its published BEAM adapter
  ([source](https://fabric.so/p/beam-3VjBcqEVRofZyA5CeazeX)). The runner downloads that file at run
  time and refuses to run unless its SHA-256 is
  `584b672d957f3e6f75e96f881ec31da22e1039da1df5fbedb53dc6a9145cfc43`. We do not redistribute it.
- **Models.** The answerer and the judge are both `gpt-5.6-luna` with reasoning effort `none`,
  called through the Codex CLI (`codex exec` 0.163.0) under a ChatGPT subscription. Codex's own
  instructions were replaced by one line ("Follow the user's instructions.") and its tools, context
  and memories were switched off. About 7.4k tokens of Codex wrapper remain on every call. This is
  not the model path of the published results, which call the model directly through an API. We
  measured the difference (next section).
- **Recall budget.** Each question's recall page is cut to at most 100 results and 8,000 tokens,
  counting tokens the way the published results do (characters / 4, rounded up, per result). That
  count reproduces all 400 rows of the published file.
- **Scoring.** Each question is answered once and judged once. Scores are the mean over questions
  times 100, with a bootstrap 95% interval over questions (10,000 resamples). The intervals cover
  question sampling only, not judge noise: nothing was judged twice. Event ordering is scored as the
  harness scores it, `(tau_b + 1) / 2`, not with BEAM's official scorer.
- **Checks.** A verifier recomputes every score and every summary from the saved verdicts. All runs
  below pass it, and so does the published results file.

### Calibration: the Codex path offset

Because our model path differs, we ran two calibrations on the published run's own saved data:

- **(a) Our judge on the published answers.** We re-judged the published answers. Our verdict
  equals theirs on 95.8% of 878 verdicts. Our overall score is 1.39 points lower (90.69 against
  92.08). The largest gap is contradiction resolution (86.9% agreement, 6.9 points lower). Every
  other category agrees on at least 94.9% of verdicts and is within 3.8 points.
- **(b) Our answerer and judge on the published recall pages.** We answered from the published
  run's saved recall pages and judged with our judge. The overall is 3.45 points lower than
  published (88.63 against 92.08).

Column (b) holds the answerer, judge, prompts and budget fixed and changes only the memory. It is the
like-for-like column for our engines. Compare our engines with (b), not with the published number.

## Results

BEAM 100K, 20 conversations, 400 questions. Mean x 100 with a bootstrap 95% interval over questions.
Every column except the first uses `gpt-5.6-luna` through the Codex CLI as answerer and judge.

| Category | Published (1) | (a) Our judge, published answers | (b) Our answerer + judge, published pages | Free engine, page as shipped in v0.7.2 | Workspace engine (2) |
|---|---:|---:|---:|---:|---:|
| abstention | 98.75 [96.2, 100.0] | 95.00 [87.5, 100.0] | 97.50 [92.5, 100.0] | 98.75 [96.2, 100.0] | 100.00 [100.0, 100.0] |
| contradiction_resolution | 78.12 [74.4, 81.9] | 71.25 [68.1, 74.4] | 69.38 [65.9, 72.5] | 67.81 [64.1, 71.2] | 65.94 [60.9, 70.6] |
| event_ordering | 100.00 [100.0, 100.0] | 100.00 [100.0, 100.0] | 98.78 [96.9, 100.0] | 99.94 [99.8, 100.0] | 100.00 [100.0, 100.0] |
| information_extraction | 95.00 [91.7, 97.9] | 93.75 [89.6, 97.3] | 89.69 [83.4, 94.9] | 84.69 [76.8, 91.6] | 71.93 [61.4, 81.9] |
| instruction_following | 96.25 [90.0, 100.0] | 96.88 [93.1, 100.0] | 91.88 [83.8, 98.1] | 92.50 [85.6, 98.1] | 86.88 [77.5, 95.0] |
| knowledge_update | 89.38 [79.4, 97.5] | 89.38 [79.4, 97.5] | 85.62 [75.0, 95.0] | 78.12 [65.6, 89.4] | 68.12 [53.8, 81.9] |
| multi_session_reasoning | 68.95 [57.6, 79.7] | 67.76 [56.5, 78.1] | 64.63 [52.8, 75.9] | 65.59 [53.8, 76.5] | 62.15 [50.6, 73.2] |
| preference_following | 100.00 [100.0, 100.0] | 100.00 [100.0, 100.0] | 98.12 [95.0, 100.0] | 100.00 [100.0, 100.0] | 99.38 [98.1, 100.0] |
| summarization | 99.38 [98.1, 100.0] | 99.17 [97.7, 100.0] | 98.25 [96.2, 99.8] | 97.69 [95.4, 99.4] | 98.85 [97.6, 99.8] |
| temporal_reasoning | 95.00 [88.8, 99.4] | 93.75 [87.5, 98.8] | 92.50 [86.2, 97.5] | 95.62 [92.5, 98.1] | 95.00 [88.8, 99.4] |
| **overall (mean over questions)** | **92.08 [90.0, 94.0]** | **90.69 [88.5, 92.7]** | **88.63 [86.2, 90.9]** | **88.07 [85.6, 90.4]** | **84.82 [82.0, 87.5]** |

(1) past.dev's published BEAM 100K results:
`beam/results/100k/results.jsonl` in
[github.com/pastdotdev/benchmarks](https://github.com/pastdotdev/benchmarks/tree/a712cd993e249cadd214168c3c3ee674f89bd5e4/beam)
at commit `a712cd993e249cadd214168c3c3ee674f89bd5e4` (CC BY-SA 4.0). Columns (a) and (b) are our
re-judging and re-answering of that same file.

(2) Measured on an unreleased branch against our development environment. See below.

### Reading the table

- **Free engine: 88.07 against (b)'s 88.63.** The difference, 0.56 points, is small next to either
  interval. The free engine is lower on information extraction (84.69 against 89.69) and knowledge
  update (78.12 against 85.62); at 40 questions per category those intervals overlap. It is level or
  ahead in the other categories.
- **Workspace engine: 84.82.** Paired over the same 400 questions it is 3.25 points
  [-5.42, -1.18] below the free engine and 3.81 points [-5.79, -1.92] below (b). Both gaps are
  outside the noise. Its weakest categories are information extraction (71.93), knowledge update
  (68.12) and contradiction resolution (65.94).
- **Against the published column.** The published 92.08 is not comparable with our engines'
  numbers: our model path alone costs about 3.45 points on the same pages.

## What each engine column is

### Free engine (this CLI)

The free engine is the memory in this repository: entries in a local SQLite store, embedded on the
computer with `minishlab/potion-base-8M`, recalled by text and meaning.

- **The page.** The answerer reads the daemon's `page` text for the question, exactly what an agent
  gets from `recall`: up to 50 facts and 50 turn excerpts (800-character windows around the
  question's words), each line with its author and date, cut by the daemon's own 100-result /
  8,000-token trim.
- **Which code.** The run used `loop/contested-line` at `94d73ff` with the `bench` ops on top. That
  change shipped as #95 (`d94c917`) in v0.7.2. The merged version adds two things: a "vs" value is
  labelled with the person an entry was written for, and the session-start block shows at most three
  lines per subject. Neither changes a question's page when one person wrote every entry, as here.
  We did not re-run on the v0.7.2 tag itself. v0.7.3 changed only `setup`.
- **Earlier pages.** The page has changed between releases, so a number is only meaningful with its
  page. The v0.7.0 page (5 turns, 1,600 characters) scored 73.82 [70.1, 77.5]. The v0.7.1 page
  (source identical to the v0.7.1 tag) scored 87.43 [84.8, 89.8]. 88.07 is the v0.7.2 page.
- **How it ran.** One `lightbulb daemon` per conversation, a release build with `--features bench`
  (see [../benchmark.md](../benchmark.md)), in a Linux container with no network. Turns went in at the
  dataset's times through `bench.ingest_turns`. The daemon keeps seconds, so turns of one session
  were placed one second apart rather than one microsecond. Order and day are kept, and displayed
  times map back exactly.

### A scripted note-taker

In the product, the free engine holds what an agent chose to `remember`. In this run, no agent was
working, so a scripted note-taker played that agent:

- `gpt-5.6-luna` through the same Codex path, with our own prompt (not ExaBase's).
- One call per 20 turns. It sees those turns and the subjects it already wrote. It never sees a
  question, a gold answer or a rubric.
- It wrote 3,164 entries through the real `memory.remember` in 296 calls. None were refused.
- The runner caches every model reply by its prompt. The later free-engine runs re-read the
  note-taker's replies from that cache, so every free-engine run held the same entries.

A different note-taker, or a real agent, would write different entries and get a different score.
The free-engine number measures this engine with this note-taker, not the engine alone.

### Workspace engine (2)

The workspace engine is the memory of a Lightbulb workspace. It runs in our backend on AWS Bedrock
AgentCore memory, which extracts claims from each conversation event. Its number is weaker evidence
than the free engine's:

- **Not released code.** It was measured on a branch of our backend (`feat/workspace-recall-page`
  `eae5d4b6a`) plus local benchmark routes, run on a laptop against our development AgentCore
  memory. A version of that branch has since been merged; we did not check what production runs.
- **No note-taker.** Each exchange went in as one event at the dataset's time, and AgentCore
  extracted 4,488 claims. The answerer read the text page an agent gets.
- **A known gap at measurement time.** Each claim line on the page was dated by the day it was
  ingested, not the day it was said. A fix for that has merged since and has not been measured.
- **Not reproducible from outside.** It needs our development AWS account. We publish it because it
  is the honest current state of the second engine, not as a number anyone can check.

## What was not run

- **BEAM 500K, 1M and 10M.** Only 100K was run. The larger splits have much longer conversations,
  and the note-taker reads every turn, so each costs many more model calls. 100K came
  first to measure the model-path offset and to fix the recall page. Nothing here says how either
  engine behaves at 1M or 10M tokens.
- **Repeats.** Each column is one run with one judgement per question. Run-to-run and judge noise
  are not in the intervals.
- **The published model path.** We did not call the model directly through an API, so we have no
  number on the published path for our engines.
- **BEAM's official event-ordering scorer.**

## Reproducing

### Code

| Part | Where | Commit |
|---|---|---|
| Free engine and `bench` ops | this repository | measured: `94d73ff` plus the bench commit `7dd1535`; current: v0.7.3 plus the same ops (this document's pull request) |
| Runner (harness fork, adapters, note-taker, calibration, verifier, table) | `benchmarks/beam/` in Lightbulb's monorepo | `9453eca96` |
| Per-question results, stripped | `benchmarks/beam/results/100k/` in the monorepo | `9453eca96` |
| Workspace engine and its bench routes | Lightbulb's monorepo | `eae5d4b6a`, routes `1f8906af2` and `323e26ef9` |

The monorepo is private. The runner and the stripped results will be published. Until they are,
this document is the method and the numbers cannot be checked by anyone outside Lightbulb.

The stripped results hold, per question, only ids, category, as-of time, the model's answer, the
judge's verdicts and the score: no questions, gold answers or recall pages. Event-ordering verdicts
keep BEAM's reference topic lists, so those files are CC BY-SA 4.0.

### Commands

The bench binary and its container are in [../benchmark.md](../benchmark.md). To build exactly the
measured binary, check out `94d73ff` and cherry-pick `7dd1535` before building.

From `benchmarks/beam/`, with the Codex CLI signed in to an isolated home:

```sh
export BEAM_LLM=codex BEAM_CODEX_HOME=/path/to/codex-home

# Calibrations, on the published results file (cited above)
uv run python calibrate.py rejudge  --past <published>/results.jsonl --output runs/cal-a-rejudge
uv run python calibrate.py reanswer --past <published>/results.jsonl --output runs/cal-b-reanswer

# Free engine, the daemon's page text as an agent gets it (container lb-bench running)
BEAM_FREE_SHIPPED=1 uv run python run.py --backend free --split 100k --output runs/free-100k

# Check every number, then print the table
uv run python verify.py runs/free-100k
uv run python table.py published=<published beam/results/100k folder> cal-a=runs/cal-a-rejudge cal-b=runs/cal-b-reanswer free=runs/free-100k
```

`--budget-calls N` stops a run before model call N+1, and a rerun resumes from the saved replies.
The free-engine run made 1,138 paid model calls, re-read 312 replies from the cache, and took
22 minutes.
Because the answerer, judge and note-taker are sampled, a fresh run will not reproduce every answer.
It should land inside the intervals above.
