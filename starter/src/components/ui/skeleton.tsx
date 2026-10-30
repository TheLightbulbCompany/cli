// Derived from Dyad's Next.js template (https://github.com/dyad-sh/nextjs-template), commit 121d5674, src/components/ui/skeleton.tsx. Copyright 2025 Dyad Tech, Inc. Apache-2.0. Modified by The Lightbulb Company.
import { cn } from "@/lib/utils"

function Skeleton({
  className,
  ...props
}: React.HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn("animate-pulse rounded-md bg-primary/10", className)}
      {...props}
    />
  )
}

export { Skeleton }
