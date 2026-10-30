import { afterEach, describe, expect, it, vi } from "vitest";

afterEach(() => {
  vi.unstubAllEnvs();
  vi.doUnmock("server-only");
  vi.resetModules();
});

describe("db", () => {
  it("sample mode uses pglite and seeds", async () => {
    // Tests run outside Next's react-server condition, so stand in for it here.
    vi.doMock("server-only", () => ({}));
    vi.stubEnv("DATABASE_URL", "");
    vi.stubEnv("LIGHTBULB_SAMPLE_DATA", "1");
    const { dbMode, getDb } = await import("./index");
    const { items } = await import("./schema");
    expect(dbMode()).toBe("sample");
    const db = await getDb();
    expect((await db.select().from(items)).length).toBeGreaterThan(0);
  });

  it("live mode refuses to load in client code", async () => {
    // Client bundles resolve server-only without the react-server condition, where it throws.
    vi.stubEnv("DATABASE_URL", "postgres://user:pass@localhost/db");
    await expect(import("./index")).rejects.toThrow(/Client Component/);
  });
});
