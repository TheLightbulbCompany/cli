import { exportJWK, generateKeyPair, SignJWT, type CryptoKey } from "jose";
import { NextRequest } from "next/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Tests run outside Next's react-server condition, so stand in for it here.
vi.mock("server-only", () => ({}));

const { verifyPass, sessionCookie, readSession, doorOpen } = await import("./door");
const { proxy } = await import("../proxy");

let issuer: CryptoKey;

async function pass(claims: Record<string, unknown> = {}, key = issuer) {
  const now = Math.floor(Date.now() / 1000);
  return new SignJWT({
    iat: now,
    exp: now + 300,
    sub: "user_1",
    name: "Ada",
    email: "ada@example.com",
    ws: "ws_1",
    room: "room_1",
    app: "demo-app",
    ...claims,
  })
    .setProtectedHeader({ alg: "EdDSA", kid: "k1" })
    .sign(key);
}

function closedTo(req: NextRequest) {
  return proxy(req).then((res) => res.headers.get("x-middleware-rewrite"));
}

beforeEach(async () => {
  const { privateKey, publicKey } = await generateKeyPair("EdDSA", { crv: "Ed25519" });
  issuer = privateKey;
  const { kty, crv, x } = await exportJWK(publicKey);
  vi.stubEnv("LIGHTBULB_ISSUER_KEY", JSON.stringify({ kty, crv, x, kid: "k1" }));
  vi.stubEnv("LIGHTBULB_APP", "demo-app");
  vi.stubEnv("LIGHTBULB_COOKIE_SECRET", "a-test-secret-that-is-long-enough-for-hs256");
  vi.stubEnv("LIGHTBULB_OPEN_TO", "room");
});

afterEach(() => vi.unstubAllEnvs());

describe("door", () => {
  it("accepts a fresh pass for this app", async () => {
    expect(await verifyPass(await pass())).toEqual({
      sub: "user_1",
      name: "Ada",
      email: "ada@example.com",
      ws: "ws_1",
      room: "room_1",
      app: "demo-app",
    });
  });

  it("rejects an expired pass", async () => {
    const token = await pass();
    expect(await verifyPass(token, new Date(Date.now() + 301_000))).toBeNull();
  });

  it("rejects a pass signed by another key", async () => {
    const { privateKey } = await generateKeyPair("EdDSA", { crv: "Ed25519" });
    expect(await verifyPass(await pass({}, privateKey))).toBeNull();
  });

  it("rejects a pass with a claim it does not know", async () => {
    expect(await verifyPass(await pass({ role: "admin" }))).toBeNull();
  });

  it("door rejects a pass for another app", async () => {
    expect(await verifyPass(await pass({ app: "other-app" }))).toBeNull();
  });

  it("session cookie round-trips and expires in 24h", async () => {
    const viewer = (await verifyPass(await pass()))!;
    const header = await sessionCookie(viewer);
    expect(header).toMatch(/^lb_session=[^;]+; HttpOnly; Secure; SameSite=Lax; Path=\/; Max-Age=86400$/);
    const value = header.split(";")[0].slice("lb_session=".length);
    expect(await readSession(value)).toEqual(viewer);
    expect(await readSession(value, new Date(Date.now() + 86_399_000))).toEqual(viewer);
    expect(await readSession(value, new Date(Date.now() + 86_401_000))).toBeNull();

    const req = new NextRequest("https://demo-app.example.com/", { headers: { cookie: `lb_session=${value}` } });
    expect(await closedTo(req)).toBeNull();
    vi.stubEnv("LIGHTBULB_APP", "other-app");
    expect(await readSession(value)).toBeNull();
  });

  it("closed without a cookie", async () => {
    expect(doorOpen()).toBe(false);
    expect(await closedTo(new NextRequest("https://demo-app.example.com/reports"))).toBe(
      "https://demo-app.example.com/_lightbulb/closed",
    );
  });

  it("public mode needs no cookie", async () => {
    vi.stubEnv("LIGHTBULB_OPEN_TO", "public");
    expect(doorOpen()).toBe(true);
    expect(await closedTo(new NextRequest("https://demo-app.example.com/reports"))).toBeNull();
  });

  it("invite pass names the email", async () => {
    const token = await pass({ sub: "invite:client@example.org", name: "client@example.org", email: "client@example.org", room: "" });
    expect(await verifyPass(token)).toMatchObject({
      sub: "invite:client@example.org",
      name: "client@example.org",
      email: "client@example.org",
      room: "",
    });
  });
});
