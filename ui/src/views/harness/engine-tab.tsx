/**
 * An agent's Engine tab, for every session whatever its engine (decided by the user, 2026-11-18; kiln
 * plan 01a1cd8e862e7169a074db3d5fa33f3d, P2, P3, P5.1, P5.2; docs/agents.md, "In the Engine tab"):
 * its sections, each folding, each found by a filter with facets and sorted (as Mission control's
 * sessions are):
 *
 *   - Sandboxes, for a session in a microVM only (one on bare metal has none): the session's, its own
 *     and its kiln subagents', a card each (engine-rows.tsx): its line and controls, its resources each
 *     with its last hour, then its sections (what it is, its components, network, volumes and
 *     directories);
 *   - Environments: the person's named sets of variables a session runs with, a card each
 *     (engine-envs.tsx): its line, its variables and the sessions running with it, changed in place
 *     and applied to this session from there. This session's is first, marked;
 *   - Agents: the agents there are, the person's at first (the system's by its facet), a card each:
 *     its line, then its settings and experiments. This session's agent is always first, marked, with
 *     what this session started on.
 *
 * The sections fold, and so does each environment in its own (asked by the user, 2026-11-19).
 */
import * as React from "react";
import { agentdefsRes, enginesOf, type AgentDef } from "@/api/agentdefs";
import type { AgentInfo } from "@/api/harness";
import { sandboxesRes, type SandboxBrief } from "@/api/microvms";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { Loadable } from "@/components/ui/loading";
import { FilterBar, SelectField } from "@/components/ui/misc";
import { labelOf, useRes } from "@/lib/data";
import { DefRow } from "./definitions";
import { agentLabel } from "./derive";
import { useEngineOf } from "./engine-of";
import { Environments } from "./engine-envs";
import { EngineVm, useSandboxed, WORD } from "./engine-rows";
import { Fold } from "./fold";
import { useAgentList } from "./use-agents";

export function EngineTab({ agentId, target, isolation }: { agentId: string; target: string; isolation?: string }) {
  // Its sandboxes only in a microVM: one on bare metal has none.
  const engine = useEngineOf(agentId, isolation);
  return (
    <div className="in-secs at">
      {engine === "microvm" && (
        <Loadable label="Loading its sandboxes" fallback={null}>
          <Vms agentId={agentId} />
        </Loadable>
      )}
      <Loadable label="Loading the environments" fallback={null}>
        <Environments agentId={agentId} target={target} />
      </Loadable>
      <Loadable label="Loading the agents" fallback={null}>
        <Agents agentId={agentId} />
      </Loadable>
    </div>
  );
}

/** A session's tokens, in and out: what agents.list would say (a proposal: the mock says it, the server doesn't yet). */
const tokensOf = (a: AgentInfo | undefined) => (a as (AgentInfo & { tokens?: number }) | undefined)?.tokens ?? 0;

/** A facet: what it's called, each thing's value in it, and how a value reads. */
type FacetOf<T> = { id: string; label: string; of: (x: T) => string; word: (v: string) => string };

/** It's shown: what's typed is in its words, and each facet chosen has its value (any of a facet's, all facets). */
function shows<T>(x: T, words: string, q: string, chosen: Chosen, facets: FacetOf<T>[], except?: string): boolean {
  const typed = q.trim().toLowerCase();
  if (typed && !words.toLowerCase().includes(typed)) return false;
  return facets.every((f) => f.id === except || !chosen[f.id]?.length || chosen[f.id]!.includes(f.of(x)));
}

/** Each facet's options, each with how many it would show, the other facets as they are. */
function facetsFor<T>(all: readonly T[], wordsOf: (x: T) => string, q: string, chosen: Chosen, facets: FacetOf<T>[]): Facet[] {
  return facets.map((f) => ({
    id: f.id,
    label: f.label,
    options: [...new Set(all.map(f.of))].sort().map((v) => ({ id: v, label: f.word(v), count: all.filter((x) => f.of(x) === v && shows(x, wordsOf(x), q, chosen, facets, f.id)).length })),
  }));
}

/* ------------------------------------------------ Sandboxes */

type VmSort = "newest" | "oldest" | "active" | "tokens-desc" | "tokens-asc" | "cost-desc" | "cost-asc" | "cpu" | "memory" | "name" | "name-desc";
const VM_SORTS: { value: VmSort; label: string }[] = [
  { value: "newest", label: "Newest" },
  { value: "oldest", label: "Oldest" },
  { value: "active", label: "Last active" },
  { value: "tokens-desc", label: "Most tokens" },
  { value: "tokens-asc", label: "Fewest tokens" },
  { value: "cost-desc", label: "Highest cost" },
  { value: "cost-asc", label: "Lowest cost" },
  { value: "cpu", label: "Most CPU" },
  { value: "memory", label: "Most memory" },
  { value: "name", label: "Name A–Z" },
  { value: "name-desc", label: "Name Z–A" },
];
type Vm = { a: AgentInfo; b: SandboxBrief };
const byName = (x: Vm, y: Vm) => agentLabel(x.a).localeCompare(agentLabel(y.a));
const VM_ORDER: Record<VmSort, (x: Vm, y: Vm) => number> = {
  newest: (x, y) => y.a.created_at - x.a.created_at || byName(x, y),
  oldest: (x, y) => x.a.created_at - y.a.created_at || byName(x, y),
  active: (x, y) => y.a.last_activity - x.a.last_activity || byName(x, y),
  "tokens-desc": (x, y) => tokensOf(y.a) - tokensOf(x.a) || byName(x, y),
  "tokens-asc": (x, y) => tokensOf(x.a) - tokensOf(y.a) || byName(x, y),
  "cost-desc": (x, y) => y.a.cost_usd - x.a.cost_usd || byName(x, y),
  "cost-asc": (x, y) => x.a.cost_usd - y.a.cost_usd || byName(x, y),
  cpu: (x, y) => y.b.cpu_pct - x.b.cpu_pct || byName(x, y),
  memory: (x, y) => y.b.memory.used - x.b.memory.used || byName(x, y),
  name: byName,
  "name-desc": (x, y) => byName(y, x),
};
const VM_FACETS: FacetOf<Vm>[] = [
  { id: "state", label: "State", of: (v) => v.b.state, word: (s) => WORD[s as SandboxBrief["state"]] ?? s },
  { id: "runner", label: "Runs on", of: (v) => v.b.runner, word: (r) => labelOf(r) },
  { id: "engine", label: "Engine", of: (v) => v.b.engine, word: (e) => (e === "microvm" ? "microVM" : "host") },
];
const vmWords = (v: Vm) => `${agentLabel(v.a)} ${labelOf(v.b.runner)} ${v.b.engine}`;

/** Its sandboxes, its own and its kiln subagents', found and sorted, a card each. */
function Vms({ agentId }: { agentId: string }) {
  const list = useAgentList();
  const a = list.find((x) => x.id === agentId);
  return a ? <VmsOf a={a} list={list} /> : null;
}

function VmsOf({ a, list }: { a: AgentInfo; list: readonly AgentInfo[] }) {
  const all = useRes(sandboxesRes).data;
  const vms = useSandboxed(a, list, all).map((x) => ({ a: x, b: all[x.id]! }));
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({});
  const [sort, setSort] = React.useState<VmSort>("newest");
  const shown = vms.filter((v) => shows(v, vmWords(v), q, chosen, VM_FACETS)).sort(VM_ORDER[sort]);
  return (
    <Fold id="engine-sandboxes" title="Sandboxes" count={`${shown.length} of ${vms.length}`}>
      <FilterBar className="at-bar">
        <FacetBox
          label="Filter sandboxes"
          placeholder="Filter by name, machine, state…"
          query={q}
          onQuery={setQ}
          facets={facetsFor(vms, vmWords, q, chosen, VM_FACETS)}
          chosen={chosen}
          onChosen={setChosen}
        />
        <SelectField label="Sort sandboxes" hideLabel value={sort} onChange={(v) => setSort(v as VmSort)} options={VM_SORTS} />
      </FilterBar>
      {vms.length === 0 ? (
        <p className="in-none">No sandboxes.</p>
      ) : shown.length === 0 ? (
        <p className="in-none">No matches.</p>
      ) : (
        <ul className="ms-list eg-list" aria-label={`${agentLabel(a)}'s sandboxes`}>
          {shown.map((v) => (
            <EngineVm key={v.a.id} a={v.a} b={v.b} fixed />
          ))}
        </ul>
      )}
    </Fold>
  );
}

/* ------------------------------------------------ Agents */

type AgentSort = "newest" | "oldest" | "active" | "tokens-desc" | "tokens-asc" | "cost-desc" | "cost-asc" | "sessions" | "name" | "name-desc" | "harness";
const AGENT_SORTS: { value: AgentSort; label: string }[] = [
  { value: "newest", label: "Newest" },
  { value: "oldest", label: "Oldest" },
  { value: "active", label: "Last active" },
  { value: "tokens-desc", label: "Most tokens" },
  { value: "tokens-asc", label: "Fewest tokens" },
  { value: "cost-desc", label: "Highest cost" },
  { value: "cost-asc", label: "Lowest cost" },
  { value: "sessions", label: "Most sessions" },
  { value: "name", label: "Name A–Z" },
  { value: "name-desc", label: "Name Z–A" },
  { value: "harness", label: "Harness" },
];
/** A definition's sessions' totals: their tokens and cost, and the last any was active (0: none). */
type Totals = { tokens: number; cost: number; active: number };
type Def = { d: AgentDef; t: Totals };
const defName = (x: Def, y: Def) => x.d.name.localeCompare(y.d.name);
const AGENT_ORDER: Record<AgentSort, (x: Def, y: Def) => number> = {
  // kiln's own have no date: after every dated one, by name.
  newest: (x, y) => (y.d.created_at ?? -1) - (x.d.created_at ?? -1) || defName(x, y),
  oldest: (x, y) => (x.d.created_at ?? Infinity) - (y.d.created_at ?? Infinity) || defName(x, y),
  active: (x, y) => y.t.active - x.t.active || defName(x, y),
  "tokens-desc": (x, y) => y.t.tokens - x.t.tokens || defName(x, y),
  "tokens-asc": (x, y) => x.t.tokens - y.t.tokens || defName(x, y),
  "cost-desc": (x, y) => y.t.cost - x.t.cost || defName(x, y),
  "cost-asc": (x, y) => x.t.cost - y.t.cost || defName(x, y),
  sessions: (x, y) => y.d.sessions.length - x.d.sessions.length || defName(x, y),
  name: defName,
  "name-desc": (x, y) => defName(y, x),
  harness: (x, y) => x.d.harness.localeCompare(y.d.harness) || defName(x, y),
};
const AGENT_FACETS: FacetOf<Def>[] = [
  { id: "source", label: "Source", of: (x) => (x.d.own ? "user" : "system"), word: (v) => (v === "user" ? "User" : "System") },
  { id: "harness", label: "Harness", of: (x) => x.d.harness, word: (v) => v },
  // Where its sessions ran, the engine more ran in (agentdefs.sessions' by_kind); one that's run nowhere yet, none.
  { id: "engine", label: "Runs in", of: (x) => enginesOf(x.d.runs)[0] ?? "none", word: (v) => (v === "microvm" ? "microVM" : v === "host" ? "host" : "no sessions yet") },
];
const defWords = (x: Def) => `${x.d.name} ${x.d.harness}`;

/** The agents, found and sorted, a card each; this session's first, marked, its settings open. */
function Agents({ agentId }: { agentId: string }) {
  const defs = useRes(agentdefsRes).data;
  const list = useAgentList();
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({ source: ["user"] });
  const [sort, setSort] = React.useState<AgentSort>("newest");
  const byId = new Map(list.map((x) => [x.id, x]));
  const all: Def[] = defs.map((d) => {
    const ss = d.sessions.map((s) => byId.get(s.id)).filter((x): x is AgentInfo => x !== undefined);
    return { d, t: { tokens: ss.reduce((n, x) => n + tokensOf(x), 0), cost: ss.reduce((n, x) => n + x.cost_usd, 0), active: Math.max(0, ...ss.map((x) => x.last_activity)) } };
  });
  const mine = all.find((x) => x.d.sessions.some((s) => s.id === agentId));
  const shown = all.filter((x) => x !== mine && shows(x, defWords(x), q, chosen, AGENT_FACETS)).sort(AGENT_ORDER[sort]);
  // This session's agent is shown whatever's chosen: it's the one this chat is about.
  const showsMine = mine !== undefined;
  return (
    <Fold id="engine-agents" title="Agents" count={`${shown.length + (showsMine ? 1 : 0)} of ${all.length}`}>
      <FilterBar className="at-bar">
        <FacetBox
          label="Filter agents"
          placeholder="Filter by name, harness, source…"
          query={q}
          onQuery={setQ}
          facets={facetsFor(all, defWords, q, chosen, AGENT_FACETS)}
          chosen={chosen}
          onChosen={setChosen}
        />
        <SelectField label="Sort agents" hideLabel value={sort} onChange={(v) => setSort(v as AgentSort)} options={AGENT_SORTS} />
      </FilterBar>
      {!showsMine && shown.length === 0 ? (
        <p className="in-none">No matches.</p>
      ) : (
        <ul className="ms-list" data-list="agents-tab">
          {showsMine && <DefRow d={mine.d} list={list} session={agentId} />}
          {shown.map((x) => (
            <DefRow key={x.d.id} d={x.d} list={list} />
          ))}
        </ul>
      )}
    </Fold>
  );
}
