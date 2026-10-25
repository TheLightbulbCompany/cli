# Connect OpenCode

## Install

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | sh
```

If setup prints an `export PATH=...` line, add it to `~/.zshrc` and open a new terminal.

Works with OpenCode 1.x and 2.x.

## What setup wires

- `~/.config/opencode/plugins/lightbulb.js`: a plugin that runs the hooks (OpenCode has no command hooks).
- `~/.config/opencode/opencode.json`: the `lightbulb-network` tool server. A `.jsonc` file is edited only if it is plain JSON; otherwise setup prints the entry for you to paste.

## What you may need to do

OpenCode runs a background service that loads plugins only when it starts. After the first
setup, stop it once, then open OpenCode again:

```sh
pkill -f 'opencode.* serve'
```

## Try it

Say to OpenCode: `Use the lightbulb tools to find my sessions and send one a message.`

## Troubleshooting

Run `lightbulb doctor`. The OpenCode line should read `connected`.

- `not installed`: fine, OpenCode was not found on this computer.
- `hooks only — tools missing (run setup)`: run `lightbulb setup`, then restart OpenCode.
- `not connected`: run `lightbulb setup`. If it names a file it could not read, fix that file first.
- `source_unidentified` from a tool: Lightbulb could not tell which session called it. On OpenCode, this usually means the background service still runs an old plugin set: run the `pkill` line above and reopen OpenCode.
- A message to someone new is **held** until they allow it. This is not a failure; do not resend.
  The receiver runs `lightbulb held`, then `lightbulb approve H1`. Check with `lightbulb receipt <id>`.

More: [Troubleshooting](../troubleshooting.md), [Agents](../agents.md).
