/** Plans (docs/plans.md): kept, changed, shared and run on any agent. */
import { request } from "./client";

/** The agent a plan came from. */
export interface PlanSource {
  session?: string | null;
  harness?: string | null;
  machine?: string | null;
  cwd?: string | null;
  agent?: string | null;
}

/** How a measure compares with its threshold (crates/kiln/src/plans/criteria.rs `Op`). */
export type CriterionOp = "lt" | "le" | "eq" | "ne" | "ge" | "gt" | "between" | "within" | "rises_at_least" | "falls_at_least" | "rises_at_most" | "falls_at_most";

/** A criterion: in words, or a measure against a threshold. */
export type Criterion =
  | { kind: "qualitative"; text: string }
  | {
      kind: "quantitative";
      /** What's measured ("p99.9 latency"). */
      target: string;
      op: CriterionOp;
      /** The threshold (a number; for rises/falls a percentage). */
      value: string;
      /** between: the upper bound; within: the tolerance. */
      value2?: string;
      unit?: string;
      /** rises/falls: compared with what. */
      baseline?: string;
      /** How it's measured. */
      measured?: string;
    };

/** A test: a scenario, and the criteria it passes by. */
export interface PlanTest {
  scenario: string;
  criteria: Criterion[];
}

/** One step of a plan: a piece of the work, what it's like, what makes it done, how it's tested. */
export interface PlanItem {
  title: string;
  /** What to do (markdown). */
  description: string;
  examples: string;
  /** When it's done. */
  acceptance: Criterion[];
  /** How it's tested. */
  testing: PlanTest[];
}

export const emptyItem = (): PlanItem => ({ title: "", description: "", examples: "", acceptance: [], testing: [] });

export interface Plan {
  id: string;
  title: string;
  /** Markdown; without items, its top-level list items are its steps. */
  body: string;
  /** The work, item by item. */
  items: PlanItem[];
  tags: string[];
  source: PlanSource | null;
  /** Its share link's token, when shared. */
  share: string | null;
  version: number;
  created_at: number;
  updated_at: number;
  steps: string[];
  runs: number;
}

export interface PlanVersion {
  version: number;
  title: string;
  body: string;
  items?: PlanItem[];
  at: number;
  by: string;
}

export interface PlanRun {
  id: string;
  version: number;
  /** The agent it went to (empty for one agent per step). */
  session: string;
  mode: "message" | "steps" | "goals" | "agents";
  at: number;
  by: string;
  /** One agent per step: how the run stands. */
  state?: "running" | "stopped" | "done";
}

/** One agent per step: a run, started. */
export interface PlanStarted {
  run: string;
  steps: number;
  /** Said of how its steps run (asked side by side, they run one at a time for now). */
  note?: string | null;
}

/** Where a step of a run stands. */
export type StepStand = "waiting" | "working" | "needs_you" | "yours" | "met" | "unmet" | "stopped";

/** An agent, as the `agents.*` operations show it. */
export interface AgentBrief {
  id: string;
  name: string;
  kind: string;
  machine: string;
  cwd: string;
  status: string;
  stand: string;
  mode: string;
  summary: string;
  files_changed: number;
  lines_added: number;
  lines_removed: number;
  cost_usd: number;
  doing?: string;
  needs?: { id: string; tool: string; tool_name: string; title: string }[];
}

/** One step of a run of one agent per step. */
export interface RunStep {
  /** Its number, from 1. */
  n: number;
  title: string;
  /** What its agent is held to. */
  goal: string;
  stand: StepStand;
  agent: AgentBrief | null;
  agent_id: string | null;
  started_at: number | null;
  ended_at: number | null;
  reason: string | null;
  tries: number;
}

/** A met step's commit in its run's stack (docs/agentic.md P4). */
export interface StackStep {
  n: number;
  /** `made`: its commit is in the stack; `nothing`: its agent changed no files; `failed`: its files go up with the next step's; `elsewhere`: its agent worked in another folder. */
  state: "made" | "nothing" | "failed" | "elsewhere";
  /** `D12`, `#34`; none until it is open. */
  change: string | null;
  /** Its page, or where to open it. */
  url: string;
  sha: string | null;
  branch: string | null;
  files: string[];
  pushed: boolean;
  error: string | null;
}

/** What a run's met steps are in review: one diff per step, each on the one before. */
export interface RunStack {
  /** Why the run has no stack (its folder isn't in a git repository, …). */
  off: string | null;
  /** `kiln`, `github`, `gitlab`, `bitbucket`, `git` (a remote kiln has no integration with) or `local` (no remote). */
  provider: string;
  repo: string | null;
  trunk: string;
  base: string | null;
  steps: StackStep[];
}

/** A run of one agent per step, as it stands. */
export interface PlanRunView {
  id: string;
  plan: string;
  title: string;
  version: number;
  state: "running" | "stopped" | "done";
  reason: string | null;
  note: string | null;
  at: number;
  ended_at: number | null;
  by: string;
  steps: RunStep[];
  met: number;
  total: number;
  /** Its met steps in review; none until a step is met. */
  stack: RunStack | null;
}

/** A run of one agent per step, as lists show it. */
export interface PlanRunBrief {
  id: string;
  plan: string;
  title: string;
  state: "running" | "stopped" | "done";
  met: number;
  total: number;
  at: number;
}

export interface PlanRan {
  session: string;
  sent: number;
  run: string;
}

export interface SharedPlan {
  title: string;
  body: string;
  items?: PlanItem[];
  steps: string[];
  version: number;
  updated_at: number;
}

/** How a plan runs: as one message, a message a step, a goal a step (the agent's goal stack), or an agent a step. */
export type PlanMode = "message" | "steps" | "goals" | "agents";

export type PlanRunTarget = { session: string } | { new: { target: string; kind: string; cwd?: string } };

const enc = encodeURIComponent;

export const plansApi = {
  list: (signal?: AbortSignal) => request<Plan[]>("GET", "/api/plans", undefined, { signal }),
  get: (id: string, signal?: AbortSignal) => request<Plan>("GET", `/api/plans/${enc(id)}`, undefined, { signal }),
  create: (p: { title?: string; body: string; items?: PlanItem[]; tags?: string[]; source?: PlanSource }) => request<Plan>("POST", "/api/plans", p),
  update: (id: string, patch: { title?: string; body?: string; items?: PlanItem[]; tags?: string[] }) => request<Plan>("PATCH", `/api/plans/${enc(id)}`, patch),
  remove: (id: string) => request<void>("DELETE", `/api/plans/${enc(id)}`),
  versions: (id: string, signal?: AbortSignal) => request<PlanVersion[]>("GET", `/api/plans/${enc(id)}/versions`, undefined, { signal }),
  runs: (id: string, signal?: AbortSignal) => request<PlanRun[]>("GET", `/api/plans/${enc(id)}/runs`, undefined, { signal }),
  restore: (id: string, version: number) => request<Plan>("POST", `/api/plans/${enc(id)}/restore`, { version }),
  run: (id: string, target: PlanRunTarget, mode: Exclude<PlanMode, "agents">) => request<PlanRan>("POST", `/api/plans/${enc(id)}/run`, { ...target, mode }),
  /** One agent per step, on the machine and harness given, one after another. */
  runAgents: (id: string, to: { target: string; kind: string; cwd?: string }) => request<PlanStarted>("POST", `/api/plans/${enc(id)}/run`, { new: to, mode: "agents" }),
  /** Runs of one agent per step (`active`: those under way). */
  agentRuns: (active: boolean, signal?: AbortSignal) => request<{ runs: PlanRunBrief[] }>("GET", `/api/plans/runs${active ? "?active=1" : ""}`, undefined, { signal }),
  runView: (run: string, signal?: AbortSignal) => request<PlanRunView>("GET", `/api/plans/runs/${enc(run)}`, undefined, { signal }),
  stopRun: (run: string) => request<PlanRunView>("POST", `/api/plans/runs/${enc(run)}/stop`),
  /** Retry a step with a new agent; take it over (kiln stops holding its agent to it); or say it's met. */
  step: (run: string, n: number, act: "retry" | "take" | "met") => request<PlanRunView>("POST", `/api/plans/runs/${enc(run)}/steps/${n}/${act}`),
  share: (id: string, on: boolean) => request<Plan>("POST", `/api/plans/${enc(id)}/share`, { on }),
  shared: (token: string, signal?: AbortSignal) => request<SharedPlan>("GET", `/api/shared/plans/${enc(token)}`, undefined, { signal }),
};

export const planHref = (id: string) => `/agents/plans/${enc(id)}`;
export const runHref = (run: string) => `/agents/plans/runs/${enc(run)}`;
/** A shared plan's link, through this address or another of the server's (`https://kiln.dev`, …). */
export const shareUrl = (token: string, base = window.location.origin) => `${base.replace(/\/+$/, "")}/shared/plans/${enc(token)}`;
