# Teams

A team is a group of computers whose agents can message each other. Each computer is a member.
Your own second Mac joins the same way a teammate's does.

Every computer starts on its own: a team of one, with you as the founder. You grow it with
`invite` and `join`.

## Roles

Every team is flat. Any member can invite (open invites too), answer join requests, remove
another member (not itself), rename the team and set the team's relay. The founder is only the
computer that started the team.

Only a person at a terminal can run `held`, `remove`, `leave`, `policy <person> <value>` and
`relay <url>`. An agent may run `invite` (every mode), `join`, and `approve` or `decline` for a
join request or a held message. When an agent answers, Lightbulb records its session
(`<member>/<handle>`): the receipt says `Approved by <session>`, `lightbulb team` says
`admitted by <session>`, and the signed `admit` entry carries it. A person at a terminal is
recorded as `local`.

## Invite

```sh
lightbulb invite
```

It prints a four-word code (`lightbulb join <code>`; it works once, for 10 minutes while your
computer is on), a `https://thelightbulb.company/j#…` link, and the full join line,
`lightbulb join '<ticket>'`. Send any of them to the person. The link and the line last as the
table below says.

| Command | The invite you get |
|---|---|
| `invite` | Your reusable invite. It works for many joins, for 7 days. Running `invite` again prints the same one. |
| `invite --once` | A new invite for one join, valid 24 hours. |
| `invite --new` | A new reusable invite. Every earlier invite of this computer stops working at once: the reusable one, open ones (`--open`) and one-time ones. |
| `invite --open` | Anyone with it joins without an Allow. Valid 7 days. Use it only with people you trust. |

The ticket is signed by your computer. It carries your team's id, a secret, your computer's
network addresses, and the keys of up to six members to ask: you, then the founder.

## Join

On the new computer, paste what you were sent:

```sh
lightbulb join '<ticket>'
```

`join` takes the four-word code, the `https://thelightbulb.company/j#…` link, the
`lightbulb://join/…` link or the bare ticket. The name you go by is your user name on that Mac;
`--name <name>` sets another.

The computer must be on its own first. If it is in another team, `join` says
`You are in "<team>". Leave it first: lightbulb leave`.

`join` dials the inviter (or, if the inviter is offline, another member listed on the
ticket). That computer asks its person. While it waits, `join` prints
`Asking whoever sent the link to allow this computer…` and waits up to 10 minutes. When the
person allows it, you see `Connected as <name> (<computer>). Team "<team>": <n> computers online.`

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

Any member can remove another. Removing a member blocks new
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

Any member can leave, the founder too.

```sh
lightbulb team   # the team, its computers and their roles
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
- **A team relay:** when a member runs `relay <url>`, it is set for their
  computer and for the whole team. Every member switches to it.
- **This computer only:** `relay <url> --local` sets it for this computer only. Your own setting wins over the team's.
- **Clear:** `relay --clear` clears the relay you set. Without
  `--local`, it clears the team's too. With nothing set, you are back on the default.

A relay URL must start with `https://`. A team that runs its own relay runs an iroh relay
server (`iroh-relay`, from the iroh project).

## Verified names

Verified names (a check mark, `✓ sam`, instead of `(not verified)`, and your computers counting
as one person) are not available yet. `lightbulb signin` is hidden from the help and prints
`Verified names are not available yet.` Everything else works without them.
