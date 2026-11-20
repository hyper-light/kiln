/**
 * A person's environments for agent sessions (`agentenvs.*`; docs/agents.md, "Environments";
 * crates/kiln/src/harness/agentenv.rs): their named sets of variables, each picked from the
 * machine's user's, set to a value, one of their own secrets (by name: never a value), or taken
 * away, over a base (the machine's user's environment, or a clean one), each scoped to where it's
 * offered and applied on its own. A session runs with a stack of them, top first: chosen as an agent
 * starts (`agentenvs` in `agents.start`) or applied to one that runs, which starts its harness again
 * on its conversation.
 *
 * The types are the server's (ts-rs, `ui/src/api/wire/Agentenv*`).
 */
import type { Res } from "@/lib/data";
import { invalidate } from "@/lib/resource";
import { ApiError, request } from "./client";

const enc = encodeURIComponent;

import type { Agentenv } from "./wire/Agentenv";
import type { AgentenvApplied } from "./wire/AgentenvApplied";
import type { AgentenvBase } from "./wire/AgentenvBase";
import type { AgentenvBrief } from "./wire/AgentenvBrief";
import type { AgentenvChip } from "./wire/AgentenvChip";
import type { AgentenvMachineVar } from "./wire/AgentenvMachineVar";
import type { AgentenvSession } from "./wire/AgentenvSession";
import type { AgentenvVar } from "./wire/AgentenvVar";

export type { Agentenv, AgentenvBase, AgentenvChip, AgentenvSession, AgentenvVar };
export type { AgentenvFacts } from "./wire/AgentenvFacts";
export type { AgentenvScopeKey } from "./wire/AgentenvScopeKey";
export type { AgentenvStackEntry } from "./wire/AgentenvStackEntry";
export type { AgentenvFrom } from "./wire/AgentenvFrom";
export type { AgentenvSource } from "./wire/AgentenvSource";
/** A variable a machine's user has, by name alone. */
export type MachineVar = AgentenvMachineVar;
/** A session's environment as its runner holds it, as `AgentInfo.agentenv` says it: a stack's ids joined by `+`. */
export type AgentenvRef = AgentenvBrief;

/** What `agentenvs.create` takes. */
export interface AgentenvAsk {
  name: string;
  description?: string | null;
  base: AgentenvBase;
  vars: AgentenvVar[];
  /** Its own labels, in order. */
  tags?: string[];
  /** Where it's offered and applied on its own; none, everywhere. */
  scope?: AgentenvChip[];
  /** Applied on its own to the sessions its scope matches. */
  auto?: boolean;
}

/** What `agentenvs.apply` answers. */
export type Applied = AgentenvApplied;

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
  /** Session `session`'s whole stack, top first (`[]`: its machine's user's), its harness started again on its conversation unless `restart` is false. */
  apply: (session: string, ids: string[], restart = true) => request<Applied>("PUT", `/api/harness/sessions/${enc(session)}/agentenv`, { agentenvs: ids, ...(restart ? {} : { restart: false }) }),
  /** What session `session` is as scopes see it, its stack, and which of the caller's match it. */
  session: (session: string, signal?: AbortSignal) => request<AgentenvSession>("GET", `/api/harness/sessions/${enc(session)}/agentenv`, undefined, { signal }),
};

/** Session `id`'s environments (`agentenvs.session`), read again as environments change. */
export const sessionEnvRes = (id: string): Res<AgentenvSession> => ({ key: `agentenvs/session/${id}`, fetch: (s) => agentenvsApi.session(id, s), opts: { on: ["agentenv"] } });

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

/** Both lists read again, and sessions' stacks (after a change here, which may change where one applies on its own: the server's event may be on its way too). */
export function relist() {
  invalidate("agentenvs/list");
  invalidate("agentenvs/choices");
  invalidate("agentenvs/session/");
}

/** What machine `target`'s user has, by name: read when a `machine` variable is picked, not before. */
export const machineVarsRes = (target: string): Res<MachineVar[]> => ({ key: `agentenvs/machine/${target}`, fetch: (s) => agentenvsApi.machine(target, s) });
