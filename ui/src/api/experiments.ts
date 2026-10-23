/** Experiments (docs/ci.md W20-R11): options measured against each other and together. */
import type { Res } from "@/lib/data";
import { request } from "./client";

const enc = encodeURIComponent;

/** A 95% interval of a variant's median over the baseline's, and its point estimate. */
export interface ExRatio {
  low: number;
  mid: number;
  high: number;
}

export interface ExMetric {
  name: string;
  better: "lower" | "higher";
  median: number | null;
  samples: number;
  vs_baseline?: ExRatio;
  verdict?: "improved" | "regressed" | "no clear difference";
  meets?: boolean;
}

export interface ExVariantResult {
  name: string;
  options: string[];
  repetitions: number;
  /** Each required pass/fail criterion: its passes of its repetitions. */
  checks: Record<string, [number, number]>;
  passes: boolean;
  metrics: ExMetric[];
}

export interface ExResults {
  variants: ExVariantResult[];
  effects: { option: string; metric: string; gain: number }[];
  interactions: { a: string; b: string; metric: string; interaction: number; kind: "synergistic" | "antagonistic" | "additive" }[];
  recommendation?: string;
  why: string;
}

export interface ExBuilt {
  name: string;
  options: string[];
  commit?: string;
  state: "building" | "gluing" | "ready" | "conflict" | "measured";
  why?: string;
  /** The agent combining its options' conflicting changes, and the diff of what it made. */
  agent?: string;
  glue?: string;
  minutes: number;
}

export interface ExRun {
  variant: string;
  round: number;
  criterion: string;
  run?: string;
  conclusion?: string;
  why?: string;
  metrics: Record<string, number>;
}

export interface ExCriterion {
  name: string;
  kind: "pipeline" | "command" | "metric";
  workflow?: string;
  command?: string;
  metric?: string;
  better?: "lower" | "higher";
  required: boolean;
  threshold?: { at_most?: number; at_least?: number; better?: number };
}

export interface Experiment {
  id: string;
  repo: string;
  base: string;
  branch: string;
  question?: string;
  agent?: string;
  options: { name: string; given: string; from: string; to: string; implement?: string; agent?: string; diff?: string; why?: string }[];
  strategy?: string | { custom: string[][] };
  baseline: boolean;
  criteria: ExCriterion[];
  repetitions: number;
  max_parallel: number;
  budget_minutes?: number;
  decide: "ask" | "auto";
  state: "implementing" | "building" | "measuring" | "done" | "cancelled" | "failed";
  why?: string;
  variants: ExBuilt[];
  runs: ExRun[];
  results?: ExResults;
  decision?: { variant: string; commit: string; by: string; at: number };
  by: string;
  created_at: number;
}

export interface ExperimentRow {
  id: string;
  repo: string;
  question?: string;
  state: Experiment["state"];
  variants: number;
  measured: number;
  runs: number;
  recommendation?: string | null;
  decision?: Experiment["decision"] | null;
  by: string;
  created_at: number;
}

/** Where an experiment from an agent's question runs: its folder's repository and the workflows that can measure one. */
export interface ExperimentWhere {
  repo: string;
  branch: string;
  workflows: string[];
  may: boolean;
}

export const experimentsApi = {
  start: (body: Record<string, unknown>) => request<{ experiment: Experiment }>("POST", "/api/experiments", body).then((r) => r.experiment),
  where: (agent: string, signal?: AbortSignal) => request<ExperimentWhere>("GET", `/api/experiments/where?agent=${enc(agent)}`, undefined, { signal }),
  list: (signal?: AbortSignal) => request<{ experiments: ExperimentRow[] }>("GET", "/api/experiments", undefined, { signal }).then((r) => r.experiments),
  get: (id: string, signal?: AbortSignal) => request<{ experiment: Experiment }>("GET", `/api/experiments/${enc(id)}`, undefined, { signal }).then((r) => r.experiment),
  decide: (id: string, variant: string) => request<{ experiment: Experiment }>("POST", `/api/experiments/${enc(id)}/decide`, { variant }).then((r) => r.experiment),
  cancel: (id: string) => request<{ experiment: Experiment }>("POST", `/api/experiments/${enc(id)}/cancel`, {}).then((r) => r.experiment),
};

export const experimentsRes: Res<ExperimentRow[]> = { key: "experiments", fetch: (s) => experimentsApi.list(s), opts: { on: ["experiment"] } };
export const experimentRes = (id: string): Res<Experiment> => ({ key: `experiments/${id}`, fetch: (s) => experimentsApi.get(id, s), opts: { on: ["experiment"] } });
export const experimentWhereRes = (agent: string): Res<ExperimentWhere | null> => ({ key: `experiments/where/${agent}`, fetch: (s) => experimentsApi.where(agent, s).catch(() => null) });
