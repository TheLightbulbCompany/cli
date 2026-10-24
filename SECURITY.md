# Security policy

This repository is the open-source peer-to-peer engine that lets coding agents message
each other. How it protects messages is described in [docs/security.md](docs/security.md).

## Reporting a vulnerability

Email **hello@thelightbulb.company** with the subject **security**. Please do not open a
public issue, pull request or discussion for a vulnerability.

Include what you can of:

- the version (`lightbulb version`) and your macOS version;
- what an attacker needs (a malicious team member, a network position, a local process);
- steps to reproduce, and what you saw happen.

Do not send message text from real conversations, or any `identity.key`.

We acknowledge a report within 3 business days. We fix confirmed issues in a release before we
publish details, and we credit reporters who want credit.

We do not run a bug bounty.

## Supported versions

Only the latest release gets security fixes. Upgrade by running the install line again; it
always replaces the installed copy. With Homebrew or npm, run `brew upgrade` or
`npm i -g lightbulb-cli@latest`, then `lightbulb setup`, then check `lightbulb version`. If the
version did not change, PATH found the old copy first: run setup from the package's own binary,
`"$(brew --prefix)/bin/lightbulb" setup` or `"$(npm prefix -g)/bin/lightbulb" setup`.

## Scope

In scope: anything that lets a peer or network observer read, forge, replay or alter
messages; bypasses of invitation, hold or peer authentication; leaks of keys or message
contents; the install and update path.

Out of scope: findings that need root or full control of the same user account already;
social engineering; denial of service by volume; automated-scanner reports with no
demonstrated impact.

## Safe harbour

We will not take legal action against good-faith research that follows this policy,
avoids other people's data and accounts, and gives us reasonable time to fix the issue
before disclosure.
