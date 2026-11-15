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
import { agentdefRes, agentdefsApi, agentdefsRes, agentdefVersionsRes, limitSeconds, limitWords, type AgentDef, type AgentDefFull, type Builtin, type Experiment, type Variant } from "@/api/agentdefs";
import { Ago } from "./parts";
import { agentfilesRes } from "@/api/agentfiles";
import { ApiError } from "@/api/client";
import { Loadable } from "@/components/ui/loading";
import { invalidate } from "@/lib/resource";
import { rebalance, variantName, variantTone } from "./variants";
import { engineOf, sandboxesRes, type SandboxBrief } from "@/api/microvms";
import type { AgentInfo } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { agentsRes, labelOf, useRes } from "@/lib/data";
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
  const machine = target.replace(/^agent:/, "");
  // A session the sandboxes don't list yet (just started): where it runs, from its machine's facts.
  const engine = engineOf(useRes(agentsRes).data.find((r) => r.name === machine)?.facts)?.engine;
  if (!b)
    return (
      <span className="df-sandbox">
        {engine && <EngineGlyph engine={engine} />}
        <span className="df-runner">{labelOf(machine)}</span>
      </span>
    );
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
 * line until opened, one open at a time (kiln's principle 1), in the tree's own columns (its rows at
 * the tree's indentation). `inRow`: under the definition's own row (the Agents tab, chat), which says
 * where it runs and has Run already.
 */
export function DefGroupRows({ d, list, first, inRow }: { d: AgentDef; list: readonly AgentInfo[]; first?: Group; inRow?: boolean }) {
  const [open, setOpen] = React.useState<Group | null>(first ?? null);
  const [setup, setSetup] = React.useState(false);
  const toggle = (g: Group) => setOpen((was) => (was === g ? null : g));
  const sessions = d.sessions.map((s) => ({ ...s, a: list.find((x) => x.id === s.id) })).filter((s): s is typeof s & { a: AgentInfo } => s.a !== undefined);
  return (
    <>
      <DefinitionGroup
        d={d}
        inRow={inRow}
        open={open === "definition"}
        onOpen={() => toggle("definition")}
        onSetup={() => {
          setSetup(true);
          setOpen("experiment");
        }}
      />
      <SessionsGroup d={d} sessions={sessions} open={open === "sessions"} onOpen={() => toggle("sessions")} />
      {(d.experiment || setup) && <ExperimentGroup d={d} open={open === "experiment"} onOpen={() => toggle("experiment")} setup={setup} onSetupDone={() => setSetup(false)} />}
    </>
  );
}

/** A definition's groups under its own row (the Agents tab, chat), under a head that stays in sight while its row is out of it. */
export function DefGroups({ d, list, first, inRow, onClose }: { d: AgentDef; list: readonly AgentInfo[]; first?: Group; inRow?: boolean; onClose: () => void }) {
  return (
    <li className="ms-tree df-groups">
      <OpenHead label={d.name} onClose={onClose} />
      <ul className="ms-tree-rows" aria-label={`${d.name}: definition, sessions, experiment`}>
        <DefGroupRows d={d} list={list} first={first} inRow={inRow} />
      </ul>
    </li>
  );
}

let unseen: IntersectionObserver | null = null;
/**
 * A row whose dropdown is open says when it's out of sight (`data-unseen`; under kiln's header, or
 * scrolled past in its panel), so its dropdown's head stands in for it (kiln's principle 1). One
 * observer for every row, as signals.tsx's.
 */
export function watchUnseen(el: HTMLElement | null) {
  if (!el) return;
  const top = getComputedStyle(document.documentElement).getPropertyValue("--header-h").trim() || "0px";
  unseen ??= new IntersectionObserver(
    (entries) => {
      for (const e of entries) e.target.toggleAttribute("data-unseen", !e.isIntersecting);
    },
    { rootMargin: `-${top} 0px 0px 0px` },
  );
  unseen.observe(el);
  return () => {
    unseen?.unobserve(el);
    el.removeAttribute("data-unseen");
  };
}

/**
 * An open dropdown's head: its row's name and a way to close it, sticking at the top while you're in
 * the dropdown and its row is out of sight; nothing at all while its row is in sight. It takes no room
 * (it overlies the dropdown), so nothing moves as it comes and goes.
 */
export function OpenHead({ label, onClose }: { label: string; onClose: () => void }) {
  return (
    <div className="ms-open-head">
      <div>
        <span className="ms-open-name">{label}</span>
        <button type="button" className="ms-go" tabIndex={-1} aria-label={`Close ${label}'s dropdown`} title="Close (Esc)" onClick={onClose}>
          <ChevronRight size={14} className="-rotate-90" aria-hidden="true" />
        </button>
      </div>
    </div>
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
  const [asking, setAsking] = React.useState<DefAskKind | null>(null);
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
            {!inRow && <RunsMarks d={d} />}
            {inRow && d.model && <span>{modelLabel(d.model)}</span>}
          </>
        }
        meta={!inRow && d.model ? <span>{modelLabel(d.model)}</span> : undefined}
        actions={
          inRow ? undefined : (
            <>
              <RunButton d={d} />
              <DefMenu d={d} onSetup={onSetup} onAsk={setAsking} />
            </>
          )
        }
      />
      {asking && <DefAsk d={d} asking={asking} onDone={() => setAsking(null)} />}
      {open && (
        <li className="ms-node ms-defnode" data-stand="done" data-depth={2}>
          <span className="ms-node-path" />
          <span className="ms-node-main">
            <span className="df-runs-full">
              <RunsMarks d={d} />
            </span>
            <Loadable label={`Loading ${d.name}'s values`} fallback={<span className="ms-fields" aria-busy="true" />}>
              <DefValues d={d} />
            </Loadable>
          </span>
          <span className="ms-node-meta">
            <span>
              <AgentfileName ref_={d.agentfile} harness={d.harness} /> · {d.harness}
            </span>
            <Loadable label="Loading what it holds" fallback={null}>
              <Holds id={d.id} />
            </Loadable>
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
    { agentfile: setupAgentfile(d), engine: "microvm", cpus: 2, memory: 4 * G, weight: 80 },
    { agentfile: setupAgentfile(d), engine: "microvm", cpus: 4, memory: 8 * G, weight: 20 },
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

type DefAskKind = "delete" | "versions";

/** Run one: the server starts an agent from it (a microVM, its Agentfile's latest), and its page opens; one that waits says so. */
function RunButton({ d }: { d: AgentDef }) {
  const navigate = useNavigate();
  const [busy, setBusy] = React.useState(false);
  return (
    <Button
      size="xs"
      variant="secondary"
      pending={busy}
      onClick={() => {
        setBusy(true);
        agentdefsApi
          .run(d.id)
          .then((r) => {
            const started = r.agents[0];
            if (started) navigate(`/agents/${started.id}`);
            else toast({ title: `${d.name} v${r.agentdef.version}`, description: r.queued, tone: "info" });
          })
          .catch((e: unknown) => toast.error(`Couldn't run ${d.name}`, errorMessage(e)))
          .finally(() => setBusy(false));
      }}
    >
      Run
    </Button>
  );
}

/** A definition's rarer acts, behind one ⋯, as a Mission row's are (actions.tsx): its versions, an experiment, a copy, delete; kiln's own copied as yours. */
function DefMenu({ d, onSetup, onAsk }: { d: AgentDef; onSetup: () => void; onAsk: (what: DefAskKind) => void }) {
  const copy = () =>
    agentdefsApi.copy(d.id).then(
      (mine) => {
        invalidate("agentdefs/list");
        toast({ title: `${mine.name} is yours now, v1`, tone: "info" });
      },
      (e: unknown) => toast.error(`Couldn't copy ${d.name}`, errorMessage(e)),
    );
  const duplicate = () =>
    agentdefsApi
      .get(d.id)
      .then((full) =>
        agentdefsApi.create({
          name: `${d.name}-copy`,
          harness: full.harness,
          model: full.model,
          instructions: full.instructions,
          tools: full.tools,
          mcp: full.mcp,
          skills: full.skills,
          limits: full.limits,
          agentfile: full.agentfile,
        }),
      )
      .then(
        (mine) => {
          invalidate("agentdefs/list");
          toast({ title: `${mine.name}, v1`, tone: "info" });
        },
        (e: unknown) => toast.error(`Couldn't duplicate ${d.name}`, errorMessage(e)),
      );
  const items: { label: string; onSelect: () => void; danger?: boolean }[] = d.own
    ? [
        { label: `Versions (v${d.version})`, onSelect: () => onAsk("versions") },
        ...(d.experiment ? [] : [{ label: "New experiment", onSelect: onSetup }]),
        { label: "Duplicate", onSelect: () => void duplicate() },
        { label: "Delete…", onSelect: () => onAsk("delete"), danger: true },
      ]
    : [{ label: "Copy as yours", onSelect: () => void copy() }];
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

/** What its ⋯ asked, under its head (no popup): delete it, confirmed in place; or its versions, each restorable as a new one. */
function DefAsk({ d, asking, onDone }: { d: AgentDef; asking: DefAskKind; onDone: () => void }) {
  const [busy, setBusy] = React.useState(false);
  if (asking === "delete")
    return (
      <li className="ms-node df-ask" data-stand="failed" data-depth={2}>
        <span className="ms-node-path" />
        <span className="ms-node-main">
          <span className="ms-confirm" role="group" aria-label={`Delete ${d.name}?`}>
            <span>Delete {d.name} and its versions?</span>
            <button
              type="button"
              className="ms-go"
              disabled={busy}
              aria-label={`Delete ${d.name}`}
              onClick={() => {
                setBusy(true);
                agentdefsApi
                  .remove(d.id)
                  .then(() => {
                    invalidate("agentdefs/list");
                    toast({ title: `Deleted ${d.name}`, tone: "info" });
                  })
                  .catch((e: unknown) => toast.error(`Couldn't delete ${d.name}`, errorMessage(e)))
                  .finally(onDone);
              }}
            >
              <Check size={15} />
            </button>
            <button type="button" className="ms-go" aria-label="Keep it" onClick={onDone}>
              <X size={15} />
            </button>
          </span>
        </span>
        <span className="ms-node-meta" />
        <span className="ms-node-actions" />
      </li>
    );
  return (
    <Loadable label={`Loading ${d.name}'s versions`} fallback={null}>
      <DefVersions d={d} onDone={onDone} />
    </Loadable>
  );
}

/** Its versions, newest first: when, by whom; any but the current made current again, as a new version. */
function DefVersions({ d, onDone }: { d: AgentDef; onDone: () => void }) {
  const res = useRes(agentdefVersionsRes(d.id));
  const [busy, setBusy] = React.useState<number | null>(null);
  const current = res.data[0]?.version;
  return (
    <>
      {res.data.map((v) => (
        <li key={v.version} className="ms-node df-ask" data-stand="done" data-depth={2}>
          <span className="ms-node-path" />
          <span className="ms-node-main">
            <span className="df-group-line">
              <span className="ms-sep-h df-group-k">v{v.version}</span>
              <span className="df-group-sum">
                <span>{v.model ? modelLabel(v.model) : "default"}</span>
                {limitWords(v.limits) && <span>{limitWords(v.limits)}</span>}
                <span className="df-clip">{v.instructions || "no instructions"}</span>
              </span>
            </span>
          </span>
          <span className="ms-node-meta">
            <span>
              <Ago ts={v.at} /> · {v.by}
            </span>
          </span>
          <span className="ms-node-actions">
            <span className="ms-tail">
              {v.version === current ? (
                <span className="ms-edit-k">current</span>
              ) : (
                <Button
                  size="xs"
                  variant="ghost"
                  pending={busy === v.version}
                  onClick={() => {
                    setBusy(v.version);
                    agentdefsApi
                      .restore(d.id, v.version)
                      .then((next) => {
                        invalidate("agentdefs/");
                        toast({ title: `v${v.version} is current again, as v${next.version}`, tone: "info" });
                      })
                      .catch((e: unknown) => toast.error(`Couldn't restore v${v.version}`, errorMessage(e)))
                      .finally(() => setBusy(null));
                  }}
                >
                  Restore
                </Button>
              )}
              {v.version === current && (
                <button type="button" className="ms-go" aria-label="Close its versions" onClick={onDone}>
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

/** The Agentfiles of yours by id, for a definition's `agentfile:<id>`. */
function useAgentfileNames(): Map<string, string> {
  const list = useRes(agentfilesRes()).data;
  return React.useMemo(() => new Map(list.agentfiles.map((a) => [`agentfile:${a.id}`, a.name])), [list]);
}
/** A definition's Agentfile by its name: one of yours, kiln's for its harness, or its harness's own (none). */
export function AgentfileName({ ref_, harness }: { ref_: string | null; harness: string }) {
  return (
    <React.Suspense fallback={<span>{ref_?.startsWith("agentfile:") ? "an Agentfile" : (ref_ ?? `harness-${harness}`)}</span>}>
      <AgentfileNameRead ref_={ref_} harness={harness} />
    </React.Suspense>
  );
}
function AgentfileNameRead({ ref_, harness }: { ref_: string | null; harness: string }) {
  const names = useAgentfileNames();
  return <span>{ref_ ? (names.get(ref_) ?? ref_) : `harness-${harness}`}</span>;
}
/** The Agentfile an experiment's setup starts from: the definition's, by name where it's one of yours. */
const setupAgentfile = (d: AgentDef) => (d.agentfile?.startsWith("agentfile:") ? (d.name === "release-notes" ? "docs-site" : "rust-dev") : (d.agentfile ?? `harness-${d.harness}`));

/** What a definition holds besides its values: its tools, MCP servers and skills, counted. */
function Holds({ id }: { id: string }) {
  const full = useRes(agentdefRes(id)).data;
  return (
    <span title={[...full.tools, ...full.mcp.map((m) => `mcp:${m.name}`), ...full.skills.map((k) => `skill:${k}`)].join(", ") || "nothing"}>
      {full.tools.length} tools · {full.mcp.length} MCP · {full.skills.length} skills
    </span>
  );
}

type Values = { model: string | null; limit: string; instructions: string };
const valuesOf = (f: AgentDefFull | Builtin): Values => ({ model: f.model, limit: limitWords(f.limits), instructions: f.instructions });
const VALUE_WORDS: Record<keyof Values, string> = { model: "model", limit: "limit", instructions: "instructions" };

/**
 * A definition's values (kiln's principle 5): each a value until pressed, then its control, in place.
 * A save is a new version (running sessions keep theirs; new ones get it), and carries the version it
 * was based on: one saved by someone else since is shown here as a diff, theirs and yours, never
 * overwritten unasked. kiln's own are read only (copy one to change it).
 */
export function DefValues({ d }: { d: AgentDef }) {
  const res = useRes(agentdefRes(d.id));
  const base = res.data;
  const own = d.own && "version" in base;
  const [editing, setEditing] = React.useState<keyof Values | null>(null);
  const [saving, setSaving] = React.useState(false);
  // Another's save between this one's read and its own: what each said, and the version now current.
  const [clash, setClash] = React.useState<{ mine: Partial<Values>; theirs: AgentDefFull } | null>(null);
  const shown = valuesOf(base);
  const save = (k: keyof Values, v: string, on: AgentDefFull = base as AgentDefFull) => {
    const change = k === "limit" ? { limits: { ...on.limits, seconds: limitSeconds(v) ?? undefined } } : k === "model" ? { model: v.trim() || null } : { instructions: v };
    setSaving(true);
    agentdefsApi
      .update(d.id, { ...change, version: on.version })
      .then((next) => {
        res.setData(() => next);
        setEditing(null);
        setClash(null);
        invalidate("agentdefs/list");
        toast({ title: next.version > on.version ? `Saved v${next.version}` : "Nothing changed", tone: "info" });
      })
      .catch((e: unknown) => {
        const current = e instanceof ApiError && e.status === 409 ? (e.body as { current?: AgentDefFull } | undefined)?.current : undefined;
        if (current) setClash({ mine: { [k]: v }, theirs: current });
        else toast.error(`Couldn't save its ${VALUE_WORDS[k]}`, errorMessage(e));
      })
      .finally(() => setSaving(false));
  };
  const field = (k: keyof Values, words: string) =>
    editing === k ? (
      <span className="ms-edit">
        <span className="ms-edit-k">{VALUE_WORDS[k]}</span>
        <input
          className="ms-edit-in"
          autoFocus
          defaultValue={k === "model" ? (shown.model ?? "") : shown[k]}
          placeholder={k === "model" ? "its harness's default" : k === "limit" ? "45m" : ""}
          aria-label={VALUE_WORDS[k]}
          disabled={saving}
          onKeyDown={(e) => {
            if (e.key === "Enter") save(k, e.currentTarget.value);
            if (e.key === "Escape") setEditing(null);
          }}
        />
        <button type="button" className="ms-go" disabled={saving} aria-label={`Save ${VALUE_WORDS[k]}`} onClick={(e) => save(k, (e.currentTarget.previousElementSibling as HTMLInputElement).value)}>
          <Check size={15} />
        </button>
      </span>
    ) : (
      <button type="button" className="ms-field" title={own ? `Change its ${VALUE_WORDS[k]}` : "kiln's: copy it to change it"} disabled={!own || saving} onClick={() => setEditing(k)}>
        <span className="ms-edit-k">{VALUE_WORDS[k]}</span>
        <span className="ms-field-v">{words || "none"}</span>
        <Pencil size={12} aria-hidden="true" />
      </button>
    );
  return (
    <>
      <span className="ms-fields">
        {field("model", shown.model ? modelLabel(shown.model) : "default")}
        {field("limit", shown.limit)}
        <span className="ms-field">
          <span className="ms-edit-k">version</span>
          <span className="ms-field-v">{own ? `v${(base as AgentDefFull).version}` : "kiln's"}</span>
        </span>
      </span>
      <span className="ms-fields">{field("instructions", shown.instructions)}</span>
      {clash && (
        <Clash
          mine={clash.mine}
          was={shown}
          theirs={clash.theirs}
          saving={saving}
          onTheirs={() => {
            res.setData(() => clash.theirs);
            setClash(null);
            setEditing(null);
            invalidate("agentdefs/list");
          }}
          onMine={() => {
            const [k, v] = Object.entries(clash.mine)[0] as [keyof Values, string];
            save(k, v, clash.theirs);
          }}
        />
      )}
    </>
  );
}

/** Saved by someone else since it was read: each value that differs, as it was, theirs now and yours, the change marked; theirs kept, or yours saved over it. */
function Clash({ mine, was, theirs, saving, onTheirs, onMine }: { mine: Partial<Values>; was: Values; theirs: AgentDefFull; saving: boolean; onTheirs: () => void; onMine: () => void }) {
  const now = valuesOf(theirs);
  const keys = (Object.keys(VALUE_WORDS) as (keyof Values)[]).filter((k) => k in mine || now[k] !== was[k]);
  const word = (v: string | null) => v || "none";
  return (
    <span className="df-clash" role="group" aria-label={`Saved as v${theirs.version} since you read it`}>
      <span className="df-clash-head">
        <span className="ms-edit-k">v{theirs.version} since</span>
        <Button size="xs" variant="ghost" disabled={saving} onClick={onTheirs}>
          Keep theirs
        </Button>
        <Button size="xs" variant="secondary" pending={saving} onClick={onMine}>
          Save mine on v{theirs.version}
        </Button>
      </span>
      {keys.map((k) => (
        <span key={k} className="df-clash-row">
          <span className="ms-edit-k">{VALUE_WORDS[k]}</span>
          <span className="df-clash-v" data-changed={now[k] !== was[k] || undefined} title="Theirs now">
            {word(now[k])}
          </span>
          {k in mine && (
            <span className="df-clash-v" data-mine="" title="Yours">
              {word(mine[k] ?? null)}
            </span>
          )}
        </span>
      ))}
    </span>
  );
}

/* ------------------------------------------------ a definition as a row of its own (the Agents tab, chat) */

/**
 * A definition as a row (the Agents tab's list, chat's card), in the dropdown's own columns: its
 * name, its sessions' stand and how many are at work; where it runs and how fast it starts; Run and
 * its rarer acts. Its chevron opens its groups under it.
 */
export function DefRow({ d, list, open, onOpen }: { d: AgentDef; list: readonly AgentInfo[]; open: boolean; onOpen: () => void }) {
  const [asking, setAsking] = React.useState<DefAskKind | null>(null);
  const sessions = d.sessions.map((s) => list.find((x) => x.id === s.id)).filter((a): a is AgentInfo => a !== undefined);
  const stands = sessions.map((a) => NODE[chatStand(a)]);
  const ORDER = ["needs", "failed", "working", "paused", "done", "offline", "stopped"];
  const stand = ORDER.find((o) => stands.includes(o)) ?? "done";
  const atWork = stands.filter((s) => s === "working" || s === "needs").length;
  return (
    <>
      <li ref={open ? watchUnseen : undefined} className="ms-node df-defrow" data-stand={stand} data-depth={0} data-open={open || undefined} data-def={d.id}>
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
            <RunButton d={d} />
            <DefMenu
              d={d}
              onSetup={() => {
                if (!open) onOpen();
              }}
              onAsk={setAsking}
            />
          </span>
        </span>
      </li>
      {asking && <DefAsk d={d} asking={asking} onDone={() => setAsking(null)} />}
      {open && <DefGroups d={d} list={list} first="definition" inRow onClose={onOpen} />}
    </>
  );
}

/**
 * A list of definitions' keys (kiln's principle 1): j and k move between its rows, Enter opens one
 * (its row's press), Esc closes the one open. Not while typing.
 */
export function defListKeys(e: React.KeyboardEvent<HTMLUListElement>, open: string | null, close: () => void) {
  if (e.altKey || e.ctrlKey || e.metaKey) return;
  const at = e.target instanceof HTMLElement ? e.target : null;
  if (!at || at.closest("input, textarea, select, [contenteditable]")) return;
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>(":scope > li.df-defrow")];
  const row = at.closest<HTMLElement>("li.df-defrow");
  if (e.key === "j" || e.key === "k") {
    const i = row ? rows.indexOf(row) : -1;
    rows[Math.max(0, Math.min(rows.length - 1, i + (e.key === "j" ? 1 : -1)))]?.querySelector<HTMLElement>(".df-group-main")?.focus();
    e.preventDefault();
  } else if (e.key === "Escape" && open) {
    e.preventDefault();
    close();
    rows
      .find((r) => r.dataset.def === open)
      ?.querySelector<HTMLElement>(".df-group-main")
      ?.focus();
  }
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
