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
  /** W18-R06: `required` while an outside contributor's run waits for a maintainer, then
   * `approved` or `rejected`. */
  approval?: "required" | "approved" | "rejected" | null;
  approved_by?: string | null;
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
  /** The run whose leg it took over (this one, for an earlier attempt's), and its number (W08-R10). */
  reused_run?: string | null;
  reused_number?: number | null;
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

/** A benchmark metric of a run, compared with its baseline (W16-R07, R08). */
export interface CiMetric {
  leg: string;
  leg_key: string;
  step: number | null;
  name: string;
  value: number;
  unit: string;
  better: "lower" | "higher";
  fingerprint: string;
  /** `change` is how much worse (+) or better (−) than the baseline, a fraction; `low`/`high` its 95% interval. */
  comparison: { verdict: "regressed" | "improved" | "no_clear_difference"; change: number; low: number; high: number; runs: number } | null;
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
/** A rollup as the insights API reads it (W19-R01): counts, rates, percentiles (seconds), resources. */
export interface CiInsight {
  count: number;
  success: number;
  failure: number;
  cancelled: number;
  skipped: number;
  flaky: number;
  retried: number;
  success_rate: number | null;
  flaky_rate: number | null;
  p50: number | null;
  p90: number | null;
  p99: number | null;
  mean: number | null;
  queue_p50: number | null;
  queue_p90: number | null;
  queue_seconds: number;
  run_seconds: number;
  cpu_seconds: number;
  peak_rss: number;
  runner_minutes: number;
  /** Queue seconds by wait reason (W05-R13). */
  waits: Record<string, number>;
  failures: Record<string, number>;
}

export interface CiInsightPoint extends CiInsight {
  /** The bucket's start (Unix seconds). */
  bucket: number;
}

interface CiInsightsHead {
  since: number;
  days: number;
  grain: "hour" | "day";
  currency: string;
}

/** A repository's runs and its share of runner time, against its fair-share weight's (W05-R10). */
export interface CiRepoShare {
  id: string;
  name: string | null;
  runs: CiInsight;
  runner_minutes: number;
  share: number | null;
  weight: number;
  weight_share: number | null;
  cost: number | null;
}

export interface CiInsightsRepo extends CiInsightsHead {
  repo: { id: string; name: string };
  share: CiRepoShare | null;
  workflows: (CiInsight & { workflow: string; name: string | null; series: CiInsightPoint[] })[];
}

export interface CiInsightsWorkflow extends CiInsightsHead, CiInsight {
  repo: { id: string; name: string };
  workflow: string;
  name: string | null;
  series: CiInsightPoint[];
  jobs: (CiInsight & { job: string; series: CiInsightPoint[] })[];
  steps: { job: string; step: string; count: number; p50: number | null; p90: number | null; mean: number | null; total: number; success_rate: number | null }[];
  executors: { executor: string; pool: string; legs: number; runner_minutes: number; cpu_seconds: number; peak_rss: number; cost: number | null }[];
  cache: { prefix: string; hits: number; misses: number; saves: number; hit_rate: number | null; bytes_restored: number; bytes_saved: number; seconds_saved: number | null }[];
  critical_path: { run: string; number: number; jobs: string[]; seconds: number } | null;
}

export interface CiRunnerInsight {
  runner: string;
  pool: string;
  connected_minutes: number;
  slot_minutes: number;
  busy_minutes: number;
  utilization: number | null;
  legs: CiInsight;
  runner_minutes: number;
  cost: number | null;
}

export interface CiInsightsRunner extends CiInsightsHead, CiRunnerInsight {
  series: (CiInsightPoint & { connected_minutes: number; slot_minutes: number; busy_minutes: number; utilization: number | null })[];
  repos: { id: string; name: string | null; legs: number; runner_minutes: number; failure: number }[];
}

/** A log line that matched a search (W19-R04). `at` is where the match starts in `text` (bytes). */
export interface CiLogHit {
  run: string;
  number: number;
  repo: string;
  repo_name: string | null;
  workflow: string;
  branch: string;
  conclusion: string | null;
  created_at: number;
  leg: string;
  job: string;
  step: number;
  step_name: string | null;
  line: number;
  text: string;
  at: number;
}

/** Something that differs between two runs or legs (W19-R03): `a` is the base's, `b` the head's (null: not there). */
export interface CiDifference {
  kind: "action" | "var" | "leg" | "conclusion" | "runner" | "executor" | "machine" | "version" | "image" | "env" | "cache" | string;
  leg?: string;
  what: string;
  a: string | null;
  b: string | null;
}

export interface CiCompareRun {
  id: string;
  number: number;
  workflow: string;
  branch: string;
  sha: string;
  conclusion: string | null;
  created_at: number;
  duration: number | null;
}

export interface CiCompareLeg {
  id: string | null;
  conclusion: string;
  duration: number | null;
  runner: string;
  executor: string;
  machine: string | null;
}

export interface CiCompare {
  base: CiCompareRun;
  head: CiCompareRun;
  differences: CiDifference[];
  files: { count: number | null; files: string[]; error?: string };
  legs: {
    key: string;
    base: CiCompareLeg | null;
    head: CiCompareLeg | null;
    steps: { name: string; a: number | null; b: number | null; a_conclusion: string | null; b_conclusion: string | null }[];
  }[];
}

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
  /** The run's benchmark metrics (W16-R07). */
  metrics: (id: string, signal?: AbortSignal) => request<{ metrics: CiMetric[] }>("GET", `/api/ci/runs/${enc(id)}/metrics`, undefined, { signal }),
  /** The run's tests, failures first (W16-R03). */
  tests: (id: string, attempt: number | undefined, signal?: AbortSignal) =>
    request<{ counts: Record<string, number>; tests: CiTest[] }>("GET", `/api/ci/runs/${enc(id)}/tests${at(attempt)}`, undefined, { signal }),
  rerun: (id: string, mode: RerunMode, job: string | null, debug: boolean, keepFailed = false) =>
    request<{ run: string; attempt: number }>("POST", `/api/ci/runs/${enc(id)}/rerun`, { mode, job, debug, keep_failed: keepFailed }),
  /** A review of the run's deployments waiting on their environments (W15-R04), or, with none
   * named, of an outside contributor's run waiting to start (W18-R06). */
  approve: (id: string, environments: string[], state: "approved" | "rejected", comment: string) =>
    request<{ run: string; environments?: string[]; approval?: "run"; state: string }>("POST", `/api/ci/runs/${enc(id)}/approve`, { environments, state, comment }),
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
  searchLogs: (p: { q: string; repo?: string; branch?: string; conclusion?: string; days?: number }, signal?: AbortSignal) =>
    request<{ hits: CiLogHit[]; took_ms: number; unindexed_legs: number }>(
      "GET",
      `/api/ci/logs/search?${new URLSearchParams(
        Object.entries(p)
          .filter(([, v]) => v !== undefined && v !== "")
          .map(([k, v]) => [k, String(v)]),
      )}`,
      undefined,
      { signal },
    ),
  /** `other`: a run's (or leg's) id, or `last-green`. */
  compare: (run: string, other: string, signal?: AbortSignal) => request<CiCompare>("GET", `/api/ci/runs/${enc(run)}/compare/${enc(other)}`, undefined, { signal }),
  compareLegs: (leg: string, other: string, signal?: AbortSignal) => request<CiCompare>("GET", `/api/ci/legs/${enc(leg)}/compare/${enc(other)}`, undefined, { signal }),
  insightsRepos: (days: number, signal?: AbortSignal) => request<CiInsightsHead & { repos: CiRepoShare[] }>("GET", `/api/ci/insights/repos?days=${days}`, undefined, { signal }),
  insightsRepo: (repo: string, days: number, signal?: AbortSignal) => request<CiInsightsRepo>("GET", `/api/ci/insights/repos/${enc(repo)}?days=${days}`, undefined, { signal }),
  insightsWorkflow: (repo: string, workflow: string, days: number, signal?: AbortSignal) =>
    request<CiInsightsWorkflow>("GET", `/api/ci/insights/workflows/${enc(repo)}?workflow=${enc(workflow)}&days=${days}`, undefined, { signal }),
  insightsRunners: (days: number, signal?: AbortSignal) => request<CiInsightsHead & { runners: CiRunnerInsight[] }>("GET", `/api/ci/insights/runners?days=${days}`, undefined, { signal }),
  insightsRunner: (runner: string, days: number, signal?: AbortSignal) => request<CiInsightsRunner>("GET", `/api/ci/insights/runners/${enc(runner)}?days=${days}`, undefined, { signal }),
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
    window?: string;
    until?: number | null;
    pool?: string;
    starting?: number;
    since?: number;
    reservation?: string;
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
      if (o.environment) return `Waiting for ${o.environment}'s other deployments (at most ${o.limit ?? 1} at once).`;
      return `Waiting for concurrency group ${o.group ?? ""}${o.holders?.length ? `, held by ${o.holders.join(", ")}` : ""}.`;
    case "quota":
      return `The repository's limit of ${o.limit ?? 0} running legs is reached.`;
    case "exclusive":
      return "Waiting to run alone on a runner.";
    case "scaling":
      return `Pool ${o.pool ?? ""} is starting ${o.starting ?? 0} executor${o.starting === 1 ? "" : "s"}${o.since ? ` since ${new Date(o.since).toLocaleTimeString()}` : ""}.`;
    case "reservation":
      return `Its runners are reserved${o.reservation ? ` (${o.reservation})` : ""} until ${o.until ? new Date(o.until).toLocaleTimeString() : "the reservation ends"}.`;
    case "retry":
      return `Retrying at ${o.at ? new Date(o.at * 1000).toLocaleTimeString() : "a moment"}.`;
    case "approval":
      return `Waiting for a review to deploy to ${o.environment ?? "its environment"}${o.reviewers?.length ? ` (${o.reviewers.join(", ")})` : ""}${o.until ? `, and until ${new Date(o.until * 1000).toLocaleTimeString()}` : ""}.`;
    case "wait-timer":
      return `Waiting until ${o.until ? new Date(o.until * 1000).toLocaleTimeString() : "its wait timer ends"} to deploy to ${o.environment ?? "its environment"}.`;
    case "window":
      return `Waiting for a deploy window of ${o.environment ?? "its environment"}: ${o.window ?? "it is closed"}.`;
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
