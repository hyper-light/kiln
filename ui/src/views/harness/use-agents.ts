/** Every agent kiln runs, kept live from the server's events. */
import * as React from "react";
import { useServerEvents } from "@/api/events";
import { harnessApi, type AgentInfo } from "@/api/harness";
import { useSuspenseResource } from "@/lib/resource";

/**
 * How often the list takes in what changed. Many agents working at once send a stream of events;
 * gathered and applied together at most this often, the list settles instead of churning (each
 * row still shows its agent's latest state).
 */
const BATCH_MS = 250;

const KEY = "harness/agents";
const list = () => harnessApi.list();

/** The agents as last read (suspends until the first list is in). */
export function useAgentList(): AgentInfo[] {
  return useSuspenseResource(KEY, list).data;
}

/**
 * The agents, live: the list (suspends until it's in; read again when the server's back after a
 * restart), with each agent's news from the server's events applied as it comes, in batches.
 */
export function useAgents() {
  const res = useSuspenseResource(KEY, list);
  // The latest word from each agent since the last batch (null: it closed).
  const [batch] = React.useState(() => ({ pending: new Map<string, AgentInfo | null>(), timer: 0 }));
  useServerEvents((ev) => {
    if (ev.type === "harness") batch.pending.set(ev.info.id, ev.info);
    else if (ev.type === "harness_closed") batch.pending.set(ev.id, null);
    else return;
    if (batch.timer) return;
    batch.timer = window.setTimeout(() => {
      batch.timer = 0;
      const news = batch.pending;
      batch.pending = new Map();
      res.setData((l) => {
        const next: AgentInfo[] = [];
        for (const a of l) {
          const info = news.has(a.id) ? news.get(a.id) : a;
          news.delete(a.id);
          if (info) next.push(info);
        }
        for (const info of news.values()) if (info) next.push(info);
        return next;
      });
    }, BATCH_MS);
  });
  return { list: res.data, reload: res.reload };
}
