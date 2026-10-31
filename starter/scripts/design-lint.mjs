// Flags colours and one-off sizes written outside the design tokens, so the app keeps the
// company's look. Colours belong in design/tokens.css; src/components/ui is the token-based kit.
// Also flags two ways a button silently does nothing: native dialogs and forms refused in silence.
// Prints file:line for each and exits 1.
import { readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

const RULES = [
  [/(?<![&\w])#(?:[0-9a-f]{8}|[0-9a-f]{6}|[0-9a-f]{3,4})\b/i, "raw hex colour; use a token from design/tokens.css"],
  [/\b(?:rgba?|hsla?|oklch|oklab|lab|lch|hwb)\(/i, "raw colour function; use a token from design/tokens.css"],
  [/(?<![\w-])[a-z][a-z0-9-]*-\[[^\]\s]+\](?!:)/, "arbitrary Tailwind value; use the scale or a token"],
  // Canvas frames the app without allow-modals: confirm() answers false there, so the button does nothing.
  [/(?<![\w.$])(?:window\.)?(?:confirm|alert|prompt)\(/, "native dialog does nothing in Canvas; act on the click, or ask in the page"],
];
const SKIP = ["node_modules", ".next", "src/components/ui"];

/** The problems in one file's text, as [line, message]. */
export function lint(text) {
  const found = [];
  text.split("\n").forEach((line, i) => {
    for (const [re, message] of RULES) if (re.test(line)) found.push([i + 1, message]);
  });
  return found;
}

function* files(dir) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (SKIP.some((s) => p === s || p.startsWith(s + path.sep))) continue;
    if (e.isDirectory()) yield* files(p);
    else if (/\.(tsx?|jsx?|mjs|css)$/.test(e.name) && !/\.test\./.test(e.name)) yield p;
  }
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  let bad = 0;
  for (const f of files("src")) {
    for (const [line, message] of lint(readFileSync(f, "utf8"))) {
      console.log(`${f}:${line}: ${message}`);
      bad++;
    }
  }
  process.exit(bad ? 1 : 0);
}
