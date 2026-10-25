/**
 * What the goal stack panel (goals.tsx) works out, kept apart to be tested: the goals its edit box
 * covers and how the box's lines go back to them, which reads of a stack came back too late to
 * show, which goals can become plans, and where a planned goal's steps stand.
 */
import type { GoalItem, GoalStack, HEvent, QueueItem, Task, ToolCall } from "@/api/harness";
import type { Stand } from "@/views/plans/status";
import { taskLive, type ToolEvent } from "./derive";
import { isSubagent } from "./tasks";

/** How long kiln's ask to plan a goal now stands, its subagent not started yet (the server's `DISPATCH_AGAIN_MS`). */
const ASK_STANDS_MS = 30 * 60_000;
/** kiln's ask to plan a goal now: a steer, its id so (harness/pause.rs `KILN_ID`, goals/ops.rs `dispatch`). */
const PLAN_ASK = "kiln-plan-";

/**
 * A goal's planning, as its agent's session shows it (Make a plan, Plan it now): kiln asked the
 * agent to send a subagent (`asked`, none started yet), a subagent of its is at it (`planning`, its
 * latest calls; `wrote`: it has written the plan's steps), or one was and ended with the plan not
 * ready (`ended`).
 */
export type Planner = { kind: "asked"; at: number } | { kind: "planning" | "ended"; task: Task; calls: ToolEvent[]; wrote: boolean };

/**
 * Who's planning plan `plan` in its agent's session: its subagent (the one its brief, which names the
 * plan, went to; the latest), with its last `shown` calls and whether it has written the steps; else kiln's ask, while it stands, said
 * or still in the agent's queue. A subagent that ended having said the plan is ready: none (the
 * stack shows its steps next).
 */
export function plannerOf(plan: string, tasks: readonly Task[], events: readonly HEvent[], queue: readonly QueueItem[], now: number, shown = 1): Planner | null {
  let task: Task | undefined;
  for (const t of tasks) if (isSubagent(t) && t.prompt?.includes(plan) && (!task || t.started > task.started)) task = t;
  if (task) {
    const live = taskLive(task);
    const calls: ToolEvent[] = [];
    let wrote = false;
    let ready = false;
    for (let i = events.length - 1; i >= 0 && !(calls.length >= shown && wrote && (live || ready)); i--) {
      const e = events[i];
      if (e?.kind !== "tool" || e.parent === undefined || e.parent !== task.tool_id) continue;
      if (calls.length < shown) calls.unshift(e);
      if (e.server === "kiln" && e.status === "done" && e.tool_short === "plans_update") wrote = true;
      if (e.server === "kiln" && e.status === "done" && e.tool_short === "plans_ready") ready = true;
    }
    if (!live && ready) return null;
    return { kind: live ? "planning" : "ended", task, calls, wrote };
  }
  const asked = (id: string | undefined, text: string) => !!id?.startsWith(PLAN_ASK) && text.includes(plan);
  for (const q of queue) if (asked(q.id, q.text) && q.at >= now - ASK_STANDS_MS) return { kind: "asked", at: q.at };
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    if (!e || e.at < now - ASK_STANDS_MS) break;
    if (e.kind === "user" && asked(e.client_id, e.text)) return { kind: "asked", at: e.at };
  }
  return null;
}

/** kiln's own tools, as a planner's call says what it did (`going`: it's at it now). */
const KILN_SAYS: Record<string, (input: Record<string, unknown>, going: boolean) => string> = {
  research_add: (input, going) => {
    const found = Array.isArray(input.findings) ? (input.findings as { claim?: unknown }[]) : [];
    const first = typeof found[0]?.claim === "string" ? `: ${found[0].claim}` : "";
    return `${going ? "Adding" : "Added"} ${found.length === 1 ? "a finding" : `${found.length || "its"} findings`}${first}`;
  },
  research_get: () => "Read its research",
  research_start: (_, going) => (going ? "Starting its research" : "Started its research"),
  plans_get: () => "Read the plan",
  plans_update: (input, going) => {
    const steps = Array.isArray(input.items) ? (input.items as { title?: unknown }[]) : [];
    const titles = steps.map((s) => (typeof s.title === "string" ? s.title : "")).filter(Boolean);
    return steps.length
      ? `${going ? "Writing" : "Wrote"} ${steps.length === 1 ? "a step" : `${steps.length} steps`}${titles.length ? `: ${titles.join(" · ")}` : ""}`
      : going
        ? "Changing the plan"
        : "Changed the plan";
  },
  plans_ready: (_, going) => (going ? "Saying the plan is ready" : "Said the plan is ready"),
};

/** A call, in a line of plain words ("Reading crates/x.rs", "Added a finding: …"): what a planner's last calls say. */
export function callWords(c: Pick<ToolCall, "tool" | "name" | "title" | "status" | "server" | "tool_short" | "input">): string {
  const going = c.status === "running" || c.status === "pending";
  const kiln = c.server === "kiln" ? KILN_SAYS[c.tool_short ?? ""] : undefined;
  if (kiln) return kiln(c.input && typeof c.input === "object" ? (c.input as Record<string, unknown>) : {}, going);
  const t = c.title;
  switch (c.tool) {
    case "read":
      return `${going ? "Reading" : "Read"} ${t}`;
    case "search":
      return `${going ? "Searching" : "Searched"} for ${t}`;
    case "web":
      return c.name === "WebSearch" ? `${going ? "Searching" : "Searched"} the web for “${t}”` : `${going ? "Reading" : "Read"} ${t}`;
    case "shell":
      return `${going ? "Running" : "Ran"} ${t}`;
    case "edit":
      return `${going ? "Editing" : "Edited"} ${t}`;
    case "agent":
      return `${going ? "Handing" : "Handed"} on: ${t}`;
    default:
      return t ? `${c.tool_short ?? c.name}: ${t}` : (c.tool_short ?? c.name);
  }
}

/** The goal that stopped a paused stack with none under way (the last to end, not met or skipped), to try again first. */
/** One of its own goals still to go, not a part, a plan's step or planned already: it can become a plan (Make a plan). */
export function canPlan(g: GoalItem): boolean {
  return (g.status === "pending" || g.status === "active") && !g.part_of && !g.planned_as && !g.step && !g.waits_on;
}

/** A goal still to go that's being planned, its planning waiting its turn: a subagent of its agent can plan it now (Plan it now). */
export function canPlanNow(g: GoalItem, items: readonly GoalItem[]): boolean {
  return g.status === "pending" && !!g.planned_as && items.some((i) => i.part_of === g.id && i.part_kind === "planning" && i.status === "pending");
}

const PART_STAND = { pending: "waiting", active: "working", met: "met", unmet: "unmet", skipped: "skipped" } as const;

/**
 * Where step `n` of the plan `goal` became stands, as its part on the stack says: under way while it
 * is, while one of its own parts (its criteria and tests) is, or while a subagent has it (side by side
 * with the plan's other steps); none when it isn't on the stack.
 */
export function stepStand(goal: GoalItem, items: readonly GoalItem[], n: number, paused: boolean): Stand | undefined {
  const part = items.find((i) => i.part_of === goal.id && i.part_kind === "plan" && i.step?.n === n);
  if (!part) return undefined;
  const busy = part.status === "active" || (part.status === "pending" && (!!part.subagent || items.some((i) => i.part_of === part.id && i.status === "active")));
  if (busy) return paused ? "paused" : "working";
  return PART_STAND[part.status];
}

/** A goal still to go that a subagent has: a plan's step, side by side with the plan's others (its subagent's id). */
export function outOn(g: GoalItem): string | undefined {
  return g.status === "pending" ? g.subagent : undefined;
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
