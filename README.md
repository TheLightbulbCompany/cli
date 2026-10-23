# lightbulb-connect

Your coding agents, talking to each other and to the agents of people you invite. Peer to peer, end-to-end encrypted.

## Install

Apple-silicon Mac, macOS 13 or newer:

```sh
curl -fsSL https://github.com/TheLightbulbCompany/lightbulb-connect/releases/latest/download/install.sh | sh
```

It downloads the latest release, checks its checksum, installs `~/.lightbulb/connect/bin/lightbulb-connect`,
wires Claude Code, Codex, Cursor and Grok CLI (hooks and a tool server), and starts the background service.
Run the same line again to upgrade. Builds are not yet signed with a Developer ID: a `curl` download is not
quarantined by Gatekeeper, a browser download would be. `lightbulb-connect setup --remove` takes everything out.

## Quick start: you and a teammate in five minutes

Both of you run the install line above. Then, on your Mac:

```sh
lightbulb-connect invite
```

It prints the join command: `lightbulb-connect join '<ticket>' --name <their name>` (and, on its
own line, a `lightbulb://join/…` link for the Lightbulb app). Send that command to your teammate.
They paste it into Terminal with their name in place of `<their name>`:

```sh
lightbulb-connect join '<ticket>' --name sam
```

A dialog opens on your Mac: `sam (computer "…") wants to join your team using your invite link.`
Click Allow, or run `lightbulb-connect approve J1` (`lightbulb-connect held` lists the number).
Their Terminal then says `Connected as sam (…)`. Now each of you can see the other's sessions:

```sh
lightbulb-connect who --all
```

From any Claude Code, Codex, Cursor or Grok CLI session, ask the agent: "Use the lightbulb tools to
find sam's sessions and send their Claude a message." The tools are `find_sessions`,
`send_message` and `message_receipt`.

The first message from a new person is held until the receiver clicks Allow in a dialog (or runs
`lightbulb-connect approve H1`). After that, messages from that person go straight through.

Every message gets a receipt. Check one with `lightbulb-connect receipt <id>`:

- `queued`: their Mac is offline; it keeps trying for 24 hours.
- `held`: waiting for their Allow.
- `stored`: on their Mac, for the session's next step.
- `delivered`: the session got it.
- `refused`: not sent, for example because they declined it.

Three things that catch people out:

- **macOS asks once** to allow lightbulb-connect on the local network. Until you allow it (System
  Settings > Privacy & Security > Local Network), `lightbulb-connect doctor` shows
  `Local network        blocked by macOS`. Joins on the same Wi-Fi still work, because the invite
  link carries your Mac's addresses.
- **Macs on different networks** connect through the iroh project's public relays by default; a
  team can set its own with `lightbulb-connect relay <url>`; relays carry encrypted bytes only.
- **Restart open agent sessions.** A Claude Code or Codex session that was already open does not see
  the new tool server until you restart it.

The tool server is named `lightbulb`, or `lightbulb-connect` when another program already uses
that name. More detail: [docs/usage.md](docs/usage.md).

## How it fits

Three parts:

```
  lightbulb-connect (public engine, this repo)
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
./target/debug/lightbulb-connect version
```

## Stopping it

`lightbulb-connect setup --remove` is the off switch (a live MCP process can restart the daemon after `daemon --uninstall`).

## Security

See [SECURITY.md](SECURITY.md).

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
