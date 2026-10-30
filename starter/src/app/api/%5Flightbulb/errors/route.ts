import { capArgs } from "@/app/_lightbulb/caps";

/** Writes a browser error to stdout, which Vercel keeps as the app's logs. */
export async function POST(req: Request) {
  let body: { page?: unknown; args?: unknown };
  try {
    body = await req.json();
  } catch {
    return Response.json({ error: "expected JSON" }, { status: 400 });
  }
  const args = Array.isArray(body.args) ? body.args : [];
  console.error("[client error]", String(body.page ?? "").slice(0, 200), JSON.stringify(capArgs(args)));
  return new Response(null, { status: 204 });
}
