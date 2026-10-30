// Derived from Dyad's Next.js template (https://github.com/dyad-sh/nextjs-template), commit 121d5674, src/lib/utils.ts. Copyright 2025 Dyad Tech, Inc. Apache-2.0. Modified by The Lightbulb Company.
import { clsx, type ClassValue } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}
