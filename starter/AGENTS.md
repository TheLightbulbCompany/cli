# Building this app

This is a Next.js app that a Lightbulb workspace asked for. You build it, check it, and hand the
room a preview in Canvas. `app.yaml` says what kind of app it is (`kind`), who can open it (`open_to`)
and which services it gets (`services`). Read this file, then `playbooks/<kind>.md`, before you
change anything.

## The workflow

1. **Read before you ask.** Read the request, the room, the workspace brand, any linked page or
   attached file, and `.lightbulb/plan.md`. Ask at most one question card, and only for what you
   could not read. The playbook lists what never to ask.
2. **Write the plan** in `.lightbulb/plan.md`: what you build first, what "done" means (from the
   playbook), what you will check.
3. **Build the screens first, on sample data.** Put sample rows in `drizzle/seed.sql`. Add real
   data, then sign-in, only when a feature needs them.
4. **Check, cheapest first:** run `pnpm install` once (and after changing `package.json`;
   `check` and `up` answer `not_installed` until you do), then `lightbulb app check` (type check, lint, design lint), then
   `lightbulb app up` (deploys the preview, runs the health check, reads the logs). When `up` asks
   for it, run `lightbulb app test-link --json`, open the `url` it prints in the shared browser
   (the plain preview address only reaches the door), walk the flow, and record it with
   `lightbulb app evidence --kind browser --file .lightbulb/evidence/<name>.png`. Keep the file
   under `.lightbulb/evidence/` (git ignores it) or outside the app, so it never changes the
   version. `lightbulb app status` shows `done` without redeploying.
5. **Post nothing until `up` says `done: true`.** Then post the Canvas line (`canvas_url`) and what
   you built, in two or three plain sentences. People open the app from Canvas: never post the
   preview address, and never post a test link anywhere; it lets its holder in as the builder.
6. **Same error twice: stop patching.** When `up` or `check` says `next: "diagnose"`, read the
   logs (`lightbulb app status --errors`), check `.lightbulb/known-fixes.md`, and write a fresh
   plan before touching code. When it says `next: "undo"`, roll back and tell the room plainly what
   failed. If a Lightbulb command itself is broken, run `lightbulb app report-broken-tool` and stop.
7. **Write a lesson** in `.lightbulb/lessons.md` after every failure you resolve: what broke, why,
   what fixed it. Read the lessons for an area before you touch it. Never put a test link or its token in
   a lesson: they are committed and uploaded.
8. **One commit per turn**, with a message that says what changed for the person.

## Safety rules (never break these)

- **No secret in code, in a commit, or in the room.** Keys arrive as environment variables through
  Lightbulb. If you need one, ask for it through a form card, never in the thread.
- **A test link is for your own check.** Don't save it, log it, or put it in a note, a commit or
  the room. It lasts 5 minutes; a new one is one command away.
- **Never remove or loosen the door** (`src/proxy.ts`, `src/lib/door.ts`, `src/app/%5Flightbulb/`,
  `src/app/api/%5Flightbulb/`). Every page and route sits behind it.
- **Files in `public/` are served without the door.** Nothing private goes there.
- **The schema only grows.** Add tables and columns; never drop, rename or narrow one. If a change
  needs that, `up` turns it into an owner card that shows the SQL; wait for the owner.
- **Data access is server-only.** Only server components, route handlers and server actions import
  `@/lib/db`. The browser never sees a database URL or key.
- **Start read-only or undoable.** Delete, pay, send, or publish only when the person asked for that
  exact action.

## Stack

- Next.js 16 App Router, React 19, TypeScript (strict).
- Tailwind CSS 4 with the shadcn/ui kit in `src/components/ui/` (Radix underneath).
- Drizzle ORM on Postgres: Neon when `DATABASE_URL` is set; PGlite in memory with the seed when
  `LIGHTBULB_SAMPLE_DATA=1`.
- lucide-react icons, react-hook-form + zod for forms, sonner for toasts, recharts for charts.
- Vitest for tests. pnpm for packages.

## Where things go

- Pages: `src/app/<route>/page.tsx`. The home page is `src/app/page.tsx`. **When you make a new
  component, import it into a page**, or nobody sees it. A site with the content switch keeps its
  pages in `src/app/(site)/` and its content in Payload (`playbooks/content-site.md`).
- Components: `src/components/`. Kit components stay in `src/components/ui/`.
- Server logic: route handlers in `src/app/api/<name>/route.ts`, or server actions.
- Tables: `src/lib/db/schema.ts`, then `pnpm db:generate` to write the migration in `drizzle/`.
  Never hand-edit a generated migration. Sample data: `drizzle/seed.sql`.
- Colours, radius, fonts: `design/tokens.css`; why: `design/DESIGN.md`.
- Tests: next to the code, `*.test.ts`.
- Plan, lessons, known fixes: `.lightbulb/`.

## Library rules

- **UI:** use `src/components/ui/` first. Build missing pieces from Radix plus Tailwind in the same
  style. No other component libraries.
- **Styling:** Tailwind classes using the tokens (`bg-primary`, `text-muted-foreground`). No raw
  colours and no one-off values like `w-[317px]`; `pnpm check` fails on them. No CSS-in-JS.
- **Icons:** `lucide-react` only.
- **Forms:** `react-hook-form` with a `zod` schema through `@hookform/resolvers/zod`, and
  `src/components/ui/form.tsx`. Validate again on the server.
- **Data:** `const db = await getDb()` from `@/lib/db`, then Drizzle queries. Don't use
  `db.transaction` (the Neon HTTP driver has none); make each write one statement
  (`insert ... returning`, `update ... where`).
- **Toasts:** `toast()` from `sonner` with `<Toaster />` from `src/components/ui/sonner.tsx`.
- **Charts:** `src/components/ui/chart.tsx` around recharts.
- **Packages:** add one only when the stack above can't do the job. Pin it to an exact version.
  pnpm refuses versions less than a day old; pick the previous one.

## What the person gets

The check can't see these, so read your own screens for them before you finish.

- **Every refused action says so on the page.** A form that fails validation, a save the server
  refuses, a failed delete: a visible message, and nothing saved. Give optional fields a `""` default.
- **A button does what it says on the first click.** No `confirm()`, `alert()` or `prompt()`: they
  do nothing in Canvas. If the request asks for a confirmation, ask in the page.
- **A value that must be unique has a unique index,** and a duplicate gets a visible message, not an error page.
- **Anything the request spells out appears exactly as written, where it says:** a label, a timezone
  name, a status word, a total, a `data-` attribute. Before you finish, search `src/` for each name
  the request or `contract.json` gives, and check each rule in the request against a screen.
- **Test what the person does, not only the API.** A route that works says nothing about the form
  in front of it. Follow each form from its button to the saved row, including with optional fields blank.
- **If people sign up for themselves,** follow `playbooks/accounts.md`. Sign-up signs them in.

## Tests

Write a small test for every business rule: a price, a limit, who can see what. Run `pnpm test`.
Keep tests that pass; they run on every check.

## Common mistakes

- Creating `src/pages/`. This app uses the App Router: `src/app/`.
- Naming a route folder `_something`. Folders that start with `_` are private and never routed;
  write `%5Fsomething` for a URL that starts with `_`.
- `import { db } from "@/lib/db"`. Wrong: there is no `db` export. Right:
  `const db = await getDb()`.
- Importing `@/lib/db` from a file that starts with `"use client"`. The build fails. Fetch in a
  server component and pass the rows down as props.
- `import { Button } from "@/components/button"`. Right: `import { Button } from "@/components/ui/button"`.
- `className="bg-[#2563eb]"`. Right: a token, `bg-primary`; add the colour to `design/tokens.css`.
- Using `useState` or `onClick` in a file without `"use client"` at the top.
- Editing a migration in `drizzle/` after it has run. Change `schema.ts` and generate a new one.
- Reading `process.env.SOME_KEY` in a client component. It is undefined there, and it must stay so.
- Saying it works before `up` says `done: true`.
