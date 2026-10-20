/** The CI engine (docs/ci.md §4.1): the /api/ci routes the run, leg and editor pages read. */
import { ApiError, request } from "./client";
import type { Schema } from "@/lib/yaml-spot";

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

/** One attempt of a leg (W05-R14's retries): how it ended, why (its failure class), and where it ran. */
export interface CiAttempt {
  number: number;
  status: string;
  conclusion: string | null;
  class: string | null;
  runner: string | null;
  executor: string | null;
  queued_at: number | null;
  started_at: number | null;
  finished_at: number | null;
  exit_code: number | null;
  error: string | null;
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
  /** The workflow's jobs as the run's plan has them, those with no legs yet included. */
  jobs?: CiPlanJob[];
}

/** A job as a run's plan has it (W21-R04), whether its legs are made yet or not. */
export interface CiPlanJob {
  job: string;
  /** Its name as written (null: an expression makes it, or it has none). */
  name: string | null;
  needs: string[];
  /** Its matrix comes from what it needs: how many legs it has is known once they're done. */
  matrix_waits: boolean;
  steps: number;
  /** It calls a workflow: its legs are that workflow's jobs (`<job>/<theirs>`). */
  calls: boolean;
  /** How long it typically takes, in seconds: the median of its last 20 successful runs. */
  typical: number | null;
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

/** A test over a window on a repository's default branch (W16-R03). */
export interface CiRepoTest {
  id: string;
  suite: string;
  name: string;
  file: string | null;
  runs: number;
  failures: number;
  p50_ms: number | null;
  p90_ms: number | null;
  flaky: { class: "flaky" | "probably_flaky"; why: string } | null;
}

export type CiTestSort = "failing" | "slowest" | "flaky";

/** A routing rule (W05-R21): what it matches, and what it adds to a leg's routing. */
export interface CiRoutingRule {
  match: { labels?: string[]; job?: string | null; repos?: string[] };
  require?: string[];
  executor?: string[];
  image?: string | null;
  prefer?: { fact?: string; label?: string; higher?: boolean; weight?: number }[];
  docker?: string | null;
}

/** Where a leg with these needs would go (W05-R20): what's left for runners to match, and each runner's checks. */
export interface CiRouteExplain {
  labels: string[];
  constraints: string[];
  executors: string[];
  image: string | null;
  rules: { source: string; rule: CiRoutingRule }[];
  runners: { name: string; eligible: boolean; room?: boolean; checks?: { requirement: string; value?: string; pass: boolean }[] }[];
}

/** An actions policy (W07-R04): globs a run may use, globs it may not, and whether every remote action is pinned by its SHA. */
export interface CiActionsPolicy {
  allow: string[];
  deny: string[];
  require_sha_pins: boolean;
}

/** kiln's OIDC issuer (W10-R06, R12): where it is, whether this server serves it, and its keys. */
export interface CiOidc {
  issuer: string;
  discovery: string;
  jwks_uri: string;
  serving: boolean;
  keys: { kid: string; created_at: number; sign_from: number; signing: boolean; published: boolean }[];
  subject_claims: string[];
  /** How providers reach it (R12): whether its URL looks public, its own addresses, and where it's published. */
  reach?: { public: boolean; listen: string[]; publish: { to: string; at: number | null; checked_at: number | null; error: string | null } | null };
}

/** A secret as listed (W09): never its value. `from` for a brokered one, made at leg start. */
export interface CiSecretMeta {
  name: string;
  size: number;
  created_at: number;
  updated_at: number;
  last_used_at: number | null;
  from?: string;
}

/** A variable (W09-R09): shown, unlike a secret. */
export interface CiVariable {
  name: string;
  value: string;
  updated_at: number;
}

/** A person's own notification rule (W19-R06): repository events, to their inbox or their email. */
export interface MyRule {
  on: string[];
  to: ("inbox" | "email")[];
  where?: string | null;
  throttle?: string | null;
  /** By name or glob; every repository they may read when empty. */
  repos?: string[];
  /** Only what's about them: runs they started, reviews asked of them, replies to and mentions of them. */
  about_me?: boolean;
}

/** A person's own rules, the address `email` sends to, and the events a rule may take; `person` false for the owner, whose rules are the server's. */
export interface MyNotifications {
  person: boolean;
  rules: MyRule[];
  email: string | null;
  events: string[];
}

/** A metric a repository's default branch recorded over a window (W16-R07): how many points, and the latest's time. */
export interface CiRepoMetric {
  name: string;
  unit: string;
  better: "lower" | "higher";
  points: number;
  last: number;
}

/** A metric's values over a window, one series per runner fingerprint (W16-AC05). */
export interface CiMetricTrend {
  name: string;
  better: "lower" | "higher" | null;
  series: { fingerprint: string; points: { sha: string; run: string; value: number; unit: string; ts: number }[] }[];
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
  /** Each job's matrix as written (W21-R06's matrix view). */
  matrices?: Record<string, CiMatrix>;
}

/** A job's matrix as written: its axes' values (or the expression that makes them), `include`, `exclude` and `where`; or the expression that makes it whole. */
export interface CiMatrix {
  axes?: { name: string; values: unknown[] | string }[];
  include?: unknown;
  exclude?: unknown;
  where?: string | null;
  platforms?: unknown;
  expr?: string;
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
  /** A workflow's, or an action's (W21-R07). */
  kind?: "workflow" | "action";
  /** Where the repository is kept (none: nowhere kiln writes), and what an edit of it may be saved as, the first its default (W21-R06). */
  host?: "kiln" | "github" | "gitlab" | "bitbucket" | null;
  saves?: CiSave[];
}

/** How an edited workflow is saved (W21-R06): a diff through review, a change (pull or merge request) on its host, a commit to a branch, or only the patch. */
export type CiSave = "diff" | "change" | "branch" | "patch";

export interface CiSaved {
  to: CiSave;
  diffs?: { name: string; title: string; kind: string; url: string }[];
  branch?: string;
  sha?: string;
  change?: { number: number; url: string };
  patch?: string;
}

/** An action the catalog has: one mirrored (its versions, its metadata), or kiln's own (W07-R10:
 *  `uses` with its version, and what it does). */
export interface CiCatalogEntry {
  uses: string;
  versions?: { ref: string; sha: string }[];
  description?: string;
  action?: {
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

/** A pool's executor now (W13-R09). */
export type CiPoolExecutor = { id: string; pool: string; runner: string; state: "starting" | "idle" | "busy"; leg?: string; since: number };
/** A pool: its settings, where they come from (the configuration, or kiln's store), and its executors now. */
export type CiPool = {
  name: string;
  executor: string;
  image: string | null;
  labels: string[];
  hosts: string | null;
  min_idle: number;
  max: number;
  per_host_max: number;
  idle_timeout_ms: number;
  starting: number;
  idle: number;
  busy: number;
  waiting: number;
  executors: CiPoolExecutor[];
  /** The last failure to start one: on which host, and why. */
  last_error: [string, string] | null;
  source: "config" | "stored";
};
/** A runner image kiln built into its registry (W13-R11). */
export type CiImage = { id: string; repo: string | null; name: string; reference: string; digest: string; image: string; platforms: string[]; run_id: string | null; created_at: number };
/** A stored pool, as `PUT /api/ci/pools/:name` takes it. */
export type CiPoolSpec = { name: string; executor: string; image?: string | null; labels: string[]; min_idle: number; max?: number | null; idle_timeout?: string | null };

export const ci = {
  pools: (signal?: AbortSignal) => request<{ pools: CiPool[] }>("GET", "/api/ci/pools", undefined, { signal }).then((r) => r.pools),
  /** A pool's scaling history (W21-R08): its executors and the legs waiting for it, a minute apart, oldest first. */
  poolHistory: (name: string, hours: number, signal?: AbortSignal) =>
    request<{ pool: string; hours: number; samples: { ts: number; starting: number; idle: number; busy: number; waiting: number }[] }>(
      "GET",
      `/api/ci/pools/${enc(name)}/history?hours=${hours}`,
      undefined,
      { signal },
    ),
  /** The pool, with what it does now (W20-R03): warnings, and the size it heads for. */
  putPool: (spec: CiPoolSpec) => request<{ diagnostics?: { message: string }[]; preview?: { heading_for: number } }>("PUT", `/api/ci/pools/${enc(spec.name)}`, spec),
  removePool: (name: string) => request<void>("DELETE", `/api/ci/pools/${enc(name)}`),
  images: (signal?: AbortSignal) => request<{ images: CiImage[] }>("GET", "/api/ci/images", undefined, { signal }).then((r) => r.images),
  /** An image's definition checked as the editor checks it (W13-R11): a Dockerfile by kiln's parser and lints; an Agentfile is shards' to check, where it builds. */
  validateImage: (kind: "dockerfile" | "agentfile", content: string, signal?: AbortSignal) =>
    request<{ kind: string; checked: boolean; diagnostics: CiDiagnostic[]; note?: string }>("POST", "/api/ci/images/validate", { kind, content }, { signal }),
  /** A definition proposed as a diff adding `.kiln/images/<name>/` ("Save to repo"). */
  proposeImage: (repo: string, body: { name: string; kind: "dockerfile" | "agentfile"; content: string }) =>
    request<{ diffs: { name: string; title: string; kind: string; url: string }[] }>("POST", `/api/ci/images?repo=${enc(repo)}`, body),
  /** A definition built once as the editor has it (W21-R08's Build), pushed and registered too with `push` (the server owner's). */
  buildImage: (repo: string, body: { name: string; kind: "dockerfile" | "agentfile"; content: string; platforms: string[]; runs_on: string; push: boolean }) =>
    request<{ run: string; sha: string; trusted: boolean }>("POST", `/api/ci/images/build?repo=${enc(repo)}`, body),
  graph: (id: string, attempt: number | undefined, signal?: AbortSignal) => request<CiGraph>("GET", `/api/ci/runs/${enc(id)}/graph${at(attempt)}`, undefined, { signal }),
  annotations: (id: string, attempt: number | undefined, signal?: AbortSignal) =>
    request<{ annotations: CiAnnotation[] }>("GET", `/api/ci/runs/${enc(id)}/annotations${at(attempt)}`, undefined, { signal }),
  summaries: (id: string, attempt: number | undefined, signal?: AbortSignal) => request<{ summaries: CiSummary[] }>("GET", `/api/ci/runs/${enc(id)}/summary${at(attempt)}`, undefined, { signal }),
  artifacts: (id: string, signal?: AbortSignal) => request<{ artifacts: CiArtifact[] }>("GET", `/api/ci/runs/${enc(id)}/artifacts`, undefined, { signal }),
  /** The run's benchmark metrics (W16-R07). */
  metrics: (id: string, signal?: AbortSignal) => request<{ metrics: CiMetric[] }>("GET", `/api/ci/runs/${enc(id)}/metrics`, undefined, { signal }),
  /** A repository's tests over `days` (W16-R03): the most failing, the slowest, or the flaky. */
  repoTests: (repo: string, sort: CiTestSort, days: number, signal?: AbortSignal) =>
    request<{ repo: string; branch: string; days: number; sort: CiTestSort; tests: CiRepoTest[] }>("GET", `/api/ci/repos/${enc(repo)}/tests?sort=${sort}&days=${days}&limit=50`, undefined, {
      signal,
    }),
  /** The server's actions policy, and with `repo` that repository's: kiln's own setting and its `.kiln/config.yaml`'s. */
  actionsPolicy: (repo: string | null, signal?: AbortSignal) =>
    request<{ server: CiActionsPolicy; repository?: CiActionsPolicy | null; config_yaml?: CiActionsPolicy | null }>("GET", `/api/ci/actions/policy${repo ? `?repo=${enc(repo)}` : ""}`, undefined, {
      signal,
    }),
  setActionsPolicy: (repo: string, p: CiActionsPolicy) => request<unknown>("PUT", `/api/ci/actions/policy?repo=${enc(repo)}`, p),
  clearActionsPolicy: (repo: string) => request<unknown>("DELETE", `/api/ci/actions/policy?repo=${enc(repo)}`),
  /** Mirrors `[ci.actions] prefetch` now: each action's pinned commit, or why not. */
  syncActions: () => request<{ synced: { uses: string; sha?: string; error?: string }[] }>("POST", "/api/ci/actions/sync", {}),
  /** kiln's issuer and its keys. */
  oidc: (signal?: AbortSignal) => request<CiOidc>("GET", "/api/ci/oidc", undefined, { signal }),
  /** A new signing key: signing in 7 days (published first, so relying parties fetch it), or `now`. */
  oidcRotate: (now: boolean) => request<{ sign_from: number; published: string | null }>("POST", "/api/ci/oidc/rotate", { now }),
  /** Publishes the issuer's documents to `[ci.oidc] publish_to` now. */
  oidcPublish: () => request<{ published: string }>("POST", "/api/ci/oidc/publish", {}),
  /** Every routing rule in the order they apply, each with where it comes from. */
  routingRules: (signal?: AbortSignal) => request<{ rules: { index: number; source: string; rule: CiRoutingRule }[]; github_labels: boolean }>("GET", "/api/ci/routing/rules", undefined, { signal }),
  /** Replaces a scope's stored rules (`server` or `org:<owner>`; a repository's are its `.kiln/routing.yaml`). */
  /** The rules, with what they do now (W20-R03): a warning for each rule no runner meets. */
  setRoutingRules: (scope: string, rules: CiRoutingRule[]) => request<{ diagnostics?: { message: string }[] }>("PUT", `/api/ci/routing/rules?scope=${enc(scope)}`, { rules }),
  /** Where a leg with these labels and constraints would go, against the runners connected now. */
  routingExplain: (body: { labels: string[]; repo?: string; requires?: string[] }) => request<CiRouteExplain>("POST", "/api/ci/routing/explain", body),
  /** A scope's secrets (`server`, `org:acme`, `repo:acme/app`, `workflow:acme/app:<path>`, `env:acme/app:<name>`): names, never values. */
  secrets: (scope: string, signal?: AbortSignal) => request<{ scope: string; secrets: CiSecretMeta[] }>("GET", `/api/ci/secrets?scope=${enc(scope)}`, undefined, { signal }),
  setSecret: (scope: string, name: string, value: string) => request<void>("PUT", `/api/ci/secrets/${enc(name)}?scope=${enc(scope)}`, { value }),
  deleteSecret: (scope: string, name: string) => request<void>("DELETE", `/api/ci/secrets/${enc(name)}?scope=${enc(scope)}`),
  /** A scope's variables, with their values. */
  variables: (scope: string, signal?: AbortSignal) => request<{ scope: string; variables: CiVariable[] }>("GET", `/api/ci/variables?scope=${enc(scope)}`, undefined, { signal }),
  setVariable: (scope: string, name: string, value: string) => request<void>("PUT", `/api/ci/variables/${enc(name)}?scope=${enc(scope)}`, { value }),
  deleteVariable: (scope: string, name: string) => request<void>("DELETE", `/api/ci/variables/${enc(name)}?scope=${enc(scope)}`),
  /** What notification rules sent (W19-R06), newest first: each delivery's rule, event, channel, and why one didn't go. */
  deliveries: (limit: number, signal?: AbortSignal) =>
    request<{ deliveries: { ts: number; rule: string; event: string; run: string | null; channel: string; status: string; error: string | null; attempts: number }[] }>(
      "GET",
      `/api/ci/notify/deliveries?limit=${limit}`,
      undefined,
      { signal },
    ),
  /** Your own notification rules (W19-R06). */
  myNotifications: (signal?: AbortSignal) => request<MyNotifications>("GET", "/api/me/notifications", undefined, { signal }),
  /** Replaces your rules, and the address email goes to when given (`null` clears it). */
  setMyNotifications: (rules: MyRule[], email?: string | null) => request<MyNotifications>("PUT", "/api/me/notifications", email === undefined ? { rules } : { rules, email }),
  /** The metrics a repository's default branch recorded over `days` (W16-R07). */
  repoMetrics: (repo: string, days: number, signal?: AbortSignal) =>
    request<{ repo: string; branch: string; days: number; data: { metrics: CiRepoMetric[] } }>("GET", `/api/ci/repos/${enc(repo)}/metrics?days=${days}`, undefined, { signal }),
  /** A metric's values on a repository's default branch over `days`, by runner fingerprint (W16-AC05). */
  repoMetric: (repo: string, name: string, days: number, signal?: AbortSignal) =>
    request<{ repo: string; branch: string; days: number; data: CiMetricTrend }>("GET", `/api/ci/repos/${enc(repo)}/metrics?name=${enc(name)}&days=${days}`, undefined, { signal }),
  /** The run's tests, failures first (W16-R03). */
  tests: (id: string, attempt: number | undefined, signal?: AbortSignal) =>
    request<{ counts: Record<string, number>; tests: CiTest[] }>("GET", `/api/ci/runs/${enc(id)}/tests${at(attempt)}`, undefined, { signal }),
  /** `fresh`: the actions pinned again now, not as the run pinned them (W07-R02). */
  rerun: (id: string, mode: RerunMode, job: string | null, debug: boolean, keepFailed = false, fresh = false) =>
    request<{ run: string; attempt: number }>("POST", `/api/ci/runs/${enc(id)}/rerun`, { mode, job, debug, keep_failed: keepFailed, fresh_actions: fresh }),
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
  /** A leg's attempts, oldest first, each with its failure class, and its outputs (W21-R05). */
  legDetail: (leg: string, signal?: AbortSignal) =>
    request<{ attempts: CiAttempt[]; outputs: Record<string, string>; steps?: { idx: number; name: string; action: string | null; status: string; conclusion: string | null }[] }>(
      "GET",
      `/api/ci/legs/${enc(leg)}`,
      undefined,
      { signal },
    ),
  /** An action as its editor has it, run once with `inputs` on what `runs_on` names (W21-R07's "Try it"). */
  tryAction: (repo: string, body: { path: string; content: string; ref?: string; inputs: Record<string, string>; runs_on: string }) =>
    request<{ run: string; sha: string; trusted: boolean }>("POST", `/api/ci/repos/${enc(repo)}/actions/try`, body),
  /** A file checked; with `repo`, against what its run would meet too (W02-R12: labels no runner has, actions its policies refuse). */
  validate: (path: string, content: string, repo?: string, signal?: AbortSignal) => request<CiChecked>("POST", `/api/ci/validate`, repo ? { path, content, repo } : { path, content }, { signal }),
  /** The JSON Schema of a kind of file (W02-R09): `gha`, `kiln`, `overlay` or `action`, for the editor's key completion and hover. */
  schema: (kind: "gha" | "kiln" | "overlay" | "action") => request<Schema>("GET", `/api/ci/schema/${kind}`),
  /** A workflow's new text saved as `to` says (W21-R06, `ci.workflows.edit`), checked first. */
  saveWorkflow: (repo: string, path: string, body: { content: string; to: CiSave; branch?: string; title?: string }) =>
    request<CiSaved>("POST", `/api/ci/repos/${enc(repo)}/workflows/${path.split("/").map(enc).join("/")}/edit`, body),
  workflows: (repo: string, ref: string, signal?: AbortSignal) =>
    request<{ ref: string; sha: string; workflows: CiWorkflowFile[]; actions?: string[] }>("GET", `/api/ci/repos/${enc(repo)}/workflows${ref ? `?ref=${enc(ref)}` : ""}`, undefined, { signal }),
  workflow: (repo: string, path: string, ref: string, signal?: AbortSignal) =>
    request<CiWorkflowSource>("GET", `/api/ci/repos/${enc(repo)}/workflows/${path.split("/").map(enc).join("/")}${ref ? `?ref=${enc(ref)}` : ""}`, undefined, { signal }),
  /** What a `runs-on` of the repository's may name (W21-R06): runners', pools' and routing rules' labels, each with where it comes from. */
  labels: (repo: string, signal?: AbortSignal) => request<{ labels: { label: string; from: ("runner" | "pool" | "rule")[] }[] }>("GET", `/api/ci/repos/${enc(repo)}/labels`, undefined, { signal }),
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
    promotion?: string;
    gate?: string;
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
    case "promotion":
      return `Waiting for ${o.promotion ?? "this commit to deploy to the environment before it"}, to deploy to ${o.environment ?? "its environment"}.`;
    case "gate":
      return `Waiting for ${o.gate ?? "its environment's gates to pass"}, to deploy to ${o.environment ?? "its environment"}.`;
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
