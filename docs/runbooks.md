# Runbooks

Three short procedures: a relay outage, a compromised key, and a bad release.

## Relay outage

Computers on the same network, or with a direct path, do not need a relay. A relay outage
affects computers that can only reach each other through one.

**Symptoms**

- `lightbulb team` shows teammates `· offline` while their Macs are on.
- Receipts stay `queued`: `Not delivered yet: <computer> is offline.` or `… has not confirmed it
  yet`.
- `~/.lightbulb/connect/daemon.log` shows `net: dial <key>: failed` again and again.
- `lightbulb doctor` still reads `Background service  running`; it does not test relays.
  `lightbulb relay` shows which relays are in use.

**What to do**

1. Check the relay's own status. For the default relays (`*.relay.n0.iroh.link`), that is the
   iroh project (iroh.computer). For a team relay, ask whoever runs it.
2. Switch the team to a relay that works. The founder or an admin runs:

   ```sh
   lightbulb relay https://relay.example.com
   ```

   Every member adopts it once they get the change from a member they can still reach.
3. A computer that cannot reach anyone gets no team change. On that computer, set it for that
   computer only:

   ```sh
   lightbulb relay https://relay.example.com --local
   ```

4. When the outage is over, go back with `lightbulb relay --clear` (founder or admin: for the
   team too; with `--local`: this computer only). With nothing set, the default relays are used.

Nothing is lost during an outage. Messages wait on the sender's computer and are retried for
24 hours.

## Key compromise

### This computer's key was copied

Someone with `~/.lightbulb/connect/identity.key` can act as this computer in your team. Replace
the key:

1. Take the old key out of the team: `lightbulb leave`. If you are the founder, first make
   someone else the founder (`lightbulb admin <name> --founder`).
2. Stop the service: close your agent sessions, then `lightbulb setup --remove`.
3. Delete the key and the state that belongs to it:

   ```sh
   rm ~/.lightbulb/connect/identity.key ~/.lightbulb/connect/state.db
   ```

   `state.db` holds this computer's team record, signed for the old key, and its messages. Copy
   it somewhere first if you need its history.
4. Start again: `lightbulb setup`. The service makes a new key.
5. Ask a teammate for a new invite (`lightbulb invite`) and `join` again.

If the computer itself is lost or you cannot run `leave` on it, the founder or an admin removes
it: `lightbulb remove <name>`. Then join from the new computer with a new invite.

### A team member is no longer trusted

The founder or an admin runs:

```sh
lightbulb remove <name>
```

New connections and new messages from that computer are blocked. Every live invite in the team
stops working, so the removed person cannot rejoin with an invite they kept. The answer prints a
new invite line; give it only to people who should join. Check with `lightbulb team`.

### An invite line was seen by someone else

A stolen invite line **can**: ask to join until it expires (7 days; 24 hours for `--once`). For
a normal invite the inviter still has to click Allow. For an `--open` invite, it joins without
asking.

It **cannot**: read messages, see sessions, or act as a member without being admitted.

It shows: the team's id, the inviter's network addresses, and the public keys of up to six
members (the inviter, the founder, the admins).

To end it: the inviter runs `lightbulb invite --new` (ends their reusable invite), or the
founder or an admin removes any member, which ends every live invite. If an `--open` invite was
used by someone you do not know, remove them: `lightbulb remove <name>`.

## Bad release

If a new release breaks something, go back to the previous one and pin it.

**curl install** (`vX.Y.Z` is the release tag you want; the setting goes before `sh`, which runs
the script)

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | LIGHTBULB_VERSION=vX.Y.Z sh
```

**npm**

```sh
npm i -g lightbulb-cli@X.Y.Z
lightbulb setup
```

**Homebrew** always installs the latest release. To go back, use the curl or npm line above.

`setup` (which the curl install runs) copies the binary to `~/.lightbulb/connect/bin/lightbulb`
and restarts the service on it. Check with `lightbulb version` and `lightbulb doctor`. Your key,
team and messages stay as they were.

Then tell us: open an issue with the output of `lightbulb version` and `lightbulb doctor`.
