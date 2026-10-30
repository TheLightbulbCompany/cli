import "server-only";

import { readdir, readFile } from "node:fs/promises";
import path from "node:path";
import { neon } from "@neondatabase/serverless";
import type { PgDatabase, PgQueryResultHKT } from "drizzle-orm/pg-core";
import { drizzle as drizzleNeon } from "drizzle-orm/neon-http";
import * as schema from "./schema";

export type Db = PgDatabase<PgQueryResultHKT, typeof schema>;

/** `live` when the app has its database, `sample` for the in-memory one, else `none`. */
export function dbMode(): "live" | "sample" | "none" {
  if (process.env.DATABASE_URL) return "live";
  if (process.env.LIGHTBULB_SAMPLE_DATA === "1") return "sample";
  return "none";
}

// Kept on globalThis so dev reloads reuse one sample database.
const cache = globalThis as { lightbulbDb?: Promise<Db> };

/** The app's database: Neon over HTTP, or PGlite in memory loaded with drizzle/*.sql and the seed. */
export function getDb(): Promise<Db> {
  cache.lightbulbDb ??= open();
  return cache.lightbulbDb;
}

async function open(): Promise<Db> {
  const mode = dbMode();
  if (mode === "live") {
    return drizzleNeon({ client: neon(process.env.DATABASE_URL!), schema }) as unknown as Db;
  }
  if (mode === "none") {
    throw new Error("No database: set DATABASE_URL, or LIGHTBULB_SAMPLE_DATA=1 for sample data.");
  }
  const { PGlite } = await import("@electric-sql/pglite");
  const { drizzle } = await import("drizzle-orm/pglite");
  const client = new PGlite();
  const dir = path.join(process.cwd(), "drizzle");
  const migrations = (await readdir(dir)).filter((f) => /^\d+_.*\.sql$/.test(f)).sort();
  for (const file of [...migrations, "seed.sql"]) {
    await client.exec(await readFile(path.join(dir, file), "utf8"));
  }
  return drizzle({ client, schema }) as unknown as Db;
}
