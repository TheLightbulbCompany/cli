# Known fixes

Failures builders hit before, and the fix. Check here before a second attempt at the same error.

Only titles that occur in check output (TypeScript, ESLint, design lint, build) become hints; the
rest are for you to read.

- **A page or route 404s and its folder starts with `_`.** Folders that start with `_` are private
  in the App Router. Rename the folder to start with `%5F` (`src/app/%5Flightbulb/`).
- **server-only** ("You're importing a module that depends on server-only"). A `"use client"` file imports
  `@/lib/db` or something that does. Load the data in a server component or route and pass it down.
- **No database: set DATABASE_URL** Locally, run with `LIGHTBULB_SAMPLE_DATA=1`. On a preview,
  `lightbulb app up` sets the database; run it again.
- **Sample data is missing or a table doesn't exist in sample mode.** Sample mode runs
  `drizzle/*.sql` then `drizzle/seed.sql`. Run `pnpm db:generate` after changing `schema.ts`, and
  make `seed.sql` match the new columns.
- **db.transaction is not a function** (or it fails on the preview). The Neon HTTP driver has no
  transactions. Make each write a single statement.
- **ERR_PNPM_NO_MATURE_MATCHING_VERSION** (pnpm refuses a package as too new). Packages must be a day old. Pin the previous version.
- **ERR_PNPM_IGNORED_BUILDS** after adding a package. Add it under `allowBuilds` in
  `pnpm-workspace.yaml` set to `false`, unless the package can't work without its install script.
- **design/tokens.css** (raw hex colour or colour function in `pnpm check`). Use a token class;
  add new colours to `design/tokens.css`.
- **arbitrary Tailwind value** (`w-[...]` in `pnpm check`). Use the scale or a token class.
- **Hydration mismatch.** Something renders differently on server and browser (dates, random
  values, `window`). Compute it in `useEffect`, or render it on the server only.
- **A component exists but nothing shows.** It isn't imported into a page.
