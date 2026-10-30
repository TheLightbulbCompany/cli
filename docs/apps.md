# Apps you build

`lightbulb app` builds a small web app for a workspace: a site, a form, an internal tool, a
dashboard, a portal. A coding agent (or you) writes it in a folder, checks it, previews it, and
publishes it. This is not the same as the [hosted assistants](connect/apps.md) (Claude, ChatGPT,
Muse), which are chat apps that talk to Lightbulb. An app you build is a Next.js project that
Lightbulb deploys and puts behind a sign-in.

Every command calls the Lightbulb builder routes. The CLI never runs the Stripe or Vercel CLI.

## Commands

Every command takes `--json` (the default when stdout is not a terminal). A failure prints
`{"code","message","next"}` and exits 1. Run all commands except `new` inside the app's folder.

| Command | What it does |
|---|---|
| `app new <NAME> [--room <ROOM>] [--kind <KIND>]` | Creates `./<NAME>` from the starter, with one git commit. Next: `cd <NAME> && pnpm install`, then `app check`. `NAME` is a DNS label: `^[a-z][a-z0-9-]{1,38}[a-z0-9]$`. `--kind` is `site` (default), `content-site`, `form`, `internal-tool`, `dashboard`, `portal`, `payments` or `automation`. `--room` is saved in `.lightbulb/app.json`. |
| `app up [--room <ROOM>] [--approved <CARD>]` | Sends HEAD to Lightbulb, waits for the preview build (up to 10 minutes), and reports `done` or what is missing. If it is still building after that, `next` says to run `status`, not `up` again. A `failed` build prints `{"code","message","next"}` (`build_failed` when the backend gave no error) and exits 1. |
| `app publish [--reason <TEXT>] [--room <ROOM>] [--approved <CARD>]` | Puts the preview live as a new version, waiting for the build like `up`. Needs a passing `check --publish` and a passing review record for HEAD (see below). `ready` and `live` both mean done. |
| `app rollback [VERSION]` | Goes back to an earlier version (default: the one before live). Code only. |
| `app status [--errors]` | Where the app is: state, URLs, versions, resources, spend, and the done gate. `--errors` adds the last client and server errors. |
| `app destroy` | Asks for removal and prints the state the backend returns: `awaiting_owner` until the owner removes it in Canvas by typing the app's name. |
| `app check [--publish]` | Runs `pnpm check` (type check, lint, design lint) and parses the errors. Without `node_modules` it fails with `not_installed` (`next: "run pnpm install, then try again"`), which is not counted toward the fix caps. `--publish` runs `pnpm check:publish`, which adds `pnpm audit --prod --audit-level high`. |
| `app evidence --kind <browser\|review> --file <FILE> [--note <TEXT>] [--verdict <pass\|fail>]` | Records a browser pass or a review for HEAD. The file stays on your computer; only its path and sha256 are kept. A review needs `--verdict` (else `verdict_required`). A browser record defaults to `pass`. |
| `app report-broken-tool <TOOL> <WHAT>` | Appends a line to `.lightbulb/lessons.md` under `## Broken tools`. Once per `up`. |

## app.yaml

Six keys. An unknown key, a repeated key or a missing key is an error (`bad_config`).

```yaml
name: roof-quotes
services: ["vercel/project", "neon/postgres", "content"]
open_to: room
budget_usd: 10
kind: site
origin: {"kind": "idea"}
```

- `name`: the DNS label.
- `services`: Stripe catalog names the app needs. `content` is a switch name, not a catalog name (see the starter).
- `open_to`: `room`, `workspace`, `invited` or `public`.
- `budget_usd`: monthly cap.
- `kind`: one of the kinds above.
- `origin`: what asked for the app: `{"kind": "idea" | "issue" | ..., "ref": ...}`.

## The .lightbulb folder

| File | What it is | In git |
|---|---|---|
| `plan.md` | What to build first, what done means, what to check. | yes |
| `lessons.md` | What broke, why, what fixed it. Also broken tools. | yes |
| `known-fixes.md` | Fixes for errors seen before; `check` matches errors against it and `lessons.md`. | yes |
| `progress.json` | Error attempts, the last check, evidence records, when the preview was first seen ready. | no |
| `app.json` | `room_id` and `app_id`. | no |
| `migrations.json` | Migration files already sent. | no |
| `evidence/` | Screenshots and recordings. | no |

The ignored files are bookkeeping that `up` and `publish` write. If they were committed, writing
them would move HEAD and the done gate (same commit as HEAD) would never hold.

## Auth

If `OPENCLAW_HOOK_TOKEN`, `ISOL8_API_URL` and `ISOL8_OWNER_ID` are all set (a workspace agent's
computer), the CLI sends `Authorization: Bearer <hook token>` and `X-Lightbulb-Owner`.
`ISOL8_API_URL` is the API origin (for example `https://api.example.com`); the CLI appends
`/api/v1` once (a value that already ends in `/api/v1` is used as is). Otherwise it
uses the proof from `lightbulb login`; if there is none, the error is `not_signed_in` with
`next: "run lightbulb login"`. Tokens are never put in argv, logs or files.

## The done gate

`up` reports `done: true` only when, for the current HEAD commit:

1. the preview deployment is READY,
2. `/api/_lightbulb/health` returned 200 with `commit` equal to HEAD, and
3. browser evidence (`pass`) for HEAD was recorded after that deployment became ready.

Otherwise it lists what is `missing` (`ready`, `preview`, `preview_commit`, `health`, `browser_evidence`)
and a `next`. `status` reports the same gate, so you can learn `done` without redeploying. Do not
run `up` again just to check: a redeploy of the same commit moves "ready" past your evidence.

## Fix caps

`check` and `up` count failures by error signature in `progress.json`. The same signature twice
gives `next: "diagnose: …"` (read the logs, `known-fixes.md`, write a new plan). Three times gives
`next: "undo: run lightbulb app rollback or git revert, …"`.

## Destructive migrations

Before `up` and `publish` send the source, the CLI reads new migration files: `drizzle/*.sql`, and
Payload's `src/migrations/*.ts`. It flags `drop_table`, `drop_column`, `rename_table`, `rename_column`,
`alter_type`, `set_not_null`, `truncate` and `delete`. `DO`, `CALL`, `EXECUTE` and `PREPARE` blocks are
`dynamic_sql`: they cannot be read, so they fail closed. A `.ts` migration that is not plain SQL
also fails closed. On a finding the command fails with `destructive_migration` and shows the SQL.
The owner approves a card in Inbox, then you run `up --approved <card id>` (or `publish
--approved <card id>`). The backend checks the card is approved for this owner. Migrations
already recorded in `migrations.json` are not gated again.

## Publish, rollback, destroy

`publish` needs `check --publish` to pass and a review record for the exact HEAD with
`--verdict pass` (`lightbulb app evidence --kind review --verdict pass --file … --note …`). If
HEAD moved after the review, it fails with `review_required`.

`rollback` is code only: Lightbulb makes the earlier version live again, and the CLI adds one new
commit on top whose files match that version. HEAD never moves backwards. Data and schema are
unchanged. It refuses with `dirty_tree` if you have uncommitted changes. A successful rollback
carries `error.code: "code_only"`: that is information (data was not rolled back), not a failure.

`destroy` does not delete anything itself; it prints the state the backend returns, which is
`awaiting_owner` until the owner removes the app in Canvas.

## The starter

`app new` writes a Next.js 16 app (App Router, React 19, strict TypeScript, Tailwind CSS 4,
shadcn/ui, Drizzle on Postgres), with exact-pinned dependencies and `minimum-release-age=1440` in
`.npmrc`. It also has:

- `AGENTS.md` (the rules for the builder) and `playbooks/<kind>.md`.
- **The door.** `src/proxy.ts` and `src/lib/door.ts` verify the Lightbulb pass (an EdDSA JWS
  checked against `LIGHTBULB_ISSUER_KEY`, with `app` equal to `LIGHTBULB_APP`), then set an
  `lb_session` cookie. `LIGHTBULB_OPEN_TO=public` turns it off. Every response sends a
  `frame-ancestors` policy for `thelightbulb.company`. Only `/api/_lightbulb/health`, the
  session route `/api/_lightbulb/session`, the door's own pages and Next's assets skip it; error
  reports (`/api/_lightbulb/errors`) need a session unless the app is public. Do not remove or
  loosen it.
- **Sample data.** `drizzle/seed.sql` seeds sample mode and the preview database.
- **Server-only data.** The browser never gets a database credential.
- **Checks.** `pnpm check` runs `tsc`, `eslint` and a design lint (no raw colours or one-off sizes).

### The content switch

`new --kind site` (the default) and `--kind content-site` add Payload 3 as the editor, at
`/admin`, behind the door, and put `content` in `services`. Pages live in `src/app/(site)/`. Drafts
and a `?draft=1` preview are on. Roles: every door viewer is an `editor` and can edit drafts. Only
viewers whose `sub` or email is in `LIGHTBULB_PUBLISHERS` (comma list) are `publisher` and can
publish. The builder publishes through a local-API script when the room says "ship it". Scheduled
publishing, translations, live preview and the draft diff are not wired. The switch is applied only
at `new`; there is no command to add it to an existing app.

## Licences

- The starter derives from Dyad's Next.js template (Apache-2.0, commit 121d5674). The error-report
  limits, the TypeScript output parser (`src/apps/check.rs`) and the migration-file reader
  (`src/apps/migrations.rs`) are ported from Dyad (Apache-2.0, commit 0ee4a524). Files carry a
  header; `starter/LICENSE` and `starter/NOTICE` ship in every app. Modified by The Lightbulb Company.
- The destructive-migration rules are ported from Dyad's `pg-schema-classifier` (MIT); the notice is
  in `src/apps/migrations.rs`.
- `src/components/ui/` is from shadcn/ui (MIT).
- The content switch uses Payload (MIT).
