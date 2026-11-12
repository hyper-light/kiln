/**
 * Agentfiles (docs/api.md, "Agentfiles"; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, steps 30 and
 * 31): Shards' images of agents' microVMs, a person's own kept with every version of their text, and
 * beside them, read only, those kiln ships per harness and a project's. What the Agentfiles pages
 * read and change; which one an agent starts from is views/agentfiles/choose.ts's.
 */
import type { Res } from "@/lib/data";
import { ApiError, request } from "./client";
import type { Agentfile, AgentfileCheck, AgentfileListing, AgentfileSaved, AgentfileVersion } from "./wire";

const enc = encodeURIComponent;

export const agentfilesApi = {
  /** Yours, newest change first, without their texts; kiln's beside them, and with `repo` that project's. */
  list: (repo?: string, signal?: AbortSignal) => request<AgentfileListing>("GET", `/api/agentfiles${repo ? `?repo=${enc(repo)}` : ""}`, undefined, { signal }),
  get: (id: string, signal?: AbortSignal) => request<Agentfile>("GET", `/api/agentfiles/${enc(id)}`, undefined, { signal }),
  /** Version 1, once Shards passes its text (refused: a 422 in Shards' words). */
  create: (name: string, text: string) => request<AgentfileSaved>("POST", "/api/agentfiles", { name, text }),
  /** A new text is a new version; a new name alone is none. */
  update: (id: string, change: { name?: string; text?: string }) => request<AgentfileSaved>("PATCH", `/api/agentfiles/${enc(id)}`, change),
  /** Every version of its text, newest first. */
  versions: (id: string, signal?: AbortSignal) => request<{ versions: AgentfileVersion[] }>("GET", `/api/agentfiles/${enc(id)}/versions`, undefined, { signal }).then((r) => r.versions),
  /** That version's text current again, as a new version. */
  restore: (id: string, version: number) => request<AgentfileSaved>("POST", `/api/agentfiles/${enc(id)}/restore`, { version }),
  remove: (id: string) => request<{ deleted: boolean }>("DELETE", `/api/agentfiles/${enc(id)}`),
  /** What Shards says of a text, keeping nothing: the editor's lint. */
  check: (text: string, signal?: AbortSignal) => request<AgentfileCheck>("POST", "/api/agentfiles/check", { text }, { signal }),
};

/** The list, yours and kiln's (and a project's, with `repo`), as the server's events change it. */
export const agentfilesRes = (repo = ""): Res<AgentfileListing> => ({ key: `agentfiles/list/${repo}`, fetch: (s) => agentfilesApi.list(repo || undefined, s), opts: { on: ["agentfile"] } });

/** One of yours; "gone" for one deleted, another's, or a wrong link. */
export const agentfileRes = (id: string): Res<Agentfile | "gone"> => ({
  key: `agentfiles/one/${id}`,
  fetch: (s) =>
    agentfilesApi.get(id, s).catch((e: unknown) => {
      if (e instanceof ApiError && e.status === 404) return "gone" as const;
      throw e;
    }),
  opts: { on: ["agentfile"], relevant: (ev) => ev.type === "agentfile" && ev.id === id },
});

/** Its versions, newest first. */
export const agentfileVersionsRes = (id: string): Res<AgentfileVersion[]> => ({
  key: `agentfiles/versions/${id}`,
  fetch: (s) => agentfilesApi.versions(id, s),
  opts: { on: ["agentfile"], relevant: (ev) => ev.type === "agentfile" && ev.id === id },
});
