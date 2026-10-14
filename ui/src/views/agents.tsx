import * as React from "react";
import { Link, useNavigate, useSearchParams } from "react-router";
import { DropdownMenu, Switch } from "radix-ui";
import { LayoutGrid, List, MoreHorizontal, Plus, Server, SquareTerminal, Trash2 } from "lucide-react";
import { api } from "@/api/client";
import { useServerEvents } from "@/api/events";
import type { Agent, HostSample, Job } from "@/api/types";
import { Button } from "@/components/ui/button";
import { ConfirmDialog, Dialog, DialogContent } from "@/components/ui/dialog";
import { CopyButton, EmptyState, ErrorState, Facts, Meter, PageIntro, SearchField, Skeleton, SkeletonRows } from "@/components/ui/misc";
import { Chips, StatusDot } from "@/components/ui/status";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { Tip } from "@/components/ui/tooltip";
import { absoluteTime, formatBytes, formatPct } from "@/lib/format";
import { errorMessage, useDocumentTitle, useLocalStorage, useResource } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { HostCharts } from "./host-charts";
import { AgentMiniCard, LiveDuration, RelTime, diskPct, memPct, upsert } from "./shared";

type Filter = "all" | "connected" | "baremetal" | "k8s" | "draining";

export function agentStatus(a: Agent) {
  return !a.connected ? "cancelled" : a.draining ? "timed_out" : a.busy > 0 ? "running" : "succeeded";
}
export function agentStateLabel(a: Agent) {
  return !a.connected ? "offline" : a.draining ? "draining" : a.busy > 0 ? "busy" : "idle";
}

export default function Agents() {
  useDocumentTitle("Agents");
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const agents = useResource((s) => api.agents(s), [], { pollMs: 30_000 });
  const [history, setHistory] = React.useState<Record<string, HostSample[]>>({});
  const [view, setView] = useLocalStorage<"table" | "cards">("kiln.agents.view", "table");
  const [filter, setFilter] = React.useState<Filter>("all");
  const [q, setQ] = React.useState("");
  const [joinOpen, setJoinOpen] = React.useState(params.get("join") === "1");
  const [forget, setForget] = React.useState<Agent | null>(null);
  const selected = params.get("agent");

  React.useEffect(() => {
    if (params.get("join") === "1") {
      setJoinOpen(true);
      const n = new URLSearchParams(params);
      n.delete("join");
      setParams(n, { replace: true });
    }
  }, [params, setParams]);

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

  const all = agents.data ?? [];
  const counts: Record<Filter, number> = {
    all: all.length,
    connected: all.filter((a) => a.connected).length,
    baremetal: all.filter((a) => !a.ephemeral).length,
    k8s: all.filter((a) => a.ephemeral).length,
    draining: all.filter((a) => a.draining).length,
  };
  const ql = q.trim().toLowerCase();
  const list = all
    .filter((a) =>
      filter === "connected" ? a.connected : filter === "baremetal" ? !a.ephemeral : filter === "k8s" ? a.ephemeral : filter === "draining" ? a.draining : true,
    )
    .filter((a) => !ql || `${a.name} ${a.hostname} ${a.labels.join(" ")}`.toLowerCase().includes(ql))
    .sort((a, b) => Number(b.connected) - Number(a.connected) || a.name.localeCompare(b.name));

  const slots = all.filter((a) => a.connected).reduce((s, a) => s + a.slots, 0);
  const busy = all.filter((a) => a.connected).reduce((s, a) => s + a.busy, 0);
  const selAgent = all.find((a) => a.name === selected) ?? null;

  const openAgent = (name: string | null) => {
    const n = new URLSearchParams(params);
    if (name) n.set("agent", name);
    else n.delete("agent");
    setParams(n, { replace: true });
  };

  return (
    <div className="container-console page">
      <PageIntro
        index={4}
        label="Agents"
        title="Fleet"
        lead={
          agents.data ? (
            <>
              {counts.connected} of {counts.all} agents connected · {busy}/{slots} slots busy. Bare-metal agents are long-lived daemons; Kubernetes agents are ephemeral pods launched per job.
            </>
          ) : (
            "Bare-metal agents are long-lived daemons; Kubernetes agents are ephemeral pods launched per job."
          )
        }
        actions={
          <Button onClick={() => setJoinOpen(true)}>
            <Plus /> Add bare-metal agent
          </Button>
        }
      />

      <div className="mb-5 flex flex-wrap items-center gap-3">
        <Segmented
          label="Filter agents"
          value={filter}
          onChange={setFilter}
          options={[
            { value: "all", label: "All", count: counts.all },
            { value: "connected", label: "Connected", count: counts.connected },
            { value: "baremetal", label: "Bare metal", count: counts.baremetal },
            { value: "k8s", label: "Pods", count: counts.k8s },
            { value: "draining", label: "Draining", count: counts.draining },
          ]}
        />
        <div className="ml-auto flex w-full items-center gap-2 sm:w-auto">
          <SearchField className="min-w-0 flex-1 sm:w-56" label="Search agents" placeholder="Name, host, label…" value={q} onChange={setQ} />
          <Segmented
            label="Layout"
            value={view}
            onChange={setView}
            options={[
              { value: "table", label: <List size={14} aria-label="Table" /> },
              { value: "cards", label: <LayoutGrid size={14} aria-label="Cards" /> },
            ]}
          />
        </div>
      </div>

      {agents.error && !agents.data ? (
        <ErrorState error={agents.error} onRetry={agents.reload} />
      ) : !agents.data ? (
        <SkeletonRows rows={6} />
      ) : all.length === 0 ? (
        <EmptyState
          icon={<Server size={24} />}
          title="No agents have enrolled."
          action={
            <Button variant="secondary" onClick={() => setJoinOpen(true)}>
              <Plus /> Add bare-metal agent
            </Button>
          }
        >
          Run the join command on a machine to enroll it, or configure Kubernetes in the server config to launch pod agents on demand.
        </EmptyState>
      ) : list.length === 0 ? (
        <EmptyState title="No agents match." action={<Button variant="secondary" onClick={() => { setFilter("all"); setQ(""); }}>Clear filters</Button>} />
      ) : view === "cards" ? (
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3 2xl:grid-cols-4">
          {list.map((a) => (
            <AgentMiniCard key={a.name} agent={a} history={history[a.name] ?? []} onOpen={() => openAgent(a.name)} />
          ))}
        </div>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <tr>
                <th className="w-6" aria-label="State" />
                <th>Agent</th>
                <th className="hidden md:table-cell">Labels</th>
                <th className="hidden lg:table-cell">Kind</th>
                <th>Slots</th>
                <th className="num hidden sm:table-cell">CPU</th>
                <th className="num hidden lg:table-cell">Mem</th>
                <th className="num hidden xl:table-cell">Disk</th>
                <th className="num hidden xl:table-cell">Load</th>
                <th className="hidden md:table-cell">Drain</th>
                <th className="w-0">
                  <span className="sr-only">Actions</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {list.map((a) => {
                const s = a.sample;
                return (
                  <tr key={a.name} data-href tabIndex={0} onClick={(e) => !(e.target as HTMLElement).closest("button,a,[role=switch]") && openAgent(a.name)} onKeyDown={(e) => e.key === "Enter" && openAgent(a.name)}>
                    <td>
                      <StatusDot status={agentStatus(a)} title={agentStateLabel(a)} />
                    </td>
                    <td className="max-w-[260px]">
                      <div className="truncate text-[#e4e5e7]">{a.name}</div>
                      <div className="truncate font-mono text-[10px] text-[#7b7e88]">
                        {a.connected ? `${a.hostname} · ${a.os}/${a.arch} · v${a.version}` : <>last seen <RelTime ts={a.last_seen} /></>}
                      </div>
                    </td>
                    <td className="hidden md:table-cell">
                      <Chips labels={a.labels} max={4} />
                    </td>
                    <td className="hidden lg:table-cell">
                      <span className="font-mono text-[10px] text-[#a1a4ad]">{a.ephemeral ? "pod · ephemeral" : "persistent"}</span>
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
                    <td className="text-right whitespace-nowrap">
                      <AgentActions agent={a} onForget={() => setForget(a)} onTerminal={() => navigate(`/terminal?open=${encodeURIComponent(`agent:${a.name}`)}`)} />
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}

      <JoinDialog open={joinOpen} onOpenChange={setJoinOpen} />
      <AgentDrawer agent={selAgent} onClose={() => openAgent(null)} onDrain={drain} onForget={(a) => setForget(a)} />
      {forget && (
        <ConfirmDialog
          open
          onOpenChange={(o) => !o && setForget(null)}
          title={`Forget ${forget.name}?`}
          description={forget.connected ? "The agent is connected; it will be disconnected and must re-enroll to come back." : "Removes the agent and its enrollment. It must re-enroll to come back."}
          confirmLabel="Forget agent"
          danger
          onConfirm={async () => {
            try {
              await api.forgetAgent(forget.name);
              agents.setData((l) => l?.filter((x) => x.name !== forget.name));
              if (selected === forget.name) openAgent(null);
              toast({ title: `${forget.name} forgotten`, tone: "info" });
            } catch (e) {
              toast.error("Forget failed", errorMessage(e));
            }
          }}
        />
      )}
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

function AgentActions({ agent, onForget, onTerminal }: { agent: Agent; onForget: () => void; onTerminal: () => void }) {
  return (
    <div className="inline-flex items-center">
      <Tip content={agent.connected ? "Open terminal" : "Agent offline"}>
        <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`Open terminal on ${agent.name}`} disabled={!agent.connected} onClick={onTerminal}>
          <SquareTerminal size={14} />
        </button>
      </Tip>
      <DropdownMenu.Root>
        <DropdownMenu.Trigger asChild>
          <button className="icon-btn" data-variant="bare" data-size="sm" aria-label={`More actions for ${agent.name}`}>
            <MoreHorizontal size={15} />
          </button>
        </DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content className="popover" align="end" sideOffset={4}>
            <DropdownMenu.Item className="menu-item" data-danger onSelect={onForget}>
              <Trash2 size={14} /> Forget agent…
            </DropdownMenu.Item>
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>
    </div>
  );
}

function JoinDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  const [info, setInfo] = React.useState<{ token: string; server: string; command: string } | null>(null);
  const [err, setErr] = React.useState<string | null>(null);
  React.useEffect(() => {
    if (!open) return;
    setInfo(null);
    setErr(null);
    const ctl = new AbortController();
    api
      .join(ctl.signal)
      .then(setInfo)
      .catch((e) => !ctl.signal.aborted && setErr(errorMessage(e)));
    return () => ctl.abort();
  }, [open]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title="Add a bare-metal agent"
        eyebrow="Agents / Join"
        description="Run this on the machine you want to enroll. It installs kiln, enrolls over QUIC + mTLS, and starts the agent on boot (systemd, cron @reboot, or XDG autostart)."
        className="!w-[min(680px,calc(100vw-24px))]"
      >
        {err ? (
          <ErrorState error={err} />
        ) : !info ? (
          <Skeleton className="h-24 w-full" />
        ) : (
          <div className="space-y-4">
            <div className="relative">
              <pre className="overflow-x-auto rounded-md border border-[#2b2e39] bg-[#101217] p-4 pr-4 font-mono text-[12px] leading-relaxed whitespace-pre-wrap break-all text-[#cdc4e0]">
                <span className="text-[#6d7078] select-none">$ </span>
                {info.command}
              </pre>
            </div>
            <div className="flex flex-wrap items-center justify-between gap-3">
              <Facts
                className="min-w-0 flex-1"
                items={[
                  { label: "Server", value: info.server },
                  { label: "Token", value: `${info.token.slice(0, 6)}…${info.token.slice(-4)}` },
                ]}
              />
              <CopyButton text={info.command} label="Copy command" />
            </div>
            <p className="text-[11.5px] leading-relaxed text-[#8f939e]">
              The agent shows up here as soon as it connects. Give it labels like <span className="chip">bench</span> or <span className="chip">gpu</span> to target it with{" "}
              <code className="font-mono text-[#cdc4e0]">runs-on</code>.
            </p>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

function AgentDrawer({
  agent,
  onClose,
  onDrain,
  onForget,
}: {
  agent: Agent | null;
  onClose: () => void;
  onDrain: (a: Agent, v: boolean) => void;
  onForget: (a: Agent) => void;
}) {
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

  const s = agent?.sample;
  return (
    <Dialog open={Boolean(agent)} onOpenChange={(o) => !o && onClose()}>
      {agent && (
        <DialogContent variant="drawer" title={agent.name} eyebrow={`Agent · ${agentStateLabel(agent)}`} description={`${agent.hostname} · ${agent.os}/${agent.arch} · kiln v${agent.version}`}>
          <div className="flex flex-wrap items-center gap-2">
            <Chips labels={agent.labels} max={12} />
          </div>
          <div className="mt-5 flex flex-wrap gap-2">
            <Button asChild variant="secondary" size="sm" aria-disabled={!agent.connected}>
              <Link to={`/terminal?open=${encodeURIComponent(`agent:${agent.name}`)}`}>
                <SquareTerminal /> Open terminal
              </Link>
            </Button>
            <Button variant="ghost" size="sm" disabled={!agent.connected} onClick={() => onDrain(agent, !agent.draining)}>
              {agent.draining ? "Resume scheduling" : "Drain"}
            </Button>
            <Button variant="ghost" size="sm" className="text-[#d1aaad]" onClick={() => onForget(agent)}>
              <Trash2 /> Forget
            </Button>
          </div>

          <div className="mt-6">
            <Facts
              items={[
                { label: "Kind", value: agent.ephemeral ? "pod (ephemeral)" : "persistent" },
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
            {err ? <ErrorState error={err} compact /> : !samples ? <Skeleton className="h-64 w-full" /> : samples.length < 2 ? <p className="text-[12.5px] text-[#8f939e]">Not enough samples yet.</p> : <HostCharts samples={samples} height={130} columns={1} />}
          </div>
        </DialogContent>
      )}
    </Dialog>
  );
}
