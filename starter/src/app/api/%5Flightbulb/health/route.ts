import { dbMode } from "@/lib/db";

export const dynamic = "force-dynamic";

/** What `lightbulb app up` checks before it reports a preview done. */
export function GET() {
  return Response.json({
    ok: true,
    // Vercel sets the git variable to "" on file deploys, so empty must fall through.
    commit: process.env.VERCEL_GIT_COMMIT_SHA || process.env.LIGHTBULB_COMMIT || null,
    db: dbMode(),
  });
}
