/** The CI engine (docs/ci.md §4.1): the /api/ci routes the run, leg and editor pages read. */
import { ApiError, request } from "./client";

const enc = encodeURIComponent;
const at = (attempt?: number) => (attempt ? `?attempt=${attempt}` : "");

export interface CiRun {
  id: string;
  number: number;
  repo: string;
  workflow: string;
  name: string;
  run_name: string | null;
  event: string;
  branch: string;
  sha: string;
  actor: string;
  status: string;
  conclusion: string | null;
  trust: string;
  /** The current attempt (W05-R17). */
  attempt: number;
  created_at: number;
  started_at: number | null;
  finished_at: number | null;
  error: string | null;
}

export interface CiLeg {
  id: string;
  key: string;
  name: string | null;
  job: string | null;
  matrix: Record<string, unknown> | null;
  status: string;
  conclusion: string | null;
  runner: string | null;
  executor: string | null;
  attempt: number;
  wait_reason: unknown;
  queued_at: number | null;
  started_at: number | null;
  finished_at: number | null;
  error: string | null;
  needs: string[] | null;
  exit_code: number | null;
  /** A leg a rerun kept: the leg whose result it keeps. */
  reused_from: string | null;
  /** A failed leg kept to look into, until when (W17-R02). */
  kept_until?: number | null;
}

export interface CiRunDetail {
  run: CiRun;
  legs: CiLeg[];
  /** What the run pinned when it started (W07-R02). */
  actions: { uses: string; repo: string | null; sha: string | null }[];
}

export interface CiGraphLeg {
  id: string;
  key: string;
  name: string | null;
  matrix: Record<string, unknown> | null;
  status: string;
  conclusion: string | null;
  wait_reason: unknown;
  reused: boolean;
}

export interface CiGraph {
  nodes: { job: string; legs: CiGraphLeg[]; started_at: number | null; finished_at: number | null }[];
  edges: { from: string; to: string }[];
  critical_path: string[];
}

export interface CiAnnotation {
  leg: string;
  leg_key: string;
  step: number | null;
  level: "error" | "warning" | "notice" | string;
  title: string | null;
  message: string;
  file: string | null;
  line: number | null;
  end_line: number | null;
  col: number | null;
  end_col: number | null;
}

/** A test's result in a run (W16-R03). */
export interface CiTest {
  id: string;
  suite: string;
  name: string;
  file: string | null;
  line: number | null;
  leg: string;
  leg_key: string;
  attempt: number;
  outcome: "passed" | "failed" | "skipped" | "error";
  duration_ms: number | null;
  message: string | null;
  trace: string | null;
  retried: number;
  quarantined: boolean;
  flaky: { class: "flaky" | "probably_flaky"; why: string } | null;
}

export interface CiSummary {
  leg: string;
  leg_key: string;
  step: number;
  markdown: string;
}

export interface CiArtifact {
  id: number;
  name: string;
  size: number | null;
  digest: string | null;
  created_at: number;
  expires_at: number | null;
  leg: string | null;
  expired: boolean;
}

export interface CiCheck {
  requirement: string;
  value: string;
  pass: boolean;
}

export interface CiRunnerFit {
  runner: string;
  name: string;
  connected: boolean;
  eligible: boolean;
  room: boolean;
  score: number | null;
  checks: CiCheck[];
}

export interface CiRouting {
  leg: string;
  known: boolean;
  wait_reason: unknown;
  runners: CiRunnerFit[];
}

export interface CiDiagnostic {
  code: string;
  severity: "error" | "warning" | "info" | string;
  message: string;
  path: string;
  /** Lines and columns from 1; offsets (bytes) aren't always there. */
  range: { start: { line: number; col: number; offset?: number }; end: { line: number; col: number; offset?: number } };
}

export interface CiPreview {
  repo: string;
  ref: string;
  sha: string;
  path: string;
  event?: string;
  runs: boolean;
  why: string | null;
  policy?: string[];
  diagnostics: CiDiagnostic[];
  plan: {
    name: string | null;
    legs: {
      key: string;
      job: string;
      name: string;
      matrix: Record<string, unknown>;
      runs_on: { labels: string[] };
      routing?: { labels: string[]; image: string | null; executors: string[]; runners: CiRunnerFit[] };
    }[];
    decided: { job: string; leg: string | null; conclusion: string; reason: string }[];
    removed: Record<string, [unknown, string][]>;
    later: string[];
    error?: string;
  } | null;
}

export type RerunMode = "all" | "failed" | "job";

/** What validation sees of a workflow (W02-R12): what the editor completes from. */
export interface CiOutline {
  name: string | null;
  events: string[];
  inputs: string[];
  env: string[];
  jobs: {
    id: string;
    name: string | null;
    needs: string[];
    runs_on: string[];
    uses: string | null;
    legs_estimate: number | string;
    steps: number;
    step_ids: string[];
    matrix_axes: string[];
    outputs: string[];
  }[];
}

export interface CiChecked {
  kind: string;
  dialect: string;
  diagnostics: CiDiagnostic[];
  outline: CiOutline | null;
}

export interface CiWorkflowFile {
  path: string;
  dialect: string;
  name: string;
  runs: boolean;
  errors: number;
  warnings: number;
}

export interface CiWorkflowSource {
  path: string;
  ref: string;
  sha: string;
  source: string;
  diagnostics: CiDiagnostic[];
  outline: CiOutline | null;
}

export interface CiCatalogEntry {
  uses: string;
  versions: { ref: string; sha: string }[];
  action: {
    name: string | null;
    description: string | null;
    using: string;
    inputs: { name: string; description: string | null; required: boolean; default: string | null }[];
    outputs: { name: string; description: string | null }[];
  } | null;
}

/** A CI run, or null when the run isn't the engine's (a v1 pipeline's, or gone). */
export async function ciRun(id: string, attempt: number | undefined, signal?: AbortSignal): Promise<CiRunDetail | null> {
  try {
    return await request<CiRunDetail>("GET", `/api/ci/runs/${enc(id)}${at(attempt)}`, undefined, { signal });
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return null;
    throw e;
  }
}

export const ci = {
  graph: (id: string, attempt: number | undefined, signal?: AbortSignal) => request<CiGraph>("GET", `/api/ci/runs/${enc(id)}/graph${at(attempt)}`, undefined, { signal }),
  annotations: (id: string, attempt: number | undefined, signal?: AbortSignal) =>
    request<{ annotations: CiAnnotation[] }>("GET", `/api/ci/runs/${enc(id)}/annotations${at(attempt)}`, undefined, { signal }),
  summaries: (id: string, attempt: number | undefined, signal?: AbortSignal) => request<{ summaries: CiSummary[] }>("GET", `/api/ci/runs/${enc(id)}/summary${at(attempt)}`, undefined, { signal }),
  artifacts: (id: string, signal?: AbortSignal) => request<{ artifacts: CiArtifact[] }>("GET", `/api/ci/runs/${enc(id)}/artifacts`, undefined, { signal }),
  /** The run's tests, failures first (W16-R03). */
  tests: (id: string, attempt: number | undefined, signal?: AbortSignal) =>
    request<{ counts: Record<string, number>; tests: CiTest[] }>("GET", `/api/ci/runs/${enc(id)}/tests${at(attempt)}`, undefined, { signal }),
  rerun: (id: string, mode: RerunMode, job: string | null, debug: boolean, keepFailed = false) =>
    request<{ run: string; attempt: number }>("POST", `/api/ci/runs/${enc(id)}/rerun`, { mode, job, debug, keep_failed: keepFailed }),
  /** A review of the run's deployments waiting on their environments (W15-R04). */
  approve: (id: string, environments: string[], state: "approved" | "rejected", comment: string) =>
    request<{ run: string; environments: string[]; state: string }>("POST", `/api/ci/runs/${enc(id)}/approve`, { environments, state, comment }),
  cancel: (id: string) => request<void>("POST", `/api/ci/runs/${enc(id)}/cancel`, {}),
  /** A kept leg kept `minutes` more from now, or let go (W17-R02). */
  keep: (leg: string, minutes: number) => request<{ leg: string; kept_until: number }>("POST", `/api/ci/legs/${enc(leg)}/keep`, { minutes }),
  release: (leg: string) => request<void>("POST", `/api/ci/legs/${enc(leg)}/release`, {}),
  /** A kept leg's step run again in place, as a new section of its log (W17-R04). */
  rerunStep: (leg: string, idx: number) => request<{ leg: string; step: number; idx: number }>("POST", `/api/ci/legs/${enc(leg)}/rerun-step`, { idx }),
  routing: (leg: string, signal?: AbortSignal) => request<CiRouting>("GET", `/api/ci/legs/${enc(leg)}/routing`, undefined, { signal }),
  validate: (path: string, content: string, signal?: AbortSignal) => request<CiChecked>("POST", `/api/ci/validate`, { path, content }, { signal }),
  workflows: (repo: string, ref: string, signal?: AbortSignal) =>
    request<{ ref: string; sha: string; workflows: CiWorkflowFile[] }>("GET", `/api/ci/repos/${enc(repo)}/workflows${ref ? `?ref=${enc(ref)}` : ""}`, undefined, { signal }),
  workflow: (repo: string, path: string, ref: string, signal?: AbortSignal) =>
    request<CiWorkflowSource>("GET", `/api/ci/repos/${enc(repo)}/workflows/${path.split("/").map(enc).join("/")}${ref ? `?ref=${enc(ref)}` : ""}`, undefined, { signal }),
  names: (repo: string, path: string, signal?: AbortSignal) => request<{ secrets: string[]; variables: string[] }>("GET", `/api/ci/repos/${enc(repo)}/names?path=${enc(path)}`, undefined, { signal }),
  catalog: (q: string, signal?: AbortSignal) => request<{ actions: CiCatalogEntry[] }>("GET", `/api/ci/actions?q=${enc(q)}`, undefined, { signal }),
  preview: (body: { repo: string; ref?: string; sha?: string; path?: string; content?: string; event?: string | { name: string; inputs?: Record<string, string> } }) =>
    request<CiPreview>("POST", `/api/ci/plan-preview`, body),
};

/** How a leg or job concluded, as the status dots know it. */
export function ciState(status: string, conclusion: string | null): string {
  if (conclusion === "success") return "succeeded";
  if (conclusion === "failure") return "failed";
  if (conclusion === "cancelled") return "cancelled";
  if (conclusion === "skipped") return "skipped";
  if (conclusion === "timed_out") return "timed_out";
  return status;
}

/** A wait reason (W05-R13) as one sentence. */
export function waitSentence(w: unknown): string | null {
  if (!w || typeof w !== "object") return typeof w === "string" ? w : null;
  const o = w as {
    reason?: string;
    eligible?: number;
    busy?: number;
    draining?: number;
    offline?: string[];
    failing?: string[];
    runners?: string[];
    group?: string;
    holders?: string[];
    limit?: number;
    at?: number;
    environment?: string;
    reviewers?: string[];
    until?: number | null;
  };
  const list = (xs?: string[]) => (xs && xs.length ? `: ${xs.join(", ")}` : "");
  switch (o.reason) {
    case "capacity":
      return `Waiting for a free runner: ${o.eligible ?? 0} can take it, ${o.busy ?? 0} busy${o.draining ? `, ${o.draining} draining` : ""}.`;
    case "no-runner":
      return `No runner that can take it is connected${list(o.offline)}.`;
    case "unroutable":
      return `No runner meets its requirements${list(o.failing)}.`;
    case "capability":
      return `The runners that match need a newer kiln${list(o.runners)}.`;
    case "concurrency":
      return `Waiting for concurrency group ${o.group ?? ""}${o.holders?.length ? `, held by ${o.holders.join(", ")}` : ""}.`;
    case "quota":
      return `The repository's limit of ${o.limit ?? 0} running legs is reached.`;
    case "exclusive":
      return "Waiting to run alone on a runner.";
    case "retry":
      return `Retrying at ${o.at ? new Date(o.at * 1000).toLocaleTimeString() : "a moment"}.`;
    case "approval":
      return `Waiting for a review to deploy to ${o.environment ?? "its environment"}${o.reviewers?.length ? ` (${o.reviewers.join(", ")})` : ""}${o.until ? `, and until ${new Date(o.until * 1000).toLocaleTimeString()}` : ""}.`;
    case "wait-timer":
      return `Waiting until ${o.until ? new Date(o.until * 1000).toLocaleTimeString() : "its wait timer ends"} to deploy to ${o.environment ?? "its environment"}.`;
    default:
      return o.reason ? o.reason.replace(/-/g, " ") : null;
  }
}

/** The environment a leg waits for a review to deploy to, if it does. */
export function reviewOf(w: unknown): string | null {
  if (!w || typeof w !== "object") return null;
  const o = w as { reason?: string; environment?: string; reviewers?: string[] };
  return o.reason === "approval" && o.environment && (o.reviewers?.length ?? 0) > 0 ? o.environment : null;
}
