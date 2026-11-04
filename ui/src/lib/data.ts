/**
 * The server data more than one page shows, read through Suspense (lib/resource.ts) under one key
 * each, so a page that comes back is there at once and every page shows the same thing. Each one
 * stays live: events the server sends are applied in place (cheaper than reading it all again),
 * and a slow poll and every reconnection read it again in case one was missed.
 *
 * A section that reads several of them `warm`s them all first, so they load at once.
 */
import { create } from "zustand";
import { api } from "@/api/client";
import { events, useServerEvents } from "@/api/events";
import { harnessApi, listed, type AgentInfo } from "@/api/harness";
import type { Agent, AiSession, HostSample, MetricsHistory, Overview, Repo, Run, TerminalSessionInfo, UpdateJob, UpdatesOverview } from "@/api/types";
import { loaded, preload, useSuspenseResource, type ResourceOptions } from "./resource";

/** What a key loads, and how. */
export interface Res<T> {
  key: string;
  fetch: (signal: AbortSignal) => Promise<T>;
  opts?: ResourceOptions<T>;
}

/** Start loading these now, without waiting (a section's keys, before it reads the first). */
export function warm(...list: Res<unknown>[]) {
  for (const r of list) preload(r.key, r.fetch, r.opts);
}

/** Settles once one has its first data: a route's loader, so its page shows no fallback. */
export function ready<T>(r: Res<T>) {
  return loaded(r.key, r.fetch, r.opts);
}

/** Read one (suspends until its first data is in). */
export function useRes<T>(r: Res<T>) {
  return useSuspenseResource(r.key, r.fetch, r.opts);
}

/** Merge or insert an entity into a list by key. */
export function upsert<T>(list: T[] | undefined, item: T, key: (t: T) => string, prepend = true): T[] {
  const arr = list ?? [];
  const k = key(item);
  const i = arr.findIndex((x) => key(x) === k);
  if (i === -1) return prepend ? [item, ...arr] : [...arr, item];
  const copy = arr.slice();
  copy[i] = item;
  return copy;
}

/** Name → label, for the machines that have one. */
export type Labels = Readonly<Record<string, string>>;

/**
 * The names people see for the machines kiln knows by their machine id (docs/machines.md): name →
 * label, from every load of the agents and from their events. Kept outside any page (as
 * `useUpdateJobs` is), so words made outside a component (`labelOf`) can say a machine's label,
 * where all they have is its name.
 */
export const useMachineLabels = create<Labels>(() => ({}));
events.on(["agent"], (ev) => {
  // A machine renamed: its event carries it whole, its label too.
  noteLabels([ev.agent]);
});

/** Keeps the labels of the agents that have one; the labels change (and what shows them re-renders) only when one did. */
export function noteLabels(agents: readonly { name: string; label?: string | null }[]) {
  const was = useMachineLabels.getState();
  let now: Record<string, string> | null = null;
  for (const a of agents) {
    if (!a.label || labelOf(a.name, now ?? was) === a.label) continue;
    now ??= { ...was };
    now[a.name] = a.label;
  }
  if (now) useMachineLabels.setState(now, true);
}

/** The name people see for machine `name`: its label, or (one known by its name, or not known here yet) the name itself. */
export function labelOf(name: string, labels: Labels = useMachineLabels.getState()): string {
  return (Object.hasOwn(labels, name) ? labels[name] : undefined) ?? name;
}

/**
 * Words a machine wrote naming another by the id kiln knows it by ("Moved here from <id>": a runner
 * has only the ids): each runner's id in them, the name it's shown by now (docs/machines.md). Other
 * ids (a session's, a plan's) have no label, and stay.
 */
export function withLabels(text: string, labels: Labels = useMachineLabels.getState()): string {
  return text.replace(/\b[0-9a-f]{32}\b/g, (id) => labelOf(id, labels));
}

/** Machines' names in the order people read them: by the names they're shown by. */
export const byLabel =
  (labels: Labels = useMachineLabels.getState()) =>
  (a: string, b: string): number =>
    labelOf(a, labels).localeCompare(labelOf(b, labels));

/** The name people see for a machine (an agent, or reach's runner): its label, else its name. Its `name` stays its key. */
export const shownName = (m: { name: string; label?: string | null }): string => m.label || m.name;

/**
 * The labels, for a component that shows machines by name only: it re-renders as they come. They
 * come with the agents, loaded here when no page has loaded them yet.
 */
export function useLabels(): Labels {
  warm(agentsRes);
  return useMachineLabels();
}

/** The agents, their labels noted on the way (`labelOf`). */
const agentsNoted = (signal?: AbortSignal) =>
  api.agents(signal).then((list) => {
    noteLabels(list);
    return list;
  });

export const overviewRes: Res<Overview> = { key: "overview", fetch: (s) => api.overview(s), opts: { pollMs: 30_000 } };
export const agentsRes: Res<Agent[]> = { key: "agents", fetch: agentsNoted, opts: { pollMs: 30_000 } };
export const historyRes: Res<MetricsHistory> = { key: "metrics/history", fetch: (s) => api.metricsHistory(undefined, s), opts: { pollMs: 120_000 } };
export const reposRes: Res<Repo[]> = { key: "repos", fetch: (s) => api.repos(s), opts: { pollMs: 30_000 } };
export const recentRunsRes = (limit: number): Res<Run[]> => ({ key: `runs/recent/${limit}`, fetch: (s) => api.runs({ limit }, s), opts: { pollMs: 60_000 } });

/** The overview: counts, the queue, the server's subsystems and its latest sample. */
export function useOverview() {
  const ov = useRes(overviewRes);
  useServerEvents(["queue", "sample"], (ev) => {
    // `queue` also says where a land queue's entry is: the counts are the scheduler's.
    if (ev.type === "queue" && "queued" in ev) ov.setData((d) => ({ ...d, queue: { queued: ev.queued, running: ev.running } }));
    else if (ev.type === "sample" && ev.agent === "server") ov.setData((d) => ({ ...d, system: { ...d.system, sample: ev.sample } }));
  });
  return ov;
}

/** Every machine that has joined (Kubernetes job pods included: callers leave them out). */
export function useAgentsLive() {
  const agents = useRes(agentsRes);
  useServerEvents(["agent", "sample"], (ev) => {
    if (ev.type === "agent") agents.setData((l) => upsert<Agent>(l, ev.agent, (a) => a.name, false));
    else if (ev.type === "sample" && ev.agent !== "server") agents.setData((l) => (l.some((a) => a.name === ev.agent) ? l.map((a) => (a.name === ev.agent ? { ...a, sample: ev.sample } : a)) : l));
  });
  return agents;
}

/** Samples kept per machine (the last hour at 5 s is 720). */
const HISTORY_CAP = 800;

/** Every machine's last hour of samples (the server's under "server"), growing as they arrive. */
export function useMetricsHistory() {
  const hist = useRes(historyRes);
  useServerEvents(["sample"], (ev) => {
    hist.setData((h) => ({ ...h, [ev.agent]: [...(h[ev.agent] ?? []), ev.sample].slice(-HISTORY_CAP) }));
  });
  return hist;
}

/** The newest `limit` runs, newest first, kept current by run events. */
export function useRecentRuns(limit: number) {
  const runs = useRes(recentRunsRes(limit));
  useServerEvents(["run"], (ev) => {
    runs.setData((list) =>
        upsert<Run>(list, ev.run, (r) => r.id)
          .sort((a, b) => b.created_at - a.created_at)
          .slice(0, limit),
      );
  });
  return runs;
}

/** The repositories kiln builds. */
export function useRepos() {
  return useRes(reposRes);
}

/** The last `n` samples of a machine. */
export function lastSamples(h: MetricsHistory, name: string, n: number): HostSample[] {
  const all = h[name] ?? [];
  return all.length > n ? all.slice(-n) : all;
}

/** System → Updates: the server's build, each runner's, the binaries handed out. */
export const updatesRes: Res<UpdatesOverview> = { key: "updates", fetch: (s) => api.updates(s), opts: { pollMs: 30_000, on: ["updates"] } };

/** The fuller of two snapshots of one job (events and refetches can cross). */
export function fuller(a: UpdateJob | undefined, b: UpdateJob): UpdateJob {
  if (!a) return b;
  const rank = (j: UpdateJob) => (j.state === "running" ? 0 : 1) * 1e6 + j.log.length;
  return rank(b) >= rank(a) ? b : a;
}

/**
 * Upgrades as they go (their output, their end), from `update_job` events: kept outside any page,
 * so a page opened mid-upgrade shows all of it, and its output doesn't make every page read
 * System → Updates again.
 */
export const useUpdateJobs = create<Record<string, UpdateJob>>(() => ({}));
events.on(["update_job"], (ev) => {
  useUpdateJobs.setState((m) => ({ ...m, [ev.job.id]: fuller(m[ev.job.id], ev.job) }), true);
});

/** Upgrade jobs: the overview's and the live ones merged, newest first. */
export function mergeJobs(data: UpdatesOverview, live: Record<string, UpdateJob>): UpdateJob[] {
  const all: Record<string, UpdateJob> = { ...live };
  for (const j of data.jobs) all[j.id] = fuller(all[j.id], j);
  return Object.values(all).sort((a, b) => b.started_at - a.started_at);
}

/** The agents kiln runs (an older server without them: none), as their events change them. */
export const harnessSessionsRes: Res<AgentInfo[]> = {
  key: "harness/sessions",
  fetch: () => harnessApi.list().catch(() => []),
  // Each event carries the agent's whole entry (or says it closed): applied, the list isn't loaded again.
  opts: {
    on: ["harness", "harness_closed"],
    apply: (list, ev) => {
      if (ev.type === "harness_closed") return list.filter((a) => a.id !== ev.id);
      if (ev.type !== "harness") return undefined;
      if (!listed(ev.info)) return list.filter((a) => a.id !== ev.info.id);
      const i = list.findIndex((a) => a.id === ev.info.id);
      return i === -1 ? [...list, ev.info] : list.map((a, j) => (j === i ? ev.info : a));
    },
  },
};

/** The terminal sessions the server keeps (any device can attach to them). */
export const terminalSessionsRes: Res<TerminalSessionInfo[]> = { key: "terminal/sessions", fetch: (s) => api.terminalSessions(s).catch(() => []), opts: { on: ["terminals"] } };

/** The AI sessions the machines report (in kiln's terminals or not). */
export const aiSessionsRes: Res<AiSession[]> = { key: "ai-sessions", fetch: (s) => api.aiSessions(s).catch(() => []), opts: { on: ["ai_sessions"] } };
