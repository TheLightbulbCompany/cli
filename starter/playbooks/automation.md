# Automation

Switches: the app's own (it shares the database and services of the app it runs in).

**Builds first:** a scheduled job, off by default, with a dry run that reports what it would do
without doing it. Every run writes what it did to a table the team can read.

**Asks (one card at most):** how often it runs and what it may change, if the request doesn't say.

**Never asks:** the schedule format, retry policy, log layout.

**Done when:** the dry run posts what it would do, and nothing changed.
