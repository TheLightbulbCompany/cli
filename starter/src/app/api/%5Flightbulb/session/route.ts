import { sessionCookie, verifyPass } from "@/lib/door";

/** Turns a Lightbulb pass (posted as `{ token }`, never in a URL) into the session cookie. */
export async function POST(req: Request) {
  // Only this app's own page may sign someone in (no login CSRF from another site).
  const sameOrigin =
    req.headers.get("origin") === new URL(req.url).origin || req.headers.get("sec-fetch-site") === "same-origin";
  const json = req.headers.get("content-type")?.split(";")[0].trim() === "application/json";
  if (!sameOrigin || !json) return Response.json({ error: "Sign in from this app's own page." }, { status: 403 });
  const body: { token?: unknown } = await req.json().catch(() => ({}));
  const viewer = typeof body.token === "string" ? await verifyPass(body.token) : null;
  if (!viewer) return Response.json({ error: "This pass is expired or not for this app." }, { status: 401 });
  return new Response(null, { status: 204, headers: { "set-cookie": await sessionCookie(viewer) } });
}
