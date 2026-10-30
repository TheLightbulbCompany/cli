# Content site (edited every week)

Switches: content (adds Payload, the editor, on the app's own database).

**Builds first:** the collections the site needs (pages, posts, people...), a template per page
type, and the editor behind the door. Drafts and publishing on; the room's members edit, publishers
publish.

**Asks (one card at most):** which kinds of page they publish, if the request and any existing site
don't show it.

**Never asks:** field names, the editor's layout, SEO fields (every page gets them).

**Done when:** an editor changes a headline in a draft, a reviewer publishes it, and the live page
shows the new headline.

**Where things are:** collections and blocks in `src/payload.config.ts`; the site's pages in
`src/app/(site)/` (the home page is the page with slug `home`); the editor at `/admin`, signed in
through the door. Every viewer is an editor (drafts); people whose Lightbulb id or email is in
`LIGHTBULB_PUBLISHERS` publish. A changed collection needs `pnpm payload migrate:create <name>`.

**Edits asked for in the room:** `pnpm content set <collection> <id> <field> <value> --draft`
(`<field>` may be a path, `layout.0.headline`), post the preview, and on "ship it"
`pnpm content publish <collection> <id>`. Code changes only for new section types or layout.
