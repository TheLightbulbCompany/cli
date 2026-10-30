import "server-only";

// The door: who may open this app. Never remove it (AGENTS.md).
import { importJWK, jwtVerify, SignJWT, type JWTPayload } from "jose";
import { cookies } from "next/headers";

/** The person opening the app, as Lightbulb named them in the pass. */
export type Viewer = { sub: string; name: string; email: string; ws: string; room: string; app: string };

export const SESSION_COOKIE = "lb_session";
/** The `typ` header of a Lightbulb pass. */
export const PASS_TYP = "lightbulb-app-pass";
const DAY = 86_400;
/** A preview-only pass (the builder's `lightbulb app test-link`) buys 30 minutes, not a day. */
const TEST_SESSION = 1_800;
const FIELDS = ["sub", "name", "email", "ws", "room", "app"] as const;
const CLAIMS = ["iat", "exp", ...FIELDS].sort().join();

/** True when the app is open to anyone (`open_to: public`). */
export function doorOpen(): boolean {
  return process.env.LIGHTBULB_OPEN_TO === "public";
}

/** Checks a Lightbulb pass (EdDSA, typ `lightbulb-app-pass`, the issuer key's kid, at most 15 minutes old, `app` equal to this app's name `LIGHTBULB_APP`, or on a preview `LIGHTBULB_PREVIEW_APP`, which live never has); null when it is not good. */
export async function verifyPass(token: string, now = new Date()): Promise<Viewer | null> {
  try {
    const jwk = JSON.parse(process.env.LIGHTBULB_ISSUER_KEY ?? "");
    const key = await importJWK(jwk, "EdDSA");
    const { payload, protectedHeader } = await jwtVerify(token, key, {
      algorithms: ["EdDSA"],
      maxTokenAge: "900s",
      clockTolerance: 30,
      currentDate: now,
      typ: PASS_TYP,
    });
    return protectedHeader.kid === jwk.kid ? toViewer(payload) : null;
  } catch {
    return null;
  }
}

/** The `Set-Cookie` value that keeps this viewer signed in: 24 hours, or 30 minutes for a preview-only pass. */
export async function sessionCookie(v: Viewer): Promise<string> {
  const life = v.app === process.env.LIGHTBULB_PREVIEW_APP ? TEST_SESSION : DAY;
  const token = await new SignJWT({ ...v })
    .setProtectedHeader({ alg: "HS256" })
    .setIssuedAt()
    .setExpirationTime(`${life}s`)
    .sign(cookieSecret());
  return `${SESSION_COOKIE}=${token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=${life}`;
}

/** Reads a session cookie value; null when it is missing, forged, expired or for another app. */
export async function readSession(value: string, now = new Date()): Promise<Viewer | null> {
  try {
    const { payload } = await jwtVerify(value, cookieSecret(), {
      algorithms: ["HS256"],
      maxTokenAge: `${DAY}s`,
      clockTolerance: 30,
      currentDate: now,
    });
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
  return v.app && (v.app === process.env.LIGHTBULB_APP || v.app === process.env.LIGHTBULB_PREVIEW_APP) ? v : null;
}
