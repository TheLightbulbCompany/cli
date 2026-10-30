// Content edits asked for in the room, through Payload's local API (the editor's rules don't apply here;
// you are the builder). Needs DATABASE_URL, the app's database.
//   pnpm content set <collection> <id> <field> <value> --draft   a draft edit; <field> may be a path: layout.0.headline
//   pnpm content publish <collection> <id>                       publishes the latest draft ("ship it")
import { pathToFileURL } from "node:url";

const USAGE = "usage: content set <collection> <id> <field> <value> --draft | content publish <collection> <id>";

/**
 * Runs one command against a Payload instance; returns the saved document.
 * @param {import("payload").Payload} payload
 * @param {string[]} args
 */
export async function run(payload, args) {
  const [cmd, collection, id, field, value, ...flags] = args;
  if (!collection || !id) throw new Error(USAGE);
  // Start from the latest draft, so a second edit keeps the first.
  const latest = await payload.findByID({ collection, id, draft: true, depth: 0 });
  if (cmd === "set" && field && value !== undefined) {
    if (!flags.includes("--draft")) throw new Error("set writes drafts only: add --draft, then publish.");
    const doc = structuredClone(latest);
    const keys = field.split(".");
    const last = keys.pop();
    let at = doc;
    for (const k of keys) at = at[k] ??= {};
    at[last] = value;
    const top = keys[0] ?? last;
    return payload.update({ collection, id, data: { [top]: doc[top] }, draft: true });
  }
  if (cmd === "publish") {
    const data = { ...latest, _status: "published" };
    for (const k of ["id", "createdAt", "updatedAt"]) delete data[k];
    return payload.update({ collection, id, data });
  }
  throw new Error(USAGE);
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (!process.env.DATABASE_URL) {
    console.error("Set DATABASE_URL to the app's database first; sample data lives only inside the running app.");
    process.exit(1);
  }
  const { getPayload } = await import("payload");
  const { default: config } = await import("../src/payload.config.ts");
  const payload = await getPayload({ config });
  try {
    const doc = await run(payload, process.argv.slice(2));
    console.log(JSON.stringify({ id: doc.id, _status: doc._status }));
  } catch (e) {
    console.error(e instanceof Error ? e.message : e);
    process.exitCode = 1;
  } finally {
    await payload.destroy();
  }
}
