/**
 * Agent definitions (`agentdefs.*`, /api/agentdefs; crates/kiln/src/agentdefs; kiln plan
 * 01a1cd8e862e7169a074db3d5fa33f3d, P5.1, P5.1b): an agent as data, one of kiln's ten harnesses, its
 * model, instructions, tools, MCP servers, skills, limits and Agentfile, kept with every version. A
 * person's own, and kiln's (`kiln:<name>`), its UI agents replaceable by one of yours (`uses`).
 *
 * Its experiments are the server's (`agentdefs.experiments.*`, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d
 * P7.1): each splits its new sessions between deployments. What the server doesn't keep yet is read from
 * proposals (p/uicontract is building them): the sessions started from each and how fast it starts where,
 * and what an experiment's variants measure. The rows the UI draws are made of both (`AgentDef`).
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

/* ------------------------------------------------ experiments (agentdefs.experiments.*, P7.1) */

/** A measure of one variant: a 95% interval of its median and the median, [low, mid, high]. */
export type Interval = [number, number, number];
/** What a variant's sessions are measured by. */
export type Measure = "warm_ms" | "peak_gib" | "failed_pct";
/**
 * A variant (crates/kiln/src/agentdefs/experiments, `Variant`): a deployment new sessions start in, and its
 * share of them. Its id is never shown: it reads as its swatch and its name.
 */
export interface Variant {
  id: string;
  /** The name a person gave it; none: what sets it apart from the others (views/harness/variants.ts). */
  name: string | null;
  engine: "microvm" | "host";
  /** A microVM's Agentfile: `agentfile:<id>` (one of yours) or `harness-<harness>` (kiln's); none on the host. */
  agentfile: string | null;
  /** A microVM's vCPUs; none on the host. */
  cpus?: number;
  /** A microVM's memory, in bytes; none on the host. */
  memory?: number;
  /** Its own model; none: its definition's. */
  model?: string;
  /** Percent of new sessions it takes; the variants' weights sum to 100. */
  weight: number;
  /** How many sessions it was drawn for started. */
  sessions: number;
  /** What new sessions start on now: the experiment's first variant, until one is promoted, then that one. */
  current: boolean;
  /** What its sessions measure: none yet (the server's next part of P7.1), and nothing is shown for it until then. */
  measures?: Record<Measure, Interval>;
  /** Each measure's median, day by day since its experiment started, oldest first; none yet, as `measures`. */
  trend?: Partial<Record<Measure, number[]>>;
}
/** How a criterion compares its query's value with its bound: kiln's monitors' comparators (kiln-obs monitor.rs `Op`). */
export type Op = ">" | ">=" | "==" | "!=" | "<" | "<=";
/** Whether `v` passes `bound` by `op`, as kiln's monitors decide it. */
export function passes(op: Op, v: number, bound: number): boolean {
  return op === ">" ? v > bound : op === ">=" ? v >= bound : op === "<" ? v < bound : op === "<=" ? v <= bound : op === "==" ? v === bound : v !== bound;
}
export interface Criterion {
  id: string;
  /** What its person calls it. */
  name: string;
  /**
   * What it measures, in kiln's monitoring query syntax (Datadog's, as its monitors take it): a time and
   * space aggregation of a metric, or a formula of several with functions, its scope, by variant
   * (`avg(last_7d):avg:kiln.session.warm_ms{experiment:…} by {variant}`).
   */
  query: string;
  op: Op;
  bound: number;
}
/** Each measure kiln keeps of a session, as its monitoring metric. */
export const METRIC: Record<Measure, string> = { warm_ms: "kiln.session.warm_ms", peak_gib: "kiln.session.peak_memory_gib", failed_pct: "kiln.session.failed_pct" };
/** A criterion's query over a measure, scoped to its experiment, by variant. */
export const queryOf = (m: Measure, experiment: string) => `avg(last_7d):avg:${METRIC[m]}{experiment:${experiment}} by {variant}`;
/** The measure a query reads, when it reads one of kiln's own (the first metric named in it). */
export function measureOf(query: string): Measure | null {
  const metric = /([a-z_][\w.]*)\{/i.exec(query)?.[1];
  return (Object.keys(METRIC) as Measure[]).find((m) => METRIC[m] === metric) ?? null;
}
/**
 * An experiment (`agentdefs.experiments.*`' answer): a split of a definition's new sessions between deployments,
 * its variants, judged by its criteria. A definition may have several, one of them live at a time.
 */
export interface Experiment {
  id: string;
  /** Its agent definition. */
  agentdef: string;
  /** What its person named it. */
  name: string;
  state: "live" | "paused" | "stopped" | "promoted";
  /** Since when it's stood so (Unix ms). */
  state_at: number;
  /** Its split's version: 1, and one more with each change of its weights. */
  version: number;
  /** How many sessions it drew started. */
  sessions: number;
  /** When it started (Unix ms). */
  started_at: number;
  /** The variant it promoted, once it did. */
  promoted?: string;
  /** Who started it. */
  by: string;
  criteria: Criterion[];
  variants: Variant[];
}
/** A variant as a new experiment asks for it, its `weight` a whole percent: a microVM's Agentfile and size (each left out, its definition's), or the host's engine with neither. */
export interface VariantAsk {
  weight: number;
  name?: string;
  engine?: "microvm" | "host";
  agentfile?: string;
  cpus?: number;
  memory?: number;
  model?: string;
}
/** A criterion as it's added: its query without its comparison, which is its `op` and `bound`. */
export type CriterionAsk = Omit<Criterion, "id">;

/* ------------------------------------------------ proposals (p/uicontract), until the server has them */

/** Where its sessions run and how fast they start: its engine, warm hosts kept of the target, start p50 by kind (ms). */
/**
 * What a definition's sessions' starts took (agentdefs.sessions' `starts`, crates/kiln/src/agentdefs/sessions.rs): how
 * many started; by engine, how many and each's median start in milliseconds; by how each microVM started (`cold`,
 * `template`, `warm`, `swapped`, or a word kiln doesn't know yet), how many, how many were timed, the median in
 * microseconds and how many each of Shards' words gave; and its warm pool's hosts, where its runners' Hex nodes report
 * them (none does yet: kiln plan 01a1dd2e…, P3.1, P3.2).
 */
export interface Starts {
  n: number;
  by_kind: Partial<Record<"host" | "microvm", { n: number; timed: number; p50_ms: number | null }>>;
  by_start: Record<string, { n: number; timed: number; p50_us: number | null; from: Record<string, number> }>;
  warm: { warm: number; target: number } | null;
}
/** One of its sessions (agentdefs.sessions' `sessions`), newest first: `gone` once kiln no longer has it; its variant where an experiment gave it one. */
export interface DefSession {
  id: string;
  version: number;
  at?: number;
  runner?: string;
  state?: string;
  engine?: "host" | "microvm" | null;
  agent?: string | null;
  variant?: string;
}
interface DefSessions {
  sessions: DefSession[];
  starts: Starts;
}

/** Where its sessions ran, the engine more ran in first (`by_kind`): none for one that's run nowhere yet. */
export const enginesOf = (s: Starts): ("microvm" | "host")[] =>
  (["microvm", "host"] as const).filter((k) => (s.by_kind[k]?.n ?? 0) > 0).sort((a, b) => (s.by_kind[b]?.n ?? 0) - (s.by_kind[a]?.n ?? 0));
/** A kind of start's median in milliseconds, of those timed (`by_start`'s microseconds): none where none was timed. */
export const startMs = (s: Starts, kind: string): number | undefined => {
  const us = s.by_start[kind]?.p50_us;
  return us == null ? undefined : us / 1000;
};

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
  sessions: DefSession[];
  /** Yours: when it was made (Unix seconds); kiln's own have none. */
  created_at?: number;
  runs: Starts;
  /** Its experiments, newest first: none, or several at once. */
  experiments: Experiment[];
}

const NONE: Starts = { n: 0, by_kind: {}, by_start: {}, warm: null };
/** kiln's own's kind, from its id as the server names them (crates/kiln/src/agentdefs/builtin.rs). */
const kindOf = (b: Builtin): AgentDef["kind"] => (b.id.startsWith("kiln:harness-") ? "harness" : b.id.startsWith("kiln:kiln-ci-") ? "skill" : "ui");

/** The rows: yours (newest change first), then kiln's, each with what the proposals say of it. */
export function compose(list: AgentdefsList, sessions: Record<string, DefSessions>, experiments: Record<string, Experiment[]>): AgentDef[] {
  const byId = new Map(list.agents.map((a) => [a.id, a]));
  const standsIn = new Map(Object.entries(list.uses).map(([builtin, own]) => [own, builtin]));
  const of = (id: string) => ({ sessions: sessions[id]?.sessions ?? [], runs: sessions[id]?.starts ?? NONE, experiments: experiments[id] ?? [] });
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
      created_at: a.created_at,
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

/** Experiment `e` of definition `def`'s route, `tail` after it. */
const experimentPath = (def: string, e: string, tail = "") => `/api/agentdefs/${enc(def)}/experiments/${enc(e)}${tail}`;
/** Experiment `e` paused, resumed or stopped: as it is now. */
const moved = (def: string, e: string, to: "pause" | "resume" | "stop") => request<{ experiment: Experiment }>("POST", experimentPath(def, e, `/${to}`)).then((r) => r.experiment);

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
  /**
   * Its experiments (`agentdefs.experiments.*`, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1): every change
   * answers the experiment as it is now. kiln's own definitions take none (403); another person's is as none (404);
   * a second live one on a definition is refused (409, naming the live one); a variant kiln can't run is too (422).
   */
  experiments: {
    /** Every one of yours by definition, each's newest first; `id`: one definition's; `all`: everyone's (the server's owner, and those with agentdefs.manage). */
    list: (signal?: AbortSignal, q: { id?: string; all?: boolean } = {}) => {
      const p = new URLSearchParams();
      if (q.id) p.set("id", q.id);
      if (q.all) p.set("all", "true");
      const s = p.toString();
      return request<Record<string, Experiment[]>>("GET", `/api/agentdef-experiments${s ? `?${s}` : ""}`, undefined, { signal });
    },
    /** Live at once: its 2 to 100 variants' weights add up to 100, its first what the definition runs now. */
    create: (def: string, ask: { name: string; variants: VariantAsk[]; criteria?: CriterionAsk[] }) =>
      request<{ experiment: Experiment }>("POST", `/api/agentdefs/${enc(def)}/experiments`, ask).then((r) => r.experiment),
    rename: (def: string, e: string, name: string) => request<{ experiment: Experiment }>("PATCH", experimentPath(def, e), { name }).then((r) => r.experiment),
    /** A variant renamed; empty: back to what sets it apart. */
    renameVariant: (def: string, e: string, variant: string, name: string) =>
      request<{ experiment: Experiment }>("PATCH", experimentPath(def, e, `/variants/${enc(variant)}`), { name }).then((r) => r.experiment),
    /** Every variant's weight, over split version `base`: its next version where one changed (409 where another change moved it on). */
    weights: (def: string, e: string, weights: Record<string, number>, base: number) =>
      request<{ experiment: Experiment }>("POST", experimentPath(def, e, "/weights"), { weights, base }).then((r) => r.experiment),
    variants: {
      /** Added last, at weight 0. */
      add: (def: string, e: string, v: Omit<VariantAsk, "weight">) => request<{ experiment: Experiment }>("POST", experimentPath(def, e, "/variants"), v).then((r) => r.experiment),
      /** Its share back to the others in proportion. */
      remove: (def: string, e: string, variant: string) => request<{ experiment: Experiment }>("DELETE", experimentPath(def, e, `/variants/${enc(variant)}`)).then((r) => r.experiment),
    },
    criteria: {
      add: (def: string, e: string, c: CriterionAsk) => request<{ experiment: Experiment }>("POST", experimentPath(def, e, "/criteria"), c).then((r) => r.experiment),
      remove: (def: string, e: string, criterion: string) => request<{ experiment: Experiment }>("DELETE", experimentPath(def, e, `/criteria/${enc(criterion)}`)).then((r) => r.experiment),
    },
    /** No more draws until it's resumed. */
    pause: (def: string, e: string) => moved(def, e, "pause"),
    /** Drawn by its weights again: refused (409, naming it) while another of its definition's is live. */
    resume: (def: string, e: string) => moved(def, e, "resume"),
    /** Over for good, its definition as it is. */
    stop: (def: string, e: string) => moved(def, e, "stop"),
    /** Its deployment made the definition's next version, the split's weights kept: the experiment over, and the definition as it is now. */
    promote: (def: string, e: string, variant: string) => request<{ experiment: Experiment; agentdef: AgentDefFull }>("POST", experimentPath(def, e, "/promote"), { variant }),
    /** Gone for good, its variants, criteria and draws with it. */
    remove: (def: string, e: string) => request<{ deleted: boolean }>("DELETE", experimentPath(def, e)),
  },
  // Proposals (p/uicontract), until the server has them.
  sessions: (signal?: AbortSignal) => request<Record<string, DefSessions>>("GET", "/api/agentdef-sessions", undefined, { signal }),
};

/** The rows, from the server's list and the proposals together. */
export const agentdefsRes: Res<AgentDef[]> = {
  key: "agentdefs/list",
  fetch: async (s) => {
    const [list, sessions, experiments] = await Promise.all([agentdefsApi.list(s), agentdefsApi.sessions(s), agentdefsApi.experiments.list(s)]);
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

/** The experiment a variant is of, and where among its variants: a session's variant names one. */
export function variantIn(d: AgentDef | undefined, variant: string | undefined): { e: Experiment; i: number } | null {
  if (!d || !variant) return null;
  for (const e of d.experiments) {
    const i = e.variants.findIndex((v) => v.id === variant);
    if (i >= 0) return { e, i };
  }
  return null;
}
