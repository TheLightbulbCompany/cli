import { sessionCookie, verifyPass } from "@/lib/door";

/** Turns a Lightbulb pass (posted as `{ token }`, never in a URL) into the session cookie. */
export async function POST(req: Request) {
  const body: { token?: unknown } = await req.json().catch(() => ({}));
  const viewer = typeof body.token === "string" ? await verifyPass(body.token) : null;
  if (!viewer) return Response.json({ error: "This pass is expired or not for this app." }, { status: 401 });
  return new Response(null, { status: 204, headers: { "set-cookie": await sessionCookie(viewer) } });
}
