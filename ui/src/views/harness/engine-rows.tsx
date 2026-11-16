/**
 * A session's engine in its Mission row's dropdown (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2,
 * P3; every session's dropdown has it): a line for each sandbox it runs in, its own and its kiln
 * subagents', each its engine and name, its stand in the agents' words' colours and where it runs, and
 * its controls on its name's line (round, small, of their intent's colour: its stop red, asked again).
 * Opened, everything its agent chat's Sandbox tab manages, under it: its live bars, what it is, its
 * harness and agents (swapped in place), its network (ports published, egress granted, networks
 * connected), its volumes and directories, and its last hour plotted.
 */
import * as React from "react";
import { Check, ChevronRight, Pause, Play, RotateCcw, Square, X } from "lucide-react";
import { sandboxApi, sandboxesRes, type SandboxBrief } from "@/api/microvms";
import { sessionStore, type AgentInfo } from "@/api/harness";
import { Loadable } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { labelOf, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { EngineGlyph } from "@/views/sandbox-marks";
import { GroupHead, StandCounts } from "./definitions";
import { agentLabel } from "./derive";
import { inspectorTabsRes } from "./inspector-code";
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

/** Its engine: the group's head (how many stand where), then a line each, the one opened its whole under it. */
export function EngineRows({ a, list }: { a: AgentInfo; list: readonly AgentInfo[] }) {
  return (
    <Loadable label={`Loading ${agentLabel(a)}'s engine`} fallback={<li className="ms-node df-head" aria-busy="true" />}>
      <EngineRowsRead a={a} list={list} />
    </Loadable>
  );
}

function EngineRowsRead({ a, list }: { a: AgentInfo; list: readonly AgentInfo[] }) {
  const all = useRes(sandboxesRes).data;
  const [shut, setShut] = React.useState(false);
  const [shown, setShown] = React.useState<string | null>(null);
  // Its own, then each kiln subagent's, as its tree has them.
  const kiln = React.useMemo(() => [...new Set(subagentTree(a, list).flatMap((n) => (n.kind === "kiln" ? [n.session] : [])))], [a, list]);
  const ids = [a.id, ...kiln].filter((id) => all[id]);
  if (ids.length === 0) return null;
  const states = ids.map((id) => all[id]!.state);
  return (
    <>
      <GroupHead
        label="Engine"
        open={!shut}
        onOpen={() => setShut(!shut)}
        stand={states.some((s) => s === "running" || s === "draining") ? "working" : "done"}
        summary={<StandCounts stands={states.map((s) => COUNTED[s])} words={COUNT_WORD} />}
      />
      {!shut &&
        ids.map((id) => (
          <EngineLine
            key={id}
            id={id}
            b={all[id]!}
            name={id === a.id ? agentLabel(a) : agentLabel(list.find((x) => x.id === id) ?? a)}
            open={shown === id}
            onOpen={() => setShown(shown === id ? null : id)}
          />
        ))}
    </>
  );
}

/** One sandbox: its line (its controls on its name's line, at its right end), and opened, its Sandbox tab's whole under it. */
function EngineLine({ id, b, name, open, onOpen }: { id: string; b: SandboxBrief; name: string; open: boolean; onOpen: () => void }) {
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
      .act(id, what)
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
      <li className="ms-node df-vm" data-stand={STAND[now]} data-depth={2} data-open={open || undefined}>
        <button type="button" className="df-vm-main" aria-expanded={open} aria-label={`${open ? "Hide" : "Show"} ${name}'s ${host ? "host" : "microVM"}`} onClick={onOpen}>
          <span className="df-chev">
            <ChevronRight size={12} aria-hidden="true" className={open ? "rotate-90" : undefined} />
          </span>
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
        </button>
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
      {open && (
        <li className="ms-node df-vm-body" data-depth={3}>
          <Loadable label={`Loading ${name}'s sandbox`}>
            <SandboxBlock agentId={id} />
          </Loadable>
        </li>
      )}
    </>
  );
}

/** Its agent chat's Sandbox tab, its line above holding its state and controls (loaded as that tab's code is). */
function SandboxBlock({ agentId }: { agentId: string }) {
  const { SandboxTab } = useRes(inspectorTabsRes).data;
  return <SandboxTab agentId={agentId} store={sessionStore(agentId)} inRow />;
}
