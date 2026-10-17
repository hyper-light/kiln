/** Deployments (docs/cd.md): the /api/cd routes, each an operation of the server's one table. */
import { request } from "./client";
import type { CdApp, CdAppDetail, CdCluster, CdDeployment, CdDiff, CdResource, CdSummary } from "./types";

const enc = encodeURIComponent;
const appPath = (id: string) => `/api/cd/apps/${id.split("/").map(enc).join("/")}`;

function qs(params: Record<string, string | number | boolean | undefined | null>) {
  const parts = Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== null && v !== "")
    .map(([k, v]) => `${enc(k)}=${enc(String(v))}`);
  return parts.length ? `?${parts.join("&")}` : "";
}

/** Filters for the app list; all optional, combined with AND. */
export interface CdAppsQuery {
  project?: string;
  cluster?: string;
  environment?: string;
  sync?: string;
  health?: string;
  /** A substring of the app's id. */
  q?: string;
  limit?: number;
  /** The `next` of the previous page. */
  cursor?: string;
}

export const cdApi = {
  summary: (signal?: AbortSignal) => request<CdSummary>("GET", "/api/cd/summary", undefined, { signal }),
  apps: (q: CdAppsQuery, signal?: AbortSignal) =>
    request<{ apps: CdApp[]; next: string | null }>("GET", `/api/cd/apps${qs({ ...q })}`, undefined, { signal }),
  app: (id: string, signal?: AbortSignal) => request<CdAppDetail>("GET", appPath(id), undefined, { signal }),
  resources: (id: string, signal?: AbortSignal) =>
    request<{ resources: CdResource[] }>("GET", `${appPath(id)}/resources`, undefined, { signal }),
  diff: (id: string, signal?: AbortSignal) => request<{ diffs: CdDiff[] }>("GET", `${appPath(id)}/diff`, undefined, { signal }),
  history: (id: string, signal?: AbortSignal) =>
    request<{ history: CdDeployment[] }>("GET", `${appPath(id)}/history`, undefined, { signal }),
  sync: (id: string, body: { prune?: boolean; dry_run?: boolean; options?: string[] }) =>
    request<unknown>("POST", `${appPath(id)}/sync`, body),
  refresh: (id: string, hard: boolean) => request<unknown>("POST", `${appPath(id)}/refresh${qs({ hard })}`, {}),
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
