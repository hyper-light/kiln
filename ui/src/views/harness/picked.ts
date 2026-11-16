/** A setting picked here, shown at once (W21-R15) until its agent says it back (head.tsx's line, a sandbox's harness rows). */
import * as React from "react";
import { useClock } from "./dom";

/** How long a setting picked here shows as picked before its agent says it back (most do at once). */
const SAID_BACK_MS = 3500;

/**
 * A setting as it's shown: picked here, at once, until its agent says it back, or hasn't within a while
 * (then as the agent has it; a command still waiting says so, asked.tsx).
 */
export function usePicked(said: string): [string, (v: string) => void] {
  const [picked, setPicked] = React.useState<{ v: string; at: number } | null>(null);
  const now = useClock(picked ? 1000 : 60_000);
  if (picked && (picked.v === said || now - picked.at > SAID_BACK_MS)) setPicked(null);
  return [picked ? picked.v : said, (v) => setPicked({ v, at: Date.now() })];
}
