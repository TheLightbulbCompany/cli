import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, expect, it, vi } from "vitest";

vi.mock("server-only", () => ({}));
const { SampleBanner } = await import("./sample-banner");

afterEach(() => vi.unstubAllEnvs());

it("shows on sample data and never on the live database", () => {
  vi.stubEnv("DATABASE_URL", "");
  vi.stubEnv("LIGHTBULB_SAMPLE_DATA", "1");
  const banner = SampleBanner();
  expect(banner && renderToStaticMarkup(banner)).toContain("Preview with sample data — nothing you enter here is kept.");
  vi.stubEnv("DATABASE_URL", "postgres://live");
  expect(SampleBanner()).toBeNull();
});
