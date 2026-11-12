import type { GoalStack } from "./harness";

/** A change on its way holds back the reads of its stack this long at most: one stuck on the server doesn't hold what's shown for good. */
const HELD_MS = 10_000;

/**
 * Each agent's stack as last shown, against the changes made here: a read asked for before a change
 * here came back may have been answered before the change, so it doesn't replace what the change
 * returned (the next read shows what's new); nor does one that lands while a change shown here at
 * once is on its way, for `HELD_MS` at most (W21-R15). `clock`: a monotonic time.
 */
export function stackReads(clock: () => number) {
  const last = new Map<string, GoalStack>();
  const changed = new Map<string, number>();
  /** Changes on their way from here, by agent: how many, and when the last of them went. */
  const out = new Map<string, { n: number; at: number }>();
  let generation = 0;
  const holding = (agent: string) => {
    const o = out.get(agent);
    return o !== undefined && clock() - o.at < HELD_MS;
  };
  return {
    clear() {
      generation++;
      last.clear();
      changed.clear();
      out.clear();
    },
    /** A read of `agent`'s stack, asked for now: what it gives back is what to show. */
    ask(agent: string): (stack: GoalStack) => GoalStack {
      const at = clock();
      return (stack) => {
        const mine = last.get(agent);
        if (mine && (at < (changed.get(agent) ?? -Infinity) || holding(agent))) return mine;
        last.set(agent, stack);
        return stack;
      };
    },
    /** A change made here came back with `stack` (or is shown so at once). */
    changed(agent: string, stack: GoalStack) {
      last.set(agent, stack);
      changed.set(agent, clock());
    },
    /** A change of `agent`'s stack, shown already, is on its way: what it gives back says it's back (answered or failed). */
    sending(agent: string): () => void {
      const at = generation;
      out.set(agent, { n: (out.get(agent)?.n ?? 0) + 1, at: clock() });
      let back = false;
      return () => {
        if (back || at !== generation) return;
        back = true;
        const o = out.get(agent);
        if (o && o.n > 1) out.set(agent, { n: o.n - 1, at: o.at });
        else out.delete(agent);
      };
    },
    /** The stack last shown (none yet: null), for a read that failed. */
    last(agent: string): GoalStack | null {
      return last.get(agent) ?? null;
    },
  };
}
