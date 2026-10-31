"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { DoorPage, type DoorNames } from "./door-page";

const API = process.env.NEXT_PUBLIC_LIGHTBULB_API ?? "https://api.thelightbulb.company";

/** Why signing in stopped, or `invite` before the person clicks Continue. */
export type SignInState = "busy" | "invite" | "expired" | "wrong_app" | "offline" | "invite_used";

export const MESSAGES: Record<SignInState, string> = {
  busy: "Signing you in…",
  invite: "You were invited to this app. Continue to sign in.",
  expired: "This link has expired — open the app from Canvas again.",
  wrong_app: "That link was for a different app — open this one from Canvas.",
  offline: "We could not reach Lightbulb. Check your connection and try again.",
  invite_used:
    "This invite link has been used or has expired. Ask the person who sent it for a new one, or open the app from Canvas.",
};

/** Swaps the link's pass (or invite) for this app's session cookie; `ok`, or why not. */
export async function exchange(hash: URLSearchParams, invite: boolean, f: typeof fetch = fetch): Promise<"ok" | SignInState> {
  try {
    let token = hash.get("token");
    if (invite) {
      const t = hash.get("t");
      const res = t ? await f(`${API}/api/v1/apps/invite/${encodeURIComponent(t)}`) : null;
      if (res && res.status >= 500) return "offline";
      token = res?.ok ? ((await res.json()) as { token?: string }).token ?? null : null;
      if (!token) return "invite_used";
    }
    if (!token) return "expired";
    const res = await f("/api/_lightbulb/session", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ token, framed: globalThis.top !== globalThis.self }),
    });
    if (res.ok) return "ok";
    if (res.status >= 500) return "offline";
    const body = (await res.json().catch(() => ({}))) as { reason?: string };
    return body.reason === "wrong_app" ? "wrong_app" : "expired";
  } catch {
    return "offline";
  }
}

/**
 * Reads the pass (or an invite) from the link's #fragment, signs in, and opens the app. A pass
 * signs in on load; an invite waits for Continue, because link scanners open links and an invite
 * works once.
 */
export function SignIn({ invite, names }: { invite?: boolean; names: DoorNames }) {
  const [state, setState] = useState<SignInState>(invite ? "invite" : "busy");
  const hash = useRef<URLSearchParams | null>(null);
  const go = useCallback(async () => {
    const r = await exchange(hash.current ?? new URLSearchParams(), Boolean(invite));
    if (r === "ok") location.replace("/");
    else setState(r);
  }, [invite]);
  const run = () => {
    setState("busy");
    return go();
  };
  useEffect(() => {
    hash.current = new URLSearchParams(location.hash.slice(1));
    // Keep the pass out of history and of any later page's referrer.
    history.replaceState(null, "", location.pathname);
    if (!invite) void go();
  }, [invite, go]);
  const action =
    state === "invite"
      ? { label: "Continue", onClick: run }
      : state === "offline"
        ? { label: "Try again", onClick: run }
        : undefined;
  return (
    <>
      <DoorPage names={names} message={MESSAGES[state]} action={action} busy={state === "busy"} />
      <noscript>
        <p className="p-8 text-center text-muted-foreground">
          Signing in needs JavaScript. Turn it on, then open the app from Canvas again.
        </p>
      </noscript>
    </>
  );
}
