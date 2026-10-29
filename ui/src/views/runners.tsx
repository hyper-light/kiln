import * as React from "react";
import { Link, Navigate, useNavigate, useSearchParams } from "@/lib/router";
import { Plus, Server, SquareTerminal, X } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, Job, RunnerFacts, RunnerInstance, RunnerUpdate } from "@/api/types";
import { Button } from "@/components/ui/button";
import { FacetBox, type Chosen, type Facet } from "@/components/ui/facet-box";
import { Hold, TableHold } from "@/components/ui/hold";
import { Loadable } from "@/components/ui/loading";
import { Confirm, EmptyState, Facts, FilterBar, Meter, PageIntro, SelectField } from "@/components/ui/misc";
import { WatchedBy } from "@/views/attach/here";
import { Chips, StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { Tip } from "@/components/ui/tooltip";
import { agentsRes, historyRes, lastSamples, mergeJobs, updatesRes, useAgentsLive, useMetricsHistory, useRes, useUpdateJobs, warm } from "@/lib/data";
import { absoluteTime, formatBytes, formatPct } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { usePref } from "@/lib/prefs";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { cn } from "@/lib/utils";
import { FrontDoorSection, frontDoorsRes } from "./front-door/front-door";
import { HeadroomSection } from "./headroom/headroom";
import { HostCharts } from "./host-charts";
import { discoveryRes, joinRequestsRes } from "./add-machines/data";
import { JoinNotice } from "./add-machines/notice";
import { AgentMiniCard, LiveDuration, RelTime, diskPct, memPct } from "./shared";
import { BatteryLine, LowBattery } from "./battery";
import { lostLine } from "./runner-lost";
import { limitLine, parseLimit, settable, standing, withEvent } from "./agent-limit";
import { ImagesPanel, PoolsPanel } from "./runners-pools";
import { useAuth } from "@/lib/auth";

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

const LEAD = "Runners are long-lived machines that take CI jobs; Kubernetes job pods show up under their jobs.";

function Intro({ lead }: { lead: React.ReactNode }) {
  return (
    <PageIntro
      index={4}
      label="Runners"
      title="Fleet"
      lead={lead}
      actions={
        <Button asChild>
          <Link to="/runners/add">
            <Plus /> Add machines
          </Link>
        </Button>
      }
    />
  );
}

/** The machines that run CI jobs. Kubernetes job pods are not listed: they show under their job. */
export default function Runners() {
  const [params] = useSearchParams();
  // Old links (`?join=1`) opened a dialog; adding machines has its own page now.
  // Pools for those who read the whole fleet; edited by those who manage them (W13-R09).
  const can = useAuth((s) => (s.kind === "authed" ? s.me?.can : undefined));
  if (params.get("join") === "1") return <Navigate to="/runners/add" replace />;
  return (
    <div className="container-console page">
      <title>Runners · kiln</title>
      <Loadable label="the runners" fallback={<RunnersHold />}>
        <RunnersBody />
      </Loadable>
      {(!can || can["runners.read"] === "server") && (
        <>
          <Loadable label="the pools" fallback={<Hold height={120} />}>
            <PoolsPanel manage={!can || can["pools.manage"] === "server"} />
          </Loadable>
          {(!can || can["runs.read"] === "server") && (
            <Loadable label="the images" fallback={<Hold height={120} />}>
              <ImagesPanel />
            </Loadable>
          )}
        </>
      )}
    </div>
  );
}

function RunnersBody() {
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  warm(agentsRes, historyRes, joinRequestsRes, discoveryRes, updatesRes, frontDoorsRes);
  const agents = useAgentsLive();
  const history = useMetricsHistory().data;
  const joinRequests = useRes(joinRequestsRes).data;
  const discovery = useRes(discoveryRes).data;
  const [view, setView] = usePref<"table" | "cards">("kiln.agents.view", "table");
  const [q, setQ] = React.useState("");
  const [chosen, setChosen] = React.useState<Chosen>({});
  const selected = params.get("agent");

  const openAgent = (name: string | null) => {
    const n = new URLSearchParams(params);
    if (name) n.set("agent", name);
    else n.delete("agent");
    setParams(n, { replace: true });
  };

  const drain = async (a: Agent, draining: boolean) => {
    agents.setData((l) => l.map((x) => (x.name === a.name ? { ...x, draining } : x)));
    try {
      await api.drainAgent(a.name, draining);
      toast({ title: draining ? `Draining ${a.name}` : `${a.name} accepting jobs`, tone: "info" });
    } catch (e) {
      agents.setData((l) => l.map((x) => (x.name === a.name ? { ...x, draining: !draining } : x)));
      toast.error("Drain failed", errorMessage(e));
    }
  };

  const all = agents.data.filter((a) => !a.ephemeral);
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
      agents.setData((l) => l.filter((x) => x.name !== a.name));
      if (selected === a.name) openAgent(null);
      toast({ title: `${a.name} forgotten`, tone: "info" });
    } catch (e) {
      toast.error("Forget failed", errorMessage(e));
    }
  };
  const details = (a: Agent) => (
    <Loadable label={`${a.name} in depth`} fallback={<Hold height={560} className="runner-details" />}>
      <AgentDetails agent={a} onClose={() => openAgent(null)} onDrain={drain} onForget={forget} onTerminal={() => navigate(`/terminal?open=${encodeURIComponent(`agent:${a.name}`)}`)} />
    </Loadable>
  );

  return (
    <>
      <Intro
        lead={
          <>
            {counts.connected} of {counts.all} runners connected · {busy}/{slots} slots busy. {LEAD}
          </>
        }
      />

      <JoinNotice requests={joinRequests} discovery={discovery} />
      <LowBattery agents={all} what={(m) => {
        const n = all.find((a) => a.name === m)?.running.length ?? 0;
        return n > 0 ? `its ${n} job${n === 1 ? "" : "s"}` : "";
      }} />
      <Upgrades />

      <RunnersTools q={q} onQ={setQ} facets={facets} chosen={chosen} onChosen={setChosen} view={view} onView={setView} />

      {all.length === 0 ? (
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
        <EmptyState
          title="No runners match."
          action={
            <Button
              variant="secondary"
              onClick={() => {
                setChosen({});
                setQ("");
              }}
            >
              Clear filters
            </Button>
          }
        />
      ) : view === "cards" ? (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4">
          {list.map((a) => (
            <React.Fragment key={a.name}>
              <AgentMiniCard agent={a} history={lastSamples(history, a.name, 120)} onOpen={() => openAgent(a.name === selected ? null : a.name)} />
              {selected === a.name && <div className="col-span-full">{details(a)}</div>}
            </React.Fragment>
          ))}
        </div>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <RunnersHead />
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
                      onClick={(e) => !(e.target as HTMLElement).closest("button,a,input,label") && toggle()}
                      onKeyDown={(e) => e.key === "Enter" && e.target === e.currentTarget && toggle()}
                    >
                      <td>
                        <StatusDot status={agentStatus(a)} title={agentStateLabel(a)} />
                      </td>
                      <td className="max-w-[260px] max-sm:w-full max-sm:max-w-0">
                        <div className="truncate text-[#e4e5e7]">{a.name}</div>
                        <div className="truncate font-mono text-[10px] text-[#7b7e88]">
                          {a.connected ? (
                            `${a.hostname} · ${a.os}/${a.arch} · v${a.version}`
                          ) : (
                            <>
                              last seen <RelTime ts={a.last_seen} />
                            </>
                          )}
                        </div>
                        {a.connected && <BatteryLine battery={a.battery} />}
                        <UpgradeNote agent={a} />
                      </td>
                      <td className="hidden md:table-cell">
                        <Chips labels={a.labels} max={4} />
                      </td>
                      {/* What it can run legs in, as it last probed (W21-R08). */}
                      <td className="cell-mono hidden lg:table-cell">{a.facts ? Object.keys(a.facts.executors).join(", ") || "—" : "—"}</td>
                      <td className="min-w-[90px]">
                        {a.outpost ? (
                          // Kiln run there without installing it: its terminals and agents, no jobs.
                          <div className="font-mono text-[11px] text-[#7b7e88]">outpost · no jobs</div>
                        ) : (
                          <>
                            <div className="font-mono text-[11px] text-[#c8cad0] tabular-nums">
                              {a.busy}/{a.slots}
                              {a.running.length > 0 && (
                                <span className="ml-1.5 text-[#7b7e88]">
                                  · {a.running.length} job{a.running.length > 1 ? "s" : ""}
                                </span>
                              )}
                            </div>
                            <div className="mt-1 w-20">
                              <Meter value={a.busy} max={Math.max(1, a.slots)} label="Slots busy" />
                            </div>
                          </>
                        )}
                      </td>
                      <td className="num cell-mono hidden sm:table-cell">{s ? formatPct(s.cpu_pct) : "—"}</td>
                      <td className="num cell-mono hidden lg:table-cell">{s ? formatPct(memPct(s)) : "—"}</td>
                      <td className="num cell-mono hidden xl:table-cell">{s ? formatPct(diskPct(s)) : "—"}</td>
                      <td className="num cell-mono hidden xl:table-cell">{s ? s.load1.toFixed(2) : "—"}</td>
                      <td className="hidden md:table-cell">{!a.outpost && <DrainCheck agent={a} onChange={(v) => drain(a, v)} />}</td>
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
                        <td colSpan={11}>{details(a)}</td>
                      </tr>
                    )}
                  </React.Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      {all.length > 0 && <FrontDoorSection agents={all} index="02" />}
      {all.length > 0 && <HeadroomSection agents={all} index="03" />}
    </>
  );
}

function RunnersTools({
  q,
  onQ,
  facets,
  chosen,
  onChosen,
  view,
  onView,
}: {
  q: string;
  onQ: (v: string) => void;
  facets: Facet[];
  chosen: Chosen;
  onChosen: (c: Chosen) => void;
  view: "table" | "cards";
  onView: (v: "table" | "cards") => void;
}) {
  return (
    <FilterBar className="runners-tools">
      <FacetBox label="Filter runners" placeholder="Filter by name, state, label, platform…" query={q} onQuery={onQ} facets={facets} chosen={chosen} onChosen={onChosen} />
      <SelectField
        label="Layout"
        hideLabel
        value={view}
        onChange={(v) => onView(v as "table" | "cards")}
        options={[
          { value: "table", label: "Table" },
          { value: "cards", label: "Cards" },
        ]}
      />
    </FilterBar>
  );
}

function RunnersHead() {
  return (
    <thead>
      <tr>
        <th className="w-6" aria-label="State" />
        <th>Runner</th>
        <th className="hidden md:table-cell">Labels</th>
        <th className="hidden lg:table-cell">Executors</th>
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
  );
}

/** The fleet while it loads: its intro, its tools, the table's head and its rows' space. */
function RunnersHold() {
  const [view] = usePref<"table" | "cards">("kiln.agents.view", "table");
  const noop = () => undefined;
  return (
    <>
      <Intro lead={LEAD} />
      <RunnersTools q="" onQ={noop} facets={[]} chosen={{}} onChosen={noop} view={view} onView={noop} />
      {view === "cards" ? <Hold height={196} label="Loading the runners" /> : <TableHold head={<RunnersHead />} rows={6} rowHeight={57} label="Loading the runners" />}
    </>
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

/** Drain as a checkbox: on, the runner takes no new jobs. */
function DrainCheck({ agent, onChange }: { agent: Agent; onChange: (v: boolean) => void }) {
  return (
    <label className="inline-flex min-h-8 cursor-pointer items-center gap-2">
      <input type="checkbox" className="check" checked={agent.draining} disabled={!agent.connected} onChange={(e) => onChange(e.target.checked)} aria-label={`Drain ${agent.name}`} />
      <span className={cn("font-mono text-[10px]", agent.draining ? "text-[#cbb69a]" : "text-[#7b7e88]")}>{agent.draining ? "draining" : "off"}</span>
    </label>
  );
}

/** A runner's last hour of samples (5 s apart), growing as samples arrive. */
function useAgentHistory(name: string) {
  const res = useSuspenseResource(`metrics/history/${name}`, (s) => api.metricsHistory(name, s).then((h) => h[name] ?? []));
  useServerEvents((ev) => {
    if (ev.type === "sample" && ev.agent === name) res.setData((l) => [...l, ev.sample].slice(-720));
  });
  return res.data;
}

/** The jobs a runner runs now, read again as that list changes. */
function useRunningJobs(ids: string[]): Job[] {
  const key = ids.join(",");
  return useSuspenseResource(`jobs/${key}`, async (s) => (key ? (await Promise.all(ids.map((id) => api.job(id, s).catch(() => null)))).filter((j): j is Job => j !== null) : [])).data;
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
  const samples = useAgentHistory(agent.name);
  const jobs = useRunningJobs(agent.running);

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
        <Button variant="secondary" size="xs" disabled={!agent.connected} onClick={onTerminal} title="Open a terminal on it">
          Terminal
        </Button>
        {!agent.outpost && (
          <Button asChild variant="secondary" size="xs">
            <Link to={`/insights?runner=${encodeURIComponent(agent.name)}`}>Insights</Link>
          </Button>
        )}
        {!agent.outpost && (
          <Button variant="secondary" size="xs" disabled={!agent.connected} onClick={() => onDrain(agent, !agent.draining)}>
            {agent.draining ? "Resume" : "Drain"}
          </Button>
        )}
        {!forgetting && (
          <Button variant="secondary" size="xs" className="text-[#d1aaad]" onClick={() => setForgetting(true)}>
            Forget
          </Button>
        )}
      </div>
      {forgetting && (
        <Confirm
          className="mt-3"
          label={`Forget ${agent.name}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" onClick={() => void onForget(agent).finally(() => setForgetting(false))}>
                Forget runner
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setForgetting(false)}>
                Keep it
              </Button>
            </>
          }
        >
          <p>
            Forget {agent.name}? {agent.connected ? "It is connected: it will be disconnected and must enroll again to come back." : "Its enrollment is removed: it must enroll again to come back."}
          </p>
        </Confirm>
      )}

      <div className="mt-6">
        <WatchedBy on={`runner:${agent.name}`} watches={(on) => on === `runner:${agent.name}` || on === "runners"} />
      </div>

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
            { label: "Lost below the channel", value: lostLine(agent) },
          ]}
        />
      </div>

      <div className="mt-8">
        <Loadable label={`${agent.name}'s agent sessions`} fallback={<Hold height={142} />}>
          <AgentLimit key={agent.name} runner={agent.name} />
        </Loadable>
      </div>

      {agent.facts && (
        <div className="mt-8">
          <div className="label mb-2">Facts</div>
          <RunnerFactsList facts={agent.facts} />
        </div>
      )}

      <div className="mt-8">
        <div className="label mb-2">Running jobs</div>
        {agent.running.length === 0 ? (
          <p className="empty-line">Idle.</p>
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
            {jobs.length === 0 &&
              agent.running.map((id) => (
                <div key={id} className="py-2 font-mono text-[11px] text-[#8c8f98]">
                  {id}
                </div>
              ))}
          </div>
        )}
      </div>

      <div className="mt-8">
        <div className="label mb-3">Last hour · 5 s resolution</div>
        {samples.length < 2 ? <p className="empty-line">Not enough samples yet.</p> : <HostCharts samples={samples} height={130} columns={2} />}
      </div>
    </section>
  );
}

/**
 * The most agents a runner has at work at once (docs/agents.md, "How many agents a machine runs"):
 * set here, it wins over the runner's config (`[agent] max_agent_sessions`), which wins over kiln's
 * default. Only agents at work count. Lowered under what's at work, those go on; a new one is
 * refused until fewer are at work.
 */
function AgentLimit({ runner }: { runner: string }) {
  const view = useSuspenseResource(`agent-limit/${runner}`, (s) => api.agentLimit(runner, s), { on: ["agent_limit", "agent", "harness", "harness_closed"], apply: withEvent });
  const v = view.data;
  const id = React.useId();
  const [draft, setDraft] = React.useState<string | null>(null);
  const [saving, setSaving] = React.useState(false);
  const [msg, setMsg] = React.useState<{ tone: "ok" | "error"; text: string } | null>(null);
  // Until something is typed, the field follows what holds (another tab, another admin, its config).
  const shown = draft ?? String(v.max_sessions ?? v.held?.max_sessions ?? v.default);
  const save = async (max: number | null) => {
    setSaving(true);
    try {
      const next = await api.setAgentLimit(runner, max);
      view.setData(() => next);
      setDraft(null);
      setMsg({ tone: "ok", text: max === null ? `Nothing set here now: ${runner} holds its config's, or kiln's default.` : `Saved: at most ${max} agents at work at once on ${runner}.` });
    } catch (e) {
      setMsg({ tone: "error", text: errorMessage(e) });
    } finally {
      setSaving(false);
    }
  };
  const submit = () => {
    const n = parseLimit(shown, v.max);
    if (typeof n === "string") setMsg({ tone: "error", text: n });
    else void save(n);
  };
  return (
    <section className="runner-limit" aria-labelledby={`${id}-head`}>
      <div className="label mb-2" id={`${id}-head`}>
        Agent sessions
      </div>
      <p className="runner-limit-line">{limitLine(v)}</p>
      {settable(v) && (
        <form
          className="runner-limit-form"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          <label htmlFor={id} className="sr-only">
            The most agents at work at once on {runner}
          </label>
          <div className="field runner-limit-field">
            <input
              id={id}
              type="text"
              inputMode="numeric"
              autoComplete="off"
              value={shown}
              aria-invalid={msg?.tone === "error" || undefined}
              onChange={(e) => {
                setDraft(e.target.value);
                setMsg(null);
              }}
            />
          </div>
          <Button type="submit" variant="secondary" size="xs" disabled={saving || draft === null}>
            {saving ? "Saving…" : "Save"}
          </Button>
          {v.max_sessions !== null && (
            <Button type="button" variant="ghost" size="xs" disabled={saving} onClick={() => void save(null)}>
              Use its config
            </Button>
          )}
        </form>
      )}
      <p className="runner-limit-msg" role="status" data-tone={msg?.tone ?? "quiet"}>
        {msg?.text ?? (standing(v) || " ")}
      </p>
      <p className="field-hint">
        Only agents at work count: starting, on a turn, or waiting on you in the middle of one. Finished, ready, paused and ended agents don&rsquo;t. At the limit a new agent is refused, and
        what&rsquo;s sent to one already there waits in its queue until there&rsquo;s room. Set here, it wins over the runner&rsquo;s config (<code>[agent] max_agent_sessions</code>), which wins over
        kiln&rsquo;s default of {v.default}.
      </p>
    </section>
  );
}

/** What a runner's probes found and its administrator declared (docs/ci.md W13-R01, R02): what
 * routing decides over. Declared facts read as constraints name them (`custom.<name>`). */
function RunnerFactsList({ facts }: { facts: RunnerFacts }) {
  const os = [facts.os_distro ?? facts.os, facts.os_version].filter(Boolean).join(" ");
  const executors = Object.entries(facts.executors).map(([name, e]) => [name, e.version, e.details?.rootless === "true" ? "rootless" : null].filter(Boolean).join(" "));
  const items = [
    { label: "System", value: [os, facts.arch, facts.kernel && `kernel ${facts.kernel}`].filter(Boolean).join(" · ") },
    { label: "Processor", value: `${facts.cpu.model ?? "—"} · ${facts.cpu.cores} cores, ${facts.cpu.threads} threads` },
    ...(facts.gpus.length > 0 ? [{ label: "GPU", value: facts.gpus.map((g) => [g.model, g.memory > 0 && formatBytes(g.memory)].filter(Boolean).join(" · ")).join(", ") }] : []),
    { label: "Virtualization", value: [facts.virt.kvm && "KVM", facts.virt.nested && "nested", facts.virt.hvf && "Hypervisor.framework"].filter(Boolean).join(", ") || "none" },
    { label: "Executors", value: executors.join(", ") },
    {
      label: "Tools",
      value:
        Object.entries(facts.tools)
          .map(([n, v]) => `${n} ${v}`)
          .join(", ") || "—",
    },
    { label: "Internet", value: facts.net.egress ? "direct" : facts.net.proxy ? `through ${facts.net.proxy}` : "none" },
    ...Object.entries(facts.custom).map(([k, v]) => ({ label: `custom.${k}`, value: v })),
    { label: "Probed", value: <RelTime ts={facts.probed_at} /> },
  ];
  return <Facts items={items} />;
}

/**
 * Runners on an older build than the one this server hands out for their platform: upgraded from
 * here, each confirmed in place; nothing they run stops. A platform with no build here yet is built
 * by kiln's own release pipeline first.
 */
function Upgrades() {
  const data = useRes(updatesRes).data;
  const live = useUpdateJobs();
  const [asking, setAsking] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);
  const upgrading = new Set(
    mergeJobs(data, live)
      .filter((j) => j.kind === "runner" && j.state === "running")
      .map((j) => j.target),
  );
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
      invalidate("updates");
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
      invalidate("updates");
    }
  };
  const confirm = (text: string, runners: RunnerUpdate[]) => (
    <Confirm
      label="Confirm the upgrade"
      actions={
        <>
          <Button size="xs" disabled={busy} onClick={() => void upgrade(runners)}>
            Upgrade
          </Button>
          <Button size="xs" variant="ghost" disabled={busy} onClick={() => setAsking(null)}>
            Cancel
          </Button>
        </>
      }
    >
      <p>{text}</p>
    </Confirm>
  );

  return (
    <section className="runner-up" aria-label="Upgrades">
      <div className="runner-up-head">
        <p>{behind.length === 1 ? "1 runner is" : `${behind.length} runners are`} on an older build than this server hands out.</p>
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
                Building every platform · <Link to={`/runs/${last?.id ?? ""}`}>run #{last?.number}</Link>; {lacking.map((r) => r.name).join(", ")} can be upgraded when it finishes.
              </>
            ) : (
              <>
                kiln builds {[...new Set(lacking.map((r) => r.target ?? r.os))].join(", ")} itself, on the server&rsquo;s own runner, then {lacking.length === 1 ? "it" : "they"} can be upgraded.
              </>
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
