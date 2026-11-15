/**
 * Agentfiles (docs/api.md, "Agentfiles"; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, steps 30 and
 * 31): Shards' images of agents' microVMs, a person's own kept with every version of their text, and
 * beside them, read only, those kiln ships per harness and a project's. What the Agentfiles pages
 * read and change; which one an agent starts from is views/agentfiles/choose.ts's.
 */
import type { Res } from "@/lib/data";
import { setData } from "@/lib/resource";
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

/* ------------------------------------------------ what it installs from, and its builds (P4.5, P4.6) */

/** An install source's kind, as Shards tells a `HARNESS` or `AGENT` source apart (crates/agentfile `source_of`). */
export type AgentfileSourceKind = "path" | "git-https" | "git-ssh" | "s3" | "https" | "oci";

/** An install source, as Shards reads a `HARNESS` or `AGENT` line: what pins it, and the secrets its fetch is given, by name. */
export interface AgentfileSource {
  line: number;
  directive: "HARNESS" | "AGENT";
  name: string;
  kind: AgentfileSourceKind;
  /** The source without its `#ref`. */
  source: string;
  /** A SHA-256, an S3 object's version, a Git ref (and the commit it named at the last build), or kiln's own pin for a path. */
  pin: { by: "sha256" | "version-id" | "ref" | "kiln"; value: string; resolved: string | null } | null;
  /**
   * The build secrets its fetch is given, each one of the person's own by name (never a value): those
   * its text's `LABEL kiln.secret.<ID>=user:<NAME>` labels name for it, in Shards' order (a proposal:
   * the server reads the labels, `secrets::mine::references`, and lists no sources yet).
   */
  secrets: { id: string; name: string }[];
  /** The origin its fetch alone is granted (default deny). */
  grant: string | null;
}

export type AgentfileBuildState = "built" | "building" | "queued" | "failed" | "none";

/** Its image on one runner: built from which version, building (which step), or why not. */
export interface AgentfileBuild {
  runner: string;
  /** Its OCI platform; none where it doesn't build. */
  platform: "linux/amd64" | "linux/arm64" | null;
  state: AgentfileBuildState;
  version: number;
  digest: string | null;
  size: number | null;
  took_s: number | null;
  /** Unix seconds: asked for, or done. */
  at: number | null;
  step: [number, number] | null;
  /** Shards' words when it refused; kiln's when a runner doesn't build. */
  why: string | null;
  /** While its newer image isn't built: the version sessions there start from. */
  serving: number | null;
}

/** One of its versions' images: built or not, its digest per platform, and the runners whose sessions start from it now. */
export interface AgentfileImage {
  version: number;
  built: boolean;
  digests: Record<string, string>;
  size: number | null;
  at: number;
  runners: string[];
  current: boolean;
}

export interface AgentfileBuilds {
  version: number;
  sources: AgentfileSource[];
  builds: AgentfileBuild[];
  images: AgentfileImage[];
}

export const agentfileBuildsApi = {
  get: (id: string, signal?: AbortSignal) => request<AgentfileBuilds>("GET", `/api/agentfiles/${enc(id)}/builds`, undefined, { signal }),
  /** Its current version built there, or on every runner that builds (without `runner`). */
  build: (id: string, runner?: string) => request<AgentfileBuilds>("POST", `/api/agentfiles/${enc(id)}/builds`, runner ? { runner } : {}),
};

/** A build asked for and not done: queued or under way. */
export const moving = (b: AgentfileBuilds) => b.builds.some((x) => x.state === "queued" || x.state === "building");

/** Its sources and builds, as its versions change. */
export const agentfileBuildsRes = (id: string): Res<AgentfileBuilds> => ({
  key: `agentfiles/builds/${id}`,
  fetch: (s) => agentfileBuildsApi.get(id, s),
  opts: { on: ["agentfile"], relevant: (ev) => ev.type === "agentfile" && ev.id === id },
});

/**
 * The same, read every 2 s while a build moves: shown in its place only then, and once every build
 * has settled it hands what it read to `agentfileBuildsRes`, which shows again and ends the reading.
 */
export const agentfileBuildingRes = (id: string): Res<AgentfileBuilds> => ({
  key: `agentfiles/building/${id}`,
  fetch: async (s) => {
    const b = await agentfileBuildsApi.get(id, s);
    if (!moving(b)) setData<AgentfileBuilds>(`agentfiles/builds/${id}`, () => b);
    return b;
  },
  opts: { pollMs: 2000 },
});
