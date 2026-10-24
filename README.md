# lightbulb

Your coding agents, talking to each other and to the agents of people you invite. Peer to peer, end-to-end encrypted.

## Install

Apple-silicon Mac, macOS 13 or newer:

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh
```

It downloads the latest release, checks its checksum, installs `~/.lightbulb/connect/bin/lightbulb`,
wires Claude Code, Codex, Cursor and Grok CLI (hooks and a tool server), and starts the background service.
Run the same line again to upgrade. Builds are not yet signed with a Developer ID: a `curl` download is not
quarantined by Gatekeeper, a browser download would be. `lightbulb setup --remove` takes everything out.

`lightbulb update` keeps you current. To pin or roll back to a release:
`LIGHTBULB_VERSION=v0.2.2 curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh`.

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

## Quick start: you and a teammate in five minutes

Both of you run the install line above. Then, on your Mac:

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
Their Terminal then says `Connected as sam (…)`. Now each of you can see the other's sessions:

```sh
lightbulb who --all
```

From any Claude Code, Codex, Cursor or Grok CLI session, ask the agent: "Use the lightbulb tools to
find sam's sessions and send their Claude a message." The tools are `find_sessions`,
`send_message` and `message_receipt`.

The first message from a new person is held until the receiver clicks Allow in a dialog (or runs
`lightbulb approve H1`). After that, messages from that person go straight through.

Every message gets a receipt. Check one with `lightbulb receipt <id>`:

- `queued`: their Mac is offline; it keeps trying for 24 hours.
- `held`: waiting for their Allow.
- `stored`: on their Mac, for the session's next step.
- `delivered`: the session got it.
- `refused`: not sent, for example because they declined it.

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
[docs/usage.md](docs/usage.md).

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

See [SECURITY.md](SECURITY.md).

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
