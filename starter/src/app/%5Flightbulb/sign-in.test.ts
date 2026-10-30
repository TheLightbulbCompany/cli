import { afterEach, beforeEach, expect, it, vi } from "vitest";

// Hooks run inline so the component's effect and its buttons can be driven without a browser.
vi.mock("react", async (orig) => ({
  ...(await orig<typeof import("react")>()),
  useState: (v: unknown) => [v, () => {}],
  useEffect: (f: () => void) => f(),
  useCallback: (f: unknown) => f,
  useRef: (v: unknown) => ({ current: v }),
}));

const { SignIn, exchange, MESSAGES } = await import("./sign-in");
const names = { canvasUrl: "https://thelightbulb.company/chat?panel=canvas" };
let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  fetchMock = vi.fn(async (url: string) =>
    url.includes("/apps/invite/") ? Response.json({ token: "pass-1" }) : new Response(null, { status: 204 }),
  );
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("location", { hash: "#t=invite-1", pathname: "/_lightbulb/invite", replace: vi.fn() });
  vi.stubGlobal("history", { replaceState: vi.fn() });
});

afterEach(() => vi.unstubAllGlobals());

/** The DoorPage element SignIn renders (inside a fragment, beside the noscript). */
function page(el: ReturnType<typeof SignIn>) {
  return (el.props as { children: { props: { message: string; action?: { label: string; onClick: () => Promise<void> } } }[] })
    .children[0].props;
}

it("an invite calls Lightbulb only after Continue is clicked", async () => {
  const p = page(SignIn({ invite: true, names }));
  expect(history.replaceState).toHaveBeenCalled();
  expect(fetchMock).not.toHaveBeenCalled();
  expect(p.message).toBe(MESSAGES.invite);
  expect(p.action?.label).toBe("Continue");
  await p.action!.onClick();
  expect(fetchMock.mock.calls[0][0]).toBe("https://api.thelightbulb.company/api/v1/apps/invite/invite-1");
  expect(fetchMock.mock.calls[1][0]).toBe("/api/_lightbulb/session");
  expect(location.replace).toHaveBeenCalledWith("/");
});

it("a pass from Canvas signs in on load", () => {
  vi.stubGlobal("location", { hash: "#token=pass-1", pathname: "/_lightbulb/session", replace: vi.fn() });
  SignIn({ names });
  expect(fetchMock.mock.calls[0][0]).toBe("/api/_lightbulb/session");
});

it("tells expired, another app's link and no connection apart", async () => {
  const pass = new URLSearchParams("token=p");
  const answer = (status: number, body: unknown) => vi.fn(async () => Response.json(body, { status }));
  expect(await exchange(pass, false, answer(401, { reason: "expired" }))).toBe("expired");
  expect(await exchange(pass, false, answer(401, { reason: "wrong_app" }))).toBe("wrong_app");
  expect(await exchange(pass, false, answer(503, {}))).toBe("offline");
  const down = vi.fn(async () => {
    throw new TypeError("Failed to fetch");
  });
  expect(await exchange(pass, false, down)).toBe("offline");
  expect(await exchange(new URLSearchParams(), false, answer(204, {}))).toBe("expired");
  expect(await exchange(new URLSearchParams("t=x"), true, answer(404, {}))).toBe("invite_used");
  for (const m of Object.values(MESSAGES)) {
    for (const jargon of ["pass", "token", "401", "cookie"]) expect(m).not.toContain(jargon);
  }
});
