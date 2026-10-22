/**
 * What the goal stack panel (goals.tsx) works out, kept apart to be tested: the goals its edit box
 * covers and how the box's lines go back to them, which reads of a stack came back too late to
 * show, which goals can become plans, and where a planned goal's steps stand.
 */
import type { GoalItem, GoalStack } from "@/api/harness";
import type { Stand } from "@/views/plans/status";

/** The goal that stopped a paused stack with none under way (the last to end, not met or skipped), to try again first. */
/** One of its own goals still to go, not a part, a plan's step or planned already: it can become a plan (Make a plan). */
export function canPlan(g: GoalItem): boolean {
  return (g.status === "pending" || g.status === "active") && !g.part_of && !g.planned_as && !g.step && !g.waits_on;
}

const PART_STAND = { pending: "waiting", active: "working", met: "met", unmet: "unmet", skipped: "skipped" } as const;

/**
 * Where step `n` of the plan `goal` became stands, as its part on the stack says: under way while it
 * is, or while one of its own parts (its criteria and tests) is; none when it isn't on the stack.
 */
export function stepStand(goal: GoalItem, items: readonly GoalItem[], n: number, paused: boolean): Stand | undefined {
  const part = items.find((i) => i.part_of === goal.id && i.part_kind === "plan" && i.step?.n === n);
  if (!part) return undefined;
  const busy = part.status === "active" || (part.status === "pending" && items.some((i) => i.part_of === part.id && i.status === "active"));
  if (busy) return paused ? "paused" : "working";
  return PART_STAND[part.status];
}

export function stoppedBy(stack: GoalStack | null): GoalItem | undefined {
  if (stack?.state !== "paused" || stack.items.some((i) => i.status === "active")) return undefined;
  const last = stack.items.filter((i) => i.status !== "pending").at(-1);
  return last && (last.status === "unmet" || last.status === "skipped") ? last : undefined;
}

/**
 * The words of the button that runs a stack that isn't running and has goals to go: Resume with a
 * goal under way (paused) or some met, Go on to the next after one that stopped it, else Start.
 */
export function startWords(stack: GoalStack | null): string | null {
  const items = stack?.items ?? [];
  const active = items.some((i) => i.status === "active");
  if (!stack || stack.state === "running" || !(active || items.some((i) => i.status === "pending"))) return null;
  if (stoppedBy(stack)) return "Go on to the next";
  return active || items.some((i) => i.status === "met") ? "Resume" : "Start";
}

/** The goals an edit covers: the one under way first (it can be put back, or given other words), then the ones to come. */
export function toGo(items: GoalItem[]): GoalItem[] {
  return [...items.filter((i) => i.status === "active"), ...items.filter((i) => i.status === "pending")];
}

/** A goal's words as the server keeps them: one line, single spaces. */
const words = (text: string) => text.split(/\s+/).filter(Boolean).join(" ");

/**
 * The edit box's lines, back to the goals `setGoals` takes, against the goals the edit began with
 * (`toGo`, as they were then). A line as a goal was keeps that goal, wherever it moved. The first
 * line changed is the goal under way given other words, when its line is gone; the other changed
 * lines take the goals to come that are left, in order; the rest are new. `dropped`: the goal under
 * way, when its line was taken out (to skip).
 */
export function fromLines(text: string, goals: GoalItem[]): { goals: { id?: string; text: string }[]; dropped: GoalItem | null } {
  const lines = text.split("\n").map(words).filter(Boolean);
  const taken = new Set<string>();
  const ids = lines.map((line) => {
    const g = goals.find((g) => !taken.has(g.id) && words(g.text) === line);
    if (g) taken.add(g.id);
    return g?.id;
  });
  const active = goals.find((g) => g.status === "active");
  if (active && !taken.has(active.id) && lines.length > 0 && ids[0] === undefined) {
    ids[0] = active.id;
    taken.add(active.id);
  }
  const left = goals.filter((g) => g.status !== "active" && !taken.has(g.id));
  for (const [i, id] of ids.entries()) {
    const g = id === undefined ? left.shift() : undefined;
    if (g) {
      ids[i] = g.id;
      taken.add(g.id);
    }
  }
  return {
    goals: lines.map((line, i) => (ids[i] ? { id: ids[i], text: line } : { text: line })),
    dropped: active && !taken.has(active.id) ? active : null,
  };
}

/**
 * Each agent's stack as last shown, against the changes made here: a read asked for before a change
 * here came back may have been answered before the change, so it doesn't replace what the change
 * returned (the next read shows what's new). `clock`: a monotonic time.
 */
export function stackReads(clock: () => number) {
  const last = new Map<string, GoalStack>();
  const changed = new Map<string, number>();
  return {
    /** A read of `agent`'s stack, asked for now: what it gives back is what to show. */
    ask(agent: string): (stack: GoalStack) => GoalStack {
      const at = clock();
      return (stack) => {
        const mine = last.get(agent);
        if (mine && at < (changed.get(agent) ?? -Infinity)) return mine;
        last.set(agent, stack);
        return stack;
      };
    },
    /** A change made here came back with `stack`. */
    changed(agent: string, stack: GoalStack) {
      last.set(agent, stack);
      changed.set(agent, clock());
    },
    /** The stack last shown (none yet: null), for a read that failed. */
    last(agent: string): GoalStack | null {
      return last.get(agent) ?? null;
    },
  };
}
