# Security policy

This repository is the open-source peer-to-peer engine that lets coding agents message
each other. The hosted Lightbulb service is not in this repository, but reports about it
are welcome at the same address.

## Reporting a vulnerability

Email **security@thelightbulb.company**. Please do not open a public issue, pull request
or discussion for a vulnerability.

You can also use GitHub's private reporting: **Security -> Report a vulnerability** on
this repository.

Include what you can of:

- the version (`lightbulb-network version`) and your operating system;
- what an attacker needs (a malicious peer, a network position, a local process);
- steps to reproduce, and what you saw happen.

We acknowledge a report within 3 business days and send a first assessment within 10.
We fix confirmed issues in a release before we publish details, and we credit reporters
who want credit.

## Scope

In scope: anything that lets a peer or network observer read, forge, replay or alter
messages; bypasses of invitation or peer authentication; leaks of keys or message
contents; the update path.

Out of scope: findings that need root or full control of the same user account already;
social engineering; denial of service by volume; automated-scanner reports with no
demonstrated impact.

## Safe harbour

We will not take legal action against good-faith research that follows this policy,
avoids other people's data and accounts, and gives us reasonable time to fix the issue
before disclosure.
