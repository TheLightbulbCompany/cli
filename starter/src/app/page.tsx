// Derived from Dyad's Next.js template (https://github.com/dyad-sh/nextjs-template), commit 121d5674, src/app/page.tsx. Copyright 2025 Dyad Tech, Inc. Apache-2.0. Modified by The Lightbulb Company.
const title = "{{name}}";

export default function Home() {
  return (
    <main className="flex min-h-screen flex-col items-center justify-center gap-4 p-8">
      <h1 className="text-3xl font-semibold tracking-tight">{title}</h1>
      <p className="text-muted-foreground">Being built. Check back soon.</p>
    </main>
  );
}
