import { SignJWT } from "jose";
import { beforeEach, expect, it, vi } from "vitest";

vi.mock("server-only", () => ({}));
vi.mock("next/headers", () => ({ cookies: async () => ({ get: () => undefined }) }));

const SECRET = "test-secret-test-secret-test-secret";
const { ACCOUNT_COOKIE, accountCookie, hashPassword, readAccount, SIGNED_OUT_COOKIE, verifyPassword } = await import("./accounts");
const value = (cookie: string) => cookie.split(";")[0].split("=")[1];

beforeEach(() => vi.stubEnv("LIGHTBULB_COOKIE_SECRET", SECRET));

it("a password checks against its hash and nothing else does", async () => {
  const stored = await hashPassword("right horse");
  expect(stored).not.toContain("right horse");
  expect(await verifyPassword("right horse", stored)).toBe(true);
  expect(await verifyPassword("wrong horse", stored)).toBe(false);
  expect(await hashPassword("right horse")).not.toBe(stored);
});

it("a damaged stored hash is a wrong password, not a crash", async () => {
  for (const bad of ["", "abc", "abc:", ":abc", "abc:abcd"]) expect(await verifyPassword("x", bad)).toBe(false);
});

it("sign-up's cookie reads back as that user, and is HttpOnly", async () => {
  const cookie = await accountCookie(42);
  expect(cookie.startsWith(`${ACCOUNT_COOKIE}=`)).toBe(true);
  expect(cookie).toMatch(/HttpOnly/);
  expect(await readAccount(value(cookie))).toEqual({ id: "42" });
});

it("refuses a forged, expired or door cookie", async () => {
  const good = value(await accountCookie(1));
  expect(await readAccount(`${good}x`)).toBeNull();
  expect(await readAccount(good, new Date(Date.now() + 8 * 86_400_000))).toBeNull();
  const door = await new SignJWT({ sub: "1" })
    .setProtectedHeader({ alg: "HS256" })
    .setExpirationTime("1h")
    .sign(new TextEncoder().encode(SECRET));
  expect(await readAccount(door)).toBeNull();
});

it("signing out empties the cookie now", () => {
  expect(SIGNED_OUT_COOKIE).toMatch(new RegExp(`^${ACCOUNT_COOKIE}=;`));
  expect(SIGNED_OUT_COOKIE).toMatch(/Max-Age=0/);
});
