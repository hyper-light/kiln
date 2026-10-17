import * as React from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { Switch } from "radix-ui";
import { Plus, Server, SquareTerminal, Trash2, X } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, HostSample, Job, RunnerInstance, RunnerUpdate } from "@/api/types";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { EmptyState, ErrorState, Facts, Meter, PageIntro, SelectField, Skeleton, SkeletonRows } from "@/components/ui/misc";
import { Chips, StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { Tip } from "@/components/ui/tooltip";
import { absoluteTime, formatBytes, formatPct } from "@/lib/format";
import { errorMessage, useDocumentTitle, useLocalStorage, useResource } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { FrontDoorSection } from "./front-door/front-door";
import { HostCharts } from "./host-charts";
import { useJoinData } from "./add-machines/data";
import { JoinNotice } from "./add-machines/notice";
import { AgentMiniCard, LiveDuration, RelTime, diskPct, memPct, upsert } from "./shared";

/** The facets runners are filtered by, with each runner's values. */
function facetValues(a: Agent): Record<string, string[]> {
  return { state: [agentStateLabel(a)], labels: a.labels, platform: [`${a.os}/${a.arch}`] };
}

const FACETS: { id: string; label: string }[] = [
  { id: "state", label: "State" },
  { id: "labels", label: "Labels" },
  { id: "platform", label: "Platform" },
];

export function agentStatus(a: Agent) {
  return !a.connected ? "cancelled" : a.draining ? "timed_out" : a.busy > 0 ? "running" : "succeeded";
}
export function agentStateLabel(a: Agent) {
  return !a.connected ? "offline" : a.draining ? "draining" : a.busy > 0 ? "busy" : "idle";
}

/** The machines that run CI jobs. Kubernetes job pods are not listed: they show under their job. */
export default function Runners() {
  useDocumentTitle("Runners");
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const agents = useResource((s) => api.agents(s), [], { pollMs: 30_000 });
  const [history, setHistory] = React.useState<Record<string, HostSample[]>>({});
  const [view, setView] = useLocalStorage<"table" | "cards">("kiln.agents.view", "table");
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({});
  const selected = params.get("agent");
  const { requests: joinRequests, discovery } = useJoinData();

  // Old links (`?join=1`) opened a dialog; adding machines has its own page now.
  React.useEffect(() => {
    if (params.get("join") === "1") navigate("/runners/add", { replace: true });
  }, [params, navigate]);

  React.useEffect(() => {
    const ctl = new AbortController();
    api
      .metricsHistory(undefined, ctl.signal)
      .then((h) => setHistory(Object.fromEntries(Object.entries(h).map(([k, v]) => [k, v.slice(-120)]))))
      .catch(() => undefined);
    return () => ctl.abort();
  }, []);

  useServerEvents((ev) => {
    if (ev.type === "agent") agents.setData((l) => upsert(l, ev.agent, (a) => a.name, false));
    if (ev.type === "sample" && ev.agent !== "server") {
      agents.setData((l) => l?.map((a) => (a.name === ev.agent ? { ...a, sample: ev.sample } : a)));
      setHistory((h) => ({ ...h, [ev.agent]: [...(h[ev.agent] ?? []), ev.sample].slice(-120) }));
    }
  });

  const drain = async (a: Agent, draining: boolean) => {
    agents.setData((l) => l?.map((x) => (x.name === a.name ? { ...x, draining } : x)));
    try {
      await api.drainAgent(a.name, draining);
      toast({ title: draining ? `Draining ${a.name}` : `${a.name} accepting jobs`, tone: "info" });
    } catch (e) {
      agents.setData((l) => l?.map((x) => (x.name === a.name ? { ...x, draining: !draining } : x)));
      toast.error("Drain failed", errorMessage(e));
    }
  };

  const all = (agents.data ?? []).filter((a) => !a.ephemeral);
  const counts = { all: all.length, connected: all.filter((a) => a.connected).length };
  const ql = q.trim().toLowerCase();
  const matches = (a: Agent) => {
    const v = facetValues(a);
    return Object.entries(chosen).every(([f, want]) => want.length === 0 || (v[f] ?? []).some((x) => want.includes(x)));
  };
  const list = all
    .filter(matches)
    .filter((a) => !ql || `${a.name} ${a.hostname} ${a.labels.join(" ")}`.toLowerCase().includes(ql))
    .sort((a, b) => Number(b.connected) - Number(a.connected) || a.name.localeCompare(b.name));
  const facets: Facet[] = FACETS.map((f) => {
    const n = new Map<string, number>();
    for (const a of all) for (const v of facetValues(a)[f.id] ?? []) n.set(v, (n.get(v) ?? 0) + 1);
    return { id: f.id, label: f.label, options: [...n.entries()].sort((x, y) => x[0].localeCompare(y[0])).map(([id, count]) => ({ id, label: id, count })) };
  }).filter((f) => f.options.length > 0);

  const slots = all.filter((a) => a.connected).reduce((s, a) => s + a.slots, 0);
  const busy = all.filter((a) => a.connected).reduce((s, a) => s + a.busy, 0);
  const forget = async (a: Agent) => {
    try {
      await api.forgetAgent(a.name);
      agents.setData((l) => l?.filter((x) => x.name !== a.name));
      if (selected === a.name) openAgent(null);
      toast({ title: `${a.name} forgotten`, tone: "info" });
    } catch (e) {
      toast.error("Forget failed", errorMessage(e));
    }
  };
  const details = (a: Agent) => (
    <AgentDetails
      agent={a}
      onClose={() => openAgent(null)}
      onDrain={drain}
      onForget={forget}
      onTerminal={() => navigate(`/terminal?open=${encodeURIComponent(`agent:${a.name}`)}`)}
    />
  );

  const openAgent = (name: string | null) => {
    const n = new URLSearchParams(params);
    if (name) n.set("agent", name);
    else n.delete("agent");
    setParams(n, { replace: true });
  };

  return (
    <div className="container-console page">
      <PageIntro
        index={6}
        label="Runners"
        title="Fleet"
        lead={
          agents.data ? (
            <>
              {counts.connected} of {counts.all} runners connected · {busy}/{slots} slots busy. Runners are long-lived machines that take CI jobs; Kubernetes job pods show up under their jobs.
            </>
          ) : (
            "Runners are long-lived machines that take CI jobs; Kubernetes job pods show up under their jobs."
          )
        }
        actions={
          <Button asChild>
            <Link to="/runners/add">
              <Plus /> Add machines
            </Link>
          </Button>
        }
      />

      <JoinNotice requests={joinRequests.data} discovery={discovery.data} />
      <Upgrades />

      <div className="runners-tools">
        <FacetBox className="runners-filter" label="Filter runners" placeholder="Filter by name, state, label, platform…" query={q} onQuery={setQ} facets={facets} chosen={chosen} onChosen={setChosen} />
        <SelectField
          className="runners-layout"
          label="Layout"
          hideLabel
          value={view}
          onChange={(v) => setView(v as "table" | "cards")}
          options={[
            { value: "table", label: "Table" },
            { value: "cards", label: "Cards" },
          ]}
        />
      </div>

      {agents.error && !agents.data ? (
        <ErrorState error={agents.error} onRetry={agents.reload} />
      ) : !agents.data ? (
        <SkeletonRows rows={6} />
      ) : all.length === 0 ? (
        <EmptyState
          icon={<Server size={24} />}
          title="No runners have enrolled."
          action={
            <Button asChild variant="secondary">
              <Link to="/runners/add">
                <Plus /> Add machines
              </Link>
            </Button>
          }
        >
          Run the join command on a machine to enroll it as a runner. With Kubernetes configured, kiln also launches a pod per job.
        </EmptyState>
      ) : list.length === 0 ? (
        <EmptyState title="No runners match." action={<Button variant="secondary" onClick={() => { setChosen({}); setQ(""); }}>Clear filters</Button>} />
      ) : view === "cards" ? (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4">
          {list.map((a) => (
            <React.Fragment key={a.name}>
              <AgentMiniCard agent={a} history={history[a.name] ?? []} onOpen={() => openAgent(a.name === selected ? null : a.name)} />
              {selected === a.name && <div className="col-span-full">{details(a)}</div>}
            </React.Fragment>
          ))}
        </div>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <tr>
                <th className="w-6" aria-label="State" />
                <th>Runner</th>
                <th className="hidden md:table-cell">Labels</th>
                <th>Slots</th>
                <th className="num hidden sm:table-cell">CPU</th>
                <th className="num hidden lg:table-cell">Mem</th>
                <th className="num hidden xl:table-cell">Disk</th>
                <th className="num hidden xl:table-cell">Load</th>
                <th className="hidden md:table-cell">Drain</th>
                <th className="hidden w-0 sm:table-cell">
                  <span className="sr-only">Terminal</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {list.map((a) => {
                const s = a.sample;
                const open = selected === a.name;
                const toggle = () => openAgent(open ? null : a.name);
                return (
                  <React.Fragment key={a.name}>
                  <tr
                    data-href
                    tabIndex={0}
                    aria-expanded={open}
                    onClick={(e) => !(e.target as HTMLElement).closest("button,a,[role=switch]") && toggle()}
                    onKeyDown={(e) => e.key === "Enter" && toggle()}
                  >
                    <td>
                      <StatusDot status={agentStatus(a)} title={agentStateLabel(a)} />
                    </td>
                    <td className="max-w-[260px]">
                      <div className="truncate text-[#e4e5e7]">{a.name}</div>
                      <div className="truncate font-mono text-[10px] text-[#7b7e88]">
                        {a.connected ? `${a.hostname} · ${a.os}/${a.arch} · v${a.version}` : <>last seen <RelTime ts={a.last_seen} /></>}
                      </div>
                      <UpgradeNote agent={a} />
                    </td>
                    <td className="hidden md:table-cell">
                      <Chips labels={a.labels} max={4} />
                    </td>
                    <td className="min-w-[90px]">
                      <div className="font-mono text-[11px] text-[#c8cad0] tabular-nums">
                        {a.busy}/{a.slots}
                        {a.running.length > 0 && <span className="ml-1.5 text-[#7b7e88]">· {a.running.length} job{a.running.length > 1 ? "s" : ""}</span>}
                      </div>
                      <div className="mt-1 w-20">
                        <Meter value={a.busy} max={Math.max(1, a.slots)} label="Slots busy" />
                      </div>
                    </td>
                    <td className="num cell-mono hidden sm:table-cell">{s ? formatPct(s.cpu_pct) : "—"}</td>
                    <td className="num cell-mono hidden lg:table-cell">{s ? formatPct(memPct(s)) : "—"}</td>
                    <td className="num cell-mono hidden xl:table-cell">{s ? formatPct(diskPct(s)) : "—"}</td>
                    <td className="num cell-mono hidden xl:table-cell">{s ? s.load1.toFixed(2) : "—"}</td>
                    <td className="hidden md:table-cell">
                      <DrainSwitch agent={a} onChange={(v) => drain(a, v)} />
                    </td>
                    <td className="hidden text-right whitespace-nowrap sm:table-cell">
                      <Tip content={a.connected ? "Open terminal" : "Runner offline"}>
                        <button
                          className="icon-btn"
                          data-variant="bare"
                          data-size="sm"
                          aria-label={`Open terminal on ${a.name}`}
                          disabled={!a.connected}
                          onClick={() => navigate(`/terminal?open=${encodeURIComponent(`agent:${a.name}`)}`)}
                        >
                          <SquareTerminal size={14} />
                        </button>
                      </Tip>
                    </td>
                  </tr>
                  {open && (
                    <tr className="runner-detail-row">
                      <td colSpan={10}>{details(a)}</td>
                    </tr>
                  )}
                  </React.Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {agents.data && all.length > 0 && <FrontDoorSection agents={all} index="02" />}

    </div>
  );
}

/** What keeps an older runner process alive, in words ("2 terminals, 1 job"). */
function holding(i: RunnerInstance) {
  const parts = [
    i.terminals ? `${i.terminals} terminal${i.terminals === 1 ? "" : "s"}` : "",
    i.agents ? `${i.agents} agent session${i.agents === 1 ? "" : "s"}` : "",
    i.jobs ? `${i.jobs} job${i.jobs === 1 ? "" : "s"}` : "",
  ].filter(Boolean);
  return parts.length ? parts.join(", ") : "nothing";
}

/**
 * A runner being replaced without a restart: the new process takes new work while the old one
 * keeps what it runs, and retires itself once that ends.
 */
function UpgradeNote({ agent, detailed }: { agent: Agent; detailed?: boolean }) {
  const inst = agent.instances ?? [];
  if (inst.length < 2) return null;
  const old = inst.filter((i) => !i.newest);
  if (!detailed) {
    const held = old.reduce((n, i) => n + i.terminals + i.agents + i.jobs, 0);
    return (
      <div className="truncate font-mono text-[10px] text-[#cbb69a]">
        upgrading · old runner {held ? `keeps ${old.map(holding).join("; ")}` : "is retiring"}
      </div>
    );
  }
  return (
    <div className="mt-5 rounded-xl border border-[#cbb69a]/30 bg-[#cbb69a]/[0.04] px-4 py-3">
      <div className="label mb-2">Upgrading without a restart</div>
      <ul className="flex flex-col gap-1.5 text-[12.5px] text-[#c8cad0]">
        {inst.map((i) => (
          <li key={i.id} className="flex flex-wrap items-baseline gap-x-2">
            <StatusDot status={i.newest ? "succeeded" : "timed_out"} />
            <span>{i.newest ? "New runner" : i.legacy ? "Old runner" : "Older runner"}</span>
            <span className="font-mono text-[10.5px] text-[#7b7e88]">
              {i.legacy ? "legacy" : i.id.slice(0, 8)} · v{i.version}
              {i.pid ? ` · pid ${i.pid}` : ""}
            </span>
            <span className="text-[#a1a4ad]">
              {i.newest ? "takes new terminals, agents and jobs" : `keeps ${holding(i)}; retires itself when ${i.terminals + i.agents + i.jobs ? "they end" : "it is stopped"}`}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

function DrainSwitch({ agent, onChange }: { agent: Agent; onChange: (v: boolean) => void }) {
  return (
    <label className="inline-flex min-h-8 cursor-pointer items-center gap-2">
      <Switch.Root
        checked={agent.draining}
        onCheckedChange={onChange}
        disabled={!agent.connected}
        aria-label={`Drain ${agent.name}`}
        className="relative h-[18px] w-8 shrink-0 rounded-full border border-white/15 bg-white/[0.04] transition-colors outline-none focus-visible:ring-2 focus-visible:ring-white/80 focus-visible:ring-offset-2 focus-visible:ring-offset-[#08090a] disabled:opacity-40 data-[state=checked]:border-[#cbb69a]/50 data-[state=checked]:bg-[#cbb69a]/20"
      >
        <Switch.Thumb className="block size-3 translate-x-[2px] rounded-full bg-[#a1a4ad] transition-transform duration-200 data-[state=checked]:translate-x-[15px] data-[state=checked]:bg-[#e3d2b8]" />
      </Switch.Root>
      <span className={cn("font-mono text-[10px]", agent.draining ? "text-[#cbb69a]" : "text-[#7b7e88]")}>{agent.draining ? "draining" : "off"}</span>
    </label>
  );
}

/** One runner in depth, open in the page under its row: what it is, what it runs, its last hour. */
function AgentDetails({
  agent,
  onClose,
  onDrain,
  onForget,
  onTerminal,
}: {
  agent: Agent;
  onClose: () => void;
  onDrain: (a: Agent, v: boolean) => void;
  onForget: (a: Agent) => Promise<void>;
  onTerminal: () => void;
}) {
  const [forgetting, setForgetting] = React.useState(false);
  const [samples, setSamples] = React.useState<HostSample[] | null>(null);
  const [err, setErr] = React.useState<string | null>(null);
  const [jobs, setJobs] = React.useState<Job[]>([]);
  const name = agent?.name;
  const running = agent?.running.join(",") ?? "";

  React.useEffect(() => {
    if (!name) return;
    setSamples(null);
    setErr(null);
    const ctl = new AbortController();
    api
      .metricsHistory(name, ctl.signal)
      .then((h) => setSamples(h[name] ?? []))
      .catch((e) => !ctl.signal.aborted && setErr(errorMessage(e)));
    return () => ctl.abort();
  }, [name]);

  React.useEffect(() => {
    if (!running) {
      setJobs([]);
      return;
    }
    const ctl = new AbortController();
    Promise.all(running.split(",").map((id) => api.job(id, ctl.signal).catch(() => null)))
      .then((js) => setJobs(js.filter((j): j is Job => j !== null)))
      .catch(() => undefined);
    return () => ctl.abort();
  }, [running]);

  useServerEvents((ev) => {
    if (ev.type === "sample" && ev.agent === name) setSamples((s) => (s ? [...s, ev.sample].slice(-720) : s));
  });

  const s = agent.sample;
  return (
    <section className="runner-details" aria-label={`${agent.name} in depth`}>
      <div className="runner-details-head">
        <div className="min-w-0">
          <div className="runner-details-eyebrow">Runner · {agentStateLabel(agent)}</div>
          <h3>{agent.name}</h3>
          <p>
            {agent.hostname} · {agent.os}/{agent.arch} · kiln v{agent.version}
          </p>
        </div>
        <Button size="xs" variant="ghost" onClick={onClose} aria-label={`Close ${agent.name}`}>
          <X /> Close
        </Button>
      </div>
          <div className="flex flex-wrap items-center gap-2">
            <Chips labels={agent.labels} max={12} />
          </div>
          <div className="mt-4 flex flex-wrap gap-2">
            <Button variant="secondary" size="xs" disabled={!agent.connected} onClick={onTerminal}>
              <SquareTerminal /> Open terminal
            </Button>
            <Button variant="ghost" size="xs" disabled={!agent.connected} onClick={() => onDrain(agent, !agent.draining)}>
              {agent.draining ? "Resume scheduling" : "Drain"}
            </Button>
            {!forgetting && (
              <Button variant="ghost" size="xs" className="text-[#d1aaad]" onClick={() => setForgetting(true)}>
                <Trash2 /> Forget
              </Button>
            )}
          </div>
          {forgetting && (
            <div className="runner-up-confirm mt-3" role="group" aria-label={`Forget ${agent.name}`}>
              <p>
                Forget {agent.name}?{" "}
                {agent.connected ? "It is connected: it will be disconnected and must enroll again to come back." : "Its enrollment is removed: it must enroll again to come back."}
              </p>
              <div className="runner-up-actions">
                <Button size="xs" variant="danger" onClick={() => void onForget(agent).finally(() => setForgetting(false))}>
                  Forget runner
                </Button>
                <Button size="xs" variant="ghost" onClick={() => setForgetting(false)}>
                  Keep it
                </Button>
              </div>
            </div>
          )}

          <UpgradeNote agent={agent} detailed />

          <div className="mt-6">
            <Facts
              items={[
                { label: "Slots", value: `${agent.busy} / ${agent.slots} busy` },
                { label: "Connected", value: agent.connected ? "yes" : "no", tone: agent.connected ? "pass" : "fail" },
                { label: "Last seen", value: <RelTime ts={agent.last_seen} /> },
                { label: "Enrolled", value: absoluteTime(agent.enrolled_at) },
                { label: "CPU", value: s ? `${formatPct(s.cpu_pct, 1)} · ${s.cpus} cores` : "—" },
                { label: "Memory", value: s ? `${formatBytes(s.mem_used)} / ${formatBytes(s.mem_total)}` : "—" },
                { label: "Disk", value: s ? `${formatBytes(s.disk_used)} / ${formatBytes(s.disk_total)}` : "—" },
              ]}
            />
          </div>

          <div className="mt-8">
            <div className="label mb-2">Running jobs</div>
            {agent.running.length === 0 ? (
              <p className="text-[12.5px] text-[#8f939e]">Idle.</p>
            ) : (
              <div className="row-list">
                {jobs.map((j) => (
                  <Link key={j.id} to={`/runs/${j.run_id}/jobs/${j.id}`} className="flex min-h-11 items-center gap-3 px-1 text-[12.5px] hover:bg-[#101114]">
                    <StatusDot status={j.status} />
                    <span className="min-w-0 flex-1 truncate">{j.display}</span>
                    <span className="font-mono text-[10px] text-[#8c8f98]">
                      <LiveDuration start={j.started_at} end={j.finished_at} />
                    </span>
                  </Link>
                ))}
                {jobs.length === 0 && agent.running.map((id) => <div key={id} className="py-2 font-mono text-[11px] text-[#8c8f98]">{id}</div>)}
              </div>
            )}
          </div>

          <div className="mt-8">
            <div className="label mb-3">Last hour · 5 s resolution</div>
            {err ? <ErrorState error={err} compact /> : !samples ? <Skeleton className="h-64 w-full" /> : samples.length < 2 ? <p className="text-[12.5px] text-[#8f939e]">Not enough samples yet.</p> : <HostCharts samples={samples} height={130} columns={2} />}
          </div>
    </section>
  );
}

/**
 * Runners on an older build than the one this server hands out for their platform: upgraded from
 * here, each confirmed in place; nothing they run stops. A platform with no build here yet is built
 * by kiln's own release pipeline first.
 */
function Upgrades() {
  const res = useResource((s) => api.updates(s), [], { pollMs: 20_000 });
  const reload = res.reload;
  useServerEvents((ev) => {
    if (ev.type === "updates" || ev.type === "update_job") reload();
  });
  const [asking, setAsking] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);
  const data = res.data;
  if (!data) return null;
  const upgrading = new Set(data.jobs.filter((j) => j.kind === "runner" && j.state === "running").map((j) => j.target));
  const behind = data.runners.filter((r) => r.connected && !r.up_to_date);
  if (behind.length === 0) return null;
  const ready = behind.filter((r) => r.target_build && !upgrading.has(r.name) && !(r.method === "terminal" && !r.terminal));
  const lacking = behind.filter((r) => !r.target_build);
  const last = data.build?.last;
  const building = last != null && (last.status === "queued" || last.status === "running" || last.status === "pending");

  const upgrade = async (runners: RunnerUpdate[]) => {
    setBusy(true);
    try {
      for (const r of runners) await api.upgradeRunner(r.name);
      toast({ title: runners.length === 1 ? `Upgrading ${runners[0]?.name ?? ""}` : `Upgrading ${runners.length} runners`, description: "Nothing they run is stopped.", tone: "running" });
    } catch (e) {
      toast.error("Couldn't upgrade", errorMessage(e));
    } finally {
      setBusy(false);
      setAsking(null);
      reload();
    }
  };
  const build = async () => {
    setBusy(true);
    try {
      const run = await api.buildEveryPlatform();
      toast({ title: "Building every platform", description: `run #${run.number}`, tone: "running" });
    } catch (e) {
      toast.error("Couldn't start the build", errorMessage(e));
    } finally {
      setBusy(false);
      reload();
    }
  };
  const confirm = (text: string, runners: RunnerUpdate[]) => (
    <div className="runner-up-confirm" role="group" aria-label="Confirm the upgrade">
      <p>{text}</p>
      <div className="runner-up-actions">
        <Button size="xs" disabled={busy} onClick={() => void upgrade(runners)}>
          Upgrade
        </Button>
        <Button size="xs" variant="ghost" disabled={busy} onClick={() => setAsking(null)}>
          Cancel
        </Button>
      </div>
    </div>
  );

  return (
    <section className="runner-up" aria-label="Upgrades">
      <div className="runner-up-head">
        <p>
          {behind.length === 1 ? "1 runner is" : `${behind.length} runners are`} on an older build than this server hands out.
        </p>
        {ready.length > 1 && asking !== "all" && (
          <Button size="xs" variant="secondary" onClick={() => setAsking("all")}>
            Upgrade all ({ready.length})
          </Button>
        )}
      </div>
      {asking === "all" && confirm(`Upgrade ${ready.map((r) => r.name).join(", ")}? Each takes the new build without a restart; nothing they run stops.`, ready)}
      <ul className="runner-up-list">
        {behind.map((r) => (
          <li key={r.name}>
            <div className="runner-up-line">
              <StatusDot status={upgrading.has(r.name) ? "running" : r.target_build ? "queued" : "timed_out"} />
              <span className="runner-up-name">{r.name}</span>
              <span className="runner-up-build">
                {r.build ?? "older runner"} → {r.target_build ?? `no ${r.target ?? "matching"} build here yet`}
              </span>
              {upgrading.has(r.name) ? (
                <span className="runner-up-dim">upgrading…</span>
              ) : (
                r.target_build &&
                asking !== r.name && (
                  <Button size="xs" variant="secondary" className="runner-up-go" disabled={r.method === "terminal" && !r.terminal} onClick={() => setAsking(r.name)}>
                    Upgrade
                  </Button>
                )
              )}
            </div>
            {asking === r.name && confirm(`Upgrade ${r.name} to build ${r.target_build ?? ""}? It takes it without a restart; its terminals, agents and jobs keep running.`, [r])}
          </li>
        ))}
      </ul>
      {lacking.length > 0 && (
        <div className="runner-up-build-every">
          <p>
            {building ? (
              <>
                Building every platform ·{" "}
                <Link to={`/runs/${last?.id ?? ""}`}>run #{last?.number}</Link>; {lacking.map((r) => r.name).join(", ")} can be upgraded when it finishes.
              </>
            ) : (
              <>kiln builds {[...new Set(lacking.map((r) => r.target ?? r.os))].join(", ")} itself, on the server&rsquo;s own runner, then {lacking.length === 1 ? "it" : "they"} can be upgraded.</>
            )}
          </p>
          {!building && (
            <Button size="xs" variant="secondary" disabled={busy || Boolean(data.build?.error)} onClick={() => void build()}>
              Build every platform
            </Button>
          )}
        </div>
      )}
    </section>
  );
}
