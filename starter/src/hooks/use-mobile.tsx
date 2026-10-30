// Derived from Dyad's Next.js template (https://github.com/dyad-sh/nextjs-template), commit 121d5674, src/hooks/use-mobile.tsx. Copyright 2025 Dyad Tech, Inc. Apache-2.0. Modified by The Lightbulb Company.
import * as React from "react"

const QUERY = "(max-width: 767px)"

function subscribe(onChange: () => void) {
  const mql = window.matchMedia(QUERY)
  mql.addEventListener("change", onChange)
  return () => mql.removeEventListener("change", onChange)
}

export function useIsMobile() {
  return React.useSyncExternalStore(
    subscribe,
    () => window.matchMedia(QUERY).matches,
    () => false
  )
}
