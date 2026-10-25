# Runbooks

Three short procedures: a relay outage, a compromised key, and a bad release.

## Relay outage

Computers on the same network, or with a direct path, do not need a relay. A relay outage
affects computers that can only reach each other through one.

**Symptoms**

- `lightbulb team` shows teammates `· offline` while their Macs are on.
- Receipts stay `queued`: `Not delivered yet: <computer> is offline.` or `… has not confirmed it
  yet`.
- `~/.lightbulb/connect/daemon.log` shows `net: dial <key>: failed` (logged once, when dials start failing).
- `lightbulb doctor` still reads `Background service   running`; it does not test relays.
  `lightbulb relay` shows which relays are in use.

**What to do**

1. Check whether the relay is the problem. We know of no public status page for the default
   relays (`*.relay.n0.iroh.link`, run by the iroh project). Run `lightbulb relay` to see which
   relay is in use, and ask a teammate on another network whether they reach the team. For a
   team relay, ask whoever runs it.
2. Switch the team to a relay that works. Any member runs:

   ```sh
   lightbulb relay https://relay.example.com
   ```

   Every member adopts it once they get the change from a member they can still reach.
3. A computer that cannot reach anyone gets no team change. On that computer, set it for that
   computer only:

   ```sh
   lightbulb relay https://relay.example.com --local
   ```

4. When the outage is over, go back with `lightbulb relay --clear` (for the
   team too; with `--local`: this computer only). With nothing set, the default relays are used.

Nothing is lost during an outage. Messages wait on the sender's computer and are retried for
24 hours.

## Key compromise

### This computer's key was copied

Someone with `~/.lightbulb/connect/identity.key` can act as this computer in your team. Replace
the key:

1. Take the old key out of the team: `lightbulb leave`. 
2. Stop the service: close your agent sessions, then `lightbulb setup --remove`.
3. Delete the key and the state that belongs to it:

   ```sh
   rm ~/.lightbulb/connect/identity.key ~/.lightbulb/connect/state.db
   ```

   `state.db` holds this computer's team record, signed for the old key, and its messages. Copy
   it somewhere first if you need its history.
4. Start again: `lightbulb setup`. The service makes a new key.
5. Ask a teammate for a new invite (`lightbulb invite`) and `join` again.

If the computer itself is lost or you cannot run `leave` on it, another member removes
it: `lightbulb remove <name>`. Then join from the new computer with a new invite.

### A team member is no longer trusted

Any other member runs:

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
members (the inviter and the founder).

To end it:

- A normal reusable invite: the inviter runs `lightbulb invite --new`. That ends their reusable
  invite only.
- An `--open` invite: `invite --new` is not enough. Any member removes another member
  (that ends every live invite), or you wait out its 7 days.
- A one-time invite: it ends when it is used, or after 24 hours. A `remove` ends it too.

A later release makes `invite --new` end every invite of this computer. If an `--open` invite
was used by someone you do not know, remove them: `lightbulb remove <name>`.

## Bad release

If a new release breaks something, go back to the previous one and pin it. The daily automatic
update would put the bad release back within a day, so turn it off first, and on again once a fixed
release is out:

```sh
lightbulb update --auto off
lightbulb update --version vX.Y.Z
```

**If you publish the releases:** automatic updates install only a *newer* version, so a computer that
already took the bad one stays on it until a fixed release exists. Do both, right away:

1. Mark the previous good release as Latest, so `releases/latest` (which install.sh, `lightbulb
   update` and the daily check all read) stops pointing at the bad one:
   `gh release edit vGOOD --repo TheLightbulbCompany/cli --latest`.
2. Ship the fix as a new, higher patch release. Computers move to it within a day by themselves.

Do not delete the bad release's assets: a computer mid-update would fail its checksum and keep its
current binary, which is safe, but people pinning that version lose it.

**curl install** (`vX.Y.Z` is the release tag you want; the setting goes before `sh`, which runs
the script). This works only once the latest release's installer reads `LIGHTBULB_VERSION`
(v0.2.3 or later). The v0.2.2 installer ignores it and installs the latest release.

```sh
curl -fsSL https://github.com/TheLightbulbCompany/cli/releases/latest/download/install.sh | LIGHTBULB_VERSION=vX.Y.Z sh
```

**npm**

```sh
npm i -g lightbulb-cli@X.Y.Z
"$(npm prefix -g)/bin/lightbulb" setup
lightbulb version
```

Run `setup` from the npm package's own binary, as above. A plain `lightbulb setup` can find the
old copy in `~/.local/bin` first; that copy then runs `setup` and nothing changes.

**Homebrew** always installs the latest release. To go back, use the curl or npm line above.
After a `brew upgrade`, run `"$(brew --prefix)/bin/lightbulb" setup` for the same reason.

`setup` copies the binary it runs from to `~/.lightbulb/connect/bin/lightbulb` and restarts the
service on it. The curl install line (and `lightbulb update`, in a later release) always replaces
that copy. Check with `lightbulb version`: if it does not show X.Y.Z, PATH found the old copy
first; run setup from the package's binary as above. Then `lightbulb doctor`. Your key, team and
messages stay as they were.

Then tell us: open an issue with the output of `lightbulb version` and `lightbulb doctor`.
