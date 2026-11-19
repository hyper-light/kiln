/**
 * The Engine tab's Environments, decided (docs/agents.md, "Environments", "In the Engine tab"): which
 * one is this session's, how they're found (Base, Uses, Where) and sorted, and a card's draft as it's
 * edited, turned into what `agentenvs.create` and `.update` take. Pure, so engine-envs.tsx and its
 * tests share one reading.
 */
import type { Agentenv, AgentenvAsk, AgentenvBase, AgentenvFrom, AgentenvRef, AgentenvSource, AgentenvVar } from "@/api/agentenvs";

/** Where a machine's user's variable is set, in words. */
export const SOURCE_WORD: Record<AgentenvSource, string> = {
  login_shell: "login shell",
  launchd: "launchd",
  systemd_user: "systemd user",
  pam: "PAM",
  user_registry: "user registry",
  system_registry: "system registry",
  runner: "runner",
};

/** Where a variable's value comes from, in words. */
export const FROM_WORD: Record<AgentenvFrom, string> = { machine: "machine", value: "value", secret: "secret", unset: "unset" };

/** What it goes over, in a card's line and its facet. */
export const BASE_WORD: Record<AgentenvBase, string> = { user: "Machine", clean: "Clean" };

/** This session's environment: the one that lists it among its sessions, else the one its entry names. */
export function mineOf(envs: readonly Agentenv[], session: string, said: AgentenvRef | null): Agentenv | undefined {
  return envs.find((e) => e.sessions.includes(session)) ?? (said ? envs.find((e) => e.id === said.id) : undefined);
}

/** A facet whose things may have several values each (an environment uses machine variables and secrets). */
export interface FacetOf<T> {
  id: string;
  label: string;
  of: (x: T) => string[];
  word: (v: string) => string;
  /** Its values in this order (else by their words). */
  order?: string[];
}

/** Chosen values by facet. */
export type Chosen = Record<string, string[] | undefined>;

/** It's shown: what's typed is in its words, and for each facet chosen it has one of the values chosen (any of a facet's, all facets). */
export function shows<T>(x: T, words: string, q: string, chosen: Chosen, facets: FacetOf<T>[], except?: string): boolean {
  const typed = q.trim().toLowerCase();
  if (typed && !words.toLowerCase().includes(typed)) return false;
  return facets.every((f) => f.id === except || !chosen[f.id]?.length || f.of(x).some((v) => chosen[f.id]!.includes(v)));
}

/** Each facet's options, each with how many it would show, the other facets as they are. */
export function facetsFor<T>(all: readonly T[], wordsOf: (x: T) => string, q: string, chosen: Chosen, facets: FacetOf<T>[]) {
  return facets.map((f) => {
    const values = [...new Set(all.flatMap(f.of))];
    const rank = (v: string) => (f.order ? f.order.indexOf(v) : -1);
    values.sort((a, b) => rank(a) - rank(b) || f.word(a).localeCompare(f.word(b)));
    return {
      id: f.id,
      label: f.label,
      options: values.map((v) => ({ id: v, label: f.word(v), count: all.filter((x) => f.of(x).includes(v) && shows(x, wordsOf(x), q, chosen, facets, f.id)).length })),
    };
  });
}

const USES_WORD: Record<string, string> = { machine: "Machine variables", value: "Values", secret: "Secrets" };
const WHERE_WORD: Record<string, string> = { this: "This session", others: "Other sessions", none: "Unused" };

/** Base, what it uses (machine variables, values, secrets), where it runs (this session, others, none) and its tags, for session `session`. */
export function envFacets(session: string): FacetOf<Agentenv>[] {
  return [
    { id: "base", label: "Base", of: (e) => [e.base], word: (v) => BASE_WORD[v as AgentenvBase] ?? v, order: ["user", "clean"] },
    { id: "uses", label: "Uses", of: (e) => [...new Set(e.vars.map((v) => v.from).filter((f) => f !== "unset"))], word: (v) => USES_WORD[v] ?? v, order: ["machine", "value", "secret"] },
    {
      id: "where",
      label: "Where",
      of: (e) => {
        const here = e.sessions.includes(session);
        const others = e.sessions.some((s) => s !== session);
        return here || others ? [...(here ? ["this"] : []), ...(others ? ["others"] : [])] : ["none"];
      },
      word: (v) => WHERE_WORD[v] ?? v,
      order: ["this", "others", "none"],
    },
    { id: "tags", label: "Tags", of: (e) => e.tags, word: (v) => v },
  ];
}

/** An environment's words, for the filter's text: its name, description and variables' names, and the secrets it names. */
export const envWords = (e: Agentenv) => [e.name, e.description ?? "", ...e.tags, ...e.vars.map((v) => `${v.name} ${v.of ?? ""} ${v.secret ?? ""}`)].join(" ");

export type EnvSort = "newest" | "oldest" | "name" | "name-desc" | "vars" | "sessions";
export const ENV_SORTS: { value: EnvSort; label: string }[] = [
  { value: "newest", label: "Newest" },
  { value: "oldest", label: "Oldest" },
  { value: "name", label: "Name A–Z" },
  { value: "name-desc", label: "Name Z–A" },
  { value: "vars", label: "Most variables" },
  { value: "sessions", label: "Most sessions" },
];
const byName = (x: Agentenv, y: Agentenv) => x.name.localeCompare(y.name);
export const ENV_ORDER: Record<EnvSort, (x: Agentenv, y: Agentenv) => number> = {
  newest: (x, y) => y.created_at - x.created_at || byName(x, y),
  oldest: (x, y) => x.created_at - y.created_at || byName(x, y),
  name: byName,
  "name-desc": (x, y) => byName(y, x),
  vars: (x, y) => y.vars.length - x.vars.length || byName(x, y),
  sessions: (x, y) => y.sessions.length - x.sessions.length || byName(x, y),
};

/* ------------------------------------------------ a card's draft */

/** A variable as it's edited: `pick` the machine's variable a `machine` one takes; each kind's field kept as others are picked. */
export interface DraftVar {
  /** This draft's own key for it (rows move). */
  key: string;
  name: string;
  from: AgentenvFrom;
  pick: string;
  value: string;
  secret: string;
}

export interface Draft {
  name: string;
  description: string;
  base: AgentenvBase;
  vars: DraftVar[];
  /** The version it was made from (none: a new one). */
  version?: number;
}

let nextKey = 0;
const key = () => `v${++nextKey}`;

/** A variable to fill in. */
export const blankVar = (): DraftVar => ({ key: key(), name: "", from: "value", pick: "", value: "", secret: "" });

/** A draft of `e` as it is, or of a new one. */
export function draftOf(e?: Agentenv): Draft {
  if (!e) return { name: "", description: "", base: "user", vars: [] };
  return {
    name: e.name,
    description: e.description ?? "",
    base: e.base,
    version: e.version,
    vars: e.vars.map((v) => ({ key: key(), name: v.name, from: v.from, pick: v.from === "machine" ? (v.of ?? v.name) : "", value: v.value ?? "", secret: v.secret ?? "" })),
  };
}

/** A name an environment may hold (`agentenv::valid_name`: POSIX's portable set). */
export const validName = (n: string) => /^[A-Za-z_][A-Za-z0-9_]*$/.test(n);

/** A draft's variable as it's kept: a `machine` one names what it takes only when that isn't its own name. */
export function varOf(d: DraftVar): AgentenvVar {
  const name = d.name.trim();
  switch (d.from) {
    case "machine": {
      const of = d.pick.trim();
      return of && of !== name ? { name, from: "machine", of } : { name, from: "machine" };
    }
    case "value":
      return { name, from: "value", value: d.value };
    case "secret":
      return { name, from: "secret", secret: d.secret };
    case "unset":
      return { name, from: "unset" };
  }
}

/** What's in the way of saving it, in words (none: it can go). */
export function problemOf(d: Draft): string | null {
  if (!d.name.trim()) return "Name it";
  for (const [i, v] of d.vars.entries()) {
    const n = v.name.trim();
    if (!n) return `Variable ${i + 1} has no name`;
    if (!validName(n)) return `${n} isn't a variable's name: letters, digits and _ only, not starting with a digit`;
    if (v.from === "secret" && !v.secret) return `Pick the secret ${n} is given`;
    if (v.from === "machine" && !v.pick.trim()) return `Pick the machine's variable ${n} takes`;
  }
  return null;
}

/** What `agentenvs.create` takes. */
export function askOf(d: Draft): AgentenvAsk {
  const description = d.description.trim();
  return { name: d.name.trim(), ...(description ? { description } : {}), base: d.base, vars: d.vars.map(varOf) };
}

/** What `agentenvs.update` takes: what changed from `e`, on the version the draft was made from. */
export function changeOf(e: Agentenv, d: Draft): Partial<AgentenvAsk> & { version: number } {
  const a = askOf(d);
  return {
    version: d.version ?? e.version,
    ...(a.name !== e.name ? { name: a.name } : {}),
    ...((a.description ?? "") !== (e.description ?? "") ? { description: a.description ?? "" } : {}),
    ...(a.base !== e.base ? { base: a.base } : {}),
    ...(JSON.stringify(a.vars) !== JSON.stringify(e.vars) ? { vars: a.vars } : {}),
  };
}

/** Variable `i` moved by `by` (−1 up, +1 down), the others as they were. */
export function moved<T>(list: readonly T[], i: number, by: -1 | 1): T[] {
  const j = i + by;
  if (j < 0 || j >= list.length) return [...list];
  const next = [...list];
  [next[i], next[j]] = [next[j]!, next[i]!];
  return next;
}

/** What a kept variable gives, in words: a value as it is, a secret by its name (never its value), the machine's variable it takes. */
export function givesOf(v: AgentenvVar): string {
  switch (v.from) {
    case "machine":
      return `$${v.of ?? v.name}`;
    case "value":
      return v.value ?? "";
    case "secret":
      return v.secret ?? "";
    case "unset":
      return "taken away";
  }
}
