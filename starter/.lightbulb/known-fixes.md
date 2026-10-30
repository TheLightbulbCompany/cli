# Known fixes

Failures builders hit before, and the fix. Check here before a second attempt at the same error.

- **A page or route 404s and its folder starts with `_`.** Folders that start with `_` are private
  in the App Router. Rename the folder to start with `%5F` (`src/app/%5Flightbulb/`).
- **"You're importing a module that depends on server-only".** A `"use client"` file imports
  `@/lib/db` or something that does. Load the data in a server component or route and pass it down.
- **"No database: set DATABASE_URL..."** Locally, run with `LIGHTBULB_SAMPLE_DATA=1`. On a preview,
  `lightbulb app up` sets the database; run it again.
- **Sample data is missing or a table doesn't exist in sample mode.** Sample mode runs
  `drizzle/*.sql` then `drizzle/seed.sql`. Run `pnpm db:generate` after changing `schema.ts`, and
  make `seed.sql` match the new columns.
- **`db.transaction is not a function` or it fails on the preview.** The Neon HTTP driver has no
  transactions. Make each write a single statement.
- **pnpm refuses a package as too new.** Packages must be a day old. Pin the previous version.
- **`ERR_PNPM_IGNORED_BUILDS` after adding a package.** Add it under `allowBuilds` in
  `pnpm-workspace.yaml` set to `false`, unless the package can't work without its install script.
- **`pnpm check` fails on a colour or `w-[...]`.** Use a token class; add new colours to
  `design/tokens.css`.
- **Hydration mismatch.** Something renders differently on server and browser (dates, random
  values, `window`). Compute it in `useEffect`, or render it on the server only.
- **A component exists but nothing shows.** It isn't imported into a page.
