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

export interface Plan {
  id: string;
  title: string;
  /** Markdown; its top-level list items are its steps. */
  body: string;
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
  at: number;
  by: string;
}

export interface PlanRun {
  id: string;
  version: number;
  session: string;
  mode: "message" | "steps" | "goals";
  at: number;
  by: string;
}

export interface PlanRan {
  session: string;
  sent: number;
  run: string;
}

export interface SharedPlan {
  title: string;
  body: string;
  steps: string[];
  version: number;
  updated_at: number;
}

/** How a plan runs: as one message, a message a step, or a goal a step (the agent's goal stack). */
export type PlanMode = "message" | "steps" | "goals";

export type PlanRunTarget = { session: string } | { new: { target: string; kind: string; cwd?: string } };

const enc = encodeURIComponent;

export const plansApi = {
  list: (signal?: AbortSignal) => request<Plan[]>("GET", "/api/plans", undefined, { signal }),
  get: (id: string, signal?: AbortSignal) => request<Plan>("GET", `/api/plans/${enc(id)}`, undefined, { signal }),
  create: (p: { title?: string; body: string; tags?: string[]; source?: PlanSource }) => request<Plan>("POST", "/api/plans", p),
  update: (id: string, patch: { title?: string; body?: string; tags?: string[] }) => request<Plan>("PATCH", `/api/plans/${enc(id)}`, patch),
  remove: (id: string) => request<void>("DELETE", `/api/plans/${enc(id)}`),
  versions: (id: string, signal?: AbortSignal) => request<PlanVersion[]>("GET", `/api/plans/${enc(id)}/versions`, undefined, { signal }),
  runs: (id: string, signal?: AbortSignal) => request<PlanRun[]>("GET", `/api/plans/${enc(id)}/runs`, undefined, { signal }),
  restore: (id: string, version: number) => request<Plan>("POST", `/api/plans/${enc(id)}/restore`, { version }),
  run: (id: string, target: PlanRunTarget, mode: PlanMode) => request<PlanRan>("POST", `/api/plans/${enc(id)}/run`, { ...target, mode }),
  share: (id: string, on: boolean) => request<Plan>("POST", `/api/plans/${enc(id)}/share`, { on }),
  shared: (token: string, signal?: AbortSignal) => request<SharedPlan>("GET", `/api/shared/plans/${enc(token)}`, undefined, { signal }),
};

export const planHref = (id: string) => `/agents/plans/${enc(id)}`;
/** A shared plan's link, through this address or another of the server's (`https://kiln.dev`, …). */
export const shareUrl = (token: string, base = window.location.origin) => `${base.replace(/\/+$/, "")}/shared/plans/${enc(token)}`;
