import { exportJWK, generateKeyPair, SignJWT, type CryptoKey } from "jose";
import { NextRequest } from "next/server";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Tests run outside Next's react-server condition, so stand in for it here.
vi.mock("server-only", () => ({}));

const { verifyPass, sessionCookie, readSession, doorOpen } = await import("./door");
const { proxy, config } = await import("../proxy");
const { unstable_doesMiddlewareMatch } = await import("next/experimental/testing/server");
const { POST: openSession } = await import("../app/api/%5Flightbulb/session/route");

let issuer: CryptoKey;

async function pass(claims: Record<string, unknown> = {}, key = issuer, kid = "k1", typ: string | null = "lightbulb-app-pass") {
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
    .setProtectedHeader({ alg: "EdDSA", kid, ...(typ ? { typ } : {}) })
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
    expect(await verifyPass(token, new Date(Date.now() + 331_000))).toBeNull();
  });

  it("rejects a pass signed by another key", async () => {
    const { privateKey } = await generateKeyPair("EdDSA", { crv: "Ed25519" });
    expect(await verifyPass(await pass({}, privateKey))).toBeNull();
  });

  it("rejects a pass older than 15 minutes", async () => {
    const token = await pass({ iat: Math.floor(Date.now() / 1000) - 931, exp: Math.floor(Date.now() / 1000) + 300 });
    expect(await verifyPass(token)).toBeNull();
  });

  it("rejects a pass issued in the future beyond 30 seconds", async () => {
    const token = await pass({ iat: Math.floor(Date.now() / 1000) + 60 });
    expect(await verifyPass(token)).toBeNull();
  });

  it("rejects a pass without the lightbulb-app-pass typ", async () => {
    expect(await verifyPass(await pass({}, issuer, "k1", null))).toBeNull();
    expect(await verifyPass(await pass({}, issuer, "k1", "JWT"))).toBeNull();
  });

  it("rejects a pass whose kid is not the issuer key's", async () => {
    expect(await verifyPass(await pass({}, issuer, "k2"))).toBeNull();
  });

  it("a pass is not a cookie and a cookie is not a pass", async () => {
    const token = await pass();
    const cookie = (await sessionCookie((await verifyPass(token))!)).split(";")[0].slice("lb_session=".length);
    expect(await readSession(token)).toBeNull();
    expect(await verifyPass(cookie)).toBeNull();
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
    expect(await readSession(value, new Date(Date.now() + 86_431_000))).toBeNull();

    const req = new NextRequest("https://demo-app.example.com/", { headers: { cookie: `lb_session=${value}` } });
    expect(await closedTo(req)).toBeNull();
    vi.stubEnv("LIGHTBULB_APP", "other-app");
    expect(await readSession(value)).toBeNull();
  });

  it("closed without a cookie, with status 401", async () => {
    expect(doorOpen()).toBe(false);
    const res = await proxy(new NextRequest("https://demo-app.example.com/reports"));
    expect(res.headers.get("x-middleware-rewrite")).toBe("https://demo-app.example.com/_lightbulb/closed");
    expect(res.status).toBe(401);
  });

  it("closed api routes answer 401 json", async () => {
    const res = await proxy(new NextRequest("https://demo-app.example.com/api/items"));
    expect(res.status).toBe(401);
    expect(await res.json()).toEqual({ code: "closed", message: "Open this from Canvas in Lightbulb." });
  });

  it("only health and the session route skip the door; error reports do not", () => {
    const gated = (path: string) => unstable_doesMiddlewareMatch({ config, url: path });
    expect(gated("/api/_lightbulb/health")).toBe(false);
    expect(gated("/api/_lightbulb/session")).toBe(false);
    expect(gated("/_lightbulb/closed")).toBe(false);
    expect(gated("/_next/static/x.js")).toBe(false);
    expect(gated("/api/_lightbulb/errors")).toBe(true);
    expect(gated("/api/_lightbulb/healthz")).toBe(true);
    expect(gated("/api/items")).toBe(true);
    expect(gated("/")).toBe(true);
  });

  it("error reports need a session unless the app is public", async () => {
    const report = () =>
      proxy(new NextRequest("https://demo-app.example.com/api/_lightbulb/errors", { method: "POST" }));
    const closed = await report();
    expect(closed.status).toBe(401);
    expect(await closed.json()).toEqual({ code: "closed", message: "Open this from Canvas in Lightbulb." });
    vi.stubEnv("LIGHTBULB_OPEN_TO", "public");
    const open = await report();
    expect(open.status).toBe(200);
    expect(open.headers.get("x-middleware-next")).toBe("1");
  });

  it("session route sets the cookie only for a same-origin json post", async () => {
    const token = await pass();
    const post = (headers: Record<string, string>) =>
      openSession(
        new Request("https://demo-app.example.com/api/_lightbulb/session", {
          method: "POST",
          headers,
          body: JSON.stringify({ token }),
        }),
      );
    const json = { "content-type": "application/json" };
    expect((await post({ ...json, origin: "https://evil.example" })).status).toBe(403);
    expect((await post({ ...json })).status).toBe(403);
    expect((await post({ "content-type": "text/plain", origin: "https://demo-app.example.com" })).status).toBe(403);
    const ok = await post({ ...json, origin: "https://demo-app.example.com" });
    expect(ok.status).toBe(204);
    expect(ok.headers.get("set-cookie")).toMatch(/^lb_session=/);
    expect((await post({ ...json, "sec-fetch-site": "same-origin" })).status).toBe(204);
  });

  it("session route says whether a refused pass is for another app", async () => {
    const post = async (token: string) =>
      (
        await openSession(
          new Request("https://demo-app.example.com/api/_lightbulb/session", {
            method: "POST",
            headers: { "content-type": "application/json", origin: "https://demo-app.example.com" },
            body: JSON.stringify({ token }),
          }),
        )
      ).json();
    expect((await post(await pass({ app: "other-app" }))).reason).toBe("wrong_app");
    expect((await post(await pass({ iat: Math.floor(Date.now() / 1000) - 2000 }))).reason).toBe("expired");
    expect((await post("not-a-pass")).reason).toBe("expired");
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
