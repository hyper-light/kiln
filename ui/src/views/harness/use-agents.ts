/** Every agent kiln runs, kept live from the server's events. */
import * as React from "react";
import { useServerEvents } from "@/api/events";
import { harnessApi, type AgentInfo } from "@/api/harness";

/**
 * How often the list takes in what changed. Many agents working at once send a stream of events;
 * gathered and applied together at most this often, the list settles instead of churning (each
 * row still shows its agent's latest state).
 */
const BATCH_MS = 250;

export function useAgents() {
  const [list, setList] = React.useState<AgentInfo[] | null>(null);
  const [error, setError] = React.useState<Error | null>(null);
  const load = React.useCallback(() => {
    harnessApi.list().then(
      (l) => {
        setList(l);
        setError(null);
      },
      (e: unknown) => setError(e instanceof Error ? e : new Error(String(e))),
    );
  }, []);
  React.useEffect(load, [load]);

  // The latest word from each agent since the last batch (null: it closed).
  const pending = React.useRef(new Map<string, AgentInfo | null>());
  const timer = React.useRef(0);
  const apply = React.useCallback(() => {
    timer.current = 0;
    const batch = pending.current;
    if (batch.size === 0) return;
    pending.current = new Map();
    setList((l) => {
      const next = (l ?? []).slice();
      for (const [id, info] of batch) {
        const i = next.findIndex((a) => a.id === id);
        if (info === null) {
          if (i !== -1) next.splice(i, 1);
        } else if (i === -1) next.push(info);
        else next[i] = info;
      }
      return next;
    });
  }, []);
  React.useEffect(() => () => window.clearTimeout(timer.current), []);
  useServerEvents((ev) => {
    if (ev.type === "harness") pending.current.set(ev.info.id, ev.info);
    else if (ev.type === "harness_closed") pending.current.set(ev.id, null);
    else return;
    if (!timer.current) timer.current = window.setTimeout(apply, BATCH_MS);
  });
  return { list, error, reload: load };
}
