/**
 * A session's engine in its Mission row's dropdown (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2,
 * P3; every session's dropdown has it): for each sandbox it runs in, its own and its kiln subagents',
 * its line (its engine and name, its stand in the agents' words' colours, where it runs and its size,
 * and its controls at the line's right end: round, small, of their intent's colour, its stop red and
 * asked again), then everything it is under that line: its CPU, memory and disk now, each with its
 * last hour, its network's last hour, its harness's context and cost, then its sections: what it is,
 * its harness and agents (swapped in place), its network (ports published, egress granted, networks
 * connected), its volumes and directories. In the Engine tab its sections fold, as that tab's own do;
 * in a Mission row's dropdown they're shown whole (a fold there would be a dropdown nested in it).
 */
import * as React from "react";
import { Check, Pause, Play, RotateCcw, Square, X } from "lucide-react";
import { sandboxApi, sandboxesRes, sandboxRes, type SandboxBrief } from "@/api/microvms";
import { sessionStore, type AgentInfo } from "@/api/harness";
import { Loadable } from "@/components/ui/loading";
import { Sparkline } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { labelOf, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { EngineGlyph } from "@/views/sandbox-marks";
import { GroupHead, StandCounts } from "./definitions";
import { agentLabel } from "./derive";
import { inspectorTabsRes } from "./inspector-code";
import { LimitMeter } from "./limits";
import { subagentTree } from "./subagent-tree";
import "./definition.css";
import "./figure/figure.css";

type State = SandboxBrief["state"];

/** A sandbox's state in its own word, coloured as the agents' stands are (definition.css .df-vm[data-stand]). */
const WORD: Record<State, string> = { running: "Running", idle: "Idle", paused: "Paused", failed: "Failed", draining: "Draining", ended: "Ended" };
const STAND: Record<State, string> = { running: "working", draining: "working", idle: "ready", paused: "paused", failed: "crashed", ended: "ended" };
/** As the group's head counts them, a dot each stand (definitions.tsx StandCounts). */
const COUNTED: Record<State, string> = { running: "working", draining: "working", idle: "done", paused: "paused", failed: "failed", ended: "stopped" };
const COUNT_WORD: Record<string, string> = { working: "Running", done: "Idle", paused: "Paused", failed: "Failed", stopped: "Ended" };

/** Its engine: the group's head (how many stand where), then each sandbox whole. */
export function EngineRows({ a, list }: { a: AgentInfo; list: readonly AgentInfo[] }) {
  return (
    <Loadable label={`Loading ${agentLabel(a)}'s engine`} fallback={<li className="ms-node df-head" aria-busy="true" />}>
      <EngineRowsRead a={a} list={list} />
    </Loadable>
  );
}

/** The agents a session's sandboxes are: its own, then each kiln subagent's, as its tree has them, those with one. */
function useSandboxed(a: AgentInfo, list: readonly AgentInfo[], all: Record<string, SandboxBrief>): AgentInfo[] {
  const kiln = React.useMemo(() => [...new Set(subagentTree(a, list).flatMap((n) => (n.kind === "kiln" ? [n.session] : [])))], [a, list]);
  return [a, ...kiln.flatMap((id) => list.filter((x) => x.id === id))].filter((x) => all[x.id]);
}

/** Its agent chat's Engine tab's first part: each of its sandboxes whole, as its Mission row's dropdown shows them, on the tab's left edge. */
export function EngineList({ a, list }: { a: AgentInfo; list: readonly AgentInfo[] }) {
  const all = useRes(sandboxesRes).data;
  const agents = useSandboxed(a, list, all);
  if (agents.length === 0) return <p className="in-none">It runs in no sandbox kiln knows of.</p>;
  return (
    <ul className="ms-list eg-list" aria-label={`${agentLabel(a)}'s engine`}>
      {agents.map((x) => (
        <EngineVm key={x.id} a={x} b={all[x.id]!} />
      ))}
    </ul>
  );
}

function EngineRowsRead({ a, list }: { a: AgentInfo; list: readonly AgentInfo[] }) {
  const all = useRes(sandboxesRes).data;
  const [shut, setShut] = React.useState(false);
  const agents = useSandboxed(a, list, all);
  if (agents.length === 0) return null;
  const states = agents.map((x) => all[x.id]!.state);
  return (
    <>
      <GroupHead
        label="Engine"
        open={!shut}
        onOpen={() => setShut(!shut)}
        stand={states.some((s) => s === "running" || s === "draining") ? "working" : "done"}
        summary={<StandCounts stands={states.map((s) => COUNTED[s])} words={COUNT_WORD} />}
      />
      {!shut && agents.map((x) => <EngineVm key={x.id} a={x} b={all[x.id]!} fixed />)}
    </>
  );
}

/** One sandbox: its line, its controls at the line's right end; under it, its resources and its sections (`fixed`: shown whole, in a dropdown). */
function EngineVm({ a, b, fixed }: { a: AgentInfo; b: SandboxBrief; fixed?: boolean }) {
  const name = agentLabel(a);
  const [asking, setAsking] = React.useState(false);
  // What a press made of it until the server says what it is: a new state from the server replaces it.
  const [state, setState] = React.useState<State | null>(null);
  const [said, setSaid] = React.useState(b.state);
  if (said !== b.state) {
    setSaid(b.state);
    setState(null);
  }
  const now = state ?? b.state;
  const host = b.engine === "host";
  const ended = now === "ended";
  const act = (what: "pause" | "resume" | "restart" | "stop") => {
    if (what === "pause" || what === "resume") setState(what === "pause" ? "paused" : "running");
    sandboxApi
      .act(a.id, what)
      .then((d) => {
        setState(d.state);
        invalidate("microvm/");
        if (what === "pause") toast({ title: `Paused ${name}'s vCPUs`, tone: "info", action: { label: "Undo", run: () => act("resume") } });
        else if (what === "restart") toast({ title: `Restarting ${name}`, tone: "running" });
        else if (what === "stop") toast({ title: `Stopped ${name}`, tone: "info" });
      })
      .catch((e: unknown) => {
        setState(null);
        toast.error(`Couldn't ${what} ${name}`, errorMessage(e));
      });
  };
  const mem = `${Math.round((b.memory.total / 2 ** 30) * 10) / 10} GiB`;
  return (
    <>
      <li className="ms-node df-vm" data-stand={STAND[now]} data-depth={2}>
        <span className="df-vm-text">
          <span className="df-vm-name">
            <EngineGlyph engine={b.engine} />
            <span>{name}</span>
          </span>
          <span className="df-vm-line">
            <span className="ms-word">
              <i aria-hidden="true" />
              {WORD[now]}
            </span>
            <span>{labelOf(b.runner)}</span>
            {!host && <span>{mem}</span>}
          </span>
        </span>
        {!ended && (
          <span className="df-vm-ctl">
            {asking ? (
              <span className="ms-confirm" role="group" aria-label={`Stop ${name}?`}>
                <span>Stop it?</span>
                <button
                  type="button"
                  className="fig-round"
                  data-size="sm"
                  data-tone="stop"
                  aria-label={`Stop ${name}`}
                  onClick={() => {
                    setAsking(false);
                    act("stop");
                  }}
                >
                  <Check size={15} />
                </button>
                <button type="button" className="fig-round" data-size="sm" aria-label="Keep it running" onClick={() => setAsking(false)}>
                  <X size={15} />
                </button>
              </span>
            ) : (
              <>
                {!host && (
                  <button
                    type="button"
                    className="fig-round"
                    data-size="sm"
                    aria-label={`${now === "paused" ? "Resume" : "Pause"} ${name}`}
                    title={now === "paused" ? "Resume its vCPUs" : "Pause its vCPUs: memory and disk stay as they are"}
                    onClick={() => act(now === "paused" ? "resume" : "pause")}
                  >
                    {now === "paused" ? <Play size={15} /> : <Pause size={15} />}
                  </button>
                )}
                <button type="button" className="fig-round" data-size="sm" aria-label={`Restart ${name}`} title="Restart it" onClick={() => act("restart")}>
                  <RotateCcw size={14} />
                </button>
                <button type="button" className="fig-round" data-size="sm" data-tone="stop" aria-label={`Stop ${name}`} title="Stop it" onClick={() => setAsking(true)}>
                  <Square size={13} />
                </button>
              </>
            )}
          </span>
        )}
      </li>
      <li className="ms-node df-vm-body" data-depth={3}>
        <Loadable label={`Loading ${name}'s sandbox`}>
          <EngineUsage a={a} />
          <SandboxBlock agentId={a.id} fixed={fixed} />
        </Loadable>
      </li>
    </>
  );
}

/** Its resources now, a bar each with its last hour plotted under it, its network's last hour, then its harness's context and cost. */
function EngineUsage({ a }: { a: AgentInfo }) {
  const d = useRes(sandboxRes(a.id)).data;
  const gib = (n: number) => Math.round((n / 2 ** 30) * 10) / 10;
  const now = d.now;
  const rows: { id: string; label: string; pct: number; used?: number; limit?: number; unit?: string; series: number[] }[] = now
    ? [
        { id: "cpu", label: "CPU", pct: now.cpu_pct, ...(d.vm ? { used: Math.round((now.cpu_pct / 100) * d.vm.vcpus * 100) / 100, limit: d.vm.vcpus, unit: "vCPU" } : {}), series: d.series.cpu },
        { id: "memory", label: "Memory", pct: (now.memory.used / now.memory.total) * 100, used: gib(now.memory.used), limit: gib(now.memory.total), unit: "GiB", series: d.series.memory },
        { id: "disk", label: "Disk", pct: (now.disk.used / now.disk.total) * 100, used: gib(now.disk.used), limit: gib(now.disk.total), unit: "GiB", series: d.series.disk },
      ]
    : [];
  const net = d.series.net;
  return (
    <div className="df-usage">
      {rows.map((r) => (
        <div key={r.id} className="df-urow">
          <LimitMeter live limit={{ id: r.id, label: r.label, used_percent: r.pct, used: r.used, limit: r.limit, unit: r.unit }} />
          <Sparkline values={r.series} max={100} width={480} height={28} className="df-uplot" />
        </div>
      ))}
      {net.length > 1 && (
        <div className="df-urow">
          <span className="df-unet">
            <span>Network</span>
            <span className="df-head-v">{((net.at(-1) ?? 0) / 10).toFixed(1)} MB/s</span>
          </span>
          <Sparkline values={net} width={480} height={28} className="df-uplot" />
        </div>
      )}
      {a.context_pct !== undefined && (
        <div className="df-urow">
          <LimitMeter limit={{ id: "context", label: "Context", used_percent: a.context_pct }} />
        </div>
      )}
      <span className="df-ucost">
        <span className="ms-edit-k">cost</span>${a.cost_usd.toFixed(2)}
      </span>
    </div>
  );
}

/** Its sections, its line above holding its state and controls (loaded as the inspector's tabs' code is). */
function SandboxBlock({ agentId, fixed }: { agentId: string; fixed?: boolean }) {
  const { SandboxTab } = useRes(inspectorTabsRes).data;
  return <SandboxTab agentId={agentId} store={sessionStore(agentId)} inRow fixed={fixed} />;
}
