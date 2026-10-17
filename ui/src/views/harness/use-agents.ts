/** Every agent kiln runs, kept live from the server's events. */
import * as React from "react";
import { useServerEvents } from "@/api/events";
import { harnessApi, type AgentInfo } from "@/api/harness";

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
  useServerEvents((ev) => {
    if (ev.type === "harness")
      setList((l) => {
        const cur = l ?? [];
        const i = cur.findIndex((a) => a.id === ev.info.id);
        if (i === -1) return [...cur, ev.info];
        const next = cur.slice();
        next[i] = ev.info;
        return next;
      });
    else if (ev.type === "harness_closed") setList((l) => (l ?? []).filter((a) => a.id !== ev.id));
  });
  return { list, error, reload: load };
}
