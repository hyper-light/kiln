/**
 * Mission control's composer with nothing selected: it starts an agent, written like a message to
 * it. Above the box, the agent page's own line (permissions · model · effort, and the bolt where
 * the model goes faster); in the box's top slot, the agent, its machine, its folder and how many,
 * each opened in place; under the box, More options (name, workspace, where its commands run, a
 * shell to run in) and the machine's past sessions, each a dropdown that opens in place. Start
 * starts it and opens it; several stay here, and the line under the box says so. There is no
 * dialog: every way in that had one fills this in (new-agent-draft.ts).
 *
 * Typing never waits: the field is here at once, and what needs the machines' answer (the line,
 * the slot, Start) comes in its own Suspense boundaries as soon as it's known.
 */
import * as React from "react";
import "./harness.css";
import "./console.css";
import { ArrowUp, Check, ChevronDown, CircleAlert, Paperclip } from "lucide-react";
import { api } from "@/api/client";
import { clientId, harnessApi, sessionStore, type AgentInfo, type KindInfo, type PastSession, type WorkspacesAvailable } from "@/api/harness";
import type { Agent, ReachRunner, TerminalSessionInfo } from "@/api/types";
import { Loadable } from "@/components/ui/loading";
import { Tip } from "@/components/ui/tooltip";
import { relativeTime } from "@/lib/format";
import { errorMessage, useLocalStorage, useMediaQuery, useNow } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { useNavigate, useSearchParams } from "@/lib/router";
import { cn } from "@/lib/utils";
import { atOf, doingOf, findAgents } from "./broadcast";
import { useAttachments } from "./controls";
import { agentLabel, modelLabel, tildify } from "./derive";
import { InlinePath } from "./inline-path";
import { usePrefill, type Prefill } from "./new-agent-draft";
import { machineName } from "./parts";
import { RunSettings, Scale, WordRow, type Word } from "./run-settings";
import { effortsFor, resumeEffort } from "./spawn-efforts";
import { harnessesOf, kindOf, machineLabel, machinesWith, modeOf, modelOf, modesFor, oneOf, reachChoices, specOf, speedsFor, targetOf, withDir } from "./spawn-setup";
import { useHeld } from "./state";
import { MicButton } from "./voice";

/** What starting agents needs to know: each machine's harnesses, the machines, their shells, the runners its commands can go to, and where it can have a workspace of its own. */
interface Machines {
  byTarget: Record<string, KindInfo[]>;
  machines: Agent[];
  shells: TerminalSessionInfo[];
  reach: ReachRunner[];
  workspaces: WorkspacesAvailable;
}

/** Asked all at once. A part that can't be had is empty: no harnesses (the line under the box says what to do), no machine names, no shells. */
const askMachines = (signal: AbortSignal): Promise<Machines> =>
  Promise.all([
    harnessApi.kindsAll().catch((): Record<string, KindInfo[]> => ({})),
    api.agents(signal).then(
      (a) => a.filter((m) => m.connected && !m.ephemeral),
      (): Agent[] => [],
    ),
    api.terminalSessions(signal).catch((): TerminalSessionInfo[] => []),
    api.reachRunners(signal).then(
      (r) => r.runners.filter((x) => x.connected && x.reach),
      (): ReachRunner[] => [],
    ),
    harnessApi.workspaces().catch((): WorkspacesAvailable => ({})),
  ]).then(([byTarget, machines, shells, reach, workspaces]) => ({ byTarget, machines, shells, reach, workspaces }));
/** Read again now and then: a machine connects, a harness is installed. */
const MACHINES = { pollMs: 60_000 } as const;

const COUNTS = [1, 2, 3, 4, 6, 8, 12];
type Part = "kind" | "machine" | "count" | null;

/** Enter starts with a keyboard and pointer; on a touch screen it's a new line (as in the composer). */
const fine = () => window.matchMedia?.("(pointer: fine)").matches;

/** The choices: those kept across visits (per harness where they're its own), those for this start, and what another page handed over. */
function useChoices() {
  const [kind, setKind] = useLocalStorage("kiln.agents.kind", "claude");
  const [lastTarget, setLastTarget] = useLocalStorage("kiln.agents.target", "");
  const [modeByKind, setModeByKind] = useLocalStorage<Record<string, string>>("kiln.agents.modes", {});
  const [effortByKind, setEffortByKind] = useLocalStorage<Record<string, string>>("kiln.agents.efforts", {});
  const [modelByKind, setModelByKind] = useLocalStorage<Record<string, string>>("kiln.agents.models", {});
  const [speedByKind, setSpeedByKind] = useLocalStorage<Record<string, string>>("kiln.agents.speeds", {});
  /** `volume`: a Slates volume of its own over the folder, where the machine can make one. */
  const [workspace, setWorkspace] = useLocalStorage<"folder" | "volume">("kiln.agents.workspace", "folder");
  const [dirs, setDirs] = useLocalStorage<Record<string, string[]>>("kiln.agents.recentDirs", {});
  const [picked, setPicked] = React.useState<string | null>(null);
  /** The folder typed or browsed to, on the machine it was for. */
  const [folder, setFolder] = React.useState<{ target: string; cwd: string } | null>(null);
  const [count, setCount] = React.useState(1);
  const [name, setName] = React.useState("");
  const [reach, setReach] = React.useState("");
  const [shell, setShell] = React.useState("");
  /** A mode that turns checks off, confirmed (in the line) for this start only. */
  const [unchecked, setUnchecked] = React.useState("");
  const [open, setOpen] = React.useState<Part>(null);
  // What another page handed over (new-agent-draft.ts): this start's choices, each until you change it.
  const handed = usePrefill((s) => s.prefill);
  const [seed, setSeed] = React.useState<Prefill | null>(null);
  if (handed && handed.at !== seed?.at) {
    setSeed(handed);
    setPicked(null);
    setFolder(null);
    setShell(handed.shell ?? "");
    setCount(1);
    setUnchecked("");
  }
  const drop = (k: keyof Prefill) => setSeed((s) => (s ? { ...s, [k]: undefined } : s));
  const clearSeed = () => {
    usePrefill.getState().clear();
    setSeed(null);
    setShell("");
  };
  /** After a start: what was for that start alone starts over. */
  const reset = () => {
    clearSeed();
    setName("");
    setCount(1);
    setUnchecked("");
    setOpen(null);
  };
  return {
    kind,
    setKind,
    lastTarget,
    setLastTarget,
    modeByKind,
    setModeByKind,
    effortByKind,
    setEffortByKind,
    modelByKind,
    setModelByKind,
    speedByKind,
    setSpeedByKind,
    workspace,
    setWorkspace,
    dirs,
    setDirs,
    picked,
    setPicked,
    folder,
    setFolder,
    count,
    setCount,
    name,
    setName,
    reach,
    setReach,
    shell,
    setShell,
    unchecked,
    setUnchecked,
    open,
    setOpen,
    seed,
    drop,
    clearSeed,
    reset,
  };
}
type Choices = ReturnType<typeof useChoices>;

/** The choices, read against what the machines have: which agent, where, its folder, how it runs. */
function resolve(c: Choices, m: Machines) {
  const all = harnessesOf(m.byTarget);
  const kind = kindOf(all, c.seed?.kind ?? c.kind);
  const where = machinesWith(m.byTarget, kind);
  const target = targetOf(where, [c.picked, c.seed?.target, c.lastTarget]);
  const recent = c.dirs[target] ?? [];
  // The folder: typed or browsed to on this machine, else the one handed over, else the last one used here.
  const cwd = (c.folder?.target === target ? c.folder.cwd : undefined) ?? (c.seed?.target === target ? c.seed.cwd : undefined) ?? recent[0] ?? "~";
  const info = m.byTarget[target]?.find((x) => x.id === kind) ?? all.find((x) => x.id === kind);
  const modes = modesFor(info);
  const mode = modeOf(modes, { unchecked: c.unchecked, remembered: c.seed?.mode ?? c.modeByKind[kind], fallback: info?.default_mode });
  const model = modelOf(info, c.seed?.model ?? c.modelByKind[kind] ?? "");
  const efforts = effortsFor(info, model.chosen);
  const effort = oneOf(efforts, c.seed?.effort ?? c.effortByKind[kind]);
  const speeds = speedsFor(info, model.chosen);
  const speed = oneOf(speeds, c.speedByKind[kind]);
  const canVolume = m.workspaces[target]?.available === true;
  // The other harnesses on that machine, by their models: choosing one starts the agent there.
  const others = (m.byTarget[target] ?? []).filter((x) => x.available && x.id !== kind && (x.models?.length ?? 0) > 0).map((x) => ({ kind: x.id, name: x.name, models: x.models ?? [] }));
  return {
    all,
    kind,
    where,
    target,
    recent,
    cwd,
    info,
    modes,
    mode,
    model,
    efforts,
    effort,
    speeds,
    speed,
    canVolume,
    volume: c.workspace === "volume" && canVolume && !c.shell,
    others,
    shells: m.shells.filter((s) => s.target === target && !s.exited),
    kindName: info?.name ?? kind,
    machine: target ? machineLabel(target, m.machines) : "",
    noHarness: !all.some((k) => k.available),
    // Connected machines whose runner never reported its harnesses: too old to run agents.
    stale: m.machines.filter((x) => !(`agent:${x.name}` in m.byTarget)),
    machines: m.machines,
    reach: m.reach,
    workspaces: m.workspaces,
  };
}
type Setup = ReturnType<typeof resolve>;

function useSetup(c: Choices): Setup {
  return resolve(c, useSuspenseResource("harness/spawn", askMachines, MACHINES).data);
}

/** Above the box: the agent page's own line, for the agent to be. */
function HowLine({ c }: { c: Choices }) {
  const s = useSetup(c);
  const efforts = s.efforts.length > 0 ? [{ id: "", label: "Default", description: "The model's own" }, ...s.efforts.map((e) => ({ id: e.id, label: e.label, description: e.hint }))] : [];
  return (
    <RunSettings
      className="ms-tuner mc-how"
      modes={s.modes}
      mode={s.mode}
      onMode={(m) => {
        if (s.modes.find((x) => x.id === m)?.dangerous) {
          c.setUnchecked(m);
          return;
        }
        c.setUnchecked("");
        c.setModeByKind({ ...c.modeByKind, [s.kind]: m });
        c.drop("mode");
      }}
      models={s.info?.models ?? []}
      model={s.model.shown}
      onModel={(m, other) => {
        if (other && other !== s.kind) {
          c.setKind(other);
          c.drop("kind");
          c.setModelByKind({ ...c.modelByKind, [other]: m });
          return;
        }
        c.setModelByKind({ ...c.modelByKind, [s.kind]: m });
        c.drop("model");
      }}
      harness={s.kind}
      elsewhere={s.others}
      modelLabel={modelLabel}
      efforts={efforts}
      effort={s.effort}
      onEffort={(e) => {
        c.setEffortByKind({ ...c.effortByKind, [s.kind]: e });
        c.drop("effort");
      }}
      speeds={s.speeds}
      speed={s.speed}
      onSpeed={(v) => c.setSpeedByKind({ ...c.speedByKind, [s.kind]: v })}
      switchWords={(name) => `Starts in ${name} instead`}
    />
  );
}

const Dot = () => (
  <span className="mc-dot" aria-hidden="true">
    ·
  </span>
);

/** In the box's top slot: the agent, its machine, its folder (or a shell), how many, each opened in place. */
function Slot({ c }: { c: Choices }) {
  const s = useSetup(c);
  const toggle = (p: Exclude<Part, null>) => c.setOpen(c.open === p ? null : p);
  const part = (p: Exclude<Part, null>, text: string, eyebrow?: string) => (
    <button type="button" className="ip-line mc-part" aria-expanded={c.open === p} onClick={() => toggle(p)}>
      {eyebrow && <span className="ip-eyebrow">{eyebrow}</span>}
      <span className="ip-path">
        <span className="ip-seg" data-last="">
          {text}
        </span>
      </span>
    </button>
  );
  const shell = c.shell ? s.shells.find((x) => x.id === c.shell) : undefined;
  const kinds: Word[] = s.all.map((k) => ({ id: k.id, text: k.name, full: k.name, hint: k.available ? (k.verified ? undefined : "Untested") : "Not installed on any connected machine" }));
  return (
    <div className="mc-slot">
      {part("kind", s.kindName)}
      <Dot />
      {part("machine", s.machine || "no machine has it", "On")}
      <Dot />
      {c.shell ? (
        <button type="button" className="ip-line mc-part" title="It starts where the shell is: tap for a folder instead" onClick={() => c.setShell("")}>
          <span className="ip-eyebrow">In shell</span>
          <span className="ip-path">
            <span className="ip-seg" data-last="">
              {shell?.title || "this shell"}
            </span>
          </span>
        </button>
      ) : s.target ? (
        <InlinePath target={s.target} value={s.cwd} onChange={(v) => c.setFolder({ target: s.target, cwd: v })} recent={s.recent} label="In" className="mc-where" />
      ) : null}
      <Dot />
      {part("count", `×${c.count}`)}
      {c.open === "kind" && (
        <div className="rs-panel mc-slot-panel" role="group" aria-label="Agent">
          <WordRow
            label="Agent"
            words={kinds}
            value={s.kind}
            onPick={(w) => {
              c.setKind(w.id);
              c.drop("kind");
              c.setOpen(null);
            }}
          />
          <p className="rs-now">
            <span className="rs-now-name">{s.kindName}</span>
            <span className="rs-now-hint">{s.where.length > 0 ? `On ${s.where.map((t) => machineLabel(t, s.machines)).join(", ")}` : "Not on any connected machine"}</span>
          </p>
        </div>
      )}
      {c.open === "machine" && (
        <div className="rs-panel mc-slot-panel" role="group" aria-label="Machine">
          <WordRow
            label="Machine"
            words={s.where.map((t) => ({ id: t, text: machineLabel(t, s.machines), full: machineLabel(t, s.machines) }))}
            value={s.target}
            onPick={(w) => {
              c.setPicked(w.id);
              c.drop("target");
              c.setShell("");
              c.setOpen(null);
            }}
          />
        </div>
      )}
      {c.open === "count" && (
        <div className="rs-panel mc-slot-panel" role="group" aria-label="How many">
          <Scale
            label="How many"
            words={COUNTS.map((n) => ({
              id: String(n),
              text: String(n),
              full: n === 1 ? "1 agent" : `${n} agents`,
              hint: n === 1 ? "One agent" : `Each gets this message, named ${s.kindName} 1–${n}`,
            }))}
            value={String(c.count)}
            onPick={(w) => c.setCount(Number(w.id))}
          />
        </div>
      )}
    </div>
  );
}

function StartButton({ c, busy, blocked, run }: { c: Choices; busy: boolean; blocked: boolean; run: (s: Setup) => void }) {
  const s = useSetup(c);
  const label = busy ? "Starting…" : c.count > 1 ? `Start ${c.count}` : "Start";
  return (
    <button
      type="button"
      className="cp-send mc-start"
      data-mode="send"
      disabled={busy || blocked || !s.target || s.noHarness}
      onClick={() => run(s)}
      aria-label={busy ? label : `${label}: ${s.kindName} on ${s.machine}`}
    >
      <ArrowUp size={15} />
      <span>{label}</span>
    </button>
  );
}

/** A choice of a few, as a word with a chevron over the system's own picker. */
function Pick({ label, value, onChange, options }: { label: string; value: string; onChange: (v: string) => void; options: { value: string; label: string }[] }) {
  return (
    <label className="mc-pick">
      <span>{options.find((o) => o.value === value)?.label ?? options[0]?.label}</span>
      <ChevronDown size={12} className="mc-chev" aria-hidden="true" />
      <select aria-label={label} value={value} onChange={(e) => onChange(e.target.value)}>
        {options.map((o) => (
          <option key={o.value} value={o.value}>
            {o.label}
          </option>
        ))}
      </select>
    </label>
  );
}

/** More options, open: one quiet row each, two to a line where there's room. */
function Options({ c, s }: { c: Choices; s: Setup }) {
  return (
    <div className="mc-opts">
      <div className="mc-opt">
        <span className="mc-opt-key">Name</span>
        <div className="mc-opt-val">
          <input
            className="mc-inline-input"
            value={c.name}
            onChange={(e) => c.setName(e.target.value)}
            placeholder={c.count > 1 ? "fix-tests (numbered)" : "It names itself from its work"}
            aria-label="Name"
          />
        </div>
      </div>
      <div className="mc-opt">
        <span className="mc-opt-key">Workspace</span>
        <div className="mc-opt-val">
          <div className="mc-seg" role="radiogroup" aria-label="Workspace">
            <button type="button" role="radio" aria-checked={!s.volume} onClick={() => c.setWorkspace("folder")}>
              This folder
            </button>
            <button
              type="button"
              role="radio"
              aria-checked={s.volume}
              disabled={!s.canVolume || Boolean(c.shell)}
              title={
                s.canVolume
                  ? "A Slates volume over the folder: what it changes stays its own until it's up for review or you write it to disk"
                  : (s.workspaces[s.target]?.why_not ?? "Not on this machine")
              }
              onClick={() => c.setWorkspace("volume")}
            >
              Own volume
            </button>
          </div>
        </div>
      </div>
      <div className="mc-opt">
        <span className="mc-opt-key">Commands</span>
        <div className="mc-opt-val">
          <Pick
            label="Where its commands run"
            value={c.reach}
            onChange={c.setReach}
            options={[{ value: "", label: "Any runner" }, ...reachChoices(s.reach, s.target), { value: "off", label: "Only its machine" }]}
          />
        </div>
      </div>
      <div className="mc-opt">
        <span className="mc-opt-key">Run in</span>
        <div className="mc-opt-val">
          <Pick
            label="Run in"
            value={c.shell}
            onChange={c.setShell}
            options={[{ value: "", label: "A folder" }, ...s.shells.map((x) => ({ value: x.id, label: `Shell: ${x.title || x.id.slice(0, 8)}` }))]}
          />
        </div>
      </div>
    </div>
  );
}

/** The machine's past sessions with this harness, once it has said (or said why it can't). */
function PastList({ target, kind, kindName, onPick }: { target: string; kind: string; kindName: string; onPick: (p: PastSession) => void }) {
  const now = useNow();
  const { list, error } = useSuspenseResource(`harness/history/${target}/${kind}`, () =>
    harnessApi.history(target, kind).then(
      (list) => ({ list, error: null }),
      (e: unknown) => ({ list: [] as PastSession[], error: e instanceof Error ? e.message : String(e) }),
    ),
  ).data;
  if (error) return <p className="ps-none">{/(502|404|not found)/i.test(error) ? "This machine's runner can't list its past sessions yet: upgrade it in System → Updates." : error}</p>;
  if (list.length === 0) return <p className="ps-none">No past {kindName} sessions on this machine.</p>;
  return (
    <ul className="ps-list">
      {list.map((p) => (
        <li key={p.id}>
          <button type="button" className="ps-row" onClick={() => onPick(p)}>
            <span className="ps-title">{p.title || p.last_prompt || p.id}</span>
            <span className="ps-meta">
              {tildify(p.cwd)}
              {p.branch ? ` · ${p.branch}` : ""} · {relativeTime(p.last_activity, now)}
            </span>
          </button>
        </li>
      ))}
    </ul>
  );
}

/** Under the box: where it was handed over from, More options and past sessions (each opens in place), and what's in the way. */
function Under({
  c,
  more,
  setMore,
  past,
  setPast,
  onResume,
}: {
  c: Choices;
  more: boolean;
  setMore: (o: boolean) => void;
  past: boolean;
  setPast: (o: boolean) => void;
  onResume: (s: Setup, p: PastSession) => void;
}) {
  const s = useSetup(c);
  // What More options holds that isn't as it starts out, said while it's closed.
  const summary = [c.name.trim() && `“${c.name.trim()}”`, s.volume && "Own volume", c.reach && (c.reach === "off" ? "Only its machine" : `Prefers ${c.reach}`), c.shell && "In a shell"]
    .filter(Boolean)
    .join(" · ");
  return (
    <>
      <div className="mc-under">
        {c.seed?.from && (
          <span className="mc-from">
            {c.seed.from}
            <button type="button" className="ms-link" onClick={c.clearSeed}>
              Clear
            </button>
          </span>
        )}
        <button type="button" className="mc-toggle" aria-expanded={more} onClick={() => setMore(!more)}>
          More options
          {!more && summary && <span className="mc-sum">{summary}</span>}
          <ChevronDown size={12} className="mc-chev" aria-hidden="true" />
        </button>
        {s.target && (
          <button type="button" className="mc-toggle" aria-expanded={past} onClick={() => setPast(!past)}>
            Continue a past session on {s.machine}
            <ChevronDown size={12} className="mc-chev" aria-hidden="true" />
          </button>
        )}
      </div>
      {more && <Options c={c} s={s} />}
      {past && s.target && (
        <div className="ps-body mc-past">
          <Loadable label="Loading past sessions" fallback={<p className="ps-none">Looking…</p>}>
            <PastList target={s.target} kind={s.kind} kindName={s.kindName} onPick={(p) => onResume(s, p)} />
          </Loadable>
        </div>
      )}
      {s.stale.length > 0 ? (
        <p className="mc-note">
          <b>{s.stale.map((m) => m.name).join(", ")}</b> {s.stale.length === 1 ? "is" : "are"} connected, but {s.stale.length === 1 ? "its kiln runner is" : "their kiln runners are"} too old to run
          agents. Upgrade without a restart (shells keep running) by running this on {s.stale.length === 1 ? "that machine" : "each machine"} with the new kiln binary:{" "}
          <code>kiln install --agent --upgrade</code>
        </p>
      ) : (
        s.noHarness && <p className="mc-note">No connected machine has a coding agent installed. Install Claude Code or Codex on any machine running kiln, then try again.</p>
      )}
    </>
  );
}

const LineLoading = () => (
  <div className="rs-tuner ms-tuner mc-how" aria-hidden="true">
    <div className="rs-top">
      <p className="rs-line" />
    </div>
  </div>
);
const SlotLoading = () => (
  <div className="mc-slot">
    <span className="mc-looking">Looking at your machines…</span>
  </div>
);

export function NewAgent({
  text,
  onText,
  all,
  onPick,
  autoFocus,
}: {
  /** What's written: kept by Mission control, so it stays as the composer turns to messaging agents and back. */
  text: string;
  onText: (t: string | ((was: string) => string)) => void;
  /** Every agent, to find by typing @: picking one messages it instead. */
  all: AgentInfo[];
  onPick: (ids: string[], on: boolean) => void;
  autoFocus?: boolean;
}) {
  const c = useChoices();
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  // ?new=1 (the command palette, an entry point that filled this in): the field takes the focus once.
  const fromLink = params.get("new") === "1";
  const linked = React.useRef(false);
  const attachments = useAttachments("any");
  const { files, pending } = attachments;
  const picker = React.useRef<HTMLInputElement>(null);
  const field = React.useRef<HTMLTextAreaElement | null>(null);
  const root = React.useRef<HTMLElement>(null);
  const [busy, setBusy] = React.useState(false);
  const [starting, setStarting] = React.useState("");
  const [error, setError] = React.useState<string | null>(null);
  const [result, setResult] = React.useState<{ n: number; kind: string; machine: string; first: string } | null>(null);
  const [note, setNote] = React.useState<string | null>(null);
  const [more, setMore] = React.useState(false);
  const [past, setPast] = React.useState(false);
  const [focused, setFocused] = React.useState(false);
  const [found, setFound] = React.useState(0);
  const [inside, setInside] = React.useState(false);
  const phone = useMediaQuery("(max-width: 640px)");

  /** Opens it as soon as its first snapshot is in (a moment at most), as one view transition. */
  const open = async (id: string) => {
    await Promise.race([sessionStore(id).ready(), new Promise<void>((r) => window.setTimeout(r, 250))]);
    // Only while it opens do its parts carry the names the transition moves them by (mission.css, console.css).
    const html = document.documentElement;
    html.dataset.opening = "";
    navigate(`/agents/${id}`, { viewTransition: true });
    window.setTimeout(() => delete html.dataset.opening, 800);
  };

  const run = async (s: Setup) => {
    if (busy || !s.target || pending > 0) return;
    const body = text.trim();
    // Files go with the first message, so then the words go with them rather than in the spec.
    const sending = files.length > 0 ? attachments.payload() : undefined;
    const spec = specOf({
      kind: s.kind,
      cwd: s.cwd,
      shell: c.shell,
      mode: s.mode,
      model: s.model.chosen,
      effort: s.effort,
      speed: s.speed,
      name: c.name,
      prompt: sending ? "" : body,
      reach: c.reach,
      volume: s.volume,
    });
    setBusy(true);
    setError(null);
    setResult(null);
    setStarting(`Starting ${s.kindName} on ${s.machine}`);
    try {
      const started = await harnessApi.start(s.target, spec, c.count);
      c.setLastTarget(s.target);
      if (!c.shell) c.setDirs(withDir(c.dirs, s.target, s.cwd));
      const ids = started.map((a) => a.id);
      if (sending) {
        // One agent: kept for it, its page sends it once it can take it. Several: sent to them all now.
        const only = ids.length === 1 ? ids[0] : undefined;
        if (only) useHeld.getState().hold(only, { id: clientId(), text: body, at: Date.now(), attachments: sending, mode: "queue" });
        else if (ids.length > 1)
          await harnessApi.broadcastTo({ to: { ids }, text: body, attachments: sending, key: clientId() }).catch((e: unknown) => setError(`Started, but the files didn't go: ${errorMessage(e)}`));
      }
      onText("");
      attachments.clear();
      c.reset();
      setMore(false);
      const first = started[0];
      if (started.length === 1 && first) {
        await open(first.id);
        return;
      }
      setResult({ n: started.length, kind: s.kindName, machine: s.machine, first: first?.id ?? "" });
      invalidate("harness/agents");
    } catch (e) {
      setError(`Couldn't start: ${errorMessage(e)}`);
    } finally {
      setBusy(false);
      setStarting("");
    }
  };

  /** Pick up a past conversation where it ended: same harness, folder and history. */
  const resume = async (s: Setup, p: PastSession) => {
    setBusy(true);
    setError(null);
    setStarting(`Continuing it on ${s.machine}`);
    try {
      const started = await harnessApi.start(
        s.target,
        { kind: p.kind, cwd: p.cwd || undefined, resume: p.id, mode: s.mode || undefined, effort: resumeEffort(s.info, p.model, s.effort), name: (p.title || "").slice(0, 80) || undefined },
        1,
      );
      c.setDirs(withDir(c.dirs, s.target, p.cwd));
      const first = started[0];
      if (first) await open(first.id);
    } catch (e) {
      setError(`Couldn't continue it: ${errorMessage(e)}`);
    } finally {
      setBusy(false);
      setStarting("");
    }
  };

  // @ and part of a name finds agents: picking one messages it instead (the composer turns to it).
  const at = focused ? atOf(text) : undefined;
  const finds = at === undefined ? [] : findAgents(all, at, new Set());
  const add = (a: AgentInfo) => {
    onText((t) => t.replace(/(^|\s)@[^\s@]*$/, "$1"));
    setFound(0);
    onPick([a.id], true);
  };

  // Grows with the text up to the stylesheet's most lines, then scrolls inside (as the composer).
  const lengthWas = React.useRef(0);
  const grow = React.useCallback(
    (el: HTMLTextAreaElement | null) => {
      field.current = el;
      if (!el) return;
      if (text.length < lengthWas.current || !text) el.style.height = "auto";
      lengthWas.current = text.length;
      const h = `${el.scrollHeight}px`;
      if (el.style.height !== h) el.style.height = h;
    },
    [text],
  );
  const attach = (el: HTMLTextAreaElement | null) => {
    grow(el);
    if (!fromLink) linked.current = false;
    if (!el || !fromLink || linked.current) return;
    linked.current = true;
    el.focus();
    const n = new URLSearchParams(params);
    n.delete("new");
    setParams(n, { replace: true });
  };

  // On a phone it's one line until it's used.
  const quiet = phone && !inside && !text && files.length === 0 && !more && !past && !busy && !error && !result && !c.seed && c.open === null;
  return (
    <section
      ref={root}
      className="ms-msg cp mc"
      data-quiet={quiet || undefined}
      aria-label="New agent"
      onFocus={() => setInside(true)}
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setInside(false);
      }}
    >
      <React.Suspense fallback={<LineLoading />}>
        <HowLine c={c} />
      </React.Suspense>
      <div
        className="ms-msg-field"
        onDragOver={(e) => {
          if (e.dataTransfer.types.includes("Files")) e.preventDefault();
        }}
        onDrop={(e) => {
          if (e.dataTransfer.files.length === 0) return;
          e.preventDefault();
          void attachments.add(e.dataTransfer.files);
        }}
      >
        <div className={cn("hx-compose-box", busy && "hx-compose-live")}>
          <React.Suspense fallback={<SlotLoading />}>
            <Slot c={c} />
          </React.Suspense>
          {attachments.chips}
          <textarea
            ref={attach}
            className="hx-compose-input"
            rows={1}
            value={text}
            disabled={busy}
            autoFocus={autoFocus}
            placeholder={quiet ? "New agent: what should it do?" : c.count > 1 ? `What should these ${c.count} agents do? Each gets this message` : "What should it do?"}
            aria-label="Its first message"
            enterKeyHint="send"
            autoCapitalize="sentences"
            onFocus={() => setFocused(true)}
            onBlur={() => setFocused(false)}
            onPaste={(e) => {
              if (e.clipboardData.files.length === 0) return;
              e.preventDefault();
              void attachments.add(e.clipboardData.files);
            }}
            onChange={(e) => {
              onText(e.target.value);
              setFound(0);
              setError(null);
              setResult(null);
            }}
            onKeyDown={(e) => {
              if (finds.length > 0 && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
                e.preventDefault();
                setFound((i) => (i + (e.key === "ArrowDown" ? 1 : finds.length - 1)) % finds.length);
              } else if (finds.length > 0 && (e.key === "Tab" || e.key === "Enter")) {
                e.preventDefault();
                const a = finds[Math.min(found, finds.length - 1)];
                if (a) add(a);
              } else if (e.key === "Enter" && !e.nativeEvent.isComposing && (e.metaKey || e.ctrlKey || (!e.shiftKey && fine()))) {
                e.preventDefault();
                // Start (which knows the machines' answer) starts it, as a click would.
                root.current?.querySelector<HTMLButtonElement>(".mc-start")?.click();
              }
            }}
          />
          <div className="cp-bar">
            <input
              ref={picker}
              type="file"
              multiple
              hidden
              onChange={(e) => {
                if (e.target.files) void attachments.add(e.target.files);
                e.target.value = "";
              }}
            />
            <Tip content="Attach files or images">
              <button type="button" className="cp-icon" aria-label="Attach files" onClick={() => picker.current?.click()}>
                <Paperclip size={15} />
                {files.length > 0 && <span className="cp-badge">{files.length}</span>}
              </button>
            </Tip>
            <MicButton field={field} setText={(t) => onText(t)} onNote={setNote} />
            <span className="cp-gap" />
            <React.Suspense
              fallback={
                <button type="button" className="cp-send" data-mode="send" disabled>
                  <ArrowUp size={15} />
                  <span>Start</span>
                </button>
              }
            >
              <StartButton c={c} busy={busy} blocked={pending > 0} run={(s) => void run(s)} />
            </React.Suspense>
          </div>
        </div>
        {finds.length > 0 && (
          <div className="ms-msg-pal">
            <ul className="pl-list" role="listbox" aria-label="Agents to message instead">
              {finds.map((a, i) => (
                <li key={a.id}>
                  <button
                    type="button"
                    role="option"
                    aria-selected={i === Math.min(found, finds.length - 1)}
                    className="pl-item"
                    onPointerDown={(e) => {
                      e.preventDefault();
                      add(a);
                    }}
                    onPointerEnter={() => setFound(i)}
                  >
                    <span className="pl-name">
                      {agentLabel(a)}
                      <span className="pl-note">{doingOf(a)}</span>
                    </span>
                    <span className="pl-desc">
                      {machineName(a.target)} · {a.kind_name}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
      <React.Suspense fallback={null}>
        <Under c={c} more={more} setMore={setMore} past={past} setPast={setPast} onResume={(s, p) => void resume(s, p)} />
      </React.Suspense>
      {(starting || error || result || note) && (
        <div className="ms-msg-line" role="status">
          {starting ? (
            <span className="sg-label" data-live="true">
              {starting}
            </span>
          ) : error ? (
            <span className="mc-error">
              <CircleAlert size={13} aria-hidden="true" />
              {error}
            </span>
          ) : result ? (
            <>
              <span className="ms-msg-result mc-ok">
                <Check size={13} aria-hidden="true" />
                Started {result.n} {result.kind} agents on {result.machine}
              </span>
              {result.first && (
                <button type="button" className="ms-link" onClick={() => navigate(`/agents/${result.first}`)}>
                  Open the first
                </button>
              )}
            </>
          ) : (
            <span className="mc-note">{note}</span>
          )}
        </div>
      )}
    </section>
  );
}
