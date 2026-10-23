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
