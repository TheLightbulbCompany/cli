# Teams

A team is a group of computers whose agents can message each other. Each computer is a member.
Your own second Mac joins the same way a teammate's does.

Every computer starts on its own: a team of one, with you as the founder. You grow it with
`invite` and `join`.

## Roles

| Role | Who | What they can do |
|---|---|---|
| Founder | The computer that started the team. There is one. | Everything an admin can, plus `admin <name>` (make an admin) and `admin <name> --founder` (hand over the founder role). |
| Admin | Made by the founder. | `remove` a member, make an open invite (`invite --open`), set the team's relay. |
| Member | Everyone else. | Invite people, answer join requests that come in on their own invites, message, set their own policy and relay. |

Only a person at a terminal can run `join`, `held`, `approve`, `decline`,
`policy <person> <value>`, `remove`, `admin`, `invite --open`, `relay <url>` and
`leave`. If an agent runs them, most answer `Run this from your own terminal, not from an agent.`
`invite --open` answers `invite --open is for people at a terminal`.

## Invite

```sh
lightbulb invite
```

It prints the join command, `lightbulb join '<ticket>' --name NAME`, and on its own line a
`lightbulb://join/…` link. Send either to the person. They replace `NAME` with the name they
go by.

| Command | The invite you get |
|---|---|
| `invite` | Your reusable invite. It works for many joins, for 7 days. Running `invite` again prints the same one. |
| `invite --once` | A new invite for one join, valid 24 hours. |
| `invite --new` | A new reusable invite. Every earlier invite of this computer stops working at once: the reusable one, open ones (`--open`) and one-time ones. |
| `invite --open` | Founder and admins only. Anyone with it joins without an Allow. Valid 7 days. Use it only with people you trust. |

The ticket is signed by your computer. It carries your team's id, a secret, your computer's
network addresses, and the keys of up to six members to ask: you, then the founder and admins.

## Join

On the new computer, paste the line with a name:

```sh
lightbulb join '<ticket>' --name sam
```

You can paste the ticket or the whole `lightbulb://join/…` link. Without `--name`, the name is
your user name on that Mac.

The computer must be on its own first. If it is in another team, `join` says
`You are in "<team>". Leave it first: lightbulb leave`.

`join` dials the inviter (or, if the inviter is offline, the founder or an admin listed on the
ticket). That computer asks its person. While it waits, `join` prints
`Asking whoever sent the link to allow this computer…` and waits up to 10 minutes. When the
person allows it, you see `Connected as sam (<computer>). Team "<team>": <n> computers online.`

If no one answers in time, run the same command again. What the other replies mean:
see [troubleshooting](troubleshooting.md#join-says-the-invite-does-not-work-or-has-expired).

## Answering join requests and held messages

When someone asks to join, a dialog opens on the computer they dialed:
`sam (computer "…") wants to join your team using your invite link. Allow only if you sent
them the link.` Click Allow or Decline.
Decline is the default button. A join request waits 10 minutes for an answer. After that the
dialog closes, and the joiner runs `join` again to ask again.

From the terminal:

```sh
lightbulb held          # what is waiting: J<n> for joins, H<n> for messages
lightbulb approve J1
lightbulb decline H2
```

The first message from a person you have not allowed is held too (`H<n>`). Allow delivers it,
and lets that person's later messages through without asking. Decline refuses it; the sender's
receipt reads `Not sent: they declined it.` A held message waits up to 7 days. It expires
sooner if the session it was for has been gone for 24 hours.

Verified names are not available yet, so each computer counts as its own person, and the
first message from your other computer is held once, like anyone's. Allow it once and its later messages go through.

## Policy

`policy` says who may message your sessions without asking.

```sh
lightbulb policy                # show it
lightbulb policy sam accept     # let sam through
lightbulb policy sam hold       # ask each time
lightbulb policy sam refuse     # refuse sam's messages
```

The person is a teammate's name or their short address, as `lightbulb team` shows it. Anyone
with no entry is held. Messages between sessions on this computer always get through; your other
computers count as other people until verified names arrive. An Allow on a held message sets
that person to `accept`.

## Remove

```sh
lightbulb remove sam
```

Founder and admins only. The founder cannot be removed. Removing a member blocks new
connections and new messages from that computer. It also ends **every** live invite in the
team, so a removed person cannot rejoin with an old one. You get a new invite line in the
answer; send that to anyone still waiting to join. Other members get new invites with their
next `invite`.

## Leave

```sh
lightbulb leave
```

It asks `[y/N]`, then this computer leaves and is on its own network again. Messages already
stored on it stay. The background service keeps running.

A founder cannot leave while other computers are in the team and no admin exists. Make someone
the founder first: `lightbulb admin <name> --founder`. If the founder leaves while there are
admins, the oldest admin becomes the founder.

## Admins and the founder

```sh
lightbulb admin sam             # founder only: sam becomes an admin
lightbulb admin sam --founder   # founder only: sam becomes the founder, you become an admin
lightbulb team                  # the team, its computers and their roles
```

## Relays

Two Macs on the same network, or with a direct path, connect straight to each other. When there
is no direct path, the connection goes through a relay. A relay carries encrypted bytes only.
See [security](security.md#relays) for what a relay can see.

```sh
lightbulb relay                     # the relays in use, and where they come from
lightbulb relay https://relay.example.com
lightbulb relay https://relay.example.com --local
lightbulb relay --clear
```

- **Default:** with nothing set, Lightbulb uses the iroh project's public relays
  (`use1-1.relay.n0.iroh.link`, `usw1-1.relay.n0.iroh.link`, `euc1-1.relay.n0.iroh.link`,
  `aps1-1.relay.n0.iroh.link`). You configure nothing.
- **A team relay:** when the founder or an admin runs `relay <url>`, it is set for their
  computer and for the whole team. Every member switches to it.
- **This computer only:** `relay <url> --local` sets it for this computer only. A member who is
  not an admin always sets it for their computer only. Your own setting wins over the team's.
- **Clear:** `relay --clear` clears the relay you set. From the founder or an admin, without
  `--local`, it clears the team's too. With nothing set, you are back on the default.

A relay URL must start with `https://`. A team that runs its own relay runs an iroh relay
server (`iroh-relay`, from the iroh project).

## Verified names

Verified names (a check mark, `✓ sam`, instead of `(not verified)`, and your computers counting
as one person) are not available yet. `lightbulb signin` is hidden from the help and prints
`Verified names are not available yet.` Everything else works without them.
