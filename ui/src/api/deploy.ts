/**
 * Deploying by API (docs/deploy-api.md): apps, environments, their registrations and releases,
 * over `/api/deploy`. A release is a commit to an environment's release repository that its CD
 * provider (KilnCD or Argo CD) makes run; it waits on the environment's gates first.
 */
import { request } from "./client";

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
  archived: boolean;
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
  pause: (app: string, env: string) => request<Registration>("POST", `${reg(app, env)}/pause`, {}),
  resume: (app: string, env: string) => request<Registration>("POST", `${reg(app, env)}/resume`, {}),
};

export const boardHref = "/deployments/releases";
export const releaseHref = (id: number) => `/deployments/releases/${id}`;
export const releaseAppHref = (app: string) => `/deployments/releases/apps/${enc(app)}`;

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
      return "config";
    default:
      return r.kind;
  }
}

/** The provider's name, as people say it. */
export function providerName(provider: string): string {
  return provider.startsWith("argocd/") ? `Argo CD ${provider.slice("argocd/".length)}` : "KilnCD";
}
