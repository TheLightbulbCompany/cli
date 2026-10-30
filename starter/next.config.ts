// Derived from Dyad's Next.js template (https://github.com/dyad-sh/nextjs-template), commit 121d5674, next.config.ts. Copyright 2025 Dyad Tech, Inc. Apache-2.0. Modified by The Lightbulb Company.
import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // PGlite loads its own wasm and data files at runtime.
  serverExternalPackages: ["@electric-sql/pglite"],
  // Sample mode reads the migrations and seed from disk.
  outputFileTracingIncludes: { "/**": ["./drizzle/*.sql"] },
  // Only Lightbulb (Canvas) may frame the app.
  async headers() {
    return [
      {
        source: "/:path*",
        headers: [
          {
            key: "Content-Security-Policy",
            value: "frame-ancestors 'self' https://thelightbulb.company https://*.thelightbulb.company",
          },
        ],
      },
    ];
  },
};

export default nextConfig;
