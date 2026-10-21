import type { Item, Turn } from "./derive";

/** These words describe elapsed work in the current turn, independent of model effort. */
export function thinkingWords(seconds: number): string {
  if (seconds >= 60) return "Thinking very deeply";
  if (seconds >= 30) return "Thinking deeply";
  return "Thinking";
}

export type ThoughtItem = Extract<Item, { type: "thought" }>;

/** Grouped thoughts can contain older settled blocks: the latest block owns the live state. */
export function latestThought(item: ThoughtItem) {
  return item.more?.at(-1) ?? item.event;
}

/** A stale streaming flag in a prior turn must not keep its thinking clock alive. */
export function thoughtIsLive(item: ThoughtItem, active: boolean, turnStarted?: number): boolean {
  const latest = latestThought(item);
  return active && Boolean(latest.streaming) && (turnStarted === undefined || latest.at >= turnStarted);
}

/** Keep one clock across model calls, tool calls, replies and remounted status rows.
 * The harness start is authoritative: a steering message can create a later visible turn
 * without starting a new harness turn. Older snapshots fall back to the prompt's timestamp. */
export function thinkingStarted(turn: Turn, turnStarted?: number): number {
  return turnStarted ?? turn.started;
}
