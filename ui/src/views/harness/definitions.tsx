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
import { Check, ChevronRight, FlaskConical, Lock, MoreHorizontal, Pause, Pencil, Play, Plus, Repeat2, RotateCw, Square, Trash2, X } from "lucide-react";
import { useAgentList } from "./use-agents";
import "./mission.css";
import { errorMessage } from "@/lib/hooks";
import {
  agentdefRes,
  agentdefsApi,
  agentdefsRes,
  agentdefVersionsRes,
  enginesOf,
  limitSeconds,
  limitWords,
  measureOf,
  METRIC,
  queryOf,
  startMs,
  variantIn,
  type AgentDef,
  type AgentDefFull,
  type Builtin,
  type Criterion,
  type CriterionAsk,
  type Experiment,
  type Interval,
  type Measure,
  type Op,
  type Variant,
  type VariantAsk,
} from "@/api/agentdefs";
import { observeApi } from "@/api/observe";
import { Ago } from "./parts";
import { agentfilesRes } from "@/api/agentfiles";
import type { AgentfileListing } from "@/api/wire";
import { ApiError } from "@/api/client";
import { Loadable } from "@/components/ui/loading";
import { sendingProps, useHold, type Failed, type Hold } from "@/lib/action";
import { invalidate, resourceData, watch } from "@/lib/resource";
import { measureAxis, onAxis, rebalance, rescale, variantName, variantTone, winsMeasure, type Deployment } from "./variants";
import { sandboxesRes, type SandboxBrief } from "@/api/microvms";
import type { AgentInfo } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Segmented } from "@/components/ui/tabs";
import { toast } from "@/components/ui/toast";
import { agentsRes, labelOf, useRes } from "@/lib/data";
import { useNavigate } from "@/lib/router";
import { EngineGlyph, START, StartMark, WarmDots, msText } from "@/views/sandbox-marks";
import { chatStand, type ChatStand } from "./chat-state";
import { leanEngine } from "./engine-of";
import { modelLabel } from "./derive";
import "@/components/code/query-input.css";
import "./definition.css";
import "./figure/figure.css";

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

/** The variant a session started in, as its swatch and name (its Agentfiles by their `names`); none outside an experiment. */
export function sessionVariant(d: AgentDef | undefined, id: string, names?: ReadonlyMap<string, string>): { tone: string; name: string } | null {
  const at = variantIn(d, d?.sessions.find((x) => x.id === id)?.variant);
  return at ? { tone: variantTone(at.i, at.e.variants.length), name: variantName(at.e.variants, at.i, names) } : null;
}

/**
 * Your Agentfiles as they come, never waiting on them (a variant names its Agentfile by its id): none until
 * they're in, or where they can't be read, and a variant's name says its Agentfile's id meanwhile.
 */
const AGENTFILES = agentfilesRes();
const followAgentfiles = (changed: () => void) => watch(AGENTFILES.key, AGENTFILES.fetch, AGENTFILES.opts ?? {}, changed);
const agentfilesNow = () => resourceData(AGENTFILES);
const notYet = () => undefined;
function useAgentfilesSoon(): AgentfileListing | undefined {
  return React.useSyncExternalStore(followAgentfiles, agentfilesNow, notYet);
}
/** Your Agentfiles' names by their `agentfile:<id>`. */
const namesOf = (l: AgentfileListing | undefined): ReadonlyMap<string, string> => new Map((l?.agentfiles ?? []).map((a) => [`agentfile:${a.id}`, a.name]));
function useAgentfileNamesSoon(): ReadonlyMap<string, string> {
  const l = useAgentfilesSoon();
  return React.useMemo(() => namesOf(l), [l]);
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

/**
 * The agent a session runs: yours it started from, else kiln's it started from, else kiln's for its harness
 * (a session started from none runs its harness as kiln's Agentfile has it), or yours that stands in for that.
 */
export function sessionDefOf(defs: readonly AgentDef[], a: { id: string; kind: string }): AgentDef | null {
  const from = ownDefOf(defs, a.id)?.d ?? defs.find((d) => d.sessions.some((s) => s.id === a.id));
  if (from) return from;
  const kilns = defs.find((d) => !d.own && d.kind === "harness" && d.harness === a.kind);
  return (kilns?.replaced_by ? defs.find((d) => d.id === kilns.replaced_by) : undefined) ?? kilns ?? null;
}

/* ------------------------------------------------ where it runs, how fast it starts */

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
  const engine = leanEngine(useRes(agentsRes).data.find((r) => r.name === machine)?.facts);
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

/**
 * What this session started on, beside the current version (Restart on it, where behind), and its
 * experiment's variant (its swatch and name, never a letter), a labelled line each.
 */
function SessionFacts({ d, session }: { d: AgentDef; session: string }) {
  const names = useAgentfileNamesSoon();
  const mine = d.sessions.find((s) => s.id === session);
  if (!mine) return null;
  const variant = sessionVariant(d, session, names);
  const at = variantIn(d, mine.variant);
  const weight = at?.e.variants[at.i]?.weight;
  return (
    <span className="df-facts">
      <span className="df-fact">
        <span className="ms-edit-k">started on</span>
        <span className="df-fact-v">
          {mine.version === d.version ? (
            <span>v{d.version} (current)</span>
          ) : (
            <>
              <span className="df-old">
                v{mine.version} (v{d.version} is current)
              </span>
              <Button size="xs" variant="secondary" onClick={() => toast({ title: `Restarting on v${d.version}`, tone: "running" })}>
                Restart on v{d.version}
              </Button>
            </>
          )}
        </span>
      </span>
      {variant && weight !== undefined && (
        <span className="df-fact" title={`${variant.name}: ${weight}% of ${at?.e.name}'s new sessions`}>
          <span className="ms-edit-k">variant</span>
          <span className="df-fact-v">
            <Swatch tone={variant.tone} />
            <span>{variant.name}</span>
            <span className="meter" role="meter" aria-valuenow={weight} aria-valuemin={0} aria-valuemax={100} aria-label={`${weight}% of new sessions`}>
              <span style={{ width: `${weight}%` }} />
            </span>
            <span>{weight}%</span>
          </span>
        </span>
      )}
    </span>
  );
}

/** How each start kind reads as a line's label. */
const START_LABEL: Record<"warm" | "template" | "cold", string> = { warm: "Warm start", template: "Template start", cold: "Cold start" };

/**
 * What a definition is and where its sessions run, a line each, its plain words at the left (its values'
 * column) and its mark after them: where it runs, its warm hosts, each kind of start's median, its
 * Agentfile, its harness and what it holds.
 */
export function DefFacts({ d }: { d: AgentDef }) {
  const r = d.runs;
  const engines = enginesOf(r);
  return (
    <span className="df-facts">
      {engines.length > 0 && (
        <span className="df-fact">
          <span className="ms-edit-k">runs in</span>
          <span className="df-fact-v">
            {engines.map((e) => (
              <EngineGlyph key={e} engine={e} word />
            ))}
          </span>
        </span>
      )}
      {r.warm && r.warm.target > 0 && (
        <span className="df-fact">
          <span className="ms-edit-k">warm hosts</span>
          <span className="df-fact-v">
            <WarmDots n={r.warm.warm} of={r.warm.target} />
            {r.warm.warm} of {r.warm.target} warm
          </span>
        </span>
      )}
      {(["warm", "template", "cold"] as const).map((k) => {
        const ms = startMs(r, k);
        if (ms === undefined) return null;
        const S = START[k];
        return (
          <span key={k} className="df-fact">
            <span className="ms-edit-k">{START_LABEL[k]}</span>
            <span className="df-fact-v sm-start" data-kind={k}>
              <S.icon size={12} aria-hidden="true" />
              {msText(ms)} median
            </span>
          </span>
        );
      })}
      <span className="df-fact">
        <span className="ms-edit-k">Agentfile</span>
        <span className="df-fact-v">
          <AgentfileName ref_={d.agentfile} harness={d.harness} />
        </span>
      </span>
      <span className="df-fact">
        <span className="ms-edit-k">harness</span>
        <span className="df-fact-v">{d.harness}</span>
      </span>
      <span className="df-fact">
        <span className="ms-edit-k">holds</span>
        <span className="df-fact-v">
          <Loadable label="Loading what it holds" fallback={null}>
            <Holds id={d.id} />
          </Loadable>
        </span>
      </span>
    </span>
  );
}

/* ------------------------------------------------ the groups */

/** A session's stand as the tree's nodes draw it (mission.css .ms-node[data-stand]). */
const NODE: Record<ChatStand, string> = { needs: "needs", crashed: "failed", paused: "paused", working: "working", finished: "done", ready: "done", offline: "offline", ended: "stopped" };
const NODE_WORD: Record<string, string> = { needs: "Needs you", failed: "Stopped", paused: "Paused", working: "Working", done: "Done", offline: "Offline", stopped: "Ended" };

type Group = "definition" | "experiment";

/**
 * A definition's groups: SETTINGS, then EXPERIMENTS, in the tree's own columns. Under a Mission row
 * (one session: its resources are its engine's) each is one line until opened, one open at a time
 * (kiln's principle 1). `inRow`: its card in the Engine tab's Agents section, which folds: there
 * they're sections shown whole (no collapsible nested in another). Its sessions are Mission control's
 * rows, counted on its card's line. For a session (`session`) both are always there, its experiments none
 * until one is set up.
 */
export function DefGroupRows({ d, first, setup: setupFirst, inRow, session }: { d: AgentDef; first?: Group; setup?: boolean; inRow?: boolean; session?: string }) {
  const [open, setOpen] = React.useState<Group | null>(first ?? null);
  const [setup, setSetup] = React.useState(setupFirst === true);
  const toggle = (g: Group) => setOpen((was) => (was === g ? null : g));
  return (
    <>
      <DefinitionGroup
        d={d}
        inRow={inRow}
        session={session}
        open={inRow === true || open === "definition"}
        onOpen={() => toggle("definition")}
        onSetup={() => {
          setSetup(true);
          setOpen("experiment");
        }}
      />
      {(d.experiments.length > 0 || setup || d.own || session !== undefined) && (
        <ExperimentsGroup
          d={d}
          fixed={inRow}
          open={inRow === true || open === "experiment"}
          onOpen={() => toggle("experiment")}
          setup={setup}
          onSetup={() => {
            setSetup(true);
            setOpen("experiment");
          }}
          onSetupDone={() => setSetup(false)}
        />
      )}
    </>
  );
}

/** A definition's groups under its own row (the Agents tab, chat), under a head that stays in sight while its row is out of it. */
export function DefGroups({ d, first, setup, inRow, session }: { d: AgentDef; first?: Group; setup?: boolean; inRow?: boolean; session?: string }) {
  return (
    <li className="ms-tree df-groups">
      <ul className="ms-tree-rows" aria-label={`${d.name}: settings and experiments`}>
        <DefGroupRows d={d} first={first} setup={setup} inRow={inRow} session={session} />
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

/**
 * Its sessions' stands, as Mission's tree colours them: a dot each stand that has any, and how many
 * stand there, in the order they need you (needs you, failed, at work, paused, done, ended). As many
 * sessions as it has, the line stays one stand wide each: dots and numbers, never words beside them.
 */
const STAND_ORDER = ["needs", "failed", "working", "paused", "done", "offline", "stopped"] as const;
export function StandCounts({ stands, words: named = NODE_WORD }: { stands: readonly string[]; words?: Record<string, string> }) {
  const counts = STAND_ORDER.map((s) => [s, stands.filter((x) => x === s).length] as const).filter(([, n]) => n > 0);
  const words = counts.map(([s, n]) => `${n} ${named[s]?.toLowerCase() ?? s}`).join(", ");
  return (
    <span className="df-counts" role="img" aria-label={words} title={words}>
      {counts.map(([s, n]) => (
        <span key={s} className="df-count" data-stand={s}>
          <i aria-hidden="true" />
          {n}
        </span>
      ))}
    </span>
  );
}

/**
 * A group's head: one line, one height for every group, its parts centred on it: its chevron and
 * label (the press that opens it), what it says in short at its right end, then its controls.
 * Nothing in its middle: what its dropdown shows, it doesn't repeat.
 */
export function GroupHead({
  label,
  open,
  onOpen,
  summary,
  actions,
  stand = "done",
  fixed,
}: {
  label: string;
  open: boolean;
  onOpen: () => void;
  summary?: React.ReactNode;
  actions?: React.ReactNode;
  stand?: string;
  /** A section shown whole (inside something that folds): its label alone, nothing to press. */
  fixed?: boolean;
}) {
  return (
    <li className="ms-node df-group df-head" data-stand={stand} data-depth={1} data-open={open || undefined} data-fixed={fixed || undefined}>
      {fixed ? (
        <span className="df-head-main">
          <span className="ms-sep-h df-group-k">{label}</span>
        </span>
      ) : (
        <button type="button" className="df-head-main" aria-expanded={open} onClick={onOpen}>
          <span className="df-chev">
            <ChevronRight size={12} aria-hidden="true" className={open ? "rotate-90" : undefined} />
          </span>
          <span className="ms-sep-h df-group-k">{label}</span>
        </button>
      )}
      <span className="df-head-end">{summary}</span>
      {actions && <span className="df-head-acts">{actions}</span>}
    </li>
  );
}

function DefinitionGroup({ d, open, onOpen, onSetup, inRow, session }: { d: AgentDef; open: boolean; onOpen: () => void; onSetup: () => void; inRow?: boolean; session?: string }) {
  const [asking, setAsking] = React.useState<DefAskKind | null>(null);
  return (
    <>
      <GroupHead
        label="Settings"
        fixed={inRow}
        open={open}
        onOpen={onOpen}
        summary={<span className="df-head-v">{inRow ? `v${d.version}` : `${d.name} v${d.version}`}</span>}
        actions={inRow ? undefined : <DefMenu d={d} onSetup={onSetup} onAsk={setAsking} />}
      />
      {asking && <DefAsk d={d} asking={asking} onDone={() => setAsking(null)} />}
      {open && (
        <li className="ms-node df-body" data-stand="done" data-depth={2}>
          {session && <SessionFacts d={d} session={session} />}
          <DefFacts d={d} />
          <Loadable label={`Loading ${d.name}'s values`} fallback={<span className="ms-fields" aria-busy="true" />}>
            <DefValues d={d} />
          </Loadable>
        </li>
      )}
    </>
  );
}

/** An experiment's state as Mission's tree's stands (its counts' dots), and in its own words. */
const EXP_STAND: Record<Experiment["state"], string> = { live: "working", paused: "paused", promoted: "done", stopped: "stopped" };
const EXP_WORD: Record<string, string> = { working: "Live", paused: "Paused", done: "Promoted", stopped: "Stopped" };
/** What each measure is, its word on a variant's line, what its middle is, how it reads, and its unit. */
const MEASURE: Record<Measure, { word: string; label: string; mid: "median" | "rate"; f: (x: number) => string; unit: string }> = {
  warm_ms: { word: "Warm start", label: "warm", mid: "median", f: (x) => `${Math.round(x)}ms`, unit: "ms" },
  peak_gib: { word: "Peak memory", label: "peak", mid: "median", f: (x) => `${x.toFixed(2)}G`, unit: "GiB" },
  failed_pct: { word: "Failed sessions", label: "failed", mid: "rate", f: (x) => `${Math.round(x)}%`, unit: "%" },
};
const MEASURES = Object.keys(MEASURE) as Measure[];

/**
 * What a control of an experiment asks the server (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1), one change
 * of a definition's experiments at a time (W21-R15): what it changes `shows` at once where that's known (null: it's
 * gone), then the experiment as the server answers it (null: deleted), `then` after it; refused, it's as it was and
 * `failed` says why (a toast headed so, with the server's words).
 */
type Ask = (which: string, e: Experiment, how: { asks: () => Promise<Experiment | null>; shows?: Experiment | null; failed: Failed; then?: (now: Experiment | null) => void }) => void;

/** A variant's deployment as an added variant asks for it, its weight 0: a host's has no Agentfile or size. */
const deploymentAsk = (v: Variant): Omit<VariantAsk, "weight"> =>
  v.engine === "host" ? { engine: "host", ...(v.model ? { model: v.model } : {}) } : { agentfile: v.agentfile ?? undefined, cpus: v.cpus, memory: v.memory, ...(v.model ? { model: v.model } : {}) };

/**
 * Its experiments (P7.1), several at once: how many stand where at its head (no split there, each has
 * its own) and a + for another; opened, a new one being set up, then each one whole, laid out as its
 * engine's microVMs are (engine-rows.tsx): its line, then what it holds under it, nothing folded. Each
 * change is the server's (`agentdefs.experiments.*`): the list every page reads shows it as the server
 * answers it.
 */
function ExperimentsGroup({
  d,
  fixed,
  open,
  onOpen,
  setup,
  onSetup,
  onSetupDone,
}: {
  d: AgentDef;
  fixed?: boolean;
  open: boolean;
  onOpen: () => void;
  setup: boolean;
  onSetup: () => void;
  onSetupDone: () => void;
}) {
  const res = useRes(agentdefsRes);
  // One change of its experiments at a time: each answer is an experiment whole, and one live at a time is its definition's.
  const hold = useHold();
  const list = res.data.find((x) => x.id === d.id)?.experiments ?? d.experiments;
  /** Its experiments as `fn` makes them, in the list every page reads, at once (and over a load already on its way). */
  const edit = (fn: (l: Experiment[]) => Experiment[]) => res.change((defs) => defs.map((x) => (x.id === d.id ? { ...x, experiments: fn(x.experiments) } : x)));
  /** Experiment `id` as `e` is (null: gone), at `at` where it was taken out. */
  const put = (id: string, e: Experiment | null, at: number) =>
    edit((l) => {
      if (!e) return l.filter((x) => x.id !== id);
      if (l.some((x) => x.id === id)) return l.map((x) => (x.id === id ? e : x));
      return [...l.slice(0, Math.max(0, at)), e, ...l.slice(Math.max(0, at))];
    });
  const ask: Ask = (which, e, how) =>
    hold.change(`${e.id}/${which}`, async () => {
      const at = list.findIndex((x) => x.id === e.id);
      if (how.shows !== undefined) put(e.id, how.shows, at);
      try {
        const now = await how.asks();
        put(e.id, now, at);
        how.then?.(now);
      } catch (err) {
        if (how.shows !== undefined) put(e.id, e, at);
        if (typeof how.failed === "function") how.failed(err);
        else toast.error(how.failed, errorMessage(err));
      }
    });
  /** A promotion made the definition its next version: its row says so at once, and its values and versions are read again. */
  const promoted = (def: AgentDefFull) => {
    res.change((defs) => defs.map((x) => (x.id === def.id ? { ...x, name: def.name, version: def.version, model: def.model, agentfile: def.agentfile } : x)));
    invalidate("agentdefs/");
  };
  const stands = list.map((e) => EXP_STAND[e.state]);
  return (
    <>
      <GroupHead
        label="Experiments"
        fixed={fixed}
        open={open || setup}
        onOpen={onOpen}
        stand={list.some((e) => e.state === "live") ? "working" : "done"}
        summary={list.length ? <StandCounts stands={stands} words={EXP_WORD} /> : <span className="df-head-v">none</span>}
        actions={
          d.own ? (
            <button type="button" className="ms-go" aria-label={`New experiment on ${d.name}`} title="New experiment" onClick={onSetup}>
              <Plus size={15} />
            </button>
          ) : undefined
        }
      />
      {(open || setup) && (
        <ExperimentsOpen
          d={d}
          list={list}
          setup={setup}
          hold={hold}
          ask={ask}
          onPromoted={promoted}
          onSetupDone={onSetupDone}
          onStart={(e) => {
            edit((l) => [e, ...l.filter((x) => x.id !== e.id)]);
            onSetupDone();
            toast({ title: `Started ${e.name}`, tone: "running" });
          }}
        />
      )}
    </>
  );
}

/** Its experiments opened: a new one being set up, then each one whole; your Agentfiles' names read only once they're shown. */
function ExperimentsOpen({
  d,
  list,
  setup,
  hold,
  ask,
  onPromoted,
  onSetupDone,
  onStart,
}: {
  d: AgentDef;
  list: Experiment[];
  setup: boolean;
  hold: Hold;
  ask: Ask;
  onPromoted: (def: AgentDefFull) => void;
  onSetupDone: () => void;
  onStart: (e: Experiment) => void;
}) {
  const agentfiles = useAgentfilesSoon();
  const names = React.useMemo(() => namesOf(agentfiles), [agentfiles]);
  return (
    <>
      {setup && <SetupNodes d={d} agentfiles={agentfiles} names={names} hold={hold} onDone={onSetupDone} onStart={onStart} />}
      {list.map((e) => (
        <ExperimentNodes key={e.id} d={d} e={e} names={names} hold={hold} ask={ask} onPromoted={onPromoted} />
      ))}
      {list.length === 0 && !setup && (
        <li className="ms-node df-exp-body" data-depth={2}>
          <span className="df-note">No experiments.</span>
        </li>
      )}
    </>
  );
}

/**
 * One experiment, laid out as a microVM in its engine is: its line (its name, renamed in place; its
 * state in the agents' words' colours, since when, its sessions and its split's version; its controls
 * at the right, round, a stop red, each asked again), then under it its split by its named variants,
 * its variants (renamed, weighted, added, removed, one promoted) and its criteria, each with every
 * variant's value, the best checked, a bound missed marked, and a chart of each variant's day by day,
 * the bound drawn across it.
 */
function ExperimentNodes({ d, e, names, hold, ask, onPromoted }: { d: AgentDef; e: Experiment; names: ReadonlyMap<string, string>; hold: Hold; ask: Ask; onPromoted: (def: AgentDefFull) => void }) {
  const own = d.own;
  const api = agentdefsApi.experiments;
  const [staged, setStaged] = React.useState<number[] | null>(null);
  const [promoting, setPromoting] = React.useState<string | null>(null);
  const [ending, setEnding] = React.useState<"stop" | "delete" | null>(null);
  const [naming, setNaming] = React.useState(false);
  const [adding, setAdding] = React.useState(false);
  const vs = e.variants;
  const weights = staged ?? vs.map((v) => v.weight);
  const live = e.state === "live";
  const running = live || e.state === "paused";
  const stand = EXP_STAND[e.state];
  /** Control `which`'s change is on its way, or another change of the definition's experiments is (it waits). */
  const doing = (which: string) => hold.doing(`${e.id}/${which}`);
  const held = (which: string) => hold.held(`${e.id}/${which}`);
  const nameOf = (id: string) =>
    variantName(
      vs,
      vs.findIndex((v) => v.id === id),
      names,
    );
  // The split as staged, over the version it was read at: shown staged until the server answers, its version then the server's.
  const apply = () =>
    ask("weights", e, {
      asks: () => api.weights(d.id, e.id, Object.fromEntries(vs.map((v, i) => [v.id, weights[i] ?? v.weight])), e.version),
      failed: (err) => {
        toast.error(`Couldn't apply ${e.name}'s split`, errorMessage(err));
        // Another change came first (or it's over): what it is now, read again, and nothing staged over what it was.
        if (err instanceof ApiError && err.status === 409) {
          setStaged(null);
          invalidate("agentdefs/list");
        }
      },
      then: (now) => {
        setStaged(null);
        if (now) toast({ title: `${now.name}: split v${now.version}, ${now.variants.map((v) => v.weight).join("/")}`, tone: "info" });
      },
    });
  const pause = () =>
    ask("move", e, {
      shows: { ...e, state: live ? "paused" : "live" },
      asks: () => (live ? api.pause(d.id, e.id) : api.resume(d.id, e.id)),
      failed: live ? `Couldn't pause ${e.name}` : `Couldn't resume ${e.name}`,
      then: () => toast({ title: live ? `Paused ${e.name}` : `Resumed ${e.name}`, tone: "info" }),
    });
  const stop = () => {
    setEnding(null);
    ask("stop", e, { shows: { ...e, state: "stopped" }, asks: () => api.stop(d.id, e.id), failed: `Couldn't stop ${e.name}`, then: () => toast({ title: `Stopped ${e.name}`, tone: "info" }) });
  };
  // The server's promotion keeps the split's weights: the variant is what new sessions start on, and the experiment's over.
  const promote = (id: string) => {
    const name = nameOf(id);
    setPromoting(null);
    ask("promote", e, {
      shows: { ...e, state: "promoted", promoted: id, variants: vs.map((v) => ({ ...v, current: v.id === id })) },
      asks: () =>
        api.promote(d.id, e.id, id).then((r) => {
          onPromoted(r.agentdef);
          return r.experiment;
        }),
      failed: `Couldn't promote ${name}`,
      then: () => toast({ title: `Promoted ${name}`, tone: "succeeded" }),
    });
  };
  // A server-side delete can't be undone, and it's asked again in place before it goes: no Undo.
  const remove = () => {
    setEnding(null);
    ask("delete", e, { shows: null, asks: () => api.remove(d.id, e.id).then(() => null), failed: `Couldn't delete ${e.name}`, then: () => toast({ title: `Deleted ${e.name}`, tone: "info" }) });
  };
  const rename = (name: string) => {
    if (held("name")) return false;
    ask("name", e, { shows: { ...e, name }, asks: () => api.rename(d.id, e.id, name), failed: `Couldn't rename ${e.name}` });
    return true;
  };
  const renameVariant = (id: string, name: string) => {
    if (held(`name/${id}`)) return false;
    ask(`name/${id}`, e, {
      shows: { ...e, variants: vs.map((v) => (v.id === id ? { ...v, name: name || null } : v)) },
      asks: () => api.renameVariant(d.id, e.id, id, name),
      failed: `Couldn't rename ${nameOf(id)}`,
    });
    return true;
  };
  // Another of the last one's deployment, at weight 0: its id is the server's, so it shows once the server answers.
  const addVariant = () => {
    const last = vs[vs.length - 1];
    if (!last) return;
    ask("variant", e, { asks: () => api.variants.add(d.id, e.id, deploymentAsk(last)), failed: `Couldn't add a variant to ${e.name}`, then: () => setStaged(null) });
  };
  // Its share back to the others in proportion, as the server will give it (its split's next version where it had one).
  const dropVariant = (id: string) => {
    const gone = vs.find((v) => v.id === id);
    const rest = vs.filter((v) => v.id !== id);
    const ws = gone && gone.weight > 0 ? rescale(rest.map((v) => v.weight)) : null;
    setStaged(null);
    ask(`variant/${id}`, e, {
      shows: gone && gone.weight > 0 && !ws ? undefined : { ...e, version: ws ? e.version + 1 : e.version, variants: rest.map((v, i) => ({ ...v, weight: ws?.[i] ?? v.weight })) },
      asks: () => api.variants.remove(d.id, e.id, id),
      failed: `Couldn't remove ${nameOf(id)}`,
    });
  };
  const addCriterion = (c: CriterionAsk, said: (why: string) => void) =>
    ask("criterion", e, { asks: () => api.criteria.add(d.id, e.id, c), failed: (err) => said(errorMessage(err)), then: () => setAdding(false) });
  const dropCriterion = (c: Criterion) =>
    ask(`criterion/${c.id}`, e, { shows: { ...e, criteria: e.criteria.filter((o) => o.id !== c.id) }, asks: () => api.criteria.remove(d.id, e.id, c.id), failed: `Couldn't remove ${c.name}` });
  return (
    <>
      <li className="ms-node df-exp" data-stand={stand} data-depth={2}>
        <span className="df-vm-text">
          <span className="df-vm-name">
            <FlaskConical size={12} aria-hidden="true" className="df-exp-glyph" />
            {naming ? (
              <input
                className="ms-edit-in"
                autoFocus
                defaultValue={e.name}
                aria-label="Its name"
                onKeyDown={(k) => {
                  if (k.key === "Enter" && k.currentTarget.value.trim() && rename(k.currentTarget.value.trim())) setNaming(false);
                  if (k.key === "Escape") setNaming(false);
                }}
              />
            ) : (
              <span>{e.name}</span>
            )}
            {own && !naming && (
              <button type="button" className="df-rename" onClick={() => setNaming(true)} aria-label={`Rename ${e.name}`} title="Rename it">
                <Pencil size={12} aria-hidden="true" />
              </button>
            )}
          </span>
          <span className="df-vm-line">
            <span className="ms-word">
              <i aria-hidden="true" />
              {EXP_WORD[stand]}
            </span>
            <span>
              <Ago ts={Math.floor(e.started_at / 1000)} />
            </span>
            <span>{e.sessions} sessions</span>
            <span>split v{e.version}</span>
          </span>
        </span>
        {own && (
          <span className="df-vm-ctl">
            {ending ? (
              <span className="ms-confirm" role="group" aria-label={`${ending === "stop" ? "Stop" : "Delete"} ${e.name}?`}>
                <span>{ending === "stop" ? "Stop it?" : "Delete it?"}</span>
                <button
                  type="button"
                  className="fig-round"
                  data-size="sm"
                  data-tone="stop"
                  aria-label={ending === "stop" ? `Stop ${e.name}` : `Delete ${e.name}`}
                  disabled={held(ending)}
                  onClick={ending === "stop" ? stop : remove}
                >
                  <Check size={15} />
                </button>
                <button type="button" className="fig-round" data-size="sm" aria-label="Keep it" onClick={() => setEnding(null)}>
                  <X size={15} />
                </button>
              </span>
            ) : (
              <>
                {running && (
                  <button
                    type="button"
                    className="fig-round"
                    data-size="sm"
                    title={live ? "Pause the split" : "Resume the split"}
                    aria-label={`${live ? "Pause" : "Resume"} ${e.name}`}
                    disabled={held("move")}
                    {...sendingProps(doing("move"))}
                    onClick={pause}
                  >
                    {live ? <Pause size={15} /> : <Play size={15} />}
                  </button>
                )}
                {running && (
                  <button type="button" className="fig-round" data-size="sm" data-tone="stop" title="Stop it" aria-label={`Stop ${e.name}`} onClick={() => setEnding("stop")}>
                    <Square size={13} />
                  </button>
                )}
                <button type="button" className="fig-round" data-size="sm" data-tone="stop" title="Delete it" aria-label={`Delete ${e.name}`} onClick={() => setEnding("delete")}>
                  <Trash2 size={14} />
                </button>
              </>
            )}
          </span>
        )}
      </li>
      <li className="ms-node df-exp-body" data-stand={stand} data-depth={3}>
        <span
          className="df-exp-split"
          data-staged={staged ? "" : undefined}
          aria-label={`Split ${vs.map((_, i) => `${variantName(vs, i, names)} ${weights[i]}%`).join(", ")}${staged ? ", not yet applied" : ""}`}
        >
          {vs.map((v, i) => (
            <i key={v.id} style={{ flexGrow: Math.max(weights[i] ?? 0, 0.5), background: variantTone(i, vs.length) }} title={`${variantName(vs, i, names)} · ${weights[i]}%`} />
          ))}
        </span>
        {staged && (
          <span className="df-exp-ctl">
            <Button size="xs" variant="secondary" pending={doing("weights")} disabled={held("weights")} onClick={apply}>
              Apply the split
            </Button>
            <Button size="xs" variant="ghost" onClick={() => setStaged(null)}>
              Discard
            </Button>
          </span>
        )}
        <span className="ms-sep-h df-exp-k">Variants</span>
        <ul className="df-vlist">
          {vs.map((v, i) => (
            <VariantLine
              key={v.id}
              all={vs}
              i={i}
              names={names}
              weight={weights[i] ?? v.weight}
              fixed={!running || !own}
              applying={doing("weights")}
              held={hold.going}
              promoting={promoting === v.id}
              onPromoting={() => setPromoting(promoting === v.id ? null : v.id)}
              onPromote={() => promote(v.id)}
              onWeight={(w) => setStaged(rebalance(weights, i, w))}
              onName={(name) => renameVariant(v.id, name)}
              onDrop={vs.length > 2 && !v.current && running && own ? () => dropVariant(v.id) : undefined}
            />
          ))}
        </ul>
        {own && running && (
          <button type="button" className="df-add" disabled={held("variant")} {...sendingProps(doing("variant"))} onClick={addVariant}>
            <Plus size={13} aria-hidden="true" />
            Add a variant
          </button>
        )}
        <span className="ms-sep-h df-exp-k">Criteria</span>
        {e.criteria.length === 0 && <span className="df-note">No criteria.</span>}
        {e.criteria.map((c) => (
          <CriterionBlock key={c.id} c={c} vs={vs} names={names} held={hold.going} onDrop={own ? () => dropCriterion(c) : undefined} />
        ))}
        {own &&
          (adding ? (
            <CriterionForm e={e} names={names} adding={doing("criterion")} held={held("criterion")} onAdd={addCriterion} onCancel={() => setAdding(false)} />
          ) : (
            <button type="button" className="df-add" onClick={() => setAdding(true)}>
              <Plus size={13} aria-hidden="true" />
              Add a criterion
            </button>
          ))}
      </li>
    </>
  );
}

/**
 * A variant: its swatch and name (renamed in place), its measures where its sessions have any (each its 95% interval on
 * the axis its experiment's variants share), its weight and sessions, Promote, its removal.
 */
function VariantLine({
  all,
  i,
  names,
  weight,
  fixed,
  applying,
  held,
  promoting,
  onPromoting,
  onPromote,
  onWeight,
  onName,
  onDrop,
}: {
  all: Variant[];
  i: number;
  names: ReadonlyMap<string, string>;
  weight: number;
  fixed: boolean;
  /** Its split's weights are on their way to the server: they hold still until it answers. */
  applying: boolean;
  /** A change of its definition's experiments is on its way: its promotion and its removal wait. */
  held: boolean;
  promoting: boolean;
  onPromoting: () => void;
  onPromote: () => void;
  onWeight: (w: number) => void;
  /** Whether the name was taken (another change on its way holds it). */
  onName: (n: string) => boolean;
  onDrop?: () => void;
}) {
  const v = all[i]!;
  const [naming, setNaming] = React.useState(false);
  const tone = variantTone(i, all.length);
  const name = variantName(all, i, names);
  const m = v.measures;
  return (
    <li className="df-vline">
      <span className="df-vline-top">
        <Swatch tone={tone} current={v.current} title={v.current ? `${name}: what new sessions start on` : name} />
        {naming ? (
          <input
            className="ms-edit-in"
            autoFocus
            defaultValue={v.name ?? ""}
            placeholder={variantName(
              all.map((x, j) => (j === i ? { ...x, name: null } : x)),
              i,
              names,
            )}
            aria-label="Variant name"
            onKeyDown={(k) => {
              if (k.key === "Enter" && onName(k.currentTarget.value.trim())) setNaming(false);
              if (k.key === "Escape") setNaming(false);
            }}
          />
        ) : (
          <button type="button" className="df-vname" title="Rename it" onClick={() => setNaming(true)}>
            {name}
            <Pencil size={11} aria-hidden="true" />
          </button>
        )}
        {v.current && <span className="df-head-v">current</span>}
      </span>
      {m && (
        <span className="df-vline-measures">
          {MEASURES.map((k) => (
            <Measure key={k} k={k} iv={m[k]} axis={measureAxis(all, k)} tone={tone} win={winsMeasure(all, i, k)} />
          ))}
        </span>
      )}
      <span className="df-vline-weight">
        <input
          type="range"
          className="ms-weight-in"
          style={{ accentColor: tone }}
          min={0}
          max={100}
          value={weight}
          disabled={fixed || applying}
          aria-label={`${name}'s weight`}
          onChange={(k) => onWeight(Number(k.target.value))}
        />
        <span className="df-head-v">
          {weight}% · {v.sessions} sessions
        </span>
      </span>
      {!fixed && (
        <span className="df-vline-ctl">
          {promoting ? (
            <span className="ms-confirm" role="group" aria-label={`Promote ${name}?`}>
              <span>Promote?</span>
              <button type="button" className="fig-round" data-size="sm" aria-label={`Promote ${name}`} disabled={held} onClick={onPromote}>
                <Check size={15} />
              </button>
              <button type="button" className="fig-round" data-size="sm" aria-label="Keep the split" onClick={onPromoting}>
                <X size={15} />
              </button>
            </span>
          ) : (
            <>
              {!v.current && (
                <Button size="xs" variant="secondary" onClick={onPromoting}>
                  Promote
                </Button>
              )}
              {onDrop && (
                <button type="button" className="ms-go" aria-label={`Remove ${name}`} title="Remove it" disabled={held} onClick={onDrop}>
                  <X size={15} />
                </button>
              )}
            </>
          )}
        </span>
      )}
    </li>
  );
}

/** A comparator in words, as a criterion's rule reads. */
const OP_WORD: Record<Op, string> = { ">": "greater than", ">=": "greater than or equal to", "==": "equal to", "!=": "not equal to", "<": "less than", "<=": "less than or equal to" };

/**
 * A criterion: its name, its rule (its query's value against its bound, as a monitor decides it) and
 * its query; each variant's value of its query as kiln's monitors evaluate it now, whatever it reads (the
 * server's `values`), passing or missing, the best marked where the rule has a direction; and, where it
 * reads one of kiln's session metrics, each variant's day by day, its bound drawn across.
 */
function CriterionBlock({ c, vs, names, held, onDrop }: { c: Criterion; vs: Variant[]; names: ReadonlyMap<string, string>; held: boolean; onDrop?: () => void }) {
  const k = measureOf(c.query);
  const f = k ? MEASURE[k].f : (x: number) => String(Number(x.toPrecision(4)));
  const vals = vs.map((v) => c.values?.[v.id]);
  const xs = vals.flatMap((x) => (x ? [x.value] : []));
  const best = xs.length === 0 ? null : c.op === "<" || c.op === "<=" ? Math.min(...xs) : c.op === ">" || c.op === ">=" ? Math.max(...xs) : null;
  return (
    <div className="df-crit">
      <span className="df-crit-top">
        <span className="df-crit-name">{c.name}</span>
        <span className="df-head-v">
          {OP_WORD[c.op]} {f(c.bound)}
        </span>
        {onDrop && (
          <button type="button" className="ms-go" aria-label={`Remove ${c.name}`} title="Remove it" disabled={held} onClick={onDrop}>
            <X size={14} />
          </button>
        )}
      </span>
      <code className="df-crit-query">
        {c.query} {c.op} {c.bound}
      </code>
      {xs.length > 0 ? (
        <span className="df-crit-vals">
          {vs.map((v, i) => {
            const x = vals[i];
            if (!x) return null;
            return (
              <span key={v.id} className="df-crit-val" data-best={x.value === best || undefined} data-missed={!x.passes || undefined}>
                <Swatch tone={variantTone(i, vs.length)} />
                {f(x.value)}
                {x.passes ? " passes" : " misses"}
                {x.value === best ? " · best" : ""}
              </span>
            );
          })}
        </span>
      ) : (
        <span className="df-note">No values yet.</span>
      )}
      {k && (
        <TrendChart lines={vs.map((v, i) => ({ label: variantName(vs, i, names), tone: variantTone(i, vs.length), values: v.trend?.[k] ?? [] }))} bound={c.bound} label={`${c.name}, day by day`} />
      )}
    </div>
  );
}

/** Each line its variant's tone, day by day, scaled to them all and the bound, which is drawn across dashed. */
function TrendChart({ lines, bound, label }: { lines: { label: string; tone: string; values: number[] }[]; bound?: number; label: string }) {
  const all = [...lines.flatMap((l) => l.values), ...(bound === undefined ? [] : [bound])];
  if (all.length < 2) return null;
  const lo = Math.min(...all);
  const span = Math.max(...all) - lo || 1;
  const W = 480,
    H = 64,
    pad = 4;
  const y = (v: number) => H - pad - ((v - lo) / span) * (H - 2 * pad);
  const x = (i: number, n: number) => pad + (i / Math.max(1, n - 1)) * (W - 2 * pad);
  const days = Math.max(...lines.map((l) => l.values.length));
  return (
    <>
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className="df-trend" role="img" aria-label={label}>
        {bound !== undefined && <line x1={0} x2={W} y1={y(bound)} y2={y(bound)} className="df-trend-bound" />}
        {lines.map((l) =>
          l.values.length > 1 ? (
            <polyline key={l.label} points={l.values.map((v, i) => `${x(i, l.values.length)},${y(v)}`).join(" ")} fill="none" stroke={l.tone} strokeWidth={1.6} vectorEffect="non-scaling-stroke" />
          ) : null,
        )}
      </svg>
      <span className="df-trend-cap" aria-hidden="true">
        <span>{days - 1} days ago</span>
        <span>today</span>
      </span>
    </>
  );
}

/** The query field (CodeMirror, loaded as it's first opened). */
const QueryInput = React.lazy(() => import("@/components/code/query-input"));

/**
 * A criterion added in place: its name, then its whole expression in kiln's monitoring query syntax
 * (Datadog's: aggregations, a metric or a formula, its scope, by variant, a comparison and a bound),
 * coloured and completed as it's typed; Add (or Enter), which kiln's monitor check passes first, then
 * the server keeps (its words in place where it refuses it), and Cancel.
 */
function CriterionForm({
  e,
  names,
  adding,
  held,
  onAdd,
  onCancel,
}: {
  e: Experiment;
  names: ReadonlyMap<string, string>;
  /** Its criterion is on its way to the server. */
  adding: boolean;
  /** Another change of its definition's experiments is: Add waits. */
  held: boolean;
  onAdd: (c: CriterionAsk, said: (why: string) => void) => void;
  onCancel: () => void;
}) {
  const [name, setName] = React.useState("");
  const [expr, setExpr] = React.useState(`${queryOf("failed_pct", e.id)} `);
  const [said, setSaid] = React.useState<string | null>(null);
  const [checking, setChecking] = React.useState(false);
  const words = React.useMemo(
    () => ({
      metrics: Object.values(METRIC),
      tags: {
        experiment: [{ value: e.id, detail: e.name }],
        variant: e.variants.map((v, i) => ({ value: v.id, detail: variantName(e.variants, i, names) })),
      },
    }),
    [e, names],
  );
  const ready = name.trim() !== "" && expr.trim() !== "";
  // The expression as the field has it (Enter passes the editor's own: a render can lag the last key typed).
  const add = (text: string) => {
    const typed = text.trim();
    if (name.trim() === "" || typed === "" || checking || adding || held) return;
    setChecking(true);
    observeApi
      .validateMonitor(typed)
      .then((c) => {
        if (!c.ok || c.op === undefined || c.threshold === undefined) return setSaid(c.error ?? "It needs a comparison and a bound: < 10");
        const query = typed.replace(/\s*(>=|<=|==|!=|>|<)\s*-?\d+(?:\.\d+)?\s*$/, "");
        setSaid(null);
        onAdd({ name: name.trim(), query, op: c.op as Op, bound: c.threshold }, setSaid);
      })
      .catch((err: unknown) => setSaid(errorMessage(err)))
      .finally(() => setChecking(false));
  };
  return (
    <span className="df-crit-form">
      <input className="ms-edit-in" value={name} aria-label="Name" placeholder="Name" onChange={(k) => setName(k.target.value)} />
      <React.Suspense fallback={<span className="ms-edit-in qi" aria-busy="true" />}>
        <QueryInput
          value={expr}
          words={words}
          label="Expression"
          placeholder="avg(last_7d):avg:kiln.session.warm_ms{experiment:…} by {variant} < 250"
          onChange={(v) => {
            setExpr(v);
            setSaid(null);
          }}
          onEnter={add}
        />
      </React.Suspense>
      {said && (
        <span className="df-note" role="alert">
          {said}
        </span>
      )}
      <span className="df-crit-acts">
        <Button size="xs" pending={checking || adding} disabled={!ready || held} onClick={() => add(expr)}>
          Add
        </Button>
        <Button size="xs" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
      </span>
    </span>
  );
}

type Draft = { agentfile: string; engine: "microvm" | "host"; cpus: number; memory: number; weight: number };
/** A draft as its deployment: on the host, no Agentfile or size (what it had stays in the draft, for a microVM again). */
const draftDeployment = (v: Draft): Deployment => (v.engine === "host" ? { engine: "host", agentfile: null } : { engine: "microvm", agentfile: v.agentfile, cpus: v.cpus, memory: v.memory });
/** A draft as a new experiment asks for it: a host's carries no Agentfile or size, which the server refuses for one (422). */
const draftAsk = (v: Draft): VariantAsk => (v.engine === "host" ? { weight: v.weight, engine: "host" } : { weight: v.weight, agentfile: v.agentfile, cpus: v.cpus, memory: v.memory });

/**
 * A new experiment, set up in place: its name, its variants (as many as you add, each a deployment and a
 * weight: a microVM's Agentfile, of yours or kiln's for its harness, and size, or the host); Start splits new
 * sessions once the server takes it (its words in place where it doesn't: one live at a time, a variant it
 * can't run), and its criteria are added on its card.
 */
function SetupNodes({
  d,
  agentfiles,
  names,
  hold,
  onDone,
  onStart,
}: {
  d: AgentDef;
  agentfiles: AgentfileListing | undefined;
  names: ReadonlyMap<string, string>;
  hold: Hold;
  onDone: () => void;
  onStart: (e: Experiment) => void;
}) {
  const G = 2 ** 30;
  // The definition's own Agentfile at first, else kiln's for its harness.
  const first = d.agentfile ?? `harness-${d.harness}`;
  const [name, setName] = React.useState(`${d.name}'s sizes`);
  const [vs, setVs] = React.useState<Draft[]>([
    { agentfile: first, engine: "microvm", cpus: 2, memory: 4 * G, weight: 80 },
    { agentfile: first, engine: "microvm", cpus: 4, memory: 8 * G, weight: 20 },
  ]);
  const [said, setSaid] = React.useState<string | null>(null);
  const shown = vs.map(draftDeployment);
  // What a microVM can start from: its first, yours, and kiln's for its harness.
  const choices = [...new Set([first, ...(agentfiles?.agentfiles ?? []).map((a) => `agentfile:${a.id}`), ...(agentfiles?.builtin ?? []).filter((b) => b.harness === d.harness).map((b) => b.name)])];
  const set = (i: number, patch: Partial<Draft>) => setVs(vs.map((v, j) => (j === i ? { ...v, ...patch } : v)));
  const start = () =>
    hold.change("create", async () => {
      setSaid(null);
      try {
        onStart(await agentdefsApi.experiments.create(d.id, { name: name.trim() || `${d.name}'s split`, variants: vs.map(draftAsk) }));
      } catch (err) {
        setSaid(errorMessage(err));
      }
    });
  return (
    <li className="ms-node df-exp-body df-exp-new" data-stand="paused" data-depth={2}>
      <span className="ms-sep-h df-exp-k">New experiment</span>
      <input className="ms-edit-in df-exp-namein" value={name} aria-label="Its name" onChange={(k) => setName(k.target.value)} />
      <span className="ms-sep-h df-exp-k">Variants</span>
      <ul className="df-vlist">
        {vs.map((v, i) => (
          <li key={i} className="df-vline">
            <span className="df-vline-top">
              <Swatch tone={variantTone(i, vs.length)} title={variantName(shown, i, names)} />
              <span className="df-vname">{variantName(shown, i, names)}</span>
              {i > 1 && (
                <button type="button" className="ms-go" aria-label="Remove this variant" onClick={() => setVs(vs.filter((_, j) => j !== i))}>
                  <X size={15} />
                </button>
              )}
            </span>
            <span className="ms-setup">
              {/* A microVM's Agentfile and size; a host runs its harness as a process of its machine, with neither. */}
              {v.engine === "microvm" && (
                <select className="ms-edit-in" value={v.agentfile} aria-label="Agentfile" onChange={(k) => set(i, { agentfile: k.target.value })}>
                  {choices.map((a) => (
                    <option key={a} value={a}>
                      {names.get(a) ?? a}
                    </option>
                  ))}
                </select>
              )}
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
                  <select className="ms-edit-in ms-narrow" value={v.cpus} aria-label="vCPUs" onChange={(k) => set(i, { cpus: Number(k.target.value) })}>
                    {[1, 2, 4, 8].map((n) => (
                      <option key={n} value={n}>
                        {n} vCPU
                      </option>
                    ))}
                  </select>
                  <select className="ms-edit-in ms-narrow" value={v.memory / G} aria-label="Memory" onChange={(k) => set(i, { memory: Number(k.target.value) * G })}>
                    {[2, 4, 8, 16].map((n) => (
                      <option key={n} value={n}>
                        {n} GiB
                      </option>
                    ))}
                  </select>
                </>
              )}
            </span>
            <span className="df-vline-weight">
              <input
                type="range"
                className="ms-weight-in"
                style={{ accentColor: variantTone(i, vs.length) }}
                min={0}
                max={100}
                value={v.weight}
                aria-label={`${variantName(shown, i, names)}'s weight`}
                onChange={(k) => {
                  const ws = rebalance(
                    vs.map((x) => x.weight),
                    i,
                    Number(k.target.value),
                  );
                  setVs(vs.map((x, j) => ({ ...x, weight: ws[j] ?? x.weight })));
                }}
              />
              <span className="df-head-v">{v.weight}%</span>
            </span>
          </li>
        ))}
      </ul>
      <button type="button" className="df-add" onClick={() => setVs([...vs, { ...vs[vs.length - 1]!, weight: 0 }])}>
        <Plus size={13} aria-hidden="true" />
        Add a variant
      </button>
      {said && (
        <span className="df-note" role="alert">
          {said}
        </span>
      )}
      <span className="df-exp-ctl">
        <Button size="xs" pending={hold.doing("create")} disabled={hold.held("create")} onClick={start}>
          Start
        </Button>
        <Button size="xs" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
      </span>
    </li>
  );
}

/**
 * A measure of a variant (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.2): its 95% interval a bar along `axis`, the
 * one its experiment's variants share, its middle a tick, in its swatch's tone; its middle beside, and a check where
 * its interval clears every other variant's. One its sessions haven't bounded is "—", in its place.
 */
function Measure({ k, iv, axis, tone, win }: { k: Measure; iv?: Interval; axis: [number, number] | null; tone: string; win: boolean }) {
  const { word, label, mid, f } = MEASURE[k];
  if (!iv || !axis)
    return (
      <span className="df-measure" data-none="" title={`${word}: no interval`}>
        <span className="df-measure-k">{label}</span>
        <span className="df-whisk" aria-hidden="true" />
        <span className="df-measure-v">—</span>
      </span>
    );
  const [low, high] = [onAxis(iv[0], axis), onAxis(iv[2], axis)];
  return (
    <span className="df-measure" data-win={win || undefined} title={`${word}: ${mid} ${f(iv[1])}, 95% ${f(iv[0])}–${f(iv[2])}${win ? ", clear of every other variant's" : ""}`}>
      <span className="df-measure-k">{label}</span>
      <span className="df-whisk" aria-hidden="true">
        <b style={{ left: `${low}%`, width: `${high - low}%`, background: tone }} />
        <s style={{ left: `${onAxis(iv[1], axis)}%`, background: tone }} />
      </span>
      <span className="df-measure-v">
        {f(iv[1])}
        {win ? " ✓" : ""}
      </span>
    </span>
  );
}

type DefAskKind = "delete" | "versions";

/**
 * A definition's acts, behind one ⋯, as a Mission row's are (actions.tsx): a session started from it
 * (the server starts an agent from it, a microVM of its Agentfile's latest, and its page opens; one
 * that waits says so), its versions, an experiment, a copy, delete; kiln's own copied as yours.
 */
function DefMenu({ d, onSetup, onAsk }: { d: AgentDef; onSetup: () => void; onAsk: (what: DefAskKind) => void }) {
  const navigate = useNavigate();
  const start = () =>
    agentdefsApi
      .run(d.id)
      .then((r) => {
        const started = r.agents[0];
        if (started) navigate(`/agents/${started.id}`);
        else toast({ title: `${d.name} v${r.agentdef.version}`, description: r.queued, tone: "info" });
      })
      .catch((e: unknown) => toast.error(`Couldn't start ${d.name}`, errorMessage(e)));
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
        { label: `Start a session from ${d.name}`, onSelect: () => void start() },
        { label: `Versions (v${d.version})`, onSelect: () => onAsk("versions") },
        { label: "New experiment", onSelect: onSetup },
        { label: "Duplicate", onSelect: () => void duplicate() },
        { label: "Delete…", onSelect: () => onAsk("delete"), danger: true },
      ]
    : [
        { label: `Start a session from ${d.name}`, onSelect: () => void start() },
        { label: "Copy as yours", onSelect: () => void copy() },
      ];
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
            <span className="df-ver-line">
              <span className="ms-sep-h df-group-k">v{v.version}</span>
              <span className="df-ver-sum">
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
const plural = (n: number, word: string) => `${n} ${word}${n === 1 ? "" : "s"}`;
/** What a definition holds besides its values: its tools, MCP servers and skills, counted. */
function Holds({ id }: { id: string }) {
  const full = useRes(agentdefRes(id)).data;
  return (
    <span title={[...full.tools, ...full.mcp.map((m) => `mcp:${m.name}`), ...full.skills.map((k) => `skill:${k}`)].join(", ") || "nothing"}>
      {plural(full.tools.length, "tool")} · {plural(full.mcp.length, "MCP server")} · {plural(full.skills.length, "skill")}
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
      <span className="ms-edit" data-k={k}>
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
      <button type="button" className="ms-field" data-k={k} title={own ? `Change its ${VALUE_WORDS[k]}` : "kiln's: copy it to change it"} disabled={!own || saving} onClick={() => setEditing(k)}>
        <span className="ms-edit-k">{VALUE_WORDS[k]}</span>
        <span className="ms-field-v">{words || "none"}</span>
        <Pencil size={12} aria-hidden="true" />
      </button>
    );
  // Each value on a line of its own, its label in one column; its instructions' words under their label, the
  // whole width theirs to wrap in.
  return (
    <>
      <span className="df-values">
        {field("model", shown.model ? modelLabel(shown.model) : "default")}
        {field("limit", shown.limit)}
        <span className="ms-field" data-k="version">
          <span className="ms-edit-k">version</span>
          <span className="ms-field-v">{own ? `v${(base as AgentDefFull).version}` : "kiln's"}</span>
        </span>
        {field("instructions", shown.instructions)}
      </span>
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
export function DefRow({ d, list, session }: { d: AgentDef; list: readonly AgentInfo[]; session?: string }) {
  const [asking, setAsking] = React.useState<DefAskKind | null>(null);
  const [setup, setSetup] = React.useState(0);
  const sessions = d.sessions.map((s) => list.find((x) => x.id === s.id)).filter((a): a is AgentInfo => a !== undefined);
  const stands = sessions.map((a) => NODE[chatStand(a)]);
  const ORDER = ["needs", "failed", "working", "paused", "done", "offline", "stopped"];
  const stand = ORDER.find((o) => stands.includes(o)) ?? "done";
  return (
    <>
      {/* Its card's line: its name and flags (this session's marked), its sessions' stands at its right, its ⋯. */}
      <li className="ms-node df-defrow df-head" data-stand={stand} data-depth={0} data-def={d.id}>
        <span className="df-head-main">
          <span className="df-strong df-defname">{d.name}</span>
          {session && <span className="df-head-v df-this">this session</span>}
          {!d.own && <Lock size={12} className="ms-flag-pin" role="img" aria-label="kiln's: copy it to change it" />}
          {(d.replaces || d.replaced_by) && <Repeat2 size={12} className="ms-flag-pin" role="img" aria-label={d.replaces ? `Stands in for kiln's ${d.replaces}` : `You use ${d.replaced_by}`} />}
        </span>
        <span className="df-head-end">{sessions.length > 0 ? <StandCounts stands={stands} /> : <span className="df-head-v">v{d.version}</span>}</span>
        <span className="df-head-acts">
          <DefMenu d={d} onSetup={() => setSetup((n) => n + 1)} onAsk={setAsking} />
        </span>
      </li>
      {asking && <DefAsk d={d} asking={asking} onDone={() => setAsking(null)} />}
      <DefGroups key={setup} d={d} first={setup ? "experiment" : session ? "definition" : undefined} setup={setup > 0} inRow session={session} />
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
        <DefRow d={d} list={list} />
      </ul>
    </div>
  );
}
