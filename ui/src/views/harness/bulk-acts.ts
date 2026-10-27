/**
 * What can be done at once to the agents selected in Mission control: each of one agent's own
 * actions (its row's buttons, its menu: actions.tsx) that some of them take, and which of them take
 * it, by the rules one agent goes by; where they can move; what ending them ends with them; and how
 * it went, in a line. Kept apart to be tested.
 */
import type { AgentInfo, GoalStack, KindInfo } from "@/api/harness";
import type { ReachRunner } from "@/api/types";
import { startWords } from "./goal-stack";
import { movable } from "./move";

/** "1 agent", "3 agents". */
export const count = (n: number, one: string, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

/** Each selected agent's goal stack as read (null: it couldn't be read); an agent not in it isn't read yet. */
export type Stacks = ReadonlyMap<string, GoalStack | null>;

/** Which of the agents each action takes. */
export interface Acts {
  /** Paused at a usage limit: Resume ends the pause, as its row's Resume does. */
  atLimit: AgentInfo[];
  /** Goals paused part-way (by you, by the agent, or for you to look at a plan): Resume runs them on. */
  goalsPaused: AgentInfo[];
  /** Goals running: Pause pauses them, as the goals panel's Pause does. */
  goalsRunning: AgentInfo[];
  /** A turn under way: Stop stops it (its menu's Stop the current turn). */
  working: AgentInfo[];
  /** Stopped or ended: Restart, Remove. */
  exited: AgentInfo[];
  /** Not ended: End. */
  live: AgentInfo[];
  /** Can go on on another machine: Move to. */
  movable: AgentInfo[];
  /** On a runner: where its commands run can change. */
  onRunner: AgentInfo[];
}

export function actsOf(agents: readonly AgentInfo[], stacks?: Stacks): Acts {
  return {
    atLimit: agents.filter((a) => Boolean(a.pause) && a.status !== "running" && a.status !== "exited" && a.status !== "disconnected"),
    // Goals it paused for you show on its row before any stack is read.
    goalsPaused: agents.filter((a) => {
      const s = stacks?.get(a.id);
      return Boolean(a.held) || (s ? startWords(s) === "Resume" : false);
    }),
    goalsRunning: agents.filter((a) => stacks?.get(a.id)?.state === "running"),
    working: agents.filter((a) => a.status === "running" || a.status === "waiting"),
    exited: agents.filter((a) => a.status === "exited"),
    live: agents.filter((a) => a.status !== "exited"),
    movable: agents.filter(movable),
    onRunner: agents.filter((a) => a.target.startsWith("agent:")),
  };
}

/** Resume's agents: those at a limit and those whose goals are paused, each once. */
export const resumable = (acts: Acts): AgentInfo[] => [...new Map([...acts.atLimit, ...acts.goalsPaused].map((a) => [a.id, a])).values()];

/** A machine some of the agents can move to, and which of them can. */
export interface Place {
  target: string;
  name: string;
  platform: string;
  takes: AgentInfo[];
}

export interface Places {
  /** By name. */
  places: Place[];
  /** Why an agent can go nowhere, by agent. */
  stuck: ReadonlyMap<string, string>;
  /** Connected machines that have one of their harnesses but run a kiln too old to take agents, or take no work from other runners. */
  older: string[];
  closed: string[];
}

/**
 * Where the agents that can move can go, as one agent's Move to… has it: a connected machine other
 * than its own that has its harness and takes work from other runners, unless its own runs a kiln
 * too old to hand it over.
 */
export function placesFor(agents: readonly AgentInfo[], byTarget: Record<string, KindInfo[]>, runners: readonly ReachRunner[]): Places {
  const places = new Map<string, Place>();
  const stuck = new Map<string, string>();
  const older = new Set<string>();
  const closed = new Set<string>();
  for (const a of agents.filter(movable)) {
    const here = runners.find((r) => `agent:${r.name}` === a.target);
    if (here && !here.fingerprint) {
      stuck.set(a.id, `${here.name} runs an older kiln that can't hand agents over`);
      continue;
    }
    const others = runners.filter((r) => r.connected && `agent:${r.name}` !== a.target && (byTarget[`agent:${r.name}`] ?? []).some((k) => k.id === a.kind && k.available));
    for (const r of others) {
      if (!r.fingerprint) older.add(r.name);
      else if (!r.reach) closed.add(r.name);
      if (!r.reach) continue;
      const target = `agent:${r.name}`;
      const p = places.get(target) ?? { target, name: r.name, platform: `${r.os}/${r.arch}`, takes: [] };
      p.takes.push(a);
      places.set(target, p);
    }
    if (!others.some((r) => r.reach)) stuck.set(a.id, `no other connected machine that takes agents has ${a.kind_name}`);
  }
  return { places: [...places.values()].sort((x, y) => x.name.localeCompare(y.name)), stuck, older: [...older].sort(), closed: [...closed].sort() };
}

/** Why agent `a`, one of those that can move, doesn't go to `place`; undefined when it goes. */
export function whyNot(a: AgentInfo, place: Place, plan: Places): string | undefined {
  if (place.takes.some((t) => t.id === a.id)) return undefined;
  if (a.target === place.target) return `it's on ${place.name} already`;
  return plan.stuck.get(a.id) ?? `${place.name} doesn't have ${a.kind_name}`;
}

/** The agents those of `ids` started (and theirs), still running, that aren't among them: ending them ends these too, unless kept. */
export function startedBy(ids: ReadonlySet<string>, all: readonly AgentInfo[]): AgentInfo[] {
  const reached = new Set(ids);
  const out: AgentInfo[] = [];
  let frontier = new Set(ids);
  while (frontier.size > 0) {
    const next = new Set<string>();
    for (const a of all) {
      if (!a.parent || !frontier.has(a.parent) || reached.has(a.id)) continue;
      reached.add(a.id);
      next.add(a.id);
      if (a.status !== "exited") out.push(a);
    }
    frontier = next;
  }
  return out;
}

/** Those of `ids` none of whose forebears is among them: ending one with the agents it started ends the rest. */
export function topmost(ids: readonly string[], all: readonly AgentInfo[]): string[] {
  const among = new Set(ids);
  const parent = new Map(all.map((a) => [a.id, a.parent]));
  return ids.filter((id) => {
    const seen = new Set<string>([id]);
    for (let p = parent.get(id); p && !seen.has(p); p = parent.get(p)) {
      if (among.has(p)) return false;
      seen.add(p);
    }
    return true;
  });
}

/** How it went for one agent. */
export interface Outcome {
  id: string;
  name: string;
  machine: string;
  result: "done" | "skipped" | "failed";
  reason?: string;
}

/** How it went, in a line: `done` says what was done to how many ("Resumed 2 agents"), then how many were skipped or failed. */
export function outcomeLine(done: (agents: string) => string, outcomes: readonly Outcome[]): string {
  const n = (r: Outcome["result"]) => outcomes.filter((o) => o.result === r).length;
  const [ok, skipped, failed] = [n("done"), n("skipped"), n("failed")];
  return [ok > 0 ? done(count(ok, "agent")) : "", skipped > 0 ? `${skipped} skipped` : "", failed > 0 ? `${failed} failed` : ""].filter(Boolean).join(" · ") || "Nothing to do";
}
