/**
 * What a runner's and an agent's pages say of the messages kiln couldn't read (docs/wire.md, "What
 * can't be read"): what the server keeps of them, `refused`, how many by wire and code and the
 * newest. Pure: words from what's kept, no React.
 */
import type { RefusedMessage, RefusedMessages } from "@/api/types";
import { relativeTime } from "@/lib/format";

/** One that came within this long puts a runner's row in the fail tone. */
const DAY_MS = 86_400_000;

/** How many in all. */
export function refusedTotal(r: RefusedMessages | undefined): number {
  return (r?.counts ?? []).reduce((n, c) => n + c.n, 0);
}

/** How many by wire and code, the most first: "runner unknown 9, server_item malformed 3". */
export function refusedCounts(r: RefusedMessages | undefined): string {
  return [...(r?.counts ?? [])]
    .filter((c) => c.n > 0)
    .sort((a, b) => b.n - a.n)
    .map((c) => `${c.wire} ${c.code} ${c.n}`)
    .join(", ");
}

/** What it was and why it couldn't be read: "variant 51: AgentMsg has 51 variants". */
export function whatWhy(m: RefusedMessage): string {
  return [m.unread.what, m.unread.why].filter(Boolean).join(": ");
}

/** The newest first. */
export function newestFirst(r: RefusedMessages | undefined): RefusedMessage[] {
  return [...(r?.newest ?? [])].reverse();
}

/** When it came: never later than now, whatever the server's clock says. */
export function refusedWhen(m: RefusedMessage, now: number): string {
  return relativeTime(Math.min(m.at, now) / 1000, now);
}

/**
 * A runner's "Refused messages" row: what the server couldn't read of it, what it couldn't read of
 * the server's, and reach's. "none", or how many by wire and code, then the newest with when, what
 * and why: "1 (runner unknown) · 2m ago, variant 51: AgentMsg has 51 variants".
 */
export function refusedLine(r: RefusedMessages | undefined, now: number): string {
  const total = refusedTotal(r);
  if (total === 0) return "none";
  const one = total === 1 ? r?.counts.find((c) => c.n > 0) : undefined;
  const by = one ? `${one.wire} ${one.code}` : refusedCounts(r);
  const latest = r?.newest.at(-1);
  return `${total} (${by})${latest ? ` · ${total === 1 ? "" : "newest "}${refusedWhen(latest, now)}, ${whatWhy(latest)}` : ""}`;
}

/** Any came within a day: a runner's row says so in the fail tone. */
export function refusedLately(r: RefusedMessages | undefined, now: number): boolean {
  return (r?.newest ?? []).some((m) => now - m.at < DAY_MS);
}

/** The flag in an agent's head, which opens the list: none while none of its messages was refused. */
export function refusedFlag(r: RefusedMessages | undefined): string | null {
  const n = refusedTotal(r);
  return n === 0 ? null : `${n} message${n === 1 ? "" : "s"} refused`;
}
