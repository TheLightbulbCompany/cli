# Internal tool on data

Switches: database, sign-in with roles.

**Builds first:** import the attached sheet or file into tables (else sample data in
`drizzle/seed.sql`), then a list with search and filters, a detail page, and edit. Every query is
scoped to who is asking.

**Asks (one card at most):** who may see or change what, if the request doesn't say. If people sign
up for themselves, `accounts.md`.

**Never asks:** column types (read them from the data), page layout, pagination size.

**Done when:** two different people are checked, and neither can see the other's private rows.
