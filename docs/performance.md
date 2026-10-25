# Performance

One team of 20 computers with 50 sessions, on one Mac. Each computer is its own `lightbulb
daemon` with a temp home; they find each other through the loopback files in
`/tmp/lightbulb-<uid>/` (Local Network is not used).

## Command

```sh
cargo test --test perf -- --ignored --nocapture
```

The test is `tests/perf.rs`. It is ignored by default, so `cargo test` does not run it.

## Machine and build

- Apple M1 Pro, 16 GB, macOS 26.2.
- Commit: v0.2.2 (`73d8e59`) plus this harness.
- Debug build (the `cargo test` default). Other test runs were active on the same Mac at the
  time.

## What the test does

1. Starts 20 daemons. Each one allows every other person first, so no message is held.
2. Computer 0 makes the team. Each of the other 19 joins in turn, with a new open link
   from computer 0.
3. Registers 30 sessions round-robin, then 20 more one at a time (50 in total).
4. Sends 100 messages across the mesh. Each one goes from computer X to a session on
   computer Y.

## Numbers (one clean run)

| measure | result |
|---|---|
| join, invite to `admitted` (19 joins) | min 80 ms / median 130 ms / max 425 ms |
| full mesh online after the last join | 107 ms |
| directory refresh, new session on X until Y lists it (20 samples) | median 266 ms / p95 886 ms |
| message, `send` on X until X's receipt reads `delivered` (100 messages) | median 8 ms / p95 9 ms (before the flush loop was woken: median 960 ms / p95 1009 ms) |
| of which: `send` until X reads `stored` | median 7 ms / p95 17 ms |
| of which: `send` until Y's session takes it | median 7 ms / p95 16 ms |
| resident memory per daemon after the run | min 35.6 MB / median 36.0 MB / max 38.0 MB |
| total wall time | 78.1 s |

Join time includes two short `perl` helper processes, one for `invite` and one for `join`,
because both operations are for a person only. Directory refresh polls the `directory`
operation, which is what `who --all` prints; the test also runs `who --all` once to check
it. Message time counts until Y's session takes the message from its inbox. This is when
the receipt becomes `delivered`. The test polls every 2 ms.

## What the numbers mean

The charter's target is 50 sessions on 20 computers, and the test reaches it in about
80 s on one laptop. The memory is flat: about 36 MB per daemon, with a spread of 2 MB. A
join takes about 0.1 s, and the full mesh connects about 0.1 s after the last join. A
message arrives in about 7 ms and is taken in about 7 ms, and the sender reads `delivered`
about 1 ms later: taking or reading a message wakes `flush_loop` (`src/net/mod.rs`), which
sends the receipt at once. (Before that, the receipt waited for the loop's 1 s poll, and
`delivered` took about 1 s; a later run measured median 8 ms / p95 9 ms with the wake.) One
result looks wrong: the median directory refresh is
the 250 ms presence coalesce (`src/presence.rs`, `COALESCE`). But the p95 is 0.8 to 1 s in
each run, and this is not explained. The directory from a few computers comes one or more
coalesce windows late. It is worth a look before the team gets bigger. All the numbers are
from a debug build: a release build will be faster on CPU-bound steps, but the 250 ms
timer stays the same.

## Re-run on 0.4.4 (2026-10-25)

Same command, same Mac and debug build, commit `cea7369` (v0.4.4), one clean run:

| measure | 0.2.2 | 0.4.4 |
|---|---|---|
| join, invite to `admitted` (19 joins) | 80 / 130 / 425 ms | min 117 / median 178 / max 400 ms |
| full mesh online after the last join | 107 ms | 50 ms |
| directory refresh (20 samples) | median 266 / p95 886 ms | median 265 / p95 902 ms |
| message, `send` until `delivered` (100) | median 8 / p95 9 ms | median 9 / p95 11 ms |
| of which `send` until `stored` | 7 / 17 ms | 6 / 7 ms |
| of which `send` until Y takes it | 7 / 16 ms | 6 / 7 ms |
| resident memory per daemon | 35.6 / 36.0 / 38.0 MB | min 35.0 / median 35.5 / max 41.5 MB |
| total wall time | 78.1 s | 15.1 s |

Nothing regressed beyond run-to-run noise; join is about 50 ms slower at the median (the four-word
code and Argon2 work added since 0.2.2 sit on that path). The directory p95 (~0.9 s) is unchanged
and still unexplained. Still no numbers for a network of real computers.
