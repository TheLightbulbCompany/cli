import "server-only";

// Accounts for an app whose people sign up for themselves (playbooks/accounts.md). Not the door:
// the door lets Lightbulb's people in; this is the app's own sign-in, with `open_to: public`.
import { randomBytes, scrypt, timingSafeEqual } from "node:crypto";
import { promisify } from "node:util";
import { jwtVerify, SignJWT } from "jose";
import { cookies } from "next/headers";

export const ACCOUNT_COOKIE = "lb_account";
const WEEK = 7 * 86_400;
const AUDIENCE = "account"; // a door session, signed with the same secret, is not an account
const KEY_LENGTH = 64;
const derive = promisify(scrypt) as (password: string, salt: Buffer, length: number) => Promise<Buffer>;

/** `Set-Cookie` that ends the session. */
export const SIGNED_OUT_COOKIE = `${ACCOUNT_COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0`;

/** A salted scrypt hash, as `salt:hash` in hex, for the `users` table. */
export async function hashPassword(password: string): Promise<string> {
  const salt = randomBytes(16);
  return `${salt.toString("hex")}:${(await derive(password, salt, KEY_LENGTH)).toString("hex")}`;
}

export async function verifyPassword(password: string, stored: string): Promise<boolean> {
  const [salt, hash] = stored.split(":");
  const expected = Buffer.from(hash ?? "", "hex");
  if (!salt || expected.length !== KEY_LENGTH) return false;
  return timingSafeEqual(await derive(password, Buffer.from(salt, "hex"), KEY_LENGTH), expected);
}

/** The `Set-Cookie` that signs this user in for a week. */
export async function accountCookie(id: string | number): Promise<string> {
  // ponytail: a stateless cookie; signing out ends this browser only. Add a sessions table when a stolen session must be revocable.
  const token = await new SignJWT({})
    .setProtectedHeader({ alg: "HS256" })
    .setSubject(String(id))
    .setAudience(AUDIENCE)
    .setIssuedAt()
    .setExpirationTime(`${WEEK}s`)
    .sign(secret());
  return `${ACCOUNT_COOKIE}=${token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=${WEEK}`;
}

/** Reads a cookie value; null when it is missing, forged, expired or not an account's. */
export async function readAccount(value: string, now = new Date()): Promise<{ id: string } | null> {
  try {
    const { payload } = await jwtVerify(value, secret(), { algorithms: ["HS256"], audience: AUDIENCE, currentDate: now });
    return payload.sub ? { id: payload.sub } : null;
  } catch {
    return null;
  }
}

/** Who is signed in to this request, for server code. A numeric `users.id`: `Number(account.id)`. */
export async function currentAccount(): Promise<{ id: string } | null> {
  const value = (await cookies()).get(ACCOUNT_COOKIE)?.value;
  return value ? readAccount(value) : null;
}

function secret(): Uint8Array {
  const value = process.env.LIGHTBULB_COOKIE_SECRET;
  if (!value) throw new Error("LIGHTBULB_COOKIE_SECRET is not set.");
  return new TextEncoder().encode(value);
}
