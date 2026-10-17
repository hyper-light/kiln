/** Deployments (docs/cd.md): the /api/cd routes, each an operation of the server's one table. */
import { request } from "./client";
import type {
  CdApp,
  CdAppDetail,
  CdCluster,
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
  resources: (id: string, signal?: AbortSignal) =>
    request<{ resources: CdResource[] }>("GET", `${appPath(id)}/resources`, undefined, { signal }),
  diff: (id: string, signal?: AbortSignal) => request<{ diffs: CdDiff[] }>("GET", `${appPath(id)}/diff`, undefined, { signal }),
  /** The app's objects and every live object they own. */
  tree: (id: string, signal?: AbortSignal) => request<CdTree>("GET", `${appPath(id)}/tree`, undefined, { signal }),
  /** One object in depth: live and desired manifests, diff, labels, conditions, owners. */
  resource: (id: string, k: CdResourceKey, signal?: AbortSignal) => request<CdResourceDetail>("GET", resPath(id, k), undefined, { signal }),
  events: (id: string, k: CdResourceKey, signal?: AbortSignal) =>
    request<{ events: CdEvent[] }>("GET", `${resPath(id, k)}/events`, undefined, { signal }),
  logs: (id: string, k: CdResourceKey, q: CdLogQuery, signal?: AbortSignal) =>
    request<CdLogs>("GET", `${resPath(id, k)}/logs${qs({ ...q, previous: q.previous ? "true" : undefined })}`, undefined, { signal }),
  /** The path of a WebSocket that follows the logs. */
  logsFollowPath: (id: string, k: CdResourceKey, q: CdLogQuery) => `${resPath(id, k)}/logs/ws${qs({ ...q, previous: undefined })}`,
  history: (id: string, signal?: AbortSignal) =>
    request<{ history: CdDeployment[] }>("GET", `${appPath(id)}/history`, undefined, { signal }),
  sync: (id: string, body: { prune?: boolean; dry_run?: boolean; options?: string[]; resources?: CdResourceKey[] }) =>
    request<unknown>("POST", `${appPath(id)}/sync`, body),
  /** Syncs every app the filters show. */
  syncMany: (f: CdFilters) => request<{ started: number }>("POST", "/api/cd/ops/cd.apps.sync_many", { ...selector(f), max: 100_000 }),
  refresh: (id: string, hard: boolean) => request<unknown>("POST", `${appPath(id)}/refresh${qs({ hard })}`, {}),
  /** Compares again every app the filters show. */
  refreshMany: (f: CdFilters) => request<{ refreshing: number }>("POST", "/api/cd/ops/cd.apps.refresh_many", selector(f)),
  terminate: (id: string) => request<unknown>("POST", `${appPath(id)}/terminate`, {}),
  takeOver: (id: string) => request<unknown>("POST", `${appPath(id)}/take-over`, {}),
  rollback: (id: string, body: { to?: number; prune?: boolean }) =>
    request<{ to: number; revision: string | null }>("POST", `${appPath(id)}/rollback`, body),
  follow: (id: string) => request<unknown>("POST", `${appPath(id)}/follow`, {}),
  remove: (id: string, cascade: "foreground" | "orphan") => request<unknown>("DELETE", `${appPath(id)}${qs({ cascade })}`),
  clusters: (signal?: AbortSignal) => request<{ clusters: CdCluster[] }>("GET", "/api/cd/clusters", undefined, { signal }),
  /** What a commit, a stack or a pull request would deploy. */
  plan: (ask: CdPlanAsk, signal?: AbortSignal) => request<CdPlan>("POST", "/api/cd/plans", ask, { signal }),
  /** The repositories apps deploy from, with their branches. */
  planRepos: (signal?: AbortSignal) => request<{ repos: CdPlanRepo[] }>("GET", "/api/cd/plans/repos", undefined, { signal }),
};

/** The route of an app's page. */
export const appHref = (id: string) => `/deployments/apps/${id.split("/").map(enc).join("/")}`;

/** The route of the deploy plans page. */
export const plansHref = "/deployments/plans";
