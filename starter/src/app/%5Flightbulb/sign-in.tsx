"use client";

import { useEffect, useState } from "react";

const API = process.env.NEXT_PUBLIC_LIGHTBULB_API ?? "https://api.thelightbulb.company";

/** Reads the pass (or an invite to swap for one) from the link's #fragment, signs in, and opens the app. */
export function SignIn({ invite }: { invite?: boolean }) {
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    const hash = new URLSearchParams(location.hash.slice(1));
    history.replaceState(null, "", location.pathname);
    void (async () => {
      let token = hash.get("token");
      if (invite) {
        const t = hash.get("t");
        const res = t ? await fetch(`${API}/api/v1/apps/invite/${encodeURIComponent(t)}`) : null;
        token = res?.ok ? ((await res.json()) as { token?: string }).token ?? null : null;
      }
      const res = token
        ? await fetch("/api/_lightbulb/session", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ token }),
          })
        : null;
      if (res?.ok) location.replace("/");
      else setFailed(true);
    })().catch(() => setFailed(true));
  }, [invite]);
  return (
    <main className="flex min-h-screen items-center justify-center p-8">
      <p className="text-muted-foreground">
        {failed
          ? invite
            ? "This invite link has been used or has expired. Ask the person who sent it for a new one."
            : "This link has expired. Open the app again from Canvas in Lightbulb."
          : "Signing you in…"}
      </p>
    </main>
  );
}
