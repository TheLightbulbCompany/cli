import "server-only";

// The content switch's glue: Payload's database, who may edit, and who may publish.
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { Forbidden, getPayload, type Access, type AuthStrategy, type CollectionBeforeChangeHook } from "payload";
import { dbMode } from "@/lib/db";
import { readSession, SESSION_COOKIE } from "@/lib/door";

/** Payload's local API, for server components, route handlers and scripts. */
export async function cms() {
  return getPayload({ config: (await import("@/payload.config")).default });
}

/** The site's public address, for canonical links and the sitemap. */
export function siteUrl(): string {
  const host = process.env.VERCEL_PROJECT_PRODUCTION_URL;
  return host ? `https://${host}` : "http://localhost:3000";
}

/** The path a page is served at: `home` is `/`. */
export function pagePath(slug: string): string {
  return slug === "home" ? "/" : `/${slug}`;
}

// Kept on globalThis so dev reloads reuse one sample database.
const cache = globalThis as { lightbulbContentDb?: Promise<string> };

/** Payload's Postgres: the app's database, or for sample data PGlite in memory behind a Unix socket. */
export function contentDatabaseUrl(): Promise<string> {
  const mode = dbMode();
  if (mode === "live") return Promise.resolve(process.env.DATABASE_URL!);
  if (mode === "none") {
    throw new Error("No database: set DATABASE_URL, or LIGHTBULB_SAMPLE_DATA=1 for sample data.");
  }
  cache.lightbulbContentDb ??= startSample();
  return cache.lightbulbContentDb;
}

async function startSample(): Promise<string> {
  const { PGlite } = await import("@electric-sql/pglite");
  const { PGLiteSocketServer } = await import("@electric-sql/pglite-socket");
  // A socket file, not a port: nothing listens on the network.
  const dir = mkdtempSync(path.join(tmpdir(), "lb-"));
  const server = new PGLiteSocketServer({
    db: await PGlite.create(),
    path: path.join(dir, ".s.PGSQL.5432"),
    maxConnections: 20,
  });
  await server.start();
  // Don't keep `next build` or a script alive for it, and leave no socket folder behind.
  (server as unknown as { server: { unref(): void } }).server.unref();
  process.once("exit", () => {
    void server.stop();
    rmSync(dir, { recursive: true, force: true });
  });
  return `postgres://postgres@localhost/postgres?host=${encodeURIComponent(dir)}`;
}

/** A door viewer as Payload knows them (the `users` collection). */
type Person = { sub?: string | null; email?: string | null } | null | undefined;

/** True when this person may publish: their `sub` or email is in `LIGHTBULB_PUBLISHERS` (comma list). */
export function isPublisher(person: Person): boolean {
  const list = (process.env.LIGHTBULB_PUBLISHERS ?? "")
    .split(",")
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  return [person?.sub, person?.email].some((id) => !!id && list.includes(id.toLowerCase()));
}

/** Signed in through the door: every viewer of the app is an editor. */
export const editors: Access = ({ req }) => !!req.user;

/** Only publishers. */
export const publishers: Access = ({ req }) => isPublisher(req.user);

/** Editors save drafts; only publishers write `_status: published`. */
export const draftsOrPublishers: Access = ({ req, data }) =>
  !!req.user && (isPublisher(req.user) || data?._status !== "published");

/**
 * Editors change pages and posts only by saving a draft (`?draft=true`): no live edit, no
 * unpublish, no restoring a version live. Covers create, update and version restore; publishers and
 * the builder's scripts (no signed-in user) pass.
 */
export const draftsOnlyForEditors: CollectionBeforeChangeHook = ({ req, data }) => {
  if (!req.user || isPublisher(req.user)) return data;
  const q = req.query ?? {};
  const publishing = ["publishAllLocales", "unpublishAllLocales", "publishSpecificLocale"].some((k) => k in q);
  if (!publishing && String(q.draft) === "true" && data._status === "draft") return data;
  throw new Forbidden(req.t);
};

/** Signed-in people see drafts; everyone else sees published documents only. */
export const publishedOrEditors: Access = ({ req }) =>
  req.user ? true : { _status: { equals: "published" } };

/** The admin's sign-in: the door's `lb_session` cookie. No Payload passwords. */
export const doorStrategy: AuthStrategy = {
  name: "lightbulb-door",
  async authenticate({ headers, payload }) {
    const cookie = headers
      .get("cookie")
      ?.split(/;\s*/)
      .find((c) => c.startsWith(`${SESSION_COOKIE}=`));
    const v = cookie ? await readSession(cookie.slice(SESSION_COOKIE.length + 1)) : null;
    if (!v) return { user: null };
    const where = { sub: { equals: v.sub } };
    const found = await payload.find({ collection: "users", where, limit: 1, depth: 0 });
    const user =
      found.docs[0] ??
      (await payload.create({ collection: "users", data: { sub: v.sub, name: v.name, email: v.email } }));
    return { user: { ...user, collection: "users", _strategy: "lightbulb-door" } };
  },
};
