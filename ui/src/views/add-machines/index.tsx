/**
 * Runners → Add machines. One command for a new machine (and a line that says when it is here),
 * the kiln machines heard on the network with the one thing to do about each, requests from
 * machines it doesn't hear, and why discovery might hear nothing.
 */
import * as React from "react";
import "./add-machines.css";
import type { InstallOs, JoinRequest } from "@/api/types";
import { Hold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, SectionHeading } from "@/components/ui/misc";
import { agentsRes, useAgentsLive, useRes, warm } from "@/lib/data";
import { detectOs, discoveryRes, joinRequestsRes, joinRes, type Arrival } from "./data";
import { FoundSection, WaitingSection } from "./found";
import { HealthSection } from "./health";
import { InstallSection } from "./install";

export default function AddMachines() {
  return (
    <div className="container-console page am">
      <title>Add machines · kiln</title>
      <header className="page-intro">
        <div className="min-w-0">
          <Crumbs index={5} items={[{ label: "Runners", to: "/runners" }, { label: "Add" }]} />
          <h1>Add machines</h1>
          <p className="lead">Run one command on a new machine, or pick one kiln already hears on your network.</p>
        </div>
      </header>
      <Loadable label="what kiln hears" fallback={<AddHold />}>
        <AddBody />
      </Loadable>
    </div>
  );
}

const n = (k: number) => String(k).padStart(2, "0");

function AddBody() {
  warm(joinRes, joinRequestsRes, discoveryRes, agentsRes);
  const userOs = React.useMemo(detectOs, []);
  const [os, setOs] = React.useState<InstallOs>(userOs);
  const info = useRes(joinRes);
  const requests = useRes(joinRequestsRes);
  const discovery = useRes(discoveryRes);
  const arrived = useArrivals();
  const pending = requests.data.filter((r) => r.status === "pending");
  const asking = useNewRequests(requests.data);

  // A request from a machine discovery hears is answered in its row; the rest wait in their own section.
  const d = discovery.data;
  const linked = new Map(d.instances.filter((i) => i.request).map((i) => [i.request!, i.id]));
  const unheard = pending.filter((r) => !linked.has(r.id));
  const rowOf = (r: JoinRequest) => (linked.has(r.id) ? `di-${linked.get(r.id)}` : `jr-${r.id}`);

  return (
    <>
      <InstallSection index={n(1)} os={os} onOs={setOs} info={info} arrived={arrived} asking={asking} rowOf={rowOf} />
      <FoundSection index={n(2)} discovery={discovery} requests={pending} onRequestsChanged={requests.reload} />
      {unheard.length > 0 && <WaitingSection index={n(3)} requests={unheard} onDecided={requests.reload} />}
      {(d.methods || d.problems || d.dns_sd) && <HealthSection index={n(unheard.length > 0 ? 4 : 3)} discovery={d} os={userOs} />}
    </>
  );
}

/** The page while it loads: its first two sections' headings and space. */
function AddHold() {
  return (
    <>
      <section className="am-section">
        <SectionHeading index={n(1)} label="One command" title="Add a machine" />
        <Hold height={180} label="Loading the command" />
      </section>
      <section className="am-section">
        <SectionHeading index={n(2)} label="Nearby" title="Found on your network" />
        <Hold height={240} />
      </section>
    </>
  );
}

/**
 * Runners that connect while the page is open: ones not among the runners it saw when it opened
 * (Kubernetes job pods aren't machines), as the live runner list shows them.
 */
function useArrivals(): Arrival[] {
  const agents = useAgentsLive().data;
  const [known] = React.useState(() => new Set(agents.map((a) => a.name)));
  const [arrived, setArrived] = React.useState<Arrival[]>([]);
  const fresh = agents.filter((a) => !a.ephemeral && a.connected && !known.has(a.name) && !arrived.some((x) => x.agent.name === a.name));
  if (fresh.length) {
    const at = Date.now() / 1000;
    setArrived([...arrived, ...fresh.map((agent) => ({ agent, at }))]);
  }
  return arrived;
}

/** Join requests that arrived after the page opened (the first list it got is the baseline). */
function useNewRequests(list: JoinRequest[]): JoinRequest[] {
  const [baseline] = React.useState(() => new Set(list.map((r) => r.id)));
  return list.filter((r) => r.status === "pending" && !baseline.has(r.id));
}
