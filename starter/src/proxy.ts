import { NextResponse, type NextRequest } from "next/server";
import { doorOpen, readSession, SESSION_COOKIE } from "@/lib/door";

// The request gate. Every page and API route passes the door here, except the door's own pages,
// /api/_lightbulb/health, /api/_lightbulb/session (which opens the door) and Next's assets. Error
// reports stay behind it: a stranger's text must not reach the builder. Never remove it (AGENTS.md).
export async function proxy(req: NextRequest) {
  // /admin (the content editor) keeps the door even on a public site.
  const admin = /^\/admin(\/|$)/.test(req.nextUrl.pathname);
  if (doorOpen() && !admin) return NextResponse.next();
  if (await readSession(req.cookies.get(SESSION_COOKIE)?.value ?? "")) return NextResponse.next();
  if (req.nextUrl.pathname.startsWith("/api/")) {
    return NextResponse.json({ code: "closed", message: "Open this from Canvas in Lightbulb." }, { status: 401 });
  }
  return NextResponse.rewrite(new URL("/_lightbulb/closed", req.url));
}

export const config = {
  matcher: ["/((?!_lightbulb/|api/_lightbulb/health$|api/_lightbulb/session$|_next/|favicon\\.ico$).*)"],
};
