# Two-Mac acceptance on a GitHub macOS runner

`.github/workflows/two-mac-test.yml` uses a GitHub-hosted `macos-latest` runner as a second Mac on
a different network (GitHub's). It proves, with the released CLI and nothing built from source:

- the public install line installs and starts the background service on a fresh Mac;
- `join` with an `--open` invite admits a computer from another network;
- a message sent from that computer reaches a person's mailbox on the home Mac, through the hold
  and the Allow, and its receipt moves to `delivered` or `read`;
- `leave` puts the runner back on its own network.

## Run it

On the home Mac (the released daemon running under launchd):

1. Mint an open invite and store it as the secret. Paste the ticket at the prompt; do not put it
   in a command line or a file:
   ```
   lightbulb invite --open
   gh secret set E2E_TICKET --repo TheLightbulbCompany/cli
   ```
2. Dispatch, with a name on the home Mac as the target:
   ```
   gh workflow run two-mac-test.yml --repo TheLightbulbCompany/cli -f target=<name>/inbox
   ```
   `wait_seconds` (default `600`) is how long the job waits after the send.
3. The first message from the runner is held. When it appears (`lightbulb held`), allow it:
   `lightbulb approve H<n>`.
4. The receipt reads `stored` until the mailbox is read. Read it:
   `lightbulb listen --as inbox --once`. The receipt then reads `delivered` and the job passes.
5. Read the job summary: version, the join result, `team` after join and after leave, the receipt
   states in order, and the seconds from send to delivered.

Afterwards, on the home Mac, kill the open ticket and delete the secret:

```
lightbulb invite --new
gh secret delete E2E_TICKET --repo TheLightbulbCompany/cli
```

The runner's `ci-mac` membership ends with its `leave`. If the job died before that step, remove
it on the home Mac: `lightbulb remove ci-mac`.

## Limits

- The runner has no agent sessions, so the message goes to a person's terminal mailbox, not to an
  agent.
- Hole punching from GitHub's network may fail, and the connection then goes through the n0 relay.
  That is still a real cross-network test.
- `join` and `leave` are person-only. The daemon decides that from the socket peer's process
  ancestry (no `claude`, `codex`, `cursor-agent` or `grok` above it), not from a TTY. The workflow
  runs them under `script` anyway.
