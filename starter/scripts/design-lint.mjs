// Flags colours and one-off sizes written outside the design tokens, so the app keeps the
// company's look. Colours belong in design/tokens.css; src/components/ui is the token-based kit.
// Also flags two ways a button silently does nothing: native dialogs and forms refused in silence,
// and copy that promises an email when the app has no way to send one.
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
// A promise of mail the person will never get. Only when no email service is installed.
const EMAIL_PROMISE = [
  /\b(?:we['’]?ll|we will|you['’]?ll|you will)\s+(?:send|email|e-mail|receive an e-?mail|get an e-?mail|get a confirmation)\b|\bcheck your (?:e-?mail|inbox)\b|\bconfirmation e-?mail\b/i,
  "promises an email this app can't send; say only what it does, or add email sending first",
];
const SKIP = ["node_modules", ".next", "src/components/ui"];
const SENDERS = /"(?:resend|@sendgrid\/mail|nodemailer|postmark|mailgun\.js|@aws-sdk\/client-sesv2?)"/;

/** The problems in one file's text, as [line, message]. */
export function lint(text, { canEmail = false } = {}) {
  const found = [];
  const rules = canEmail ? RULES : [...RULES, EMAIL_PROMISE];
  text.split("\n").forEach((line, i) => {
    for (const [re, message] of rules) if (re.test(line)) found.push([i + 1, message]);
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
  const canEmail = SENDERS.test(readFileSync("package.json", "utf8"));
  for (const f of files("src")) {
    for (const [line, message] of lint(readFileSync(f, "utf8"), { canEmail })) {
      console.log(`${f}:${line}: ${message}`);
      bad++;
    }
  }
  process.exit(bad ? 1 : 0);
}
