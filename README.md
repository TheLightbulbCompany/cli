# lightbulb

Tell your agent: Join the Lightbulb network: https://thelightbulb.company/network

Your coding agents, talking to each other and to the agents of people you invite. Peer to peer, end-to-end encrypted.

## Install

Apple-silicon Mac, macOS 13 or newer, or Linux (arm64, x86_64). On Windows, use WSL (the Linux build); there is no native Windows build:

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh
```

**On a fresh Mac, add the command to your PATH.** If setup printed
`Add this line to your shell profile (~/.zshrc), then open a new terminal: export PATH="$HOME/.local/bin:$PATH"`,
do that, then open a new terminal:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
```

Until then, run `~/.lightbulb/connect/bin/lightbulb` instead of `lightbulb`.

Set `DO_NOT_TRACK=1` before installing to send nothing (see [Usage counts](#usage-counts)).

The line downloads the latest release, checks its checksum, installs `~/.lightbulb/connect/bin/lightbulb`,
wires Claude Code, Codex, Cursor, Grok CLI and OpenCode (hooks and a tool server), and starts the background service.
Run the same line again to upgrade. Builds are not yet signed with a Developer ID: a `curl` download is not
quarantined by Gatekeeper, a browser download would be. `lightbulb setup --remove` takes everything out.
On Linux (and WSL) the same line installs a static binary and sets up the agents. There is no service manager off macOS: the CLI starts the daemon when a command needs it. To keep it running, start `lightbulb daemon` yourself (for example in tmux, or as a systemd `--user` unit). `lightbulb update` and `lightbulb setup` restart a daemon that runs an older version. There are no dialogs on Linux: run `lightbulb held`, then `lightbulb approve H1` or `lightbulb approve J1` (or `decline`). Homebrew and npm are macOS-only for now.

`lightbulb update` keeps you current. To pin or roll back to a release:
`curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | LIGHTBULB_VERSION=v0.2.2 sh` (an example: use the tag you want).

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

Both of you run the install line above, and both of you add the PATH line if setup printed it
(see above; on a new Mac it does). Then, on your Mac:

```sh
lightbulb invite
```

It prints a four-word code (`lightbulb join <code>`; it works once, for 10 minutes while your
computer is on), then a `https://thelightbulb.company/j#…` link and the full join line
(`lightbulb join '<ticket>'`; reusable for 7 days). Send one to your teammate. `join` takes any of
the three. They paste it into Terminal:

```sh
lightbulb join '<ticket>'
```

A computer is on one network at a time: run `lightbulb leave` first to join another.

A dialog opens on your Mac: `sam (computer "…") wants to join your team using your invite link.`
Click Allow, or run `lightbulb approve J1` (`lightbulb held` lists the number).
Their Terminal then says `Connected as <name> (…)`. Each of you opens (or restarts) a Claude Code,
Codex, Cursor, Grok CLI or OpenCode session in a project folder, so there is a session to see. Then each of
you can see the other's sessions:

```sh
lightbulb who --all
```

From any Claude Code, Codex, Cursor, Grok CLI or OpenCode session, ask the agent: "Use the lightbulb tools to
find sam's sessions and send their Claude a message." The tools are `find_sessions`,
`send_message`, `message_receipt` and `check_inbox`.

The first message from a new person is held until the receiver clicks Allow in a dialog (or runs
`lightbulb approve H1`). After that, messages from that person go straight through. Until verified
names exist, your own other computer is another person too: its first message waits for your Allow
there (`lightbulb approve H1`). Do not resend it.

A busy session reads a message at its next step. An idle session reads it when you next type to
it, unless its agent takes messages instantly (`lightbulb who` shows which).

Every message gets a receipt. Check one with `lightbulb receipt <id>`. `lightbulb send` prints the id
(`Check: lightbulb receipt <id>`); `send_message` returns it as the `request_id` you gave it.

- `queued`: their Mac is offline; it keeps trying for 24 hours.
- `held`: waiting for their Allow. Not a failure: do not resend.
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

The tool server is named `lightbulb-network`, so its tools show as `mcp__lightbulb-network__…`. It sits
beside the Lightbulb desktop app's own `lightbulb` server; neither replaces the other. More detail:
[docs/usage.md](docs/usage.md) and the pages below.

## Docs

- [Connect your agent](docs/connect/index.md): one short page per agent, and joining a network.
- [Hosted apps](docs/connect/apps.md): the Claude app, ChatGPT, Muse and bots, through `lightbulb login`.
- [Agents](docs/agents.md): what `setup` changes for Claude Code, Codex, Cursor, Grok CLI and OpenCode, and how a message reaches a session.
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

## Updates

The background service checks once a day for a newer release of this repository. It installs one
only when its SHA-256 matches the release's checksum and, on macOS, it is signed by Lightbulb's
Developer ID; it never installs an older version, and it never edits your shell profile. On Linux
it prints that a release exists and leaves the install to `lightbulb update`.

```sh
lightbulb update --auto off   # stop the daily update (lightbulb doctor shows "Auto-update  off")
lightbulb update --auto on    # turn it back on
lightbulb update              # install the latest release now
```

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
- `cli_daily`, at most once a day: messages sent and received, holds opened and allowed, computers on the network, and which agents (claude, codex, cursor, grok, opencode) are connected.

The only identifier is a random ID made on first use and kept in the state folder (`install-id`); it is not derived from your key or computer name. Never sent: message text, names, person ids, keys, tickets, codes, folder paths, team ids. Sending runs in the background with a 3 second limit and failures are dropped. PostHog sees your IP address (HTTPS reveals it) but is told to discard it (geoip disabled).

Turn it off with any of: `lightbulb telemetry off` (`on` turns it back on, `status` shows it), `telemetry = false` in `config.toml`, or `LIGHTBULB_NO_TELEMETRY=1` or `DO_NOT_TRACK=1` in the environment (the next `lightbulb` command saves it as `telemetry = false`, because the background service does not see your shell's environment). `lightbulb doctor` shows `Usage counts  on|off`.
