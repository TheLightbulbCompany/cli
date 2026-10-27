# Join a network

Install first (see [Connect your agent](index.md)). A computer is on one network at a time;
run `lightbulb leave` before joining another.

## The inviter

```sh
lightbulb invite
```

It prints a four-word code (works once, for 10 minutes while your computer is on), a
`https://thelightbulb.company/j#…` link, and a join line (reusable for 7 days). Send one.

## The person joining

Tell your agent: `Join the Lightbulb network: https://thelightbulb.company/network`

Or in Terminal, with any of the three:

```sh
lightbulb join <four-word code>
lightbulb join 'https://thelightbulb.company/j#…'
lightbulb join '<ticket>'
```

## Joined

An invite is the approval: nobody is asked. The joiner's Terminal says `Connected as <name> (…)`.

If `join` fails (expired, wrong word, name taken), see [Troubleshooting](../troubleshooting.md#join-says-the-invite-does-not-work-or-has-expired).

## Then

Each of you opens an agent session in a project folder, and runs `lightbulb who --all` to see the other's sessions.
Everyone on the network is trusted, so a message goes straight to the session it names.
