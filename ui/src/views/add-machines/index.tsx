/**
 * Runners → Add machines. One command for a new machine (and a line that says when it is here),
 * the kiln machines heard on the network with the one thing to do about each, requests from
 * machines it doesn't hear, and why discovery might hear nothing.
 */
import * as React from "react";
import "./add-machines.css";
import { Link } from "react-router";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, InstallOs, JoinRequest } from "@/api/types";
import { useDocumentTitle, useResource } from "@/lib/hooks";
import { useInterval } from "@/views/shared";
import { detectOs, useJoinData, type Arrival } from "./data";
import { FoundSection, WaitingSection } from "./found";
import { HealthSection } from "./health";
import { InstallSection } from "./install";

export default function AddMachines() {
  useDocumentTitle("Add machines");
  const userOs = React.useMemo(detectOs, []);
  const [os, setOs] = React.useState<InstallOs>(userOs);
  const info = useResource((s) => api.join(s), []);
  const { requests, discovery } = useJoinData();
  const arrived = useArrivals();
  const pending = (requests.data ?? []).filter((r) => r.status === "pending");
  const asking = useNewRequests(requests.data);

  // A request from a machine discovery hears is answered in its row; the rest wait in their own section.
  const d = discovery.data;
  const linked = new Map((d?.instances ?? []).filter((i) => i.request).map((i) => [i.request!, i.id]));
  const unheard = pending.filter((r) => !linked.has(r.id));
  const rowOf = (r: JoinRequest) => (linked.has(r.id) ? `di-${linked.get(r.id)}` : `jr-${r.id}`);
  const n = (k: number) => String(k).padStart(2, "0");

  return (
    <div className="container-console page am">
      <header className="page-intro">
        <div className="min-w-0">
          <div className="eyebrow">
            <span className="spectrum-rule" />
            <span>
              04 /{" "}
              <Link to="/runners" className="am-crumb">
                Runners
              </Link>{" "}
              / Add
            </span>
          </div>
          <h1>Add machines</h1>
          <p className="lead">Run one command on a new machine, or pick one kiln already hears on your network.</p>
        </div>
      </header>

      <InstallSection index={n(1)} os={os} onOs={setOs} info={info} arrived={arrived} asking={asking} rowOf={rowOf} />
      <FoundSection index={n(2)} discovery={discovery} requests={pending} onRequestsChanged={requests.reload} />
      {unheard.length > 0 && <WaitingSection index={n(3)} requests={unheard} onDecided={requests.reload} />}
      {d && (d.methods || d.problems || d.dns_sd) && <HealthSection index={n(unheard.length > 0 ? 4 : 3)} discovery={d} os={userOs} />}
    </div>
  );
}

/**
 * Runners that connect while the page is open: ones not among the runners it saw when it opened
 * (Kubernetes job pods aren't machines). From `agent` events, with a slow re-check behind them.
 */
function useArrivals(): Arrival[] {
  const known = React.useRef<Set<string> | null>(null);
  const [arrived, setArrived] = React.useState<Arrival[]>([]);
  const see = React.useCallback((list: Agent[]) => {
    const k = known.current;
    if (!k) {
      known.current = new Set(list.map((a) => a.name));
      return;
    }
    const fresh = list.filter((a) => !a.ephemeral && a.connected && !k.has(a.name));
    if (!fresh.length) return;
    fresh.forEach((a) => k.add(a.name));
    const at = Date.now() / 1000;
    setArrived((l) => [...l, ...fresh.map((agent) => ({ agent, at }))]);
  }, []);
  React.useEffect(() => {
    const ctl = new AbortController();
    api
      .agents(ctl.signal)
      .then(see)
      .catch(() => undefined);
    return () => ctl.abort();
  }, [see]);
  useServerEvents((ev) => {
    if (ev.type === "agent" && known.current) see([ev.agent]);
  });
  useInterval(() => {
    if (document.hidden) return;
    api
      .agents()
      .then(see)
      .catch(() => undefined);
  }, 20_000);
  return arrived;
}

/** Join requests that arrived after the page opened (the first list it got is the baseline). */
function useNewRequests(list: JoinRequest[] | undefined): JoinRequest[] {
  const [baseline, setBaseline] = React.useState<Set<string> | null>(null);
  if (list && baseline === null) setBaseline(new Set(list.map((r) => r.id)));
  return baseline ? (list ?? []).filter((r) => r.status === "pending" && !baseline.has(r.id)) : [];
}
