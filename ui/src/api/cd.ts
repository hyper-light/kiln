/** Deployments (docs/cd.md): the /api/cd routes, each an operation of the server's one table. */
import { request } from "./client";
import type {
  CdAdoption,
  CdApp,
  CdAppDetail,
  CdAutoRollback,
  CdCluster,
  CdDiscovery,
  CdProvider,
  CdTarget,
  CdValidation,
  CdDeployment,
  CdDiff,
  CdEvent,
  CdFacets,
  CdLogs,
  CdPlan,
  CdPlanAsk,
  CdPlanRepo,
  CdResource,
  CdResourceDetail,
  CdResourceKey,
  CdRollbackTargets,
  CdRollout,
  CdRolloutAction,
  CdSummary,
  CdTree,
} from "./types";

const enc = encodeURIComponent;
const appPath = (id: string) => `/api/cd/apps/${id.split("/").map(enc).join("/")}`;

function qs(params: Record<string, string | number | boolean | undefined | null>) {
  const parts = Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== null && v !== "")
    .map(([k, v]) => `${enc(k)}=${enc(String(v))}`);
  return parts.length ? `?${parts.join("&")}` : "";
}

/** Filters over apps; all optional. Values within a field widen (any of), fields narrow (all of). */
export interface CdFilters {
  sync?: string[];
  health?: string[];
  cluster?: string[];
  project?: string[];
  namespace?: string[];
  /** The environments its deploy file names it for. */
  environment?: string[];
  /** A part of the app's id, any case. */
  q?: string;
}

/** How the apps list is ordered. */
export type CdSort = "name" | "health" | "sync" | "deployed";

/** How a log is read. */
export interface CdLogQuery {
  container?: string;
  pod?: string;
  tail_lines?: number;
  since_seconds?: number;
  previous?: boolean;
}

/** An object's key in a path: `<group>~<kind>~<namespace>~<name>`. */
export const resourceKey = (k: CdResourceKey) => [k.group, k.kind, k.namespace, k.name].join("~");
const resPath = (id: string, k: CdResourceKey) => `${appPath(id)}/resources/${enc(resourceKey(k))}`;

/** The filters as the server takes them: each field's values joined by commas. */
const selector = (f: CdFilters) => ({
  sync: f.sync?.join(","),
  health: f.health?.join(","),
  cluster: f.cluster?.join(","),
  project: f.project?.join(","),
  namespace: f.namespace?.join(","),
  environment: f.environment?.join(","),
  q: f.q,
});

export const cdApi = {
  summary: (signal?: AbortSignal) => request<CdSummary>("GET", "/api/cd/summary", undefined, { signal }),
  apps: (f: CdFilters & { limit?: number; cursor?: string; sort?: CdSort }, signal?: AbortSignal) =>
    request<{ apps: CdApp[]; next: string | null }>(
      "GET",
      `/api/cd/apps${qs({ ...selector(f), limit: f.limit, cursor: f.cursor, sort: f.sort && f.sort !== "name" ? f.sort : undefined })}`,
      undefined,
      { signal },
    ),
  facets: (f: CdFilters, signal?: AbortSignal) => request<CdFacets>("GET", `/api/cd/facets${qs(selector(f))}`, undefined, { signal }),
  app: (id: string, signal?: AbortSignal) => request<CdAppDetail>("GET", appPath(id), undefined, { signal }),
  resources: (id: string, signal?: AbortSignal) => request<{ resources: CdResource[] }>("GET", `${appPath(id)}/resources`, undefined, { signal }),
  diff: (id: string, signal?: AbortSignal) => request<{ diffs: CdDiff[] }>("GET", `${appPath(id)}/diff`, undefined, { signal }),
  /** The app's objects and every live object they own. */
  tree: (id: string, signal?: AbortSignal) => request<CdTree>("GET", `${appPath(id)}/tree`, undefined, { signal }),
  /** One object in depth: live and desired manifests, diff, labels, conditions, owners. */
  resource: (id: string, k: CdResourceKey, signal?: AbortSignal) => request<CdResourceDetail>("GET", resPath(id, k), undefined, { signal }),
  events: (id: string, k: CdResourceKey, signal?: AbortSignal) => request<{ events: CdEvent[] }>("GET", `${resPath(id, k)}/events`, undefined, { signal }),
  logs: (id: string, k: CdResourceKey, q: CdLogQuery, signal?: AbortSignal) =>
    request<CdLogs>("GET", `${resPath(id, k)}/logs${qs({ ...q, previous: q.previous ? "true" : undefined })}`, undefined, { signal }),
  /** The path of a WebSocket that follows the logs. */
  logsFollowPath: (id: string, k: CdResourceKey, q: CdLogQuery) => `${resPath(id, k)}/logs/ws${qs({ ...q, previous: undefined })}`,
  history: (id: string, signal?: AbortSignal) => request<{ history: CdDeployment[] }>("GET", `${appPath(id)}/history`, undefined, { signal }),
  sync: (id: string, body: { prune?: boolean; dry_run?: boolean; options?: string[]; resources?: CdResourceKey[] }) => request<unknown>("POST", `${appPath(id)}/sync`, body),
  /** Syncs every app the filters show. */
  syncMany: (f: CdFilters) => request<{ started: number }>("POST", "/api/cd/ops/cd.apps.sync_many", { ...selector(f), max: 100_000 }),
  refresh: (id: string, hard: boolean) => request<unknown>("POST", `${appPath(id)}/refresh${qs({ hard })}`, {}),
  /** Create or replace an app kept in kiln, from its whole definition (as `get` gives it). */
  put: (definition: Record<string, unknown>) => request<{ app: string }>("POST", "/api/cd/apps", definition),
  /** Compares again every app the filters show. */
  refreshMany: (f: CdFilters) => request<{ refreshing: number }>("POST", "/api/cd/ops/cd.apps.refresh_many", selector(f)),
  terminate: (id: string) => request<unknown>("POST", `${appPath(id)}/terminate`, {}),
  takeOver: (id: string) => request<unknown>("POST", `${appPath(id)}/take-over`, {}),
  /** Back to a deployment that succeeded: by its id (`to`), or by what it ran (`revision`: a commit, a tag, a chart version). */
  rollback: (id: string, body: { to?: number; revision?: string; prune?: boolean }) => request<{ to: number; revision: string | null; tag: string | null }>("POST", `${appPath(id)}/rollback`, body),
  /** What the app can be rolled back to, newest first, each with its commit's message and tags. */
  rollbackTargets: (id: string, signal?: AbortSignal) => request<CdRollbackTargets>("GET", `${appPath(id)}/rollback-targets`, undefined, { signal }),
  /** Auto-rollback for one app: on, off, or `null` to follow its repository. */
  setAutoRollback: (id: string, on: boolean | null) => request<CdAutoRollback>("PUT", `${appPath(id)}/auto-rollback`, { on }),
  /** Auto-rollback for every app of a repository: on, off, or `null` to clear it. */
  setRepoAutoRollback: (repo: string, on: boolean | null) => request<unknown>("PUT", "/api/cd/repos/auto-rollback", { repo, on }),
  follow: (id: string) => request<unknown>("POST", `${appPath(id)}/follow`, {}),
  /** The app's rollouts on targets, the one under way first. */
  rollouts: (id: string, signal?: AbortSignal) => request<{ app: string; rollouts: CdRollout[] }>("GET", `${appPath(id)}/rollout`, undefined, { signal }),
  rollout: (id: string, action: CdRolloutAction, body: { full?: boolean; check?: string; reason?: string; rollout?: string } = {}) =>
    request<unknown>("POST", `${appPath(id)}/rollout/${action}`, body),
  remove: (id: string, cascade: "foreground" | "orphan") => request<unknown>("DELETE", `${appPath(id)}${qs({ cascade })}`),
  clusters: (signal?: AbortSignal) => request<{ clusters: CdCluster[] }>("GET", "/api/cd/clusters", undefined, { signal }),
  /** What a commit, a stack or a pull request would deploy. */
  plan: (ask: CdPlanAsk, signal?: AbortSignal) => request<CdPlan>("POST", "/api/cd/plans", ask, { signal }),
  /** The repositories apps deploy from, with their branches. */
  planRepos: (signal?: AbortSignal) => request<{ repos: CdPlanRepo[] }>("GET", "/api/cd/plans/repos", undefined, { signal }),
  /** Every platform kiln deploys to (docs/cd-targets.md §3), with the accounts that could serve each. */
  providers: (signal?: AbortSignal) => request<{ providers: CdProvider[] }>("GET", "/api/cd/providers", undefined, { signal }),
  /** The targets apps deploy to, connected or why not. */
  targets: (signal?: AbortSignal) => request<{ targets: CdTarget[] }>("GET", "/api/cd/targets", undefined, { signal }),
  /** What an account holds there. */
  discover: (q: Record<string, string>, signal?: AbortSignal) => request<CdDiscovery>("GET", `/api/cd/discover${qs(q)}`, undefined, { signal }),
  /** One resource as it runs, as the definition that adopts it. */
  adopt: (q: Record<string, string>, signal?: AbortSignal) => request<CdAdoption>("GET", `/api/cd/discover${qs(q)}`, undefined, { signal }),
  /** A deploy file checked as kiln reads it, provider apps rendered from `repo` at trunk. */
  validate: (body: { deploy: string; environments?: string; repo?: string }) => request<CdValidation>("POST", "/api/cd/validate", body),
  /** The apps a deploy file defines, kept in kiln rather than in the repository. */
  keep: (body: { deploy: string; environments?: string; repo: string }) => request<{ apps: string[] }>("POST", "/api/cd/ops/cd.apps.keep", body),
};

/** The route of an app's page. */
export const appHref = (id: string) => `/deployments/apps/${id.split("/").map(enc).join("/")}`;

/** The route of the deploy plans page. */
export const plansHref = "/deployments/plans";

/** The route of the new app builder. */
export const newAppHref = "/deployments/new";
