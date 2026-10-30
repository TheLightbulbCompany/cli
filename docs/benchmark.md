# The benchmark build

The memory benchmark (BEAM, see [benchmarks/beam-100k.md](benchmarks/beam-100k.md)) needs three things a
released daemon cannot do: store a conversation's turns at the dataset's own times, finish embedding
before a question is asked, and recall as of a past moment. These are behind the `bench` cargo
feature. A normal build, and every release, has none of them.

```sh
cargo build --release                       # the CLI as released: no bench ops
cargo build --release --features bench      # adds the ops below
```

## What the feature adds

All of it is on the daemon's IPC socket (`<home>/sock`), one JSON line per request.

| Op | Body | Answer | What it does |
|---|---|---|---|
| `bench.ingest_turns` | `{session, turns: [{role, text, at}]}` | `{stored}` | Records each turn through the same path as session capture, at `at` (unix seconds) instead of now. |
| `bench.settle` | `{}` | `{embedded}` | Embeds every entry and turn that has no vector yet, so a question sees all of it. |
| `memory.recall` | adds `as_of` (unix seconds) | echoes `as_of` | Replays only entries whose `when` is at or before `as_of`, and only turns recorded by then. A value replaced later is still current at that moment. |

Without the feature, `bench.*` is an unknown op and `memory.recall` ignores `as_of`. The code is in
`src/daemon/ipc.rs` (`bench_ingest_turns`, `bench_settle`, `bench_as_of`), `src/memory.rs`
(`search_as_of`, `replay_as_of`) and `src/capture.rs` (`search_turns`'s `as_of`). Entries go in through
the normal `memory.remember`, which already takes a `when`.

## Tests

```sh
cargo test --features bench --test bench_ops
cargo clippy --all-targets --features bench -- -D warnings
```

CI runs both on every pull request, next to the normal build's tests. The tests drive a daemon
through debug-only test settings, so they need debug assertions: a plain `--release` test run
fails. For an optimised test build, add `CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true`.

## The build used for the published run

The run used a Linux release build, made and run in containers so the daemons had no network:

```sh
docker volume create lb-bench-target
docker run --rm -v "$PWD":/src:ro -v lb-bench-target:/out -w /src rust:1.91-bookworm \
  cargo build --release --locked --features bench --target-dir /out
```

The runner expects a container named `lb-bench` (Debian bookworm, which has `perl`) with no network,
the build at `/out/release/lightbulb`, and the embedding model at `/models/model`:

```sh
docker run -d --name lb-bench --network none -v lb-bench-target:/out:ro debian:bookworm \
  sh -c 'mkdir -p /bench /models; sleep infinity'
docker cp ./model lb-bench:/models/model
```

`./model` holds the three files of `minishlab/potion-base-8M` at the revision and SHA-256 sums pinned
in `src/embed_model.rs` (`config.json`, `tokenizer.json`, `model.safetensors`). A normal
`lightbulb setup` downloads and checks the same files into `~/.lightbulb/connect/model/`; copy that
folder.

The runner starts one daemon per conversation under `/bench/<project>/`, with LAN and telemetry off
and a config that keeps captured turns for 100 years. It talks to each socket through one
`docker exec -i lb-bench perl …` bridge. The runner is described in
[benchmarks/beam-100k.md](benchmarks/beam-100k.md#reproducing).
