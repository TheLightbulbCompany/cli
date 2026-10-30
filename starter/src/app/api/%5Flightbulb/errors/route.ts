import { capArgs } from "@/app/_lightbulb/caps";

/** Writes a browser error to the app's logs. Lightbulb's error reader picks up only `[lightbulb:client]` lines. */
export async function POST(req: Request) {
  let body: { page?: unknown; args?: unknown };
  try {
    body = await req.json();
  } catch {
    return Response.json({ error: "expected JSON" }, { status: 400 });
  }
  const args = Array.isArray(body.args) ? body.args : [];
  // JSON-encoded so a newline in `page` cannot forge a log line.
  console.error("[lightbulb:client]", JSON.stringify(String(body.page ?? "").slice(0, 200)), JSON.stringify(capArgs(args)));
  return new Response(null, { status: 204 });
}
