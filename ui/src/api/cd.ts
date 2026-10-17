/** Deployments (docs/cd.md): the /api/cd routes, each an operation of the server's one table. */
import { request } from "./client";
import type { CdApp, CdAppDetail, CdCluster, CdDeployment, CdDiff, CdFacets, CdResource, CdSummary } from "./types";

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
  /** A part of the app's id, any case. */
  q?: string;
}

/** The filters as the server takes them: each field's values joined by commas. */
const selector = (f: CdFilters) => ({
  sync: f.sync?.join(","),
  health: f.health?.join(","),
  cluster: f.cluster?.join(","),
  project: f.project?.join(","),
  q: f.q,
});

export const cdApi = {
  summary: (signal?: AbortSignal) => request<CdSummary>("GET", "/api/cd/summary", undefined, { signal }),
  apps: (f: CdFilters & { limit?: number; cursor?: string }, signal?: AbortSignal) =>
    request<{ apps: CdApp[]; next: string | null }>("GET", `/api/cd/apps${qs({ ...selector(f), limit: f.limit, cursor: f.cursor })}`, undefined, {
      signal,
    }),
  facets: (f: CdFilters, signal?: AbortSignal) => request<CdFacets>("GET", `/api/cd/facets${qs(selector(f))}`, undefined, { signal }),
  app: (id: string, signal?: AbortSignal) => request<CdAppDetail>("GET", appPath(id), undefined, { signal }),
  resources: (id: string, signal?: AbortSignal) =>
    request<{ resources: CdResource[] }>("GET", `${appPath(id)}/resources`, undefined, { signal }),
  diff: (id: string, signal?: AbortSignal) => request<{ diffs: CdDiff[] }>("GET", `${appPath(id)}/diff`, undefined, { signal }),
  history: (id: string, signal?: AbortSignal) =>
    request<{ history: CdDeployment[] }>("GET", `${appPath(id)}/history`, undefined, { signal }),
  sync: (id: string, body: { prune?: boolean; dry_run?: boolean; options?: string[] }) =>
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
};

/** The route of an app's page. */
export const appHref = (id: string) => `/deployments/apps/${id.split("/").map(enc).join("/")}`;
