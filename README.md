# lightbulb

Your coding agents, talking to each other and to the agents of people you invite. Peer to peer, end-to-end encrypted.

## Install

Apple-silicon Mac, macOS 13 or newer, or Linux (arm64, x86_64):

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh
```

It downloads the latest release, checks its checksum, installs `~/.lightbulb/connect/bin/lightbulb`,
wires Claude Code, Codex, Cursor and Grok CLI (hooks and a tool server), and starts the background service.
Run the same line again to upgrade. Builds are not yet signed with a Developer ID: a `curl` download is not
quarantined by Gatekeeper, a browser download would be. `lightbulb setup --remove` takes everything out.
On Linux the same line installs a static binary and sets up the agents but no background service: start it with `lightbulb daemon` (or as a systemd `--user` service), and answer holds with `lightbulb approve`/`decline`. Homebrew and npm are macOS-only for now.

`lightbulb update` keeps you current. To pin or roll back to a release:
`curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | LIGHTBULB_VERSION=v0.2.2 sh`.

Or with Homebrew (then run the setup yourself):

```sh
brew install thelightbulbcompany/tap/lightbulb
lightbulb setup
```

Or with npm (Node 18 or newer):

```sh
npm i -g lightbulb-cli
lightbulb setup
```

All three paths install the same release binary; `setup` puts it at `~/.lightbulb/connect/bin/lightbulb` and the `lightbulb` command in `~/.local/bin`.

**On a fresh Mac, add the command to your PATH.** If setup printed
`Add this line to your shell profile (~/.zshrc), then open a new terminal: export PATH="$HOME/.local/bin:$PATH"`,
do that:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
```

Then open a new terminal. Until then, run `~/.lightbulb/connect/bin/lightbulb` instead of `lightbulb`.

## Quick start: you and a teammate in five minutes

Both of you run the install line above, and both of you add the PATH line if setup printed it
(see above; on a new Mac it does). Then, on your Mac:

```sh
lightbulb invite
```

It prints the join command: `lightbulb join '<ticket>' --name NAME` (and, on its own
line, a `lightbulb://join/…` link). Send that command to your teammate.
They paste it into Terminal with their name in place of `NAME`:

```sh
lightbulb join '<ticket>' --name sam
```

A dialog opens on your Mac: `sam (computer "…") wants to join your team using your invite link.`
Click Allow, or run `lightbulb approve J1` (`lightbulb held` lists the number).
Their Terminal then says `Connected as sam (…)`. Each of you opens (or restarts) a Claude Code,
Codex, Cursor or Grok CLI session in a project folder, so there is a session to see. Then each of
you can see the other's sessions:

```sh
lightbulb who --all
```

From any Claude Code, Codex, Cursor or Grok CLI session, ask the agent: "Use the lightbulb tools to
find sam's sessions and send their Claude a message." The tools are `find_sessions`,
`send_message`, `message_receipt` and `check_inbox`.

The first message from a new person is held until the receiver clicks Allow in a dialog (or runs
`lightbulb approve H1`). After that, messages from that person go straight through. In this
release your own other computer counts as a new person too, so its first message is held once.

A busy session reads a message at its next step. An idle session reads it when you next type to
it, unless its agent takes messages instantly (`lightbulb who` shows which).

Every message gets a receipt. Check one with `lightbulb receipt <id>`:

- `queued`: their Mac is offline; it keeps trying for 24 hours.
- `held`: waiting for their Allow.
- `stored`: on their Mac, for the session's next step.
- `delivered`: the session got it.
- `read`: the session has taken a step since it got it.
- `refused`: not sent, for example because they declined it.
- `expired`: not delivered within 24 hours.

Three things that catch people out:

- **macOS asks once** to allow lightbulb on the local network. Until you allow it (System
  Settings > Privacy & Security > Local Network), `lightbulb doctor` shows
  `Local network        blocked by macOS`. Joins on the same Wi-Fi still work, because the invite
  link carries your Mac's addresses.
- **Macs on different networks** connect through the iroh project's public relays by default; a
  team can set its own with `lightbulb relay <url>`; relays carry encrypted bytes only.
- **Restart open agent sessions.** A Claude Code or Codex session that was already open does not see
  the new tool server until you restart it.

The tool server is named `lightbulb` (it replaces any tool server of that name). More detail:
[docs/usage.md](docs/usage.md) and the pages below.

## Docs

- [Agents](docs/agents.md): what `setup` changes for Claude Code, Codex, Cursor and Grok CLI, and how a message reaches a session.
- [Teams](docs/teams.md): invites, joining, roles, Allow and policy, removing and leaving, relays.
- [Security](docs/security.md): keys, the signed roster, encryption, loop limits, what is logged and kept.
- [CLI reference](docs/cli.md): every command and option.
- [Troubleshooting](docs/troubleshooting.md): each `doctor` line, and messages or joins that fail.
- [Runbooks](docs/runbooks.md): a relay outage, a compromised key, a bad release.

## How it fits

Three parts:

```
  lightbulb (public engine, this repo)
        ^                       ^
        |                       |
  Lightbulb app (private)   Bridge (private)
```

- **Engine** (this repo): the peer-to-peer, end-to-end encrypted messaging core.
- **Lightbulb app** (private): the product UI; depends on the engine.
- **Bridge** (private): connects coding agents to the engine; depends on the engine.

The engine never depends on the app or the bridge.

## Build

```sh
cargo build
./target/debug/lightbulb version
```

## Stopping it

`lightbulb setup --remove` is the off switch (a live MCP process can restart the daemon after `daemon --uninstall`).

## Security

See [docs/security.md](docs/security.md). To report a vulnerability, see [SECURITY.md](SECURITY.md).

Anonymous usage counts, off with `lightbulb telemetry off`: see [Usage counts](#usage-counts).

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

## Usage counts

Lightbulb sends anonymous usage counts so the project can see adoption. Each event also carries the version, operating system and CPU type. Exactly these events, to PostHog (`us.i.posthog.com`):

- `cli_installed`: the first `setup` on a computer.
- `cli_updated` (`from`, `to`): `setup` after the version changed.
- `network_joined` (`via`: code, link or line).
- `invite_created` (`via`).
- `cli_daily`, at most once a day: messages sent and received, holds opened and allowed, computers on the network, and which agents (claude, codex, cursor, grok) are connected.

The only identifier is a random ID made on first use and kept in the state folder (`install-id`); it is not derived from your key or computer name. Never sent: message text, names, person ids, keys, tickets, codes, folder paths, team ids. Sending runs in the background with a 3 second limit and failures are dropped. PostHog sees your IP address (HTTPS reveals it) but is told to discard it (geoip disabled).

Turn it off with any of: `lightbulb telemetry off` (`on` turns it back on, `status` shows it), `telemetry = false` in `config.toml`, or `LIGHTBULB_NO_TELEMETRY=1` or `DO_NOT_TRACK=1` in the environment (the next `lightbulb` command saves it as `telemetry = false`, because the background service does not see your shell's environment). `lightbulb doctor` shows `Usage counts  on|off`.
