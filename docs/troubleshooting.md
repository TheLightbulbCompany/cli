# Troubleshooting

Start with:

```sh
lightbulb doctor
```

It prints one line per check, then `All good.` or the fix for the first check that failed
(and exits 1). Fix that, run `doctor` again, and repeat.

## The `doctor` lines

### `Background service   not running`

The background service is not answering. Fix: `lightbulb setup`. It installs the service and
starts it.

If `setup` then says `The old background service did not stop (pid N). Run: kill N, then
lightbulb setup again.`, an old copy is still running and holds the state folder. Do what it
says.

### `Starts at login      no`

The login item (`~/Library/LaunchAgents/company.thelightbulb.connect.plist`) is not loaded, so
the service will not start at login. Fix: `lightbulb setup`.

If `setup` prints `Background service: could not start it (…)`, macOS refused to load the job.
Check that the file named there exists, then log out and in, and run `setup` again.

### `Local network        blocked by macOS`

macOS has not allowed lightbulb on the local network, so your other Macs on the same Wi-Fi
cannot find this one. Fix: System Settings > Privacy & Security > Local Network, and turn on
lightbulb. macOS asks once; if you missed the question, the switch is there.

Joins on the same Wi-Fi still work while it is blocked, because the invite carries your Mac's
addresses. Computers on other networks connect through a relay and do not need it.

### `State folder         readable by others`

`~/.lightbulb/connect` must be readable only by you (mode `700`, its socket `600`). Fix:
`chmod 700 ~/.lightbulb/connect`. If the line stays red, the socket is too open: run
`lightbulb setup` to restart the service, which makes the socket again.

### An agent: `not connected` or `hooks only — tools missing (run setup)`

The line is named after the agent (`Claude Code`, `Codex`, `Cursor`, `Grok CLI`).
`not installed` is fine: that agent's folder does not exist.

- `hooks only — tools missing (run setup)`: the hooks are there, but the `lightbulb` tool server
  entry is not. Fix: `lightbulb setup`.
- `not connected`: the hooks are not there. Fix: `lightbulb setup`. If setup prints
  `<agent>: not connected — <file>: <error>`, that config file could not be read (for example,
  it is not valid JSON or TOML). Fix the file, then run `setup` again. The backup is beside it as
  `<file>.lightbulb-backup`.

After a fix, restart the agent's open sessions so they load the tool server.

### `Command name         "lightbulb" is a link to another program (run setup)`

`~/.local/bin/lightbulb` points at a program that is not ours. Fix: `lightbulb setup` (or
`~/.lightbulb/connect/bin/lightbulb setup` if the `lightbulb` command runs the other program).
Setup replaces the link.

`Command name         not installed` means there is no `~/.local/bin/lightbulb`; the fix is the same.

If the line reads `~/.local/bin/lightbulb is another program's; put ~/.lightbulb/connect/bin on
your PATH`, a real file (not a link) is there. Setup leaves it alone. Add
`export PATH="$HOME/.lightbulb/connect/bin:$PATH"` to `~/.zshrc` and open a new terminal.

### `Relay`

This line is never red. It shows the relay in use: `default` (the iroh project's public relays),
`yours <url>`, `team <url>`, or `none`.

## Other problems

### `lightbulb: command not found` after the curl install

The command is in `~/.local/bin`, which a new Mac does not have on its PATH. Right after the
install, `setup` prints:
`Add this line to your shell profile (~/.zshrc), then open a new terminal: export PATH="$HOME/.local/bin:$PATH"`.
This applies to both of you: the person who invites and the teammate who joins. Add it to
`~/.zshrc`, then open a new terminal:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
```

Until then, run `~/.lightbulb/connect/bin/lightbulb`.

### `lightbulb version` still shows the old version after an upgrade

After `brew upgrade` or `npm i -g lightbulb-cli@latest`, run `lightbulb setup`, then
`lightbulb version`. If the version did not change, your PATH found the old copy in
`~/.local/bin` first, and that copy ran `setup`. Run setup from the package's own binary:

```sh
"$(brew --prefix)/bin/lightbulb" setup     # Homebrew
"$(npm prefix -g)/bin/lightbulb" setup     # npm
```

The curl install line, and `lightbulb update` in a later release, always replace the copy in
`~/.lightbulb/connect/bin`.

### `The background service did not answer in 10 s`

The full text ends `If macOS is asking whether lightbulb may read a folder, click Allow, then try
again.` Look for a macOS permission window, answer it, and run the command again. If no window
is open, run `lightbulb doctor`.

### Codex asks you to trust a hook

`setup` writes Codex's hook trust for its own hooks, so Codex normally does not ask. It asks
when one of our hooks shares a hook group with another tool's hook; setup then prints
`Codex: the <event> hook shares a group with another tool's; Codex will ask you to trust it
once (/hooks)`. Trust it in Codex with `/hooks`. Until you do, Codex skips that hook.

### Two copies of the service

Only one service can use the state folder; a second one waits and touches nothing. If setup
says the old service did not stop, run the `kill` it prints, then `lightbulb setup`. If you
uninstalled with `setup --remove` and the service came back, an open agent session started it:
close your agent sessions, then run `setup --remove` again.

### Teammates' computers show `offline`, or a relay is down

`doctor` has no relay check. Signs that your computers cannot reach each other:
`lightbulb team` shows them `· offline`, and receipts stay `queued`
(`Not delivered yet: <computer> is offline.`). `~/.lightbulb/connect/daemon.log` has a line like
`net: dial <key>: failed` (logged once, when a dial starts failing). See the [relay outage runbook](runbooks.md#relay-outage).

If every command says
`identity.key does not match this computer's team record. Run lightbulb leave, then join again with a new invite.`,
the key was replaced (or deleted) while `state.db` was kept, so the team record is for a key this
computer no longer has. The background service will not start. `lightbulb leave` works without
it; then ask a teammate for a new invite and `join`.

## My teammate's message did not arrive

Ask the sender to run `lightbulb receipt <id>` (the id is in what `send` printed). An agent can
call `message_receipt`. What the answer means:

| Receipt | What happened | What to do |
|---|---|---|
| `Not delivered yet: <computer> is offline. Lightbulb keeps trying until <time>.` | The other Mac is asleep or unreachable. | Wake it. Lightbulb retries for 24 hours. |
| `… has not confirmed it yet` | It was sent; the other Mac has not answered. | Wait, or check the relay (above). |
| `Sent. <person> has to allow it first (they see a prompt).` | Held: the receiver has not allowed you yet. | The receiver runs `lightbulb held`, then `approve H<n>`. |
| `Sent. Stored on <computer>.` | On their Mac, waiting for the session. | The session reads it at its next step; an idle session when someone next types to it. See [agents](agents.md#how-a-message-reaches-a-session). |
| `Delivered …` / `Read …` | The session got it. | Nothing. |
| `Not sent: they declined it.` | The receiver clicked Decline. | Ask them. |
| `Not sent: they do not take messages from you.` | Their policy refuses you. | They run `lightbulb policy <you> accept`. |
| `Not sent: loop limit: …` / `paused after … agent replies` | A loop limit (see [security](security.md#loop-limits)). | A person sends the next message, or waits 10 minutes. |
| `Not sent: that computer is not in your team.` | The address is not a member. | Check `lightbulb team`. |
| `Not delivered: the other computer could not be reached for 24 hours.` | Expired on your Mac: it never reached theirs. | Send it again. |
| `<session> did not come back within 24 hours; the message expired.` | It reached their Mac, but the session never took it. | Send it again when the session is open. |
| `Nobody answered the Allow for 7 days; the message expired.` | Held, and never allowed or declined. | Ask them to look at `lightbulb held`, then send it again. |

If the receiver cannot see the sender's session at all, check `lightbulb who --all` on both
sides, and whether the session's folder is hidden (`lightbulb folder <path> on`).

## `join` says the invite does not work or has expired

| `join` says | Why | What to do |
|---|---|---|
| `This invite link does not work. Ask for a new one.` | The line was cut or changed when pasted; or the invite was ended by `invite --new` (which ends every earlier invite of that computer, open and one-time ones too), ended by a `remove`, or was a one-time invite already used. | Ask for a fresh line (`lightbulb invite`), and paste it whole, in single quotes. |
| `This invite link has expired. Ask for a new one.` | One-time invites last 24 hours, others 7 days. | Ask for a fresh line. |
| `No one from this network is online right now. …` | Neither the inviter nor the founder or admins listed on it are reachable. | Ask them to open their laptop, then run the same command again. |
| `No one has answered yet. Run the same command again to keep asking.` | The person did not click Allow in 10 minutes. | Run it again, and tell them to look for the dialog or run `lightbulb held`. |
| `The name "sam" is already used in this network. …` | Another member has that name. | Use the name it suggests, or another `--name`. |
| `You are in "<team>". Leave it first: lightbulb leave` | This computer is already in a team. | `lightbulb leave`, then join. |
| `Your request to join was declined.` | They clicked Decline. | Ask them. |
| `That name has characters Lightbulb cannot show. …` | The name or computer name has control characters. | Use a plain name. |

## Collecting information for an issue

Run `lightbulb version` and `lightbulb doctor`. The log is `~/.lightbulb/connect/daemon.log`; it
never holds message text. Never share `identity.key`.
