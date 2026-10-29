/**
 * What the responsiveness check measures (docs/ci.md W26-R15, AC06): `counted` marks a render of
 * the part it's in, on `window.__rvRenders` for the e2e counters to read; `useStable` keeps a
 * refetch's data on its last reference while it reads the same, so the memoized files, threads
 * and rows under it see no change and don't render again.
 */
import * as React from "react";

const counts = new Map<string, number>();
if (typeof window !== "undefined") (window as { __rvRenders?: Map<string, number> }).__rvRenders = counts;

/** One more render of `what`: called from a component's body, so it counts the renders it makes. */
export function counted(what: string) {
  counts.set(what, (counts.get(what) ?? 0) + 1);
}

/** `v` while `sig` reads the same as last render's: the last reference, so memoized children see nothing changed. */
export function useStable<T>(v: T, sig: string): T {
  const r = React.useRef<{ sig: string; v: T } | null>(null);
  if (r.current === null || r.current.sig !== sig) r.current = { sig, v };
  return r.current.v;
}
