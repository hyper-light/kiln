/**
 * Mission control's composer with nothing selected: it starts an agent, written like a message to
 * it. Above the box, the agent page's own line (permissions · model · effort, and the bolt where
 * the model goes faster); in the box's top slot, the agent, its machine, its folder and how many,
 * each opened in place; under the box, More options (name, workspace, the Agentfile its microVM is
 * made from, where its commands run, a shell to run in) and the machine's past sessions, each a
 * dropdown that opens in place. Start
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
import { heldCommand, useHeld } from "@/api/held";
import type { Agent, ReachRunner, TerminalSessionInfo } from "@/api/types";
import { Loadable } from "@/components/ui/loading";
import { Tip } from "@/components/ui/tooltip";
import { labelOf, noteLabels, shownName, useLabels, useRes, type Res } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { readLocal, useLocalStorage, useNow } from "@/lib/hooks";
import { sendingProps } from "@/lib/action";
import { useSuspenseResource } from "@/lib/resource";
import { useRouter } from "@tanstack/react-router";
import { Link, useNavigate, useSearchParams } from "@/lib/router";
import { agentfileChoicesRes, choicesFor, chosenFor, projectOf, whyNoMicrovm, withChoice, type AgentfileChoice } from "@/views/agentfiles/choose";
import { atOf, doingOf, findAgents } from "./broadcast";
import { ComposeField } from "./compose-field";
import { useAttachments } from "./controls";
import { agentLabel, modelLabel, tildify } from "./derive";
import { InlinePath } from "./inline-path";
import { usePrefill, type Prefill } from "./new-agent-draft";
import { machineShown } from "./parts";
import { RunSettings, WordRow } from "./run-settings";
import { effortsFor, resumeEffort } from "./spawn-efforts";
import {
  carryEffort,
  carryMode,
  harnessesOf,
  kindOf,
  machineLabel,
  machinesWithAny,
  shortMachineLabel,
  modeOf,
  modelOf,
  modesFor,
  onMachine,
  oneOf,
  reachChoices,
  specOf,
  speedsFor,
  targetOf,
  withDir,
  type Chosen,
} from "./spawn-setup";
import { putBack, showsLine, startAgain, startAgents, useStart, type Start } from "./starting";
import { holdManyWith } from "./state";
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
      (a) => {
        noteLabels(a);
        return a.filter((m) => m.connected && !m.ephemeral);
      },
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

/** The machines' answer, as the composer reads it: Mission control asks for it with its agents, so its page comes whole. */
export const machinesRes: Res<Machines> = { key: "harness/spawn", fetch: askMachines, opts: MACHINES };

const COUNTS = [1, 2, 3, 4, 6, 8, 12];
type Part = "machine" | null;

/** Enter starts with a keyboard and pointer; on a touch screen it's a new line (as in the composer). */
const fine = () => window.matchMedia?.("(pointer: fine)").matches;

/** The choices: those kept across visits (per harness where they're its own), those for this start, and what another page handed over. */
function useChoices() {
  const [kind, setKind] = useLocalStorage("kiln.agents.kind", "claude");
  const [lastTarget, setLastTarget] = useLocalStorage("kiln.agents.target", "");
  // The permissions and effort chosen last, whichever harness: every harness starts at them, carried
  // to its own as an agent's switch carries them. Until they're chosen again, what was kept for the
  // harness used last (each harness kept its own before).
  const [chosenMode, setMode] = useLocalStorage<Chosen | null>("kiln.agents.mode", null);
  const [chosenEffort, setEffort] = useLocalStorage<string | null>("kiln.agents.effort", null);
  const keptMode = readLocal<Record<string, string>>("kiln.agents.modes", {})[kind];
  const keptEffort = readLocal<Record<string, string>>("kiln.agents.efforts", {})[kind];
  const mode = chosenMode ?? (keptMode ? { kind, id: keptMode } : null);
  const effort = chosenEffort ?? keptEffort ?? "";
  /** The agent picked in another harness: what the one used last kept is chosen from now on, so it goes along. */
  const switchTo = (other: string) => {
    if (!chosenMode && mode) setMode(mode);
    if (chosenEffort === null && keptEffort !== undefined) setEffort(keptEffort);
    setKind(other);
  };
  const [modelByKind, setModelByKind] = useLocalStorage<Record<string, string>>("kiln.agents.models", {});
  const [speedByKind, setSpeedByKind] = useLocalStorage<Record<string, string>>("kiln.agents.speeds", {});
  /** `volume`: a Slates volume of its own over the folder, where the machine can make one. */
  const [workspace, setWorkspace] = useLocalStorage<"folder" | "volume">("kiln.agents.workspace", "folder");
  /** The Agentfile each harness's microVM is made from, per project (views/agentfiles/choose.ts); none kept: a process on its machine. */
  const [agentfiles, setAgentfiles] = useLocalStorage<Record<string, string>>("kiln.agents.agentfiles", {});
  const [dirs, setDirs] = useLocalStorage<Record<string, string[]>>("kiln.agents.recentDirs", {});
  const [picked, setPicked] = React.useState<string | null>(null);
  /** The folder typed or browsed to, on the machine it was for. */
  const [folder, setFolder] = React.useState<{ target: string; cwd: string } | null>(null);
  const [count, setCount] = React.useState(1);
  const [name, setName] = React.useState("");
  const [reach, setReach] = React.useState("");
  const [shell, setShell] = React.useState("");
  /** A mode that turns checks off, confirmed (in the line) for this start only. */
  const [unchecked, setUnchecked] = React.useState<Chosen | null>(null);
  const [open, setOpen] = React.useState<Part>(null);
  // What another page handed over (new-agent-draft.ts): this start's choices, each until you change it.
  // Its mode is the harness's it came with (else this one's), whichever the agent then starts in.
  const handed = usePrefill((s) => s.prefill);
  const [seed, setSeed] = React.useState<(Prefill & { modeIn?: string }) | null>(null);
  if (handed && handed.at !== seed?.at) {
    setSeed({ ...handed, modeIn: handed.kind ?? kind });
    setPicked(null);
    setFolder(null);
    setShell(handed.shell ?? "");
    setCount(1);
    setUnchecked(null);
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
    setUnchecked(null);
    setOpen(null);
  };
  return {
    kind,
    switchTo,
    lastTarget,
    setLastTarget,
    mode,
    setMode,
    effort,
    setEffort,
    modelByKind,
    setModelByKind,
    speedByKind,
    setSpeedByKind,
    workspace,
    setWorkspace,
    agentfiles,
    setAgentfiles,
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
  // The machine first; then the agents it runs (the others shown, not to be chosen there).
  const where = machinesWithAny(m.byTarget);
  const target = targetOf(where, [c.picked, c.seed?.target, c.lastTarget]);
  const here = m.byTarget[target] ?? [];
  const kind = kindOf(here.length > 0 ? here : all, c.seed?.kind ?? c.kind);
  const recent = c.dirs[target] ?? [];
  // The folder: typed or browsed to on this machine, else the one handed over, else the last one used here.
  const cwd = (c.folder?.target === target ? c.folder.cwd : undefined) ?? (c.seed?.target === target ? c.seed.cwd : undefined) ?? recent[0] ?? "~";
  const info = onMachine(m.byTarget, target, kind) ?? all.find((x) => x.id === kind);
  const modes = modesFor(info);
  // The permissions chosen (confirmed for this start where they turn checks off; else handed over,
  // else kept) and the effort, as this harness and model name them: a harness switch carries them.
  const offered = modes.map((x) => x.id);
  const carried = (x: Chosen | null) => (x ? carryMode(x.kind, x.id, kind, offered) : undefined);
  const handedMode = c.seed?.mode ? { kind: c.seed.modeIn ?? kind, id: c.seed.mode } : null;
  const mode = modeOf(modes, { unchecked: carried(c.unchecked) ?? "", remembered: carried(handedMode ?? c.mode), fallback: info?.default_mode });
  const model = modelOf(info, c.seed?.model ?? c.modelByKind[kind] ?? "");
  const efforts = effortsFor(info, model.chosen);
  const effort = carryEffort(
    c.seed?.effort ?? c.effort,
    efforts.map((x) => x.id),
  );
  const speeds = speedsFor(info, model.chosen);
  const speed = oneOf(speeds, c.speedByKind[kind]);
  const canVolume = m.workspaces[target]?.available === true;
  // The other harnesses on that machine, by their models: choosing one starts the agent there.
  // The other harnesses the machine runs, by their models (another machine's list where it has none yet; "Default" where none
  // has one): choosing one starts the agent there, as the agent page's model select switches it.
  const others = here.filter((x) => x.available && x.id !== kind).map((x) => ({ kind: x.id, name: x.name, models: onMachine(m.byTarget, target, x.id)?.models ?? [] }));
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
    machine: target ? shortMachineLabel(target, m.machines) : "",
    machineFull: target ? machineLabel(target, m.machines) : "",
    noHarness: !all.some((k) => k.available),
    machines: m.machines,
    reach: m.reach,
    workspaces: m.workspaces,
  };
}
type Setup = ReturnType<typeof resolve>;

function useSetup(c: Choices): Setup {
  // Machines are shown by their labels: their words change as those do.
  useLabels();
  return resolve(c, useRes(machinesRes).data);
}

/**
 * The Agentfile its microVM is made from, as chosen for its harness in its project (none: a process
 * on its machine), what there is to choose, and why its machine would refuse it, where it would
 * (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 31).
 */
function useAgentfile(c: Choices, s: Setup) {
  const choices = choicesFor(useRes(agentfileChoicesRes).data, s.kind);
  const project = projectOf(s.cwd, c.shell);
  const chosen = chosenFor(c.agentfiles, choices, s.kind, project);
  return { choices, project, chosen, why: chosen && s.target ? whyNoMicrovm(s.target, s.machines, s.machine) : null };
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
          c.setUnchecked({ kind: s.kind, id: m });
          return;
        }
        c.setUnchecked(null);
        c.setMode({ kind: s.kind, id: m });
        c.drop("mode");
      }}
      models={s.info?.models ?? []}
      model={s.model.shown}
      onModel={(m, other) => {
        if (other && other !== s.kind) {
          c.switchTo(other);
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
        c.setEffort(e);
        c.drop("effort");
      }}
      speeds={s.speeds}
      speed={s.speed}
      onSpeed={(v) => c.setSpeedByKind({ ...c.speedByKind, [s.kind]: v })}
      switchWords={(name) => `Starts in ${name} instead`}
    />
  );
}

/** Start. `sending`: several are starting from here: it says so, and a press waits (a second would start more). */
function StartButton({ c, sending, run }: { c: Choices; sending: boolean; run: (s: Setup, agentfile: AgentfileChoice | null) => void }) {
  const s = useSetup(c);
  const { chosen } = useAgentfile(c, s);
  const label = c.count > 1 ? `Start ${c.count}` : "Start";
  return (
    <button
      type="button"
      className="cp-send mc-start"
      data-mode="send"
      disabled={!s.target || s.noHarness}
      {...sendingProps(sending)}
      onClick={() => {
        if (!sending) run(s, chosen);
      }}
      aria-label={`${label}: ${s.kindName} on ${s.machine}${chosen ? `, in a microVM from ${chosen.name}` : ""}`}
    >
      <ArrowUp size={15} />
      <span>{label}</span>
    </button>
  );
}

/** A choice of a few, as a word with a chevron over the system's own picker; those with a `group` under its name, as the picker lists them. */
function Pick({ label, value, onChange, options }: { label: string; value: string; onChange: (v: string) => void; options: { value: string; label: string; group?: string }[] }) {
  const option = (o: { value: string; label: string }) => (
    <option key={o.value} value={o.value}>
      {o.label}
    </option>
  );
  const groups = [...new Set(options.map((o) => o.group))];
  return (
    <label className="mc-pick">
      <span>{options.find((o) => o.value === value)?.label ?? options[0]?.label}</span>
      <ChevronDown size={12} className="mc-chev" aria-hidden="true" />
      <select aria-label={label} value={value} onChange={(e) => onChange(e.target.value)}>
        {groups.map((g) =>
          g === undefined ? (
            <React.Fragment key="">{options.filter((o) => o.group === undefined).map(option)}</React.Fragment>
          ) : (
            <optgroup key={g} label={g}>
              {options.filter((o) => o.group === g).map(option)}
            </optgroup>
          ),
        )}
      </select>
    </label>
  );
}

/**
 * The Agentfile its microVM is made from, kept for this harness in this project (kiln plan
 * 01a1cd8e862e7169a074db3d5fa33f3d, step 31): kiln's for the harness, or one of yours; none, a
 * process on its machine. Where its machine would refuse it, it says so under the row.
 */
function AgentfileRow({ c, s }: { c: Choices; s: Setup }) {
  const af = useAgentfile(c, s);
  return (
    <>
      <div className="mc-opt mc-opt-agentfile">
        <span className="mc-opt-key">Agentfile</span>
        <div className="mc-opt-val">
          <Pick
            label={`The Agentfile ${s.kindName}'s microVM is made from`}
            value={af.chosen?.image ?? ""}
            onChange={(v) => c.setAgentfiles(withChoice(c.agentfiles, s.kind, af.project, v || null))}
            options={[
              { value: "", label: "None: a process on its machine" },
              ...af.choices.map((x) => ({ value: x.image, label: x.whose === "kiln" ? `${x.name} (kiln's)` : x.name, group: x.whose === "kiln" ? "kiln's" : "Yours" })),
            ]}
          />
          <Link to="/agents/agentfiles" className="ms-link">
            Agentfiles
          </Link>
        </div>
      </div>
      {af.why && <p className="mc-note mc-opt-panel">{af.why}. An agent from an Agentfile starts only on a machine that starts microVMs.</p>}
    </>
  );
}

/** A row's value that opens in place under it (the machine). */
function Opens({ c, part, text, title }: { c: Choices; part: Exclude<Part, null>; text: string; title?: string }) {
  return (
    <button type="button" className="mc-pick" aria-expanded={c.open === part} title={title} onClick={() => c.setOpen(c.open === part ? null : part)}>
      <span>{text}</span>
      <ChevronDown size={12} className="mc-chev" aria-hidden="true" />
    </button>
  );
}

/**
 * The options, open: one quiet row each. Where it starts first
 * (the machine, the folder, how many), then the rest. The agent is picked as the agent page switches
 * one: by a model of its in the model select above the box.
 */
function Options({ c, s, past, setPast, onResume }: { c: Choices; s: Setup; past: boolean; setPast: (o: boolean) => void; onResume: (s: Setup, p: PastSession) => void }) {
  const shell = c.shell ? s.shells.find((x) => x.id === c.shell) : undefined;
  return (
    <div className="mc-opts">
      <div className="mc-opt">
        <span className="mc-opt-key">Machine</span>
        <div className="mc-opt-val">
          <Opens c={c} part="machine" text={s.machine || "None runs agents"} title={s.machineFull} />
        </div>
      </div>
      {c.open === "machine" && (
        <div className="rs-panel mc-opt-panel" role="group" aria-label="Machine">
          <WordRow
            label="Machine"
            words={s.where.map((t) => ({ id: t, text: shortMachineLabel(t, s.machines), full: machineLabel(t, s.machines) }))}
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
      <div className="mc-opt mc-opt-folder">
        <span className="mc-opt-key">Folder</span>
        <div className="mc-opt-val">
          {c.shell ? (
            <button type="button" className="mc-pick" title="It starts where the shell is: tap for a folder instead" onClick={() => c.setShell("")}>
              <span>Shell: {shell?.title || "this shell"}</span>
            </button>
          ) : s.target ? (
            <InlinePath target={s.target} value={s.cwd} onChange={(v) => c.setFolder({ target: s.target, cwd: v })} recent={s.recent} label="Folder" />
          ) : (
            <span className="mc-none">—</span>
          )}
        </div>
      </div>
      <div className="mc-opt">
        <span className="mc-opt-key">How many</span>
        <div className="mc-opt-val">
          <div className="mc-seg" role="radiogroup" aria-label="How many">
            {COUNTS.map((n) => (
              <button
                key={n}
                type="button"
                role="radio"
                aria-checked={c.count === n}
                title={n === 1 ? "One agent" : `Each gets this message, named ${s.kindName} 1–${n}`}
                onClick={() => c.setCount(n)}
              >
                {n}
              </button>
            ))}
          </div>
        </div>
      </div>
      <div className="mc-opt">
        <span className="mc-opt-key">Name</span>
        <div className="mc-opt-val">
          <input className="mc-inline-input" value={c.name} onChange={(e) => c.setName(e.target.value)} placeholder={c.count > 1 ? "fix-tests (numbered)" : "Optional"} aria-label="Name" />
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
      <AgentfileRow c={c} s={s} />
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
      {s.target && (
        <div className="mc-opt">
          <span className="mc-opt-key">Continue</span>
          <div className="mc-opt-val">
            <button type="button" className="mc-pick" aria-expanded={past} onClick={() => setPast(!past)}>
              <span>A past session on {s.machine}</span>
              <ChevronDown size={12} className="mc-chev" aria-hidden="true" />
            </button>
          </div>
        </div>
      )}
      {past && s.target && (
        <div className="ps-body mc-past mc-opt-panel">
          <Loadable label="Loading past sessions" fallback={<p className="ps-none">Looking…</p>}>
            <PastList target={s.target} kind={s.kind} kindName={s.kindName} onPick={(p) => onResume(s, p)} />
          </Loadable>
        </div>
      )}
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
  const { chosen } = useAgentfile(c, s);
  // What More options holds that isn't as it starts out, said while it's closed.
  const summary = [
    c.name.trim() && `“${c.name.trim()}”`,
    s.volume && "Own volume",
    chosen && `microVM: ${chosen.name}`,
    c.reach && (c.reach === "off" ? "Only its machine" : `Prefers ${labelOf(c.reach)}`),
  ]
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
      </div>
      {more && <Options c={c} s={s} past={past} setPast={setPast} onResume={onResume} />}
      <MachinesNote />
    </>
  );
}

/**
 * What's in the way on the machines themselves: runners too old to run agents, or none with a coding
 * agent. Under the composer as it starts an agent and as it messages those selected (mission.tsx),
 * so it stays where it is as the composer turns.
 */
export function MachinesNote() {
  const m = useRes(machinesRes).data;
  // Machines are shown by their labels (docs/machines.md).
  useLabels();
  // Connected machines whose runner never reported its harnesses: too old to run agents.
  const stale = m.machines.filter((x) => !(`agent:${x.name}` in m.byTarget));
  if (stale.length > 0)
    return (
      <p className="mc-note">
        <b>{stale.map(shownName).join(", ")}</b> {stale.length === 1 ? "is" : "are"} connected, but {stale.length === 1 ? "its kiln runner is" : "their kiln runners are"} too old to run agents.
        Upgrade without a restart (shells keep running) by running this on {stale.length === 1 ? "that machine" : "each machine"} with the new kiln binary: <code>kiln install --agent --upgrade</code>
      </p>
    );
  return harnessesOf(m.byTarget).some((k) => k.available) ? null : (
    <p className="mc-note">No connected machine has a coding agent installed. Install Claude Code or Codex on any machine running kiln, then try again.</p>
  );
}

const LineLoading = () => (
  <div className="rs-tuner ms-tuner mc-how" aria-hidden="true">
    <div className="rs-top">
      <p className="rs-line" />
    </div>
  </div>
);

/** More options before the machines have answered: there, turned off, so nothing under it moves when they do. */
const UnderLoading = () => (
  <div className="mc-under">
    <button type="button" className="mc-toggle" disabled>
      More options
      <ChevronDown size={12} className="mc-chev" aria-hidden="true" />
    </button>
  </div>
);

const NO_FIELD: React.RefObject<HTMLTextAreaElement | null> = { current: null };
const IGNORE = () => undefined;

/**
 * The composer before Mission control has its agents (its page's skeleton, mission.tsx): its own
 * parts with what's written in it, none of them usable yet, so nothing moves when it comes.
 */
export function NewAgentLoading({ text }: { text: string }) {
  return (
    <section className="ms-msg cp mc" aria-hidden="true" inert>
      <LineLoading />
      <div className="ms-msg-field">
        <div className="hx-compose-box">
          <ComposeField text={text} readOnly placeholder="What should it do?" />
          <div className="cp-bar">
            <button type="button" className="cp-icon" disabled>
              <Paperclip size={15} />
            </button>
            <MicButton field={NO_FIELD} setText={IGNORE} onNote={IGNORE} disabled />
            <span className="cp-gap" />
            <button type="button" className="cp-send" data-mode="send" disabled>
              <ArrowUp size={15} />
              <span>Start</span>
            </button>
          </div>
        </div>
      </div>
      <UnderLoading />
    </section>
  );
}

/** How several agents started at once go (starting.ts), under the box: their first one opens at once, starting or not. */
function ManyLine({ start, onOpen }: { start: Start; onOpen: (id: string) => void }) {
  const first = start.started?.[0]?.id ?? start.ids[0];
  const what = `${start.ids.length} ${start.kindName} agents on ${start.machine}`;
  if (start.state === "failed") {
    const some = start.started?.length ?? 0;
    return (
      <>
        <span className="mc-error">
          <CircleAlert size={13} aria-hidden="true" />
          {some > 0 ? `${some} of ${what} started; the rest couldn't` : `Couldn't start ${what}`}: {start.why}
        </span>
        <button type="button" className="ms-link" onClick={() => start.ids[0] && startAgain(start.ids[0])}>
          Try again
        </button>
        <button type="button" className="ms-link" onClick={() => putBack(start)}>
          Put it back
        </button>
      </>
    );
  }
  return (
    <>
      {start.state === "starting" ? (
        <span className="sg-label" data-live="true">
          Starting {what}
        </span>
      ) : start.why ? (
        <span className="mc-error">
          <CircleAlert size={13} aria-hidden="true" />
          {start.why}
        </span>
      ) : (
        <span className="ms-msg-result mc-ok">
          <Check size={13} aria-hidden="true" />
          Started {what}
        </span>
      )}
      {first && (
        <button type="button" className="ms-link" onClick={() => onOpen(first)}>
          Open the first
        </button>
      )}
    </>
  );
}

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
  useLabels();
  const navigate = useNavigate();
  const router = useRouter();
  const [params, setParams] = useSearchParams();
  // ?new=1 (the command palette, an entry point that filled this in): the field takes the focus once.
  const fromLink = params.get("new") === "1";
  const linked = React.useRef(false);
  const attachments = useAttachments("any");
  const { files } = attachments;
  const picker = React.useRef<HTMLInputElement>(null);
  const field = React.useRef<HTMLTextAreaElement | null>(null);
  const root = React.useRef<HTMLElement>(null);
  const [note, setNote] = React.useState<string | null>(null);
  const [more, setMore] = React.useState(false);
  const [past, setPast] = React.useState(false);
  const [focused, setFocused] = React.useState(false);
  const [found, setFound] = React.useState(0);
  // Several started at once stay here: the line under the box says how they go (the first one's id).
  const [many, setMany] = React.useState<string | null>(null);
  const manyStart = useStart(many ?? "");

  /** Opens agent `id`'s page at once, as one view transition: it shows the agent starting until its machine has it (W21-R15). */
  const open = (id: string) => {
    // Only while it opens do its parts carry the names the transition moves them by (mission.css, console.css).
    const html = document.documentElement;
    html.dataset.opening = "";
    navigate(`/agents/${id}`, { viewTransition: true });
    window.setTimeout(() => delete html.dataset.opening, 800);
  };

  // Start doesn't wait for the server (starting.ts): the agents are listed starting at once, under
  // ids made here, and one opens at once.
  const run = async (s: Setup, agentfile: AgentfileChoice | null) => {
    if (!s.target) return;
    const body = text.trim();
    // Files go with the first message (those still uploading too: Start doesn't wait for them), so
    // then the words go with them rather than in the spec.
    const taken = attachments.take();
    const sending = taken.files.length > 0 || taken.uploading.length > 0 ? taken : undefined;
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
      agentfile: agentfile?.image,
    });
    const ids = Array.from({ length: c.count }, () => clientId());
    // One agent: its files are kept for it, and its page sends them once it can take them (and
    // they're up). Several: sent to them all once they've started and the files are up.
    const only = ids.length === 1 ? ids[0] : undefined;
    const heldFirst = sending && only ? clientId() : undefined;
    const heldIds = ids.map((id) => ({ agent: id, message: only && heldFirst ? heldFirst : clientId() }));
    if (sending) {
      if (
        !(await holdManyWith(
          heldIds.map((item) => ({ agent: item.agent, message: { id: item.message, text: body, at: Date.now(), mode: "queue" } })),
          sending,
        ))
      )
        return;
    }
    // Once they've all started, the machine and folder are the next start's, and several agents'
    // files go to them all; a start that couldn't changes nothing kept.
    const dirs = c.shell ? undefined : withDir(c.dirs, s.target, s.cwd);
    const { setLastTarget, setDirs } = c;
    const after = () => {
      setLastTarget(s.target);
      if (dirs) setDirs(dirs);
      if (sending)
        return sending.up.then(async () => {
          await useHeld.getState().ready();
          for (const item of heldIds) {
            const original = useHeld.getState().held[item.agent]?.find((message) => message.id === item.message);
            if (original && !original.uploading?.length) await sessionStore(item.agent).sendMail(heldCommand(original), original.originals);
          }
        });
    };
    startAgents({
      ids,
      target: s.target,
      spec,
      kindName: s.kindName,
      machine: s.machine,
      text: body,
      files: sending ? [...sending.files.map((f) => f.name), ...sending.uploading] : [],
      heldFirst,
      after,
    });
    onText("");
    c.reset();
    setMore(false);
    if (only) {
      setMany(null);
      open(only);
      return;
    }
    setMany(ids[0] ?? null);
  };

  /** Pick up a past conversation where it ended: same harness, folder and history, its page open at once. */
  const resume = (s: Setup, p: PastSession) => {
    const id = clientId();
    const dirs = withDir(c.dirs, s.target, p.cwd);
    startAgents({
      ids: [id],
      target: s.target,
      spec: { kind: p.kind, cwd: p.cwd || undefined, resume: p.id, mode: s.mode || undefined, effort: resumeEffort(s.info, p.model, s.effort), name: (p.title || "").slice(0, 80) || undefined },
      kindName: s.kindName,
      machine: s.machine,
      text: "",
      files: [],
      after: () => c.setDirs(dirs),
    });
    open(id);
  };

  // @ and part of a name finds agents: picking one messages it instead (the composer turns to it).
  const at = focused ? atOf(text) : undefined;
  const finds = at === undefined ? [] : findAgents(all, at, new Set());
  const add = (a: AgentInfo) => {
    onText((t) => t.replace(/(^|\s)@[^\s@]*$/, "$1"));
    setFound(0);
    onPick([a.id], true);
  };

  // ?new=1's focus, given from the box: React attaches the field inside it first, so `field` holds it.
  const attach = (box: HTMLDivElement | null) => {
    if (!fromLink) linked.current = false;
    if (!box || !fromLink || linked.current) return;
    linked.current = true;
    field.current?.focus();
    const n = new URLSearchParams(params);
    n.delete("new");
    setParams(n, { replace: true });
  };

  return (
    <section ref={root} className="ms-msg cp mc" aria-label="New agent">
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
        <div ref={attach} className="hx-compose-box">
          {attachments.chips}
          {/* The agent page's field: it grows with the text, and what's typed is marked as it is there. */}
          <ComposeField
            text={text}
            field={field}
            autoFocus={autoFocus}
            placeholder={c.count > 1 ? `What should these ${c.count} agents do? Each gets this message` : "What should it do?"}
            aria-label="Its first message"
            enterKeyHint="send"
            autoCapitalize="sentences"
            onFocus={() => {
              setFocused(true);
              // Its page's code, fetched while the message is written: Start's page opens without waiting for it.
              void router.preloadRoute({ to: "/agents/$id", params: { id: "new" } }).catch(() => undefined);
            }}
            onBlur={() => setFocused(false)}
            onPaste={(e) => {
              if (e.clipboardData.files.length === 0) return;
              e.preventDefault();
              void attachments.add(e.clipboardData.files);
            }}
            onChange={(e) => {
              onText(e.target.value);
              setFound(0);
              // A new message: the line under the box lets go of the last start once it's done
              // (one that couldn't, or only partly, stays to be tried again or put back).
              if (manyStart?.state === "started" && !manyStart.why) setMany(null);
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
              <StartButton c={c} sending={manyStart?.state === "starting"} run={run} />
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
                      {machineShown(a.target)} · {a.kind_name}
                    </span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
      <React.Suspense fallback={<UnderLoading />}>
        <Under c={c} more={more} setMore={setMore} past={past} setPast={setPast} onResume={resume} />
      </React.Suspense>
      {(manyStart || note) && (
        <div className="ms-msg-line" role="status" ref={manyStart ? showsLine(manyStart.ids[0] ?? "") : undefined}>
          {manyStart ? <ManyLine start={manyStart} onOpen={(id) => navigate(`/agents/${id}`)} /> : <span className="mc-note">{note}</span>}
        </div>
      )}
    </section>
  );
}
