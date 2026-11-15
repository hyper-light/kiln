/**
 * Agents you manage (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P5.2, P7.2), inside the dropdown
 * that opens under an agent's row (Mission control's ⫷, the Agents tab, chat), each kind of thing a
 * small labelled group in a fixed order, one line until opened:
 *
 *   SUBAGENTS   the agent's own tree (mission.tsx, unchanged)
 *   DEFINITION  its version and values, edited in place; where it runs and how fast it starts
 *   SESSIONS    how many, who's at work; each session its sandbox, CPU and memory, its controls
 *   EXPERIMENT  the split between deployments, each variant a swatch and a name, Promote
 *
 * A variant is never a letter (kiln's research grades are): it's its prism swatch, the colour of its
 * slice of the split, and a name, by default what sets it apart from the others ("4 vCPU · 8 GiB").
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { ArrowUpRight, Check, ChevronRight, Lock, MoreHorizontal, Pause, Pencil, Play, Repeat2, RotateCw, Square, X } from "lucide-react";
import { useAgentList } from "./use-agents";
import "./mission.css";
import { harnessApi } from "@/api/harness";
import { errorMessage } from "@/lib/hooks";
import { agentdefsRes, type AgentDef, type Experiment, type Variant } from "@/api/agentdefs";
import { rebalance, variantName, variantTone } from "./variants";
import { sandboxesRes, type SandboxBrief } from "@/api/microvms";
import type { AgentInfo } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { labelOf, useRes } from "@/lib/data";
import { useNavigate } from "@/lib/router";
import { EngineGlyph, START, StartMark, WarmDots, msText } from "@/views/sandbox-marks";
import { chatStand, type ChatStand } from "./chat-state";
import { agentLabel, modelLabel } from "./derive";
import { LimitMeter } from "./limits";
import { Shimmer } from "./signals";
import "./definition.css";

/* ------------------------------------------------ variants: a swatch and a name */

/** A variant's swatch: the colour of its slice of the split, a short bar as that slice is. */
export function Swatch({ tone, title, current }: { tone: string; title?: string; current?: boolean }) {
  return (
    <i
      className="df-swatch"
      style={{ background: tone }}
      title={title}
      data-current={current || undefined}
      aria-hidden={title ? undefined : true}
      role={title ? "img" : undefined}
      aria-label={title}
    />
  );
}

/** The variant a session started in, as its swatch and name; none outside an experiment. */
export function sessionVariant(d: AgentDef | undefined, id: string): { tone: string; name: string } | null {
  const s = d?.sessions.find((x) => x.id === id);
  const vs = d?.experiment?.variants ?? [];
  const i = s?.variant ? vs.findIndex((v) => v.id === s.variant) : -1;
  return i < 0 ? null : { tone: variantTone(i, vs.length), name: variantName(vs, i) };
}

/** The definition of yours a session started from, and the version it started on. */
export function ownDefOf(defs: readonly AgentDef[], id: string): { d: AgentDef; version: number } | null {
  for (const d of defs) {
    if (!d.own) continue;
    const s = d.sessions.find((x) => x.id === id);
    if (s) return { d, version: s.version };
  }
  return null;
}

/* ------------------------------------------------ where it runs, how fast it starts */

/** Where a definition's sessions run and how fast they start: its engine, warm hosts of the target, start p50 by kind. */
export function RunsMarks({ d }: { d: AgentDef }) {
  const r = d.runs;
  return (
    <span className="df-runs">
      <EngineGlyph engine={r.engine} />
      {r.target > 0 && <WarmDots n={r.warm} of={r.target} />}
      {(["warm", "template", "cold"] as const).map((k) => {
        const ms = r.p50_ms[k];
        if (ms === undefined) return null;
        const S = START[k];
        const first = (["warm", "template", "cold"] as const).find((x) => r.p50_ms[x] !== undefined) === k;
        return (
          <span key={k} className="sm-start" data-kind={k} data-first={first || undefined} title={`${S.word}: p50 ${msText(ms)}`}>
            <S.icon size={12} aria-hidden="true" />
            {msText(ms)}
          </span>
        );
      })}
    </span>
  );
}

/** A session's sandbox in short: its engine, its runner, how it started. */
export function SandboxLine({ id, target }: { id: string; target: string }) {
  return (
    <React.Suspense fallback={<span>{labelOf(target.replace(/^agent:/, ""))}</span>}>
      <SandboxLineRead id={id} target={target} />
    </React.Suspense>
  );
}
function SandboxLineRead({ id, target }: { id: string; target: string }) {
  const b: SandboxBrief | undefined = useRes(sandboxesRes).data[id];
  if (!b) return <span>{labelOf(target.replace(/^agent:/, ""))}</span>;
  return (
    <span className="df-sandbox">
      <EngineGlyph engine={b.engine} />
      <span className="df-runner">{labelOf(b.runner)}</span>
      {b.start && <StartMark kind={b.start.kind} ms={b.start.ms} />}
    </span>
  );
}

/* ------------------------------------------------ the groups */

/** A session's stand as the tree's nodes draw it (mission.css .ms-node[data-stand]). */
const NODE: Record<ChatStand, string> = { needs: "needs", crashed: "failed", paused: "paused", working: "working", finished: "done", ready: "done", offline: "offline", ended: "stopped" };
const NODE_WORD: Record<string, string> = { needs: "Needs you", failed: "Stopped", paused: "Paused", working: "Working", done: "Done", offline: "Offline", stopped: "Ended" };

type Group = "definition" | "sessions" | "experiment";

/**
 * A definition's groups under its row: DEFINITION, SESSIONS and EXPERIMENT in that order, each one
 * line until opened, in the tree's own columns (its rows at the tree's indentation).
 */
/** `inRow`: under the definition's own row (the Agents tab, chat), which says where it runs and has Run already. */
export function DefGroups({ d, list, first, inRow }: { d: AgentDef; list: readonly AgentInfo[]; first?: Group; inRow?: boolean }) {
  const [open, setOpen] = React.useState<ReadonlySet<Group>>(() => new Set(first ? [first] : []));
  const [setup, setSetup] = React.useState(false);
  const toggle = (g: Group) =>
    setOpen((s) => {
      const n = new Set(s);
      if (!n.delete(g)) n.add(g);
      return n;
    });
  const sessions = d.sessions.map((s) => ({ ...s, a: list.find((x) => x.id === s.id) })).filter((s): s is typeof s & { a: AgentInfo } => s.a !== undefined);
  return (
    <li className="ms-tree df-groups">
      <ul className="ms-tree-rows" aria-label={`${d.name}: definition, sessions, experiment`}>
        <DefinitionGroup
          d={d}
          inRow={inRow}
          open={open.has("definition")}
          onOpen={() => toggle("definition")}
          onSetup={() => {
            setSetup(true);
            setOpen((s) => new Set([...s, "experiment"]));
          }}
        />
        <SessionsGroup d={d} sessions={sessions} open={open.has("sessions")} onOpen={() => toggle("sessions")} />
        {(d.experiment || setup) && <ExperimentGroup d={d} open={open.has("experiment")} onOpen={() => toggle("experiment")} setup={setup} onSetupDone={() => setSetup(false)} />}
      </ul>
    </li>
  );
}

/** A group's line: its label in Mission's mono, its one-line summary, what belongs at its end. */
export function GroupHead({
  label,
  open,
  onOpen,
  summary,
  meta,
  actions,
  stand = "done",
}: {
  label: string;
  open: boolean;
  onOpen: () => void;
  summary: React.ReactNode;
  meta?: React.ReactNode;
  actions?: React.ReactNode;
  stand?: string;
}) {
  return (
    <li className="ms-node df-group" data-stand={stand} data-depth={1} data-open={open || undefined}>
      <span className="ms-node-path df-chev">
        <ChevronRight size={12} aria-hidden="true" className={open ? "rotate-90" : undefined} />
      </span>
      <button type="button" className="ms-node-main df-group-main" aria-expanded={open} onClick={onOpen}>
        <span className="df-group-line">
          <span className="ms-sep-h df-group-k">{label}</span>
          <span className="df-group-sum">{summary}</span>
        </span>
      </button>
      <span className="ms-node-meta">{meta}</span>
      {/* Its controls stay together at its end, a phone's too (where a row's actions are laid out apart). */}
      <span className="ms-node-actions">{actions && <span className="ms-tail">{actions}</span>}</span>
    </li>
  );
}

function DefinitionGroup({ d, open, onOpen, onSetup, inRow }: { d: AgentDef; open: boolean; onOpen: () => void; onSetup: () => void; inRow?: boolean }) {
  return (
    <>
      <GroupHead
        label="Definition"
        open={open}
        onOpen={onOpen}
        summary={
          <>
            <span className="df-strong">{d.name}</span>
            <span>v{d.version}</span>
            {d.conflict && <span className="ms-flag" data-flag="stale" title={`${d.conflict.field}: ${d.conflict.here} here, ${d.conflict.there} in ${d.conflict.from}`} />}
            {!inRow && <RunsMarks d={d} />}
            {inRow && d.model && <span>{modelLabel(d.model)}</span>}
          </>
        }
        meta={!inRow && d.model ? <span>{modelLabel(d.model)}</span> : undefined}
        actions={
          inRow ? undefined : (
            <>
              <Button size="xs" variant="secondary" onClick={() => toast({ title: `Starting ${d.name}`, tone: "running" })}>
                Run
              </Button>
              <DefMenu d={d} onSetup={onSetup} />
            </>
          )
        }
      />
      {open && (
        <li className="ms-node ms-defnode" data-stand="done" data-depth={2}>
          <span className="ms-node-path" />
          <span className="ms-node-main">
            <span className="df-narrow">
              <RunsMarks d={d} />
            </span>
            <DefFields d={d} />
          </span>
          <span className="ms-node-meta">
            <span>
              {d.agentfile} · {d.harness}
            </span>
            <span>
              {d.tools} kiln · {d.mcp} MCP · {d.skills} skills
            </span>
          </span>
          <span className="ms-node-actions" />
        </li>
      )}
    </>
  );
}

/** Its sessions: how many, and who's at work, a dot each in its stand's colour; opened, a plain row each. */
function SessionsGroup({ d, sessions, open, onOpen }: { d: AgentDef; sessions: { id: string; version: number; a: AgentInfo }[]; open: boolean; onOpen: () => void }) {
  const stands = sessions.map((s) => NODE[chatStand(s.a)]);
  const atWork = stands.filter((s) => s === "working" || s === "needs").length;
  return (
    <>
      <GroupHead
        label="Sessions"
        open={open}
        onOpen={onOpen}
        stand={atWork ? "working" : "done"}
        summary={
          sessions.length ? (
            <>
              <span className="df-stands" aria-hidden="true">
                {stands.map((s, i) => (
                  <i key={i} data-stand={s} />
                ))}
              </span>
              {sessions.length} · {atWork} at work
            </>
          ) : (
            "none yet"
          )
        }
      />
      {open &&
        sessions.map((s) => {
          const v = sessionVariant(d, s.id);
          return <SessionNode key={s.id} a={s.a} version={s.version} current={d.version} variant={v} />;
        })}
    </>
  );
}

/** A session started from a definition, a plain row at the tree's indentation: its stand, version and variant; its sandbox; CPU and memory; its controls. */
function SessionNode({ a, version, current, variant }: { a: AgentInfo; version: number; current: number; variant: { tone: string; name: string } | null }) {
  const navigate = useNavigate();
  const stand = NODE[chatStand(a)];
  const to = `/agents/${a.id}`;
  return (
    <li className="ms-node df-session" data-stand={stand} data-depth={2}>
      <span className="ms-node-path" />
      <button type="button" className="ms-node-main" onClick={() => navigate(to)}>
        <span className="ms-node-name">{agentLabel(a)}</span>
        <span className="ms-node-line">
          <Shimmer on={stand === "working"} flow={a.id} className="ms-word">
            <i aria-hidden="true" />
            {NODE_WORD[stand]}
          </Shimmer>
          <span
            className="ms-node-text df-version"
            title={version === current ? "The current version" : `Started on v${version}; new sessions get v${current}`}
            data-behind={version < current || undefined}
          >
            v{version}
          </span>
          {variant && (
            <span className="df-variant">
              <Swatch tone={variant.tone} />
              <span className="ms-node-text">{variant.name}</span>
            </span>
          )}
        </span>
        {/* A phone has no meta column: its sandbox goes under its words. */}
        <span className="df-narrow ms-node-text">
          <SandboxLine id={a.id} target={a.target} />
        </span>
      </button>
      <span className="ms-node-meta">
        <SandboxLine id={a.id} target={a.target} />
        <SessionBars id={a.id} />
      </span>
      <span className="ms-node-actions">
        <span className="ms-decide">
          <SessionControls a={a} stand={stand} />
        </span>
        <span className="ms-tail">
          <button type="button" className="ms-go" onClick={() => navigate(to)} aria-label={`Open ${agentLabel(a)}`}>
            <ArrowUpRight size={16} />
          </button>
        </span>
      </span>
    </li>
  );
}

/** A session's CPU and memory now, as compact bars (a list row's: no plots). */
function SessionBars({ id }: { id: string }) {
  return (
    <React.Suspense fallback={null}>
      <SessionBarsRead id={id} />
    </React.Suspense>
  );
}
function SessionBarsRead({ id }: { id: string }) {
  const b = useRes(sandboxesRes).data[id];
  if (!b || b.state === "ended") return null;
  return (
    <span className="df-bars">
      <LimitMeter compact live limit={{ id: "cpu", label: "CPU", used_percent: b.cpu_pct }} />
      <LimitMeter compact live limit={{ id: "memory", label: "Mem", used_percent: (b.memory.used / b.memory.total) * 100 }} />
    </span>
  );
}

/** Its experiment: one line of its split's swatches until opened, then each variant, Promote on all but the current. */
function ExperimentGroup({ d, open, onOpen, setup, onSetupDone }: { d: AgentDef; open: boolean; onOpen: () => void; setup: boolean; onSetupDone: () => void }) {
  const [e, setE] = React.useState<Experiment | undefined>(d.experiment);
  const [staged, setStaged] = React.useState<number[] | null>(null);
  const [asking, setAsking] = React.useState<string | null>(null);
  const [names, setNames] = React.useState<Record<string, string>>({});
  if (!e)
    return setup ? (
      <SetupNodes
        d={d}
        onDone={onSetupDone}
        onStart={(next) => {
          setE(next);
          onSetupDone();
        }}
      />
    ) : null;
  const vs = e.variants.map((v) => ({ ...v, name: names[v.id] ?? v.name }));
  const weights = staged ?? vs.map((v) => v.weight);
  const live = e.state === "live";
  const apply = () => {
    const before = e;
    const next = { ...e, version: e.version + 1, variants: e.variants.map((v, i) => ({ ...v, weight: weights[i] ?? v.weight })) };
    setE(next);
    setStaged(null);
    toast({ title: `Split v${next.version}: ${next.variants.map((v) => v.weight).join("/")}`, tone: "info", action: { label: "Undo", run: () => setE(before) } });
  };
  const pause = () => {
    const before = e;
    setE({ ...e, state: live ? "paused" : "live" });
    if (live) toast({ title: `Paused ${e.id}`, tone: "info", action: { label: "Undo", run: () => setE(before) } });
  };
  const promote = (id: string) => {
    const before = e;
    const name = variantName(
      vs,
      vs.findIndex((v) => v.id === id),
    );
    setE({ ...e, state: "promoted", variants: e.variants.map((v) => ({ ...v, current: v.id === id })) });
    setAsking(null);
    toast({ title: `Promoted ${name}`, tone: "succeeded", action: { label: "Undo", run: () => setE(before) } });
  };
  const split = (
    <span className="ms-split" data-staged={staged ? "" : undefined} aria-label={`Split ${vs.map((_, i) => `${variantName(vs, i)} ${weights[i]}%`).join(", ")}${staged ? ", not yet applied" : ""}`}>
      {vs.map((v, i) => (
        <i key={v.id} style={{ flexGrow: weights[i], background: variantTone(i, vs.length) }} title={`${variantName(vs, i)} · ${weights[i]}%`} />
      ))}
    </span>
  );
  return (
    <>
      <GroupHead
        label="Experiment"
        open={open}
        onOpen={onOpen}
        stand={live ? "working" : e.state === "paused" ? "paused" : "done"}
        summary={
          <>
            {split}
            <span className="df-weights">{weights.join("/")}</span>
            <span>
              {e.sessions} · {live ? "splitting" : e.state}
            </span>
          </>
        }
        actions={
          <>
            {staged && (
              <>
                <Button size="xs" variant="secondary" onClick={apply}>
                  Apply
                </Button>
                <Button size="xs" variant="ghost" onClick={() => setStaged(null)}>
                  Discard
                </Button>
              </>
            )}
            {e.state !== "promoted" && (
              <button type="button" className="ms-go" title={live ? "Pause the split" : "Resume the split"} aria-label={live ? "Pause the split" : "Resume the split"} onClick={pause}>
                {live ? <Pause size={15} /> : <Play size={15} />}
              </button>
            )}
          </>
        }
      />
      {open &&
        vs.map((v, i) => (
          <VariantNode
            key={v.id}
            all={vs}
            i={i}
            weight={weights[i] ?? v.weight}
            fixed={e.state === "promoted"}
            onWeight={(w) => setStaged(rebalance(weights, i, w))}
            onName={(name) => setNames({ ...names, [v.id]: name })}
            asking={asking === v.id}
            onAsk={() => setAsking(asking === v.id ? null : v.id)}
            onPromote={() => promote(v.id)}
          />
        ))}
    </>
  );
}

/** A variant: its swatch and name (renamed in place), its measures, its weight, Promote. */
function VariantNode({
  all,
  i,
  weight,
  fixed,
  onWeight,
  onName,
  asking,
  onAsk,
  onPromote,
}: {
  all: Variant[];
  i: number;
  weight: number;
  fixed: boolean;
  onWeight: (w: number) => void;
  onName: (n: string) => void;
  asking: boolean;
  onAsk: () => void;
  onPromote: () => void;
}) {
  const v = all[i]!;
  const [naming, setNaming] = React.useState(false);
  const tone = variantTone(i, all.length);
  const name = variantName(all, i);
  const win = (k: keyof Variant["measures"]) => all.every((o) => o === v || v.measures[k][2] < o.measures[k][0]);
  return (
    <li className="ms-node ms-variant" data-stand="done" data-depth={2}>
      <span className="ms-node-path df-swatch-cell">
        <Swatch tone={tone} current={v.current} title={v.current ? `${name}: what new sessions start on` : name} />
      </span>
      <span className="ms-node-main">
        {naming ? (
          <span className="ms-edit">
            <input
              className="ms-edit-in"
              autoFocus
              defaultValue={v.name ?? ""}
              placeholder={variantName(
                all.map((x, j) => (j === i ? { ...x, name: null } : x)),
                i,
              )}
              aria-label="Variant name"
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  onName(e.currentTarget.value.trim());
                  setNaming(false);
                }
                if (e.key === "Escape") setNaming(false);
              }}
            />
            <button
              type="button"
              className="ms-go"
              aria-label="Save the name"
              onClick={(e) => {
                onName((e.currentTarget.previousElementSibling as HTMLInputElement).value.trim());
                setNaming(false);
              }}
            >
              <Check size={15} />
            </button>
          </span>
        ) : (
          <button type="button" className="ms-node-name df-vname" title="Rename it" onClick={() => setNaming(true)}>
            {name}
            <Pencil size={11} aria-hidden="true" />
          </button>
        )}
        <span className="ms-node-line">
          <span className="ms-node-text">
            <Measure label="warm" iv={v.measures.warm_ms} f={(x) => `${Math.round(x)}ms`} win={win("warm_ms")} />
            <Measure label="peak" iv={v.measures.peak_gib} f={(x) => `${x.toFixed(2)}G`} win={win("peak_gib")} />
            <Measure label="failed" iv={v.measures.failed_pct} f={(x) => `${Math.round(x)}%`} win={win("failed_pct")} />
          </span>
        </span>
      </span>
      <span className="ms-node-meta">
        <input
          type="range"
          className="ms-weight-in"
          style={{ accentColor: tone }}
          min={0}
          max={100}
          value={weight}
          disabled={fixed}
          aria-label={`${name}'s weight`}
          onChange={(e) => onWeight(Number(e.target.value))}
        />
        <span>
          {weight}% · {v.sessions} sessions
        </span>
      </span>
      <span className="ms-node-actions">
        <span className="ms-decide">
          {!v.current && !fixed && !asking && (
            <Button size="xs" variant="secondary" onClick={onAsk}>
              Promote
            </Button>
          )}
          {asking && (
            <span className="ms-confirm" role="group" aria-label={`Promote ${name}?`}>
              <span>Promote?</span>
              <button type="button" className="ms-go" aria-label={`Promote ${name}`} onClick={onPromote}>
                <Check size={15} />
              </button>
              <button type="button" className="ms-go" aria-label="Keep the split" onClick={onAsk}>
                <X size={15} />
              </button>
            </span>
          )}
        </span>
      </span>
    </li>
  );
}

type Draft = { agentfile: string; engine: "microvm" | "host"; cpus: number; memory: number; weight: number };

/** A new experiment, set up in place: variants (as many as you add), each a deployment and a weight; Start splits new sessions. */
function SetupNodes({ d, onDone, onStart }: { d: AgentDef; onDone: () => void; onStart: (e: Experiment) => void }) {
  const G = 2 ** 30;
  const [vs, setVs] = React.useState<Draft[]>([
    { agentfile: d.agentfile, engine: "microvm", cpus: 2, memory: 4 * G, weight: 80 },
    { agentfile: d.agentfile, engine: "microvm", cpus: 4, memory: 8 * G, weight: 20 },
  ]);
  const set = (i: number, patch: Partial<Draft>) => setVs(vs.map((v, j) => (j === i ? { ...v, ...patch } : v)));
  const start = () => {
    onStart({
      id: `${d.name}-split`,
      state: "live",
      version: 1,
      sessions: 0,
      variants: vs.map((v, i) => ({ id: `new-${i}`, name: null, ...v, sessions: 0, current: i === 0, measures: { warm_ms: [0, 0, 0], peak_gib: [0, 0, 0], failed_pct: [0, 0, 0] } })),
    });
    toast({ title: `Splitting ${d.name}'s new sessions`, tone: "running" });
  };
  return (
    <>
      <GroupHead
        label="Experiment"
        open
        onOpen={onDone}
        stand="paused"
        summary={
          <>
            <span className="ms-split" data-staged="">
              {vs.map((v, i) => (
                <i key={i} style={{ flexGrow: v.weight, background: variantTone(i, vs.length) }} />
              ))}
            </span>
            <span className="df-weights">{vs.map((v) => v.weight).join("/")}</span>
            <span>new</span>
          </>
        }
        actions={
          <>
            <Button size="xs" variant="secondary" onClick={() => setVs([...vs, { ...vs[vs.length - 1]!, weight: 0 }])}>
              Add variant
            </Button>
            <Button size="xs" onClick={start}>
              Start
            </Button>
            <button type="button" className="ms-go" aria-label="Cancel" onClick={onDone}>
              <X size={15} />
            </button>
          </>
        }
      />
      {vs.map((v, i) => (
        <li key={i} className="ms-node ms-variant" data-stand="done" data-depth={2}>
          <span className="ms-node-path df-swatch-cell">
            <Swatch tone={variantTone(i, vs.length)} title={variantName(vs, i)} />
          </span>
          <span className="ms-node-main ms-setup">
            <select className="ms-edit-in" value={v.agentfile} aria-label="Agentfile" onChange={(e) => set(i, { agentfile: e.target.value })}>
              {["rust-dev", "docs-site", `harness-${d.harness}`].map((a) => (
                <option key={a}>{a}</option>
              ))}
            </select>
            <Segmented
              size="xs"
              label="Engine"
              value={v.engine}
              onChange={(x) => set(i, { engine: x })}
              options={[
                { value: "microvm", label: "microVM" },
                { value: "host", label: "host" },
              ]}
            />
            {v.engine === "microvm" && (
              <>
                <select className="ms-edit-in ms-narrow" value={v.cpus} aria-label="vCPUs" onChange={(e) => set(i, { cpus: Number(e.target.value) })}>
                  {[1, 2, 4, 8].map((n) => (
                    <option key={n} value={n}>
                      {n} vCPU
                    </option>
                  ))}
                </select>
                <select className="ms-edit-in ms-narrow" value={v.memory / G} aria-label="Memory" onChange={(e) => set(i, { memory: Number(e.target.value) * G })}>
                  {[2, 4, 8, 16].map((n) => (
                    <option key={n} value={n}>
                      {n} GiB
                    </option>
                  ))}
                </select>
              </>
            )}
          </span>
          <span className="ms-node-meta">
            <input
              type="range"
              className="ms-weight-in"
              style={{ accentColor: variantTone(i, vs.length) }}
              min={0}
              max={100}
              value={v.weight}
              aria-label={`${variantName(vs, i)}'s weight`}
              onChange={(e) => {
                const ws = rebalance(
                  vs.map((x) => x.weight),
                  i,
                  Number(e.target.value),
                );
                setVs(vs.map((x, j) => ({ ...x, weight: ws[j] ?? x.weight })));
              }}
            />
            <span>{v.weight}%</span>
          </span>
          <span className="ms-node-actions">
            <span className="ms-tail">
              {i > 1 && (
                <button type="button" className="ms-go" aria-label="Remove this variant" onClick={() => setVs(vs.filter((_, j) => j !== i))}>
                  <X size={15} />
                </button>
              )}
            </span>
          </span>
        </li>
      ))}
    </>
  );
}

/** A measure as its median, with a check where its 95% interval clears every other variant's. */
function Measure({ label, iv, f, win }: { label: string; iv: [number, number, number]; f: (x: number) => string; win: boolean }) {
  return (
    <span className="ms-measure" data-win={win || undefined} title={`${label}: median ${f(iv[1])}, 95% ${f(iv[0])}–${f(iv[2])}`}>
      {label} {f(iv[1])}
      {win ? " ✓" : ""}
    </span>
  );
}

/** A definition's rarer acts, behind one ⋯, as a Mission row's are (actions.tsx): its history, an experiment, a copy, delete. */
function DefMenu({ d, onSetup }: { d: AgentDef; onSetup: () => void }) {
  const items: { label: string; onSelect: () => void; danger?: boolean }[] = d.own
    ? [
        { label: `History (v${d.version})`, onSelect: () => toast({ title: `${d.name}: v1 to v${d.version}`, tone: "info" }) },
        ...(d.experiment ? [] : [{ label: "New experiment", onSelect: onSetup }]),
        { label: "Duplicate", onSelect: () => toast({ title: `Copied ${d.name}`, tone: "info" }) },
        { label: "Delete…", onSelect: () => toast({ title: `Delete ${d.name}? Confirm on its row`, tone: "info" }), danger: true },
      ]
    : [{ label: "Copy as yours", onSelect: () => toast({ title: `${d.name} copied as yours`, tone: "info" }) }];
  return (
    <DropdownMenu.Root modal={false}>
      <DropdownMenu.Trigger asChild>
        <button type="button" className="hx-icon" aria-label={`Actions for ${d.name}`}>
          <MoreHorizontal size={16} />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content className="popover w-56" sideOffset={6} align="end" collisionPadding={12}>
          {items.map((i) => (
            <DropdownMenu.Item key={i.label} className="menu-item" onSelect={i.onSelect} style={i.danger ? { color: "var(--color-fail)" } : undefined}>
              <span className="flex-1">{i.label}</span>
            </DropdownMenu.Item>
          ))}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

/** A session's own controls on its row: pause or resume (undone from its toast), and stop, asked in place. */
function SessionControls({ a, stand }: { a: AgentInfo; stand: string }) {
  const [asking, setAsking] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const paused = stand === "paused";
  const live = stand !== "stopped" && stand !== "failed";
  const pause = (on: boolean) => {
    setBusy(true);
    harnessApi
      .command(a.id, { type: "pause", on })
      .then(() =>
        toast({
          title: on ? `Paused ${agentLabel(a)}` : `Resumed ${agentLabel(a)}`,
          tone: "info",
          action: on ? { label: "Undo", run: () => void harnessApi.command(a.id, { type: "pause", on: false }) } : undefined,
        }),
      )
      .catch((e: unknown) => toast.error(on ? "Couldn't pause" : "Couldn't resume", errorMessage(e)))
      .finally(() => setBusy(false));
  };
  if (!live) return null;
  if (asking)
    return (
      <span className="ms-confirm" role="group" aria-label={`Stop ${agentLabel(a)}?`}>
        <span>Stop?</span>
        <button
          type="button"
          className="ms-go"
          aria-label="Stop it"
          onClick={() => {
            setAsking(false);
            toast({ title: `Stopping ${agentLabel(a)}`, tone: "info" });
          }}
        >
          <Check size={15} />
        </button>
        <button type="button" className="ms-go" aria-label="Keep it" onClick={() => setAsking(false)}>
          <X size={15} />
        </button>
      </span>
    );
  return (
    <span className="ms-ctl">
      <button type="button" className="ms-go" disabled={busy} title={paused ? "Resume (P)" : "Pause (P)"} aria-label={`${paused ? "Resume" : "Pause"} ${agentLabel(a)}`} onClick={() => pause(!paused)}>
        {paused ? <Play size={15} /> : <Pause size={15} />}
      </button>
      <button type="button" className="ms-go" title="Stop (S)" aria-label={`Stop ${agentLabel(a)}`} onClick={() => setAsking(true)}>
        <Square size={14} />
      </button>
    </span>
  );
}

/** A definition's values: each a value until pressed, then its control, in place; a save is a new version, running sessions keep theirs. */
export function DefFields({ d }: { d: AgentDef }) {
  const [editing, setEditing] = React.useState<string | null>(null);
  const [vals, setVals] = React.useState({ model: d.model, limit: d.limit, instructions: d.instructions });
  const [version, setVersion] = React.useState(d.version);
  const save = (k: keyof typeof vals, v: string) => {
    setVals({ ...vals, [k]: v });
    setEditing(null);
    setVersion((n) => n + 1);
    toast({ title: `Saved v${version + 1}`, tone: "info" });
  };
  const field = (k: keyof typeof vals, label: string, shown: string) =>
    editing === k ? (
      <span className="ms-edit">
        <span className="ms-edit-k">{label}</span>
        <input
          className="ms-edit-in"
          autoFocus
          defaultValue={vals[k]}
          aria-label={label}
          onKeyDown={(e) => {
            if (e.key === "Enter") save(k, e.currentTarget.value);
            if (e.key === "Escape") setEditing(null);
          }}
        />
        <button type="button" className="ms-go" aria-label={`Save ${label}`} onClick={(e) => save(k, (e.currentTarget.previousElementSibling as HTMLInputElement).value)}>
          <Check size={15} />
        </button>
      </span>
    ) : (
      <button type="button" className="ms-field" title={d.own ? `Change ${label}` : "kiln's: copy it to change it"} disabled={!d.own} onClick={() => setEditing(k)}>
        <span className="ms-edit-k">{label}</span>
        <span className="ms-field-v">{shown || "none"}</span>
        <Pencil size={12} aria-hidden="true" />
      </button>
    );
  return (
    <>
      <span className="ms-fields">
        {field("model", "model", vals.model ? modelLabel(vals.model) : "default")}
        {field("limit", "limit", vals.limit)}
        <span className="ms-field">
          <span className="ms-edit-k">version</span>
          <span className="ms-field-v">v{version}</span>
        </span>
      </span>
      <span className="ms-fields">{field("instructions", "instructions", vals.instructions)}</span>
    </>
  );
}

/* ------------------------------------------------ a definition as a row of its own (the Agents tab, chat) */

/**
 * A definition as a row (the Agents tab's list, chat's card), in the dropdown's own columns: its
 * name, its sessions' stand and how many are at work; where it runs and how fast it starts; Run and
 * its rarer acts. Its chevron opens its groups under it.
 */
export function DefRow({ d, list, open, onOpen }: { d: AgentDef; list: readonly AgentInfo[]; open: boolean; onOpen: () => void }) {
  const sessions = d.sessions.map((s) => list.find((x) => x.id === s.id)).filter((a): a is AgentInfo => a !== undefined);
  const stands = sessions.map((a) => NODE[chatStand(a)]);
  const ORDER = ["needs", "failed", "working", "paused", "done", "offline", "stopped"];
  const stand = ORDER.find((o) => stands.includes(o)) ?? "done";
  const atWork = stands.filter((s) => s === "working" || s === "needs").length;
  return (
    <>
      <li className="ms-node df-defrow" data-stand={stand} data-depth={0} data-open={open || undefined} data-def={d.id}>
        <span className="ms-node-path df-chev">
          <ChevronRight size={13} aria-hidden="true" className={open ? "rotate-90" : undefined} />
        </span>
        <button type="button" className="ms-node-main df-group-main" aria-expanded={open} aria-label={`${open ? "Close" : "Open"} ${d.name}`} onClick={onOpen}>
          <span className="df-group-line">
            <span className="df-strong df-defname">{d.name}</span>
            {!d.own && <Lock size={12} className="ms-flag-pin" role="img" aria-label="kiln's: copy it to change it" />}
            {(d.replaces || d.replaced_by) && <Repeat2 size={12} className="ms-flag-pin" role="img" aria-label={d.replaces ? `Stands in for kiln's ${d.replaces}` : `You use ${d.replaced_by}`} />}
            {sessions.length > 0 ? (
              <span className="df-group-sum">
                <span className="df-stands" aria-hidden="true">
                  {stands.map((st, i) => (
                    <i key={i} data-stand={st} />
                  ))}
                </span>
                {sessions.length} · {atWork} at work
              </span>
            ) : (
              <span className="df-group-sum">v{d.version}</span>
            )}
          </span>
        </button>
        <span className="ms-node-meta">
          <RunsMarks d={d} />
        </span>
        <span className="ms-node-actions">
          <span className="ms-tail">
            <Button size="xs" variant="secondary" onClick={() => toast({ title: `Starting ${d.name}`, tone: "running" })}>
              Run
            </Button>
            <DefMenu
              d={d}
              onSetup={() => {
                if (!open) onOpen();
              }}
            />
          </span>
        </span>
      </li>
      {open && <DefGroups d={d} list={list} first="definition" inRow />}
    </>
  );
}

/** A kiln MCP call about an agent definition (agentdefs_get, _update, _run …): the definition's id it names, so chat shows its rows. */
export function defCall(c: { server?: string; tool_short?: string; input?: unknown; status?: string }): string | null {
  if (c.server !== "kiln" || !c.tool_short?.startsWith("agentdefs_") || c.status === "error") return null;
  const id = (c.input as { id?: unknown } | null)?.id;
  return typeof id === "string" ? id : null;
}

/**
 * An agent definition in chat: the same row and groups, with the same controls, as everywhere. Live
 * while it's on screen; further back, as it stood when the agent asked ("as of"), with ↻ to go live.
 */
export function DefinitionCard({ id, at }: { id: string; at?: number }) {
  const defs = useRes(agentdefsRes).data;
  const list = useAgentList();
  const [live, setLive] = React.useState(false);
  const [open, setOpen] = React.useState(false);
  const d = defs.find((x) => x.id === id);
  if (!d) return null;
  const old = at !== undefined && Date.now() - at > 60_000 && !live;
  return (
    <div className="ms-card" data-asof={old || undefined}>
      <div className="ms-card-head">
        <span className="ms-edit-k">agent</span>
        {old ? (
          <>
            <span className="ms-card-asof">as of {new Date(at!).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</span>
            <button type="button" className="ms-go" title="Go live" aria-label="Show it as it is now" onClick={() => setLive(true)}>
              <RotateCw size={14} />
            </button>
          </>
        ) : (
          <span className="ms-card-asof" data-live>
            live
          </span>
        )}
      </div>
      <ul className="ms-list" data-list="defs-chat" inert={old || undefined}>
        <DefRow d={d} list={list} open={open} onOpen={() => setOpen(!open)} />
      </ul>
    </div>
  );
}
