import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, expect, it, vi } from "vitest";
import { doorNames } from "./door-page";
import Closed from "./closed/page";

afterEach(() => vi.unstubAllEnvs());

it("names the app and workspace and links to the app in Canvas", () => {
  vi.stubEnv("LIGHTBULB_APP_NAME", "Roof Quotes");
  vi.stubEnv("LIGHTBULB_WORKSPACE_NAME", "Acme Roofing");
  vi.stubEnv("LIGHTBULB_APP_ID", "app_42");
  const html = renderToStaticMarkup(Closed());
  expect(html).toContain("Open Roof Quotes from Lightbulb");
  expect(html).toContain("Acme Roofing");
  expect(html).toContain("Open this from Canvas in Lightbulb to get in.");
  expect(html).toContain('href="https://thelightbulb.company/chat?panel=canvas&amp;app=app_42"');
  expect(html).toContain("Invited by email? Use the link in your invitation.");
});

it("drops the names it does not know and keeps the sentence", () => {
  vi.stubEnv("LIGHTBULB_APP_NAME", "");
  vi.stubEnv("LIGHTBULB_WORKSPACE_NAME", "");
  vi.stubEnv("LIGHTBULB_APP_ID", "");
  expect(doorNames()).toEqual({ canvasUrl: "https://thelightbulb.company/chat?panel=canvas" });
  const html = renderToStaticMarkup(Closed());
  expect(html).toContain("Open this app from Lightbulb");
  expect(html).toContain("Open this from Canvas in Lightbulb to get in.");
  expect(html).toContain("Open Canvas");
  for (const jargon of ["pass", "token", "401", "JWS", "cookie"]) expect(html).not.toContain(jargon);
});
