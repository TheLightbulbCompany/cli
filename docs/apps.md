# Apps you build

`lightbulb app` builds a small web app for a workspace: a site, a form, an internal tool, a
dashboard, a portal. A coding agent (or you) writes it in a folder, checks it, previews it, and
publishes it. This is not the same as the [hosted assistants](connect/apps.md) (Claude, ChatGPT,
Muse), which are chat apps that talk to Lightbulb. An app you build is a Next.js project that
Lightbulb deploys and puts behind a sign-in.

Every command calls the Lightbulb builder routes. The CLI never runs the Stripe or Vercel CLI.

## Commands

Every command takes `--json` (the default when stdout is not a terminal). A failure prints
`{"code","message","next"}` and exits 1 (in a terminal: the message, then a `Next:` line). An
`up` or `publish` still building after 10 minutes exits 2 with `code: "timeout"`: not done, not
failed; run `status` later. Run all commands except `new` inside the app's folder. Output for a
person uses three words: the **preview** (built from your latest commit), **live** (published),
and **done** (the preview was checked by a person). JSON keeps the backend's states.

| Command | What it does |
|---|---|
| `app new <NAME> [--room <ROOM>] [--kind <KIND>]` | Creates `./<NAME>` from the starter, with one git commit, and says who can open it. Next: `cd <NAME> && pnpm install`, then `lightbulb app up` to get a preview. `NAME` is a DNS label: `^[a-z][a-z0-9-]{1,38}[a-z0-9]$`, and may not end in `-preview` (the preview's address is `<name>-preview.<apps domain>`). `--kind` is `site` (default), `content-site`, `form`, `internal-tool`, `dashboard`, `portal`, `payments` or `automation`. `--room` is saved in `.lightbulb/app.json`. |
| `app up [--room <ROOM>] [--approved <CARD>] [--requested-by <ID>]` | Sends HEAD to Lightbulb (a source over 9 MiB packed is refused first with `source_too_large`), waits for the preview build (up to 10 minutes), and reports `done` or what is missing. Still building after that: `timeout`, exit 2. A `failed` build prints `{"code","message","next"}` (`build_failed` when the backend gave no error) and exits 1. `--requested-by` names the person who asked (sent with the first `up`). After the first `up`, who can open the app and its budget change in Canvas: when `app.yaml`'s `open_to` or `budget_usd` differ from Lightbulb's, `up` adds a `note` saying the app.yaml values are ignored. `--approved-destructive` is another name for `--approved`. |
| `app publish [--reason <TEXT>] [--room <ROOM>] [--approved <CARD>]` | Puts the preview live as a new version, waiting for the build like `up`. Needs a passing `check --publish` and a passing review record for HEAD (see below). `ready` and `live` both mean done. |
| `app rollback [VERSION]` | Goes back to an earlier version (default: the one before live). Code only. |
| `app status [--errors]` | Where the app is: state, URLs, versions, resources, spend, and the done gate. `--errors` adds the last client and server errors. |
| `app destroy` | Asks for removal and prints the state the backend returns: `awaiting_owner` until the owner removes it in Canvas by typing the app's name. |
| `app check [--publish]` | Runs `pnpm check` (type check, lint, design lint) and parses the errors. Without `node_modules` it fails with `not_installed` (`next: "run pnpm install, then try again"`), which is not counted toward the fix caps. `--publish` runs `pnpm check:publish`, which adds `pnpm audit --prod --audit-level high`. |
| `app test-link` | Prints `{"url"}`: a link past the preview's door, for the builder's and its reviewer's own check only. The pass in it must be opened within 5 minutes and gives a 30-minute session on the preview, as "Builder agent" (no email). It is never accepted on the live address. A public app gets its plain preview address. It is printed only, never written to `.lightbulb/` or a log; never post it anywhere, or put it in an evidence note (`note_has_pass`). No preview yet: `not_ready`. People open the app from Canvas. |
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
also fails closed. On a finding the command fails with `destructive_migration`. The message says
in words what each change would do ("This change would drop the `contacts` table (data loss).",
one line each, then "Nothing was applied."); the SQL is under `details` (`file: statement`), or
indented under the message in a terminal.
The owner approves a card in Inbox, then you run `up --approved <card id>` (or `publish
--approved <card id>`). The backend checks the card is approved for this owner. Migrations
already recorded in `migrations.json` are not gated again.

## Publish, rollback, destroy

`publish` needs `check --publish` to pass and a review record for the exact HEAD with
`--verdict pass`. Without one it fails with `review_required`, "Publish needs a passing review
recorded for this commit.", and a `next` with the reviewer step, the preview address `up` last
saw, and the exact command: `lightbulb app evidence --kind review --verdict pass --file
<screenshot> --note "<what was checked>"`. If HEAD moved after the review, it fails the same way.

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
  `lb_session` cookie. The pass's header has `typ: "lightbulb-app-pass"`; its `app` claim is the
  app's name. `LIGHTBULB_OPEN_TO=public` turns it off. Every response sends a
  `frame-ancestors` policy for `thelightbulb.company`. Only `/api/_lightbulb/health`, the
  session route `/api/_lightbulb/session`, the door's own pages and Next's assets skip it; error
  reports (`/api/_lightbulb/errors`) need a session unless the app is public. Do not remove or
  loosen it.
- **The builder's pass.** `app test-link` mints a pass whose `sub` is `builder:<id>`, whose `email`
  is empty and whose `app` is `<name>:preview`. Only `LIGHTBULB_PREVIEW_APP`, which Lightbulb sets
  on the preview and never on live, accepts it, so neither the pass nor its session opens the live
  address. An app made before this door refuses the pass as `wrong_app`, everywhere, until its
  `src/lib/door.ts` is updated.
- **The door page.** Someone without a session gets status 401 and a page that names the app
  and workspace (`LIGHTBULB_APP_NAME`, `LIGHTBULB_WORKSPACE_NAME`; dropped when unset), says "Open
  this from Canvas in Lightbulb to get in.", has one "Open Canvas" button
  (`https://thelightbulb.company/chat?panel=canvas&app=<LIGHTBULB_APP_ID>`) and the line "Invited
  by email? Use the link in your invitation." An expired link, a link for another app, and no
  connection (with Try again) each have their own sentence. An email invite is used only after the
  person clicks Continue, so a link scanner cannot spend it.
- **Error reports.** The browser's errors are logged with the prefix `[lightbulb:client]`, which
  is what `status --errors` reads.
- **Sample data.** `drizzle/seed.sql` seeds sample mode and the preview database. Every page of a
  sample-data preview shows "Preview with sample data — nothing you enter here is kept." (not
  Payload's `/admin`), and the CLI says "(sample data)" after the preview address.
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
