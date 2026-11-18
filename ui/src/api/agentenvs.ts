/**
 * A person's environments for agent sessions (`agentenvs.*`; docs/agents.md, "Environments";
 * crates/kiln/src/harness/agentenv.rs): their named sets of variables, each picked from the
 * machine's user's, set to a value, one of their own secrets (by name: never a value), or taken
 * away, over a base (the machine's user's environment, or a clean one). Chosen as an agent starts
 * (`agentenv` in `agents.start`) or applied to one that runs, which starts its harness again on its
 * conversation.
 *
 * The types mirror the Rust structs as they go on the wire (written here until ts-rs makes them).
 */
import type { Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import { ApiError, request } from "./client";

const enc = encodeURIComponent;

/** What its variables go over (`agentenv::Base`). */
export type AgentenvBase = "user" | "clean";

/** Where a variable's value comes from (`agentenv::From`). */
export type AgentenvFrom = "machine" | "value" | "secret" | "unset";

/** One variable as its owner wrote it (`agentenv::Var`): a secret by its name, never its value. */
export interface AgentenvVar {
  name: string;
  from: AgentenvFrom;
  /** `machine`: the machine's variable it takes, when not its own name. */
  of?: string;
  /** `value`: its value. */
  value?: string;
  /** `secret`: the owner's secret's name. */
  secret?: string;
}

/** An environment as `agentenvs.list` and `.get` answer. */
export interface Agentenv {
  id: string;
  owner: string;
  name: string;
  description?: string;
  base: AgentenvBase;
  /** In order: a later one of a name wins. */
  vars: AgentenvVar[];
  /** Up with each change; a session says the version it started with. */
  version: number;
  /** Unix seconds. */
  created_at: number;
  updated_at: number;
  /** The ids of the sessions running with it. */
  sessions: string[];
}

/** Where a machine's user's variable is set (`agentenv::Source`). */
export type AgentenvSource = "login_shell" | "launchd" | "systemd_user" | "pam" | "user_registry" | "system_registry" | "runner";

/** A variable a machine's user has, by name alone (`agentenv::MachineVar`). */
export interface MachineVar {
  name: string;
  sources: AgentenvSource[];
}

/** A session's environment, as `AgentInfo.agentenv` says it. */
export interface AgentenvRef {
  id: string;
  name: string;
  version: number;
}

/** What `agentenvs.create` takes. */
export interface AgentenvAsk {
  name: string;
  description?: string;
  base: AgentenvBase;
  vars: AgentenvVar[];
}

/** What `agentenvs.apply` answers. */
export interface Applied {
  agent: unknown;
  agentenv: AgentenvRef | null;
  restarted: boolean;
}

/** A refusal's kind (docs/agents.md, "Operations"): its words, the error's, are the UI's to show. */
export type AgentenvRefusal = "agentenv_unknown" | "agentenv_name_taken" | "agentenv_stale" | "agentenv_in_use" | "agentenv_secret_missing" | "agentenv_secret_in_microvm" | "agentenv_bad_var";

/** The kind of a refusal `e` is, where it's one of these. */
export function refusalOf(e: unknown): AgentenvRefusal | null {
  const code = e instanceof ApiError ? (e.body as { code?: unknown } | undefined)?.code : undefined;
  return typeof code === "string" && code.startsWith("agentenv_") ? (code as AgentenvRefusal) : null;
}

/** A session's environment, from its entry (`AgentInfo.agentenv`; none from a server before environments). */
export function agentenvOf(a: object | undefined): AgentenvRef | null {
  const v = (a as { agentenv?: unknown } | undefined)?.agentenv;
  if (typeof v !== "object" || v === null) return null;
  const r = v as Partial<AgentenvRef>;
  return typeof r.id === "string" && typeof r.name === "string" && typeof r.version === "number" ? { id: r.id, name: r.name, version: r.version } : null;
}

export const agentenvsApi = {
  /** The caller's, by name; a server without environments has none. */
  list: (signal?: AbortSignal) =>
    request<{ agentenvs: Agentenv[] }>("GET", "/api/agentenvs", undefined, { signal }).then(
      (r) => r.agentenvs,
      (e: unknown) => {
        if (e instanceof ApiError && e.status === 404) return [];
        throw e;
      },
    ),
  get: (id: string, signal?: AbortSignal) => request<Agentenv>("GET", `/api/agentenvs/${enc(id)}`, undefined, { signal }),
  create: (ask: AgentenvAsk) => request<Agentenv>("POST", "/api/agentenvs", ask),
  /** A change on version `version`: refused (`agentenv_stale`) when it isn't its current one. */
  update: (id: string, change: Partial<AgentenvAsk> & { version: number }) => request<Agentenv>("PATCH", `/api/agentenvs/${enc(id)}`, change),
  /** Refused (`agentenv_in_use`) while a session runs with it, naming them. */
  remove: (id: string) => request<void>("DELETE", `/api/agentenvs/${enc(id)}`),
  /** The variables machine `target` (`local`, `agent:<name>`) says its user has, by name; refused while it's away. */
  machine: (target: string, signal?: AbortSignal) => request<{ vars: MachineVar[] }>("GET", `/api/agentenvs/machine?target=${enc(target)}`, undefined, { signal }).then((r) => r.vars),
  /** Session `session` on environment `id` (null: its machine's user's), its harness started again on its conversation unless `restart` is false. */
  apply: (session: string, id: string | null, restart = true) => request<Applied>("PUT", `/api/harness/sessions/${enc(session)}/agentenv`, { agentenv: id, ...(restart ? {} : { restart: false }) }),
};

/** The person's environments, kept current by `agentenv` events. */
export const agentenvsRes: Res<Agentenv[]> = { key: "agentenvs/list", fetch: (s) => agentenvsApi.list(s), opts: { on: ["agentenv"] } };

/**
 * The same, as Mission control's composer offers them: none where they can't be had (a server that
 * fails to list them), so a start never waits on that failing (as `agentfileChoicesRes`).
 */
export const agentenvChoicesRes: Res<Agentenv[]> = {
  key: "agentenvs/choices",
  fetch: (signal) =>
    agentenvsApi.list(signal).catch((e: unknown) => {
      if (signal.aborted) throw e;
      return [];
    }),
  opts: { on: ["agentenv"] },
};

/** Both lists read again (after a change here: the server's event may be on its way too). */
export function relist() {
  invalidate("agentenvs/list");
  invalidate("agentenvs/choices");
}

/** What machine `target`'s user has, by name: read when a `machine` variable is picked, not before. */
export const machineVarsRes = (target: string): Res<MachineVar[]> => ({ key: `agentenvs/machine/${target}`, fetch: (s) => agentenvsApi.machine(target, s) });
