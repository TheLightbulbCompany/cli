"use client";
// Derived from Dyad (https://github.com/dyad-sh/dyad), commit 0ee4a524, worker/dyad_logs.js. Copyright (c) Dyad contributors. Apache-2.0. Modified by The Lightbulb Company.

import { useEffect } from "react";
import { capArgs } from "./caps";

const MAX_REPORTS = 20;
const sent = new Set<string>();

// Each distinct error once per page load, and at most 20 in all, so a loop cannot flood the log.
function report(args: unknown[]) {
  const key = `${args[0]}@${args[1]}`;
  if (sent.has(key) || sent.size >= MAX_REPORTS) return;
  sent.add(key);
  void fetch("/api/_lightbulb/errors", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ page: location.pathname, args: capArgs(args) }),
    keepalive: true,
  }).catch(() => {});
}

/** Sends this page's uncaught errors to the app's own log, where the builder reads them. */
export function ErrorReporter() {
  useEffect(() => {
    const onError = (e: ErrorEvent) =>
      report([e.message, `${e.filename}:${e.lineno}:${e.colno}`, e.error?.stack]);
    const onRejection = (e: PromiseRejectionEvent) =>
      report(["Unhandled promise rejection", e.reason?.stack ?? e.reason]);
    window.addEventListener("error", onError);
    window.addEventListener("unhandledrejection", onRejection);
    return () => {
      window.removeEventListener("error", onError);
      window.removeEventListener("unhandledrejection", onRejection);
    };
  }, []);
  return null;
}
