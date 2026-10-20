import type { Item, Turn } from "./derive";

/** These words describe time spent in this thinking phase, independent of model effort. */
export function thinkingWords(seconds: number): string {
  if (seconds >= 60) return "Thinking very deeply";
  if (seconds >= 30) return "Thinking deeply";
  return "Thinking";
}

export type ThoughtItem = Extract<Item, { type: "thought" }>;

/** Grouped thoughts can contain older settled blocks: the latest block owns the live clock. */
export function latestThought(item: ThoughtItem) {
  return item.more?.at(-1) ?? item.event;
}

/** A stale streaming flag in a prior turn must not keep its thinking clock alive. */
export function thoughtIsLive(item: ThoughtItem, active: boolean, turnStarted?: number): boolean {
  const latest = latestThought(item);
  return active && Boolean(latest.streaming) && (turnStarted === undefined || latest.at >= turnStarted);
}

/** A known phase start, or a key for timing the transition when no completion timestamp exists. */
export function quietThinkingPhase(turn: Turn, turnStarted?: number): { key: string; since?: number } {
  const started = Math.max(turn.started, turnStarted ?? turn.started);
  const last = turn.items.findLast((item) => item.type !== "notice");
  if (!last) return { key: `turn:${turn.key}:${started}`, since: started };
  const events = last.type === "step" ? last.calls : last.type === "thought" ? [latestThought(last)] : [last.event];
  const latest = Math.max(...events.map((event) => event.kind === "tool" ? event.ended ?? event.started : event.at));
  // A continuation may reuse the old prompt and history. Its new turn start is the boundary.
  if (latest < started) return { key: `turn:${turn.key}:${started}`, since: started };
  if (last.type === "thought") return { key: last.key, since: latestThought(last).at };
  if (events.every((event) => event.kind === "tool" && event.ended !== undefined)) return { key: last.key, since: latest };
  // Replies (and older tool records) have no end time. Start when the quiet phase is observed,
  // rather than counting time spent writing the reply or running the tool as thinking.
  return { key: last.key };
}
