/**
 * Deploying by API (docs/deploy-api.md): apps, environments, their registrations and releases,
 * over `/api/deploy`. A release is a commit to an environment's release repository that its CD
 * provider (KilnCD or Argo CD) makes run; it waits on the environment's gates first.
 */
import { request } from "./client";
import type { CdDiff, CdResourceKey } from "./types";

export type ReleaseState = "waiting" | "waiting_approval" | "committing" | "delivering" | "succeeded" | "failed" | "cancelled" | "rolled_back" | "superseded";
export type ReleaseKind = "deploy" | "config" | "rollback" | "enable" | "disable";

export interface Artifact {
  repository: string;
  tag: string;
  digest: string;
}

/** A gate's result on a release, with its evidence. */
export interface GateResult {
  gate: string;
  passed: boolean;
  detail: string;
  evidence?: Record<string, unknown> | null;
}

/** A person's answer to a release waiting for approval. */
export interface Approval {
  by: string;
  at: number;
  approved: boolean;
  note?: string;
}

export interface Release {
  id: number;
  app: string;
  environment: string;
  kind: ReleaseKind;
  state: ReleaseState;
  origin: "api" | "git";
  version?: string;
  artifacts?: Record<string, Artifact>;
  values?: string;
  commit?: string;
  requested_by: string;
  message?: string;
  rollback_of?: number;
  rolled_back_by?: number;
  keep?: boolean;
  provider?: Record<string, unknown> | null;
  gates?: GateResult[];
  approvals?: Approval[];
  promoted_from?: number;
  created_at: number;
  committed_at?: number;
  ended_at?: number;
  /** Secrets' changes it delivers (docs/deploy-api.md §3.5): a secrets release's reason (`DB_PASSWORD v4`), or ones it took on while it waited. */
  secrets?: string | null;
}

export interface Registration {
  app: string;
  environment: string;
  enabled: boolean;
  paused: boolean;
  running?: number;
  created_at: number;
}

export interface Gates {
  after?: string[];
  soak?: string;
  approvals?: { count: number; reviewers?: string[]; self_approve?: boolean };
  windows?: { allow?: string; deny?: string; duration: string; timezone?: string }[];
  checks?: string[];
  freeze?: string;
  /** `auto`: a version that lands in one of `after` is released here by itself (§9.2). */
  promote?: "auto" | "manual";
}

export interface Environment {
  name: string;
  /** `kilncd`, or `argocd/<integration>`. */
  provider: string;
  release_repo: { host: string; repo: string; branch: string; root: string };
  targets: { kubernetes: string[]; platforms?: Record<string, Record<string, unknown>> };
  gates: Gates;
  concurrency: "reject" | "queue";
  agents: { deploy: boolean; approve: boolean };
}

export type Package =
  | { kind: "helm"; repo: string; chart: string; chart_version?: string; set?: Record<string, string> }
  | { kind: "git"; repo: string; path: string; set?: Record<string, string> }
  | { kind: "platform"; provider: string; spec: unknown; repo?: string; path?: string };

export interface App {
  name: string;
  project: string;
  owners: string[];
  package: Package;
  namespace?: string;
  properties?: Record<string, string>;
  template?: string;
  contacts?: { oncall?: string; channels?: string[]; links?: Record<string, string> };
  /** Its source repository, as kiln names it (`acme/web`). */
  source?: string;
  archived: boolean;
}

/** The source repository a template makes for each app (§3.1): one of kiln's own. */
export interface RepoTemplate {
  /** `acme/{app}`: `{app}` and `{project}` filled in. */
  name: string;
  starter?: string;
  starter_ref?: string;
  branch: string;
}

/** What apps are made from (§3.2). */
export interface Template {
  name: string;
  description?: string;
  package?: Package;
  namespace?: string;
  checks?: string[];
  properties?: Record<string, string[]>;
  environments?: string[];
  repository?: RepoTemplate;
}

/** An app as asked for: a template fills what isn't given. `source: ""` makes none. */
export interface AppAsk {
  name: string;
  project?: string;
  owners?: string[];
  template?: string;
  package?: Package;
  namespace?: string;
  properties?: Record<string, string>;
  source?: string;
}

/** What changes in an app: only what is given. A property set to `null` is removed; `source: ""` unlinks. */
export interface AppPatch {
  project?: string;
  owners?: string[];
  package?: Package;
  namespace?: string;
  properties?: Record<string, string | null>;
  template?: string;
  contacts?: App["contacts"];
  source?: string;
  archived?: boolean;
}

/** One registration on the board: what runs, and what's on its way. */
export interface Cell {
  registration: Registration;
  running: Release | null;
  in_flight: Release | null;
}

export interface Board {
  environments: Environment[];
  apps: App[];
  cells: Cell[];
  waiting_approval: Release[];
}

export interface RegistrationDetail {
  registration: Registration;
  running: Release | null;
  in_flight: Release | null;
  releases: Release[];
  provider: { kind: "kilncd" | "argocd"; app: string; integration?: string; status: Record<string, unknown> | null };
}

export interface AppDetail {
  app: App;
  environments: { registration: Registration; running: Release | null }[];
}

/** One object of what runs, as the provider sees it (KilnCD's shape, from either provider). */
export interface RunningObject extends CdResourceKey {
  sync: string;
  health: string | null;
  message: string | null;
  missing?: boolean;
  requires_pruning?: boolean;
  hook?: boolean;
  why?: string | null;
}

/** A workload's pods' logs, the last lines of each (Argo CD gives only the lines). */
export interface LogRead {
  lines: { pod: string; ts: string; text: string }[];
  pods?: string[];
  more_pods?: number;
  container?: string | null;
  containers?: string[];
  errors?: string[];
}

/** DORA's four keys, from the releases (durations in seconds). */
export interface Metrics {
  days: number;
  changes: number;
  landed: number;
  per_day: number;
  lead_time_secs: { count: number; p50: number | null; p90: number | null };
  failed: number;
  change_failure_rate: number | null;
  time_to_restore_secs: { count: number; p50: number | null; p90: number | null };
  by_day: { day: string; landed: number; failed: number }[];
}

/** One of an app's secrets in an environment, as listed: never its value (docs/deploy-api.md §3.5). */
export interface SecretMeta {
  name: string;
  version: number;
  size: number;
  set_by: string;
  set_at: number;
}

export interface SecretVersion {
  version: number;
  set_by: string;
  set_at: number;
  /** A deletion: the secret was gone from this version on. */
  deleted: boolean;
  /** Its value is no longer kept (older than the last ten), or it never had one. */
  destroyed: boolean;
}

/** A secret's change on its way to what runs: the release that delivers it (made, or a waiting one it joined), and what happens next. */
export interface SecretReleased {
  release: Release | null;
  next: string;
}

/** A read of an app's secrets for a delivery: who, for what, which versions (`DB_PASSWORD@3,API_KEY@1`). */
export interface SecretRead {
  by: string;
  purpose: string;
  versions: string;
  at: number;
}

/** What a release asks for: exactly one of a version, values, enabled, or a promotion. */
export interface ReleaseAsk {
  app: string;
  environment: string;
  version?: string;
  artifacts?: Record<string, Artifact>;
  values?: string;
  enabled?: boolean;
  keep?: boolean;
  promote_from?: string;
  rollback?: { to?: number };
  idempotency_key?: string;
}

const enc = encodeURIComponent;
const reg = (app: string, env: string) => `/api/deploy/apps/${enc(app)}/environments/${enc(env)}`;

export const deployApi = {
  board: (signal?: AbortSignal) => request<Board>("GET", "/api/deploy/board", undefined, { signal }),
  app: (name: string, signal?: AbortSignal) => request<AppDetail>("GET", `/api/deploy/apps/${enc(name)}`, undefined, { signal }),
  registration: (app: string, env: string, signal?: AbortSignal) => request<RegistrationDetail>("GET", reg(app, env), undefined, { signal }),
  candidates: (app: string, env: string) => request<{ candidates: Release[] }>("GET", `${reg(app, env)}/candidates`).then((r) => r.candidates),
  releases: (q: { app?: string; environment?: string; state?: ReleaseState; limit?: number }, signal?: AbortSignal) => {
    const qs = Object.entries(q)
      .filter(([, v]) => v !== undefined && v !== "")
      .map(([k, v]) => `${enc(k)}=${enc(String(v))}`)
      .join("&");
    return request<{ releases: Release[] }>("GET", `/api/deploy/releases${qs ? `?${qs}` : ""}`, undefined, { signal }).then((r) => r.releases);
  },
  release: (id: number, signal?: AbortSignal) => request<Release>("GET", `/api/deploy/releases/${id}`, undefined, { signal }),
  metrics: (q: { app?: string; environment?: string; days?: number }, signal?: AbortSignal) => {
    const qs = Object.entries(q)
      .filter(([, v]) => v !== undefined && v !== "")
      .map(([k, v]) => `${enc(k)}=${enc(String(v))}`)
      .join("&");
    return request<Metrics>("GET", `/api/deploy/metrics${qs ? `?${qs}` : ""}`, undefined, { signal });
  },
  create: (ask: ReleaseAsk) => request<Release>("POST", "/api/deploy/releases", ask),
  cancel: (id: number) => request<Release>("POST", `/api/deploy/releases/${id}/cancel`, {}),
  approve: (id: number, note?: string) => request<Release>("POST", `/api/deploy/releases/${id}/approve`, { note }),
  reject: (id: number, note?: string) => request<Release>("POST", `/api/deploy/releases/${id}/reject`, { note }),
  templates: (signal?: AbortSignal) => request<{ templates: Template[] }>("GET", "/api/deploy/templates", undefined, { signal }).then((r) => r.templates),
  environments: (signal?: AbortSignal) => request<{ environments: Environment[] }>("GET", "/api/deploy/environments", undefined, { signal }).then((r) => r.environments),
  nameFree: (name: string) => request<{ name: string; available: boolean; why?: string | null }>("GET", `/api/deploy/app-names/${enc(name)}`),
  createApp: (ask: AppAsk) => request<App>("POST", "/api/deploy/apps", ask),
  patchApp: (name: string, patch: AppPatch) => request<App>("PATCH", `/api/deploy/apps/${enc(name)}`, patch),
  deleteApp: (name: string, o: { everywhere: boolean; keep: boolean }) => request<void>("DELETE", `/api/deploy/apps/${enc(name)}?everywhere=${o.everywhere}&keep=${o.keep}`),
  environment: (name: string, signal?: AbortSignal) => request<Environment>("GET", `/api/deploy/environments/${enc(name)}`, undefined, { signal }),
  putEnvironment: (e: Environment) => request<Environment>("PUT", `/api/deploy/environments/${enc(e.name)}`, e),
  /** A new environment: what isn't given is the server's default (no gates, one release at a time, no agents). */
  createEnvironment: (e: { name: string; provider: string; release_repo: { repo: string; branch: string; root: string }; targets: { kubernetes: string[] } }) =>
    request<Environment>("POST", "/api/deploy/environments", e),
  register: (app: string, env: string) => request<Registration>("POST", reg(app, env), {}),
  resources: (app: string, env: string, signal?: AbortSignal) => request<{ resources: RunningObject[] }>("GET", `${reg(app, env)}/resources`, undefined, { signal }).then((r) => r.resources),
  manifests: (app: string, env: string, version?: string, signal?: AbortSignal) =>
    request<{ manifests: { key: CdResourceKey; yaml: string }[] }>("GET", `${reg(app, env)}/manifests${version ? `?version=${enc(version)}` : ""}`, undefined, { signal }).then((r) => r.manifests),
  diff: (app: string, env: string, version?: string, signal?: AbortSignal) =>
    request<{ diffs: CdDiff[] }>("GET", `${reg(app, env)}/diff${version ? `?version=${enc(version)}` : ""}`, undefined, { signal }).then((r) => r.diffs),
  logs: (app: string, env: string, q: { kind: string; name: string; namespace?: string; container?: string; tail: number }, signal?: AbortSignal) => {
    const qs = Object.entries(q)
      .filter(([, v]) => v !== undefined && v !== "")
      .map(([k, v]) => `${enc(k)}=${enc(String(v))}`)
      .join("&");
    return request<LogRead>("GET", `${reg(app, env)}/logs?${qs}`, undefined, { signal });
  },
  pause: (app: string, env: string) => request<Registration>("POST", `${reg(app, env)}/pause`, {}),
  resume: (app: string, env: string) => request<Registration>("POST", `${reg(app, env)}/resume`, {}),
  secrets: (app: string, env: string, signal?: AbortSignal) => request<{ backend: string; secrets: SecretMeta[] }>("GET", `${reg(app, env)}/secrets`, undefined, { signal }),
  /** A secret's next version. Its value is never read back. */
  setSecret: (app: string, env: string, name: string, value: string) => request<SecretMeta & SecretReleased>("PUT", `${reg(app, env)}/secrets/${enc(name)}`, { value }),
  deleteSecret: (app: string, env: string, name: string) => request<SecretReleased>("DELETE", `${reg(app, env)}/secrets/${enc(name)}`),
  secretVersions: (app: string, env: string, name: string, signal?: AbortSignal) =>
    request<{ versions: SecretVersion[] }>("GET", `${reg(app, env)}/secrets/${enc(name)}/versions`, undefined, { signal }).then((r) => r.versions),
  secretUses: (app: string, env: string, signal?: AbortSignal) => request<{ reads: SecretRead[] }>("GET", `${reg(app, env)}/secret-uses?limit=20`, undefined, { signal }).then((r) => r.reads),
};

export const boardHref = "/deployments/releases";
export const newAppHref = "/deployments/releases/new";
export const releaseHref = (id: number) => `/deployments/releases/${id}`;
export const releaseAppHref = (app: string) => `/deployments/releases/apps/${enc(app)}`;
export const releaseEnvHref = (env: string) => `/deployments/releases/environments/${enc(env)}`;
export const registrationHref = (app: string, env: string) => `/deployments/releases/apps/${enc(app)}/environments/${enc(env)}`;

/** The status dot a release's state shows as. */
export const STATE_DOT: Record<ReleaseState, string> = {
  waiting: "queued",
  waiting_approval: "pending",
  committing: "running",
  delivering: "running",
  succeeded: "succeeded",
  failed: "failed",
  cancelled: "cancelled",
  rolled_back: "skipped",
  superseded: "skipped",
};

export const STATE_WORDS: Record<ReleaseState, string> = {
  waiting: "waiting",
  waiting_approval: "waiting for approval",
  committing: "committing",
  delivering: "delivering",
  succeeded: "succeeded",
  failed: "failed",
  cancelled: "cancelled",
  rolled_back: "rolled back",
  superseded: "superseded",
};

/** Still moving: a registration has at most one release in flight. */
export const inFlight = (s: ReleaseState) => s === "waiting" || s === "waiting_approval" || s === "committing" || s === "delivering";

/** What a release does, in a few words: `deploy 1.4.2`, `rollback to 1.4.1 (release 40)`. */
export function describe(r: Release): string {
  switch (r.kind) {
    case "deploy":
      return r.promoted_from ? `promote ${r.version ?? ""} (from release ${r.promoted_from})` : `deploy ${r.version ?? ""}`;
    case "rollback":
      return `rollback to ${r.version ?? ""}${r.rollback_of ? ` (release ${r.rollback_of})` : ""}`;
    case "config":
      return r.secrets && !r.values ? `secrets: ${r.secrets}` : r.secrets ? `config (and secrets: ${r.secrets})` : "config";
    default:
      return r.kind;
  }
}

/** How a package deploys, in a few words: `oci://ghcr.io/acme/charts web`. */
export function packageWords(p: Package): string {
  switch (p.kind) {
    case "helm":
      return `${p.repo} ${p.chart}`;
    case "git":
      return `${p.repo}${p.path ? ` ${p.path}` : ""}`;
    case "platform":
      return p.provider;
  }
}

/** The provider's name, as people say it. */
export function providerName(provider: string): string {
  return provider.startsWith("argocd/") ? `Argo CD ${provider.slice("argocd/".length)}` : "KilnCD";
}
