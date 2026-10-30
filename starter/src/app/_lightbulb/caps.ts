// Derived from Dyad (https://github.com/dyad-sh/dyad), commit 0ee4a524, worker/dyad_logs.js. Copyright (c) Dyad contributors. Apache-2.0. Modified by The Lightbulb Company.

const MAX_ARGUMENTS = 20;
const MAX_ARGUMENT_BYTES = 8 * 1024;
const MAX_CALL_BYTES = 64 * 1024;

/** One error report, capped: 20 values, 8 KB each, 64 KB in all. */
export function capArgs(args: unknown[]): string[] {
  const out: string[] = [];
  let total = 0;
  for (const arg of args.slice(0, MAX_ARGUMENTS)) {
    const text = clip(typeof arg === "string" ? arg : safeJson(arg), MAX_ARGUMENT_BYTES);
    const size = bytes(text);
    if (total + size > MAX_CALL_BYTES) break;
    out.push(text);
    total += size;
  }
  return out;
}

function bytes(s: string): number {
  return new TextEncoder().encode(s).length;
}

function clip(s: string, max: number): string {
  if (bytes(s) <= max) return s;
  const suffix = "… [truncated]";
  const kept = new TextEncoder().encode(s).slice(0, max - bytes(suffix));
  return new TextDecoder().decode(kept).replace(/�$/, "") + suffix;
}

function safeJson(v: unknown): string {
  try {
    return JSON.stringify(v) ?? String(v);
  } catch {
    return String(v);
  }
}
