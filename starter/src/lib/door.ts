import "server-only";

// The door: who may open this app. Never remove it (AGENTS.md).
import { importJWK, jwtVerify, SignJWT, type JWTPayload } from "jose";
import { cookies } from "next/headers";

/** The person opening the app, as Lightbulb named them in the pass. */
export type Viewer = { sub: string; name: string; email: string; ws: string; room: string; app: string };

export const SESSION_COOKIE = "lb_session";
const DAY = 86_400;
const FIELDS = ["sub", "name", "email", "ws", "room", "app"] as const;
const CLAIMS = ["iat", "exp", ...FIELDS].sort().join();

/** True when the app is open to anyone (`open_to: public`). */
export function doorOpen(): boolean {
  return process.env.LIGHTBULB_OPEN_TO === "public";
}

/** Checks a Lightbulb pass (EdDSA, signed by the issuer key, for this app); null when it is not good. */
export async function verifyPass(token: string, now = new Date()): Promise<Viewer | null> {
  try {
    const key = await importJWK(JSON.parse(process.env.LIGHTBULB_ISSUER_KEY ?? ""), "EdDSA");
    const { payload } = await jwtVerify(token, key, { algorithms: ["EdDSA"], currentDate: now });
    return toViewer(payload);
  } catch {
    return null;
  }
}

/** The `Set-Cookie` value that keeps this viewer signed in for 24 hours. */
export async function sessionCookie(v: Viewer): Promise<string> {
  const token = await new SignJWT({ ...v })
    .setProtectedHeader({ alg: "HS256" })
    .setIssuedAt()
    .setExpirationTime(`${DAY}s`)
    .sign(cookieSecret());
  return `${SESSION_COOKIE}=${token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=${DAY}`;
}

/** Reads a session cookie value; null when it is missing, forged, expired or for another app. */
export async function readSession(value: string, now = new Date()): Promise<Viewer | null> {
  try {
    const { payload } = await jwtVerify(value, cookieSecret(), { algorithms: ["HS256"], currentDate: now });
    return toViewer(payload);
  } catch {
    return null;
  }
}

/** Who is signed in to this request, for server code. */
export async function viewer(): Promise<Viewer | null> {
  const value = (await cookies()).get(SESSION_COOKIE)?.value;
  return value ? readSession(value) : null;
}

function cookieSecret(): Uint8Array {
  const secret = process.env.LIGHTBULB_COOKIE_SECRET;
  if (!secret) throw new Error("LIGHTBULB_COOKIE_SECRET is not set.");
  return new TextEncoder().encode(secret);
}

// Exactly the agreed claims, every field a string, and for this app.
function toViewer(p: JWTPayload): Viewer | null {
  if (Object.keys(p).sort().join() !== CLAIMS) return null;
  if (!FIELDS.every((f) => typeof p[f] === "string")) return null;
  const v = Object.fromEntries(FIELDS.map((f) => [f, p[f]])) as Viewer;
  return v.app && v.app === process.env.LIGHTBULB_APP ? v : null;
}
