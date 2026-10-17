/**
 * The server data more than one page shows, read through Suspense (lib/resource.ts) under one key
 * each, so a page that comes back is there at once and every page shows the same thing. Each one
 * stays live: events the server sends are applied in place (cheaper than reading it all again),
 * and a slow poll and every reconnection read it again in case one was missed.
 */
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, HostSample, MetricsHistory, Run } from "@/api/types";
import { useSuspenseResource } from "./resource";

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

/** The overview: counts, the queue, the server's subsystems and its latest sample. */
export function useOverview() {
  const ov = useSuspenseResource("overview", (s) => api.overview(s), { pollMs: 30_000 });
  useServerEvents((ev) => {
    if (ev.type === "queue") ov.setData((d) => ({ ...d, queue: { queued: ev.queued, running: ev.running } }));
    else if (ev.type === "sample" && ev.agent === "server") ov.setData((d) => ({ ...d, system: { ...d.system, sample: ev.sample } }));
  });
  return ov;
}

/** Every machine that has joined (Kubernetes job pods included: callers leave them out). */
export function useAgentsLive() {
  const agents = useSuspenseResource("agents", (s) => api.agents(s), { pollMs: 30_000 });
  useServerEvents((ev) => {
    if (ev.type === "agent") agents.setData((l) => upsert<Agent>(l, ev.agent, (a) => a.name, false));
    else if (ev.type === "sample" && ev.agent !== "server") agents.setData((l) => (l.some((a) => a.name === ev.agent) ? l.map((a) => (a.name === ev.agent ? { ...a, sample: ev.sample } : a)) : l));
  });
  return agents;
}

/** Samples kept per machine (the last hour at 5 s is 720). */
const HISTORY_CAP = 800;

/** Every machine's last hour of samples (the server's under "server"), growing as they arrive. */
export function useMetricsHistory() {
  const hist = useSuspenseResource("metrics/history", (s) => api.metricsHistory(undefined, s), { pollMs: 120_000 });
  useServerEvents((ev) => {
    if (ev.type === "sample") hist.setData((h: MetricsHistory) => ({ ...h, [ev.agent]: [...(h[ev.agent] ?? []), ev.sample].slice(-HISTORY_CAP) }));
  });
  return hist;
}

/** The newest `limit` runs, newest first, kept current by run events. */
export function useRecentRuns(limit: number) {
  const runs = useSuspenseResource(`runs/recent/${limit}`, (s) => api.runs({ limit }, s), { pollMs: 60_000 });
  useServerEvents((ev) => {
    if (ev.type === "run") runs.setData((list) => upsert<Run>(list, ev.run, (r) => r.id).sort((a, b) => b.created_at - a.created_at).slice(0, limit));
  });
  return runs;
}

/** The repositories kiln builds. */
export function useRepos() {
  return useSuspenseResource("repos", (s) => api.repos(s), { pollMs: 30_000 });
}

/** The last `n` samples of a machine. */
export function lastSamples(h: MetricsHistory, name: string, n: number): HostSample[] {
  const all = h[name] ?? [];
  return all.length > n ? all.slice(-n) : all;
}
