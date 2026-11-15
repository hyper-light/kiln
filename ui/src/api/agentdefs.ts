/**
 * Agent definitions (`agentdefs.*`, /api/agentdefs; crates/kiln/src/agentdefs; kiln plan
 * 01a1cd8e862e7169a074db3d5fa33f3d, P5.1, P5.1b): an agent as data, one of kiln's ten harnesses, its
 * model, instructions, tools, MCP servers, skills, limits and Agentfile, kept with every version. A
 * person's own, and kiln's (`kiln:<name>`), its UI agents replaceable by one of yours (`uses`).
 *
 * What the server doesn't keep yet is read from proposals (p/uicontract is building them): the sessions
 * started from each and how fast it starts where, and its experiment (P7.1). The rows the UI draws are
 * made of both (`AgentDef`).
 */
import type { Res } from "@/lib/data";
import { request } from "./client";

const enc = encodeURIComponent;

/* ------------------------------------------------ the server's shapes */

export interface Mcp {
  name: string;
  url?: string;
  command?: string[];
}
/** Its limits: time in seconds, cost in US cents, tokens in and out; none set, none. */
export interface Limits {
  seconds?: number;
  cents?: number;
  tokens?: number;
}
export interface Settings {
  harness: string;
  /** None: its harness's default. */
  model: string | null;
  instructions: string;
  tools: string[];
  mcp: Mcp[];
  skills: string[];
  limits: Limits;
  /** `agentfile:<id>` (one of yours), `harness-<harness>` (kiln's for it), or none. */
  agentfile: string | null;
}
/** One of yours, whole (`GET /api/agentdefs/:id`). Times are Unix seconds. */
export interface AgentDefFull extends Settings {
  id: string;
  /** `owner` (the server's), or `user:<id>`. */
  owner: string;
  name: string;
  version: number;
  created_at: number;
  updated_at: number;
}
/** One of yours in the list: no instructions, tools, MCP, skills or limits. */
export interface AgentDefBrief {
  id: string;
  owner: string;
  name: string;
  harness: string;
  model: string | null;
  agentfile: string | null;
  version: number;
  created_at: number;
  updated_at: number;
}
/** One of kiln's own: `kiln:<name>`, a CI skill's, a harness's as it is, or one its UI starts (`replaceable`). */
export interface Builtin extends Settings {
  id: string;
  name: string;
  about: string;
  replaceable: boolean;
  brief?: string;
}
export interface AgentdefsList {
  agents: AgentDefBrief[];
  reaches_all: boolean;
  builtin: Builtin[];
  /** kiln's UI agent → the one of yours that stands in for it. */
  uses: Record<string, string>;
}
export interface AgentDefVersion extends Settings {
  version: number;
  at: number;
  by: string;
}
/** A session started from it (`agentdefs.run`'s answer), the version it ran. */
export interface RunAnswer {
  agents: { id: string; name: string }[];
  parent: string | null;
  queued?: string;
  agentdef: { id: string; version: number };
}

/* ------------------------------------------------ proposals (p/uicontract), until the server has them */

/** A measure of one variant: a 95% interval of its median and the median, [low, mid, high]. */
export type Interval = [number, number, number];
/** A variant: a deployment new sessions start in. Its id is never shown: it reads as its swatch and its name. */
export interface Variant {
  id: string;
  /** The name a person gave it; none: what sets it apart from the others (views/harness/variants.ts). */
  name: string | null;
  agentfile: string;
  engine: "microvm" | "host";
  cpus: number;
  memory: number;
  /** Percent of new sessions it takes; the variants' weights sum to 100. */
  weight: number;
  sessions: number;
  /** What new sessions start on now. */
  current?: boolean;
  measures: { warm_ms: Interval; peak_gib: Interval; failed_pct: Interval };
}
export interface Experiment {
  id: string;
  state: "live" | "paused" | "promoted";
  version: number;
  sessions: number;
  variants: Variant[];
}
/** Where its sessions run and how fast they start: its engine, warm hosts kept of the target, start p50 by kind (ms). */
export interface Starts {
  engine: "microvm" | "host";
  warm: number;
  target: number;
  p50_ms: { warm?: number; template?: number; cold?: number };
}
interface DefSessions {
  sessions: { id: string; version: number; variant?: string }[];
  starts: Starts;
}

/* ------------------------------------------------ what the UI draws */

/**
 * A definition as its rows draw it: the server's (yours, or kiln's) with its sessions, starts and
 * experiment from the proposals; kind and stand-ins derived as the server has them (`uses`).
 */
export interface AgentDef {
  id: string;
  name: string;
  /** Yours; kiln's are read only (copy one to change it). */
  own: boolean;
  /** kiln's: a harness's as it is, a CI skill's, or one its UI starts. */
  kind?: "harness" | "skill" | "ui";
  harness: string;
  model: string | null;
  agentfile: string | null;
  version: number;
  /** kiln's: what it is. */
  about?: string;
  /** Yours stands in for this one of kiln's (its name). */
  replaces?: string;
  /** This one of kiln's is stood in for by yours (its name). */
  replaced_by?: string;
  sessions: { id: string; version: number; variant?: string }[];
  runs: Starts;
  experiment?: Experiment;
}

const NONE: Starts = { engine: "microvm", warm: 0, target: 0, p50_ms: {} };
/** kiln's own's kind, from its id as the server names them (crates/kiln/src/agentdefs/builtin.rs). */
const kindOf = (b: Builtin): AgentDef["kind"] => (b.id.startsWith("kiln:harness-") ? "harness" : b.id.startsWith("kiln:kiln-ci-") ? "skill" : "ui");

/** The rows: yours (newest change first), then kiln's, each with what the proposals say of it. */
export function compose(list: AgentdefsList, sessions: Record<string, DefSessions>, experiments: Record<string, Experiment>): AgentDef[] {
  const byId = new Map(list.agents.map((a) => [a.id, a]));
  const standsIn = new Map(Object.entries(list.uses).map(([builtin, own]) => [own, builtin]));
  const of = (id: string) => ({ sessions: sessions[id]?.sessions ?? [], runs: sessions[id]?.starts ?? NONE, experiment: experiments[id] });
  const yours: AgentDef[] = list.agents.map((a) => {
    const builtin = standsIn.get(a.id);
    return {
      id: a.id,
      name: a.name,
      own: true,
      harness: a.harness,
      model: a.model,
      agentfile: a.agentfile,
      version: a.version,
      replaces: builtin ? list.builtin.find((b) => b.id === builtin)?.name : undefined,
      ...of(a.id),
    };
  });
  const kilns: AgentDef[] = list.builtin.map((b) => {
    const own = list.uses[b.id];
    return {
      id: b.id,
      name: b.name,
      own: false,
      kind: kindOf(b),
      harness: b.harness,
      model: b.model,
      agentfile: b.agentfile,
      version: 0,
      about: b.about,
      replaced_by: own ? byId.get(own)?.name : undefined,
      ...of(b.id),
    };
  });
  return [...yours, ...kilns];
}

export const agentdefsApi = {
  list: (signal?: AbortSignal) => request<AgentdefsList>("GET", "/api/agentdefs", undefined, { signal }),
  get: (id: string, signal?: AbortSignal) => request<AgentDefFull | Builtin>("GET", `/api/agentdefs/${enc(id)}`, undefined, { signal }),
  /** Any of its settings or its name; `version` (a proposal: p/uicontract) is the one this save was based on. */
  create: (def: Partial<Settings> & { name: string; harness: string }) => request<AgentDefFull>("POST", "/api/agentdefs", def),
  update: (id: string, change: Partial<Settings> & { name?: string; version?: number }) => request<AgentDefFull>("PATCH", `/api/agentdefs/${enc(id)}`, change),
  versions: (id: string, signal?: AbortSignal) => request<{ versions: AgentDefVersion[] }>("GET", `/api/agentdefs/${enc(id)}/versions`, undefined, { signal }).then((r) => r.versions),
  restore: (id: string, version: number) => request<AgentDefFull>("POST", `/api/agentdefs/${enc(id)}/restore`, { version }),
  remove: (id: string) => request<{ deleted: boolean }>("DELETE", `/api/agentdefs/${enc(id)}`),
  run: (id: string, body: { prompt?: string; on?: string; cwd?: string; name?: string } = {}) => request<RunAnswer>("POST", `/api/agentdefs/${enc(id)}/run`, body),
  copy: (id: string, name?: string) => request<AgentDefFull>("POST", `/api/agentdefs/${enc(id)}/copy`, name ? { name } : {}),
  use: (builtin: string, id: string | null) => request<{ uses: Record<string, string> }>("POST", "/api/agentdefs/use", { builtin, id }),
  // Proposals (p/uicontract), until the server has them.
  sessions: (signal?: AbortSignal) => request<Record<string, DefSessions>>("GET", "/api/agentdef-sessions", undefined, { signal }),
  experiments: (signal?: AbortSignal) => request<Record<string, Experiment>>("GET", "/api/agentdef-experiments", undefined, { signal }),
};

/** The rows, from the server's list and the proposals together. */
export const agentdefsRes: Res<AgentDef[]> = {
  key: "agentdefs/list",
  fetch: async (s) => {
    const [list, sessions, experiments] = await Promise.all([agentdefsApi.list(s), agentdefsApi.sessions(s), agentdefsApi.experiments(s)]);
    return compose(list, sessions, experiments);
  },
};
/** Its versions, newest first. */
export const agentdefVersionsRes = (id: string): Res<AgentDefVersion[]> => ({ key: `agentdefs/versions/${id}`, fetch: (s) => agentdefsApi.versions(id, s) });
/** One whole (its instructions, tools, MCP, skills and limits), read once its values open. */
export const agentdefRes = (id: string): Res<AgentDefFull | Builtin> => ({ key: `agentdefs/one/${id}`, fetch: (s) => agentdefsApi.get(id, s) });

/** A time limit in a few words: "45m", "2m", "1h 30m"; none, none. */
export function limitWords(l: Limits): string {
  const s = l.seconds;
  if (!s) return "";
  const m = Math.round(s / 60);
  return m < 60 ? `${m}m` : `${Math.floor(m / 60)}h${m % 60 ? ` ${m % 60}m` : ""}`;
}
/** "45m", "90", "1h 30m" as seconds; none for nothing or nonsense. */
export function limitSeconds(words: string): number | null {
  const w = words.trim().toLowerCase();
  if (!w) return null;
  const hm = /^(?:(\d+)\s*h)?\s*(?:(\d+)\s*m?)?$/.exec(w);
  if (!hm || (!hm[1] && !hm[2])) return null;
  return (Number(hm[1] ?? 0) * 60 + Number(hm[2] ?? 0)) * 60;
}
