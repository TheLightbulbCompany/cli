import { NextRequest } from "next/server";
import type { Where } from "payload";
import { beforeAll, describe, expect, it, vi } from "vitest";

// Tests run outside Next's react-server condition, so stand in for it here.
vi.mock("server-only", () => ({}));
vi.stubEnv("LIGHTBULB_SAMPLE_DATA", "1");
vi.stubEnv("DATABASE_URL", "");
vi.stubEnv("LIGHTBULB_APP", "demo-app");
vi.stubEnv("LIGHTBULB_OPEN_TO", "room");
vi.stubEnv("LIGHTBULB_COOKIE_SECRET", "a-test-secret-that-is-long-enough-for-hs256");
vi.stubEnv("LIGHTBULB_PUBLISHERS", "user_pub, Boss@Example.com");

const { getPayload } = await import("payload");
const { default: config } = await import("../payload.config");
const { sessionCookie } = await import("./door");
const { proxy } = await import("../proxy");
const { default: sitemap } = await import("../app/sitemap");
const { run: content } = await import("../../scripts/content.mjs");
const { REST_DELETE, REST_PATCH, REST_POST } = await import("@payloadcms/next/routes");

const payload = await getPayload({ config });

/** The `cookie` header a door viewer's browser sends. */
async function cookieOf(sub: string, email: string) {
  const v = { sub, name: sub, email, ws: "ws_1", room: "room_1", app: "demo-app" };
  return (await sessionCookie(v)).split(";")[0];
}

/** Who Payload says is signed in for that cookie (the door strategy). */
async function signedIn(cookie?: string) {
  const headers = new Headers(cookie ? { cookie } : {});
  return (await payload.auth({ headers })).user;
}

let pageId: string | number;

beforeAll(async () => {
  const page = await payload.create({
    collection: "pages",
    data: { title: "About", slug: "about", _status: "published", layout: [{ blockType: "hero", headline: "Old" }] },
  });
  pageId = page.id;
});

describe("content", () => {
  it("admin requires the door", async () => {
    const closed = await proxy(new NextRequest("https://demo-app.example.com/admin"));
    expect(closed.headers.get("x-middleware-rewrite")).toBe("https://demo-app.example.com/_lightbulb/closed");
    const api = await proxy(new NextRequest("https://demo-app.example.com/api/pages"));
    expect(api.status).toBe(401);

    const cookie = await cookieOf("user_1", "ada@example.com");
    const open = await proxy(new NextRequest("https://demo-app.example.com/admin", { headers: { cookie } }));
    expect(open.headers.get("x-middleware-rewrite")).toBeNull();
    expect(await signedIn()).toBeNull();
    expect(await signedIn(cookie)).toMatchObject({ sub: "user_1", email: "ada@example.com" });

    // Editors sign in through the door even when the site is public.
    vi.stubEnv("LIGHTBULB_OPEN_TO", "public");
    const pub = await proxy(new NextRequest("https://demo-app.example.com/admin/collections/pages"));
    expect(pub.headers.get("x-middleware-rewrite")).toBe("https://demo-app.example.com/_lightbulb/closed");
    expect((await proxy(new NextRequest("https://demo-app.example.com/about"))).headers.get("x-middleware-rewrite")).toBeNull();
    vi.stubEnv("LIGHTBULB_OPEN_TO", "room");
  });

  it("an editor can save a draft and a publisher can publish it", async () => {
    // Through Payload's REST API, as the admin calls it, signed in by the door's cookie.
    const call = async (method: "PATCH" | "DELETE", cookie: string, query: string, body?: object) => {
      const handler = method === "PATCH" ? REST_PATCH(config) : REST_DELETE(config);
      const req = new Request(`https://demo-app.example.com/api/pages/${pageId}${query}`, {
        method,
        headers: { cookie, "content-type": "application/json" },
        body: body && JSON.stringify(body),
      });
      return (await handler(req, { params: Promise.resolve({ slug: ["pages", String(pageId)] }) })).status;
    };
    const editor = await cookieOf("user_1", "ada@example.com");
    const publisher = await cookieOf("user_2", "boss@example.com");
    const live = async () => (await payload.findByID({ collection: "pages", id: pageId })).title;

    expect(await call("PATCH", editor, "?draft=true", { title: "About us" })).toBe(200);
    expect((await payload.findByID({ collection: "pages", id: pageId, draft: true })).title).toBe("About us");
    expect(await live()).toBe("About");

    expect(await call("PATCH", editor, "", { title: "About us", _status: "published" })).toBe(403);
    expect(await call("DELETE", editor, "")).toBe(403);
    // No live edit without a status, and no unpublishing.
    expect(await call("PATCH", editor, "", { title: "Live edit" })).toBe(403);
    expect(await call("PATCH", editor, "", { _status: "draft" })).toBe(403);
    expect(await live()).toBe("About");
    expect((await payload.findByID({ collection: "pages", id: pageId }))._status).toBe("published");
    // Nor restoring their own draft version live.
    const where: Where = { and: [{ parent: { equals: pageId } }, { "version.title": { equals: "About us" } }] };
    const old = (await payload.findVersions({ collection: "pages", where, sort: "-createdAt", limit: 1 })).docs[0];
    const restore = new Request(`https://demo-app.example.com/api/pages/versions/${old.id}`, { method: "POST", headers: { cookie: editor } });
    const restored = await REST_POST(config)(restore, { params: Promise.resolve({ slug: ["pages", "versions", String(old.id)] }) });
    expect(restored.status).toBe(403);
    expect(await live()).toBe("About");
    // Nor publishing through Payload's locale flags.
    expect(await call("PATCH", editor, "?draft=true&publishAllLocales=true", { title: "About us" })).toBe(403);

    expect(await call("PATCH", publisher, "", { title: "About us", _status: "published" })).toBe(200);
    expect(await live()).toBe("About us");
  });

  it("content script sets a draft field", async () => {
    await content(payload, ["set", "pages", String(pageId), "layout.0.headline", "Change is here", "--draft"]);
    const draft = await payload.findByID({ collection: "pages", id: pageId, draft: true });
    const live = await payload.findByID({ collection: "pages", id: pageId });
    expect(draft.layout?.[0]).toMatchObject({ headline: "Change is here" });
    expect(live.layout?.[0]).toMatchObject({ headline: "Old" });

    await content(payload, ["publish", "pages", String(pageId)]);
    const shipped = await payload.findByID({ collection: "pages", id: pageId });
    expect(shipped.layout?.[0]).toMatchObject({ headline: "Change is here" });
    expect(shipped._status).toBe("published");
  });

  it("sitemap lists published pages only", async () => {
    await payload.create({ collection: "pages", data: { title: "Secret", slug: "secret", _status: "draft" }, draft: true });
    const urls = (await sitemap()).map((e) => e.url);
    expect(urls).toContain("http://localhost:3000/about");
    expect(urls).toContain("http://localhost:3000/");
    expect(urls.some((u) => u.endsWith("/secret"))).toBe(false);
  });
});
