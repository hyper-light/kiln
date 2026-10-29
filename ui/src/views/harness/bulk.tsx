/**
 * What can be done to the agents selected in Mission control, all of them at once: one agent's own
 * actions (its row's Resume and Restart, the goals panel's Pause, its menu), in the list's head on
 * one line with the count: an Actions menu, as a row's own (each with its icon, turned off where none
 * of those selected takes it, saying how many it's for when not all), and Clear, which ends selecting.
 * Moving, ending and where their commands run are asked in a line under it; how it went is said there
 * too, and opens to each agent's outcome. The line is the same from the moment selecting starts until
 * it ends, so picking more moves nothing (and a touch screen keeps the head in view while selecting).
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { ArrowRightLeft, Bell, BellOff, Check, ChevronDown, ChevronRight, Minus, Pause, Play, Power, RotateCcw, Route, Square, Trash, X, type LucideIcon } from "lucide-react";
import { clientId, harnessApi, type AgentInfo, type Command } from "@/api/harness";
import { pushApi, pushStateRes } from "@/api/push";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate, preload, useSuspenseResource } from "@/lib/resource";
import { Tries } from "@/lib/tries";
import { cn } from "@/lib/utils";
import { actsOf, count, outcomeLine, placesFor, resumable, startedBy, topmost, whyNot, type Outcome, type Place, type Places, type Stacks } from "./bulk-acts";
import { agentLabel } from "./derive";
import { askPlaces, inWords } from "./move";
import { machineName } from "./parts";
import { reachChoices } from "./spawn-setup";

/** What came of an action: its line, each agent's outcome. */
interface Result {
  line: string;
  outcomes: Outcome[];
}
/** Does an action, saying what it's at meanwhile (`doing`), then how it went. */
type Acting = (doing: string, go: () => Promise<Result>) => void;

const outcome = (a: AgentInfo, result: Outcome["result"], reason?: string): Outcome => ({ id: a.id, name: agentLabel(a), machine: machineName(a.target), result, reason });
/** The outcomes, with their line: `done` says what was done to how many. */
const told = (done: (agents: string) => string, outcomes: Outcome[]): Result => ({ line: outcomeLine(done, outcomes), outcomes });
const busyWith = (a: AgentInfo) => a.status === "running" || a.status === "waiting";

/** One command to each of `agents`, in one request (`/api/harness/broadcast`): how it went for each. */
async function commandEach(agents: AgentInfo[], command: Command): Promise<Outcome[]> {
  const report = await harnessApi.broadcastTo({ to: { ids: agents.map((a) => a.id) }, command, key: clientId() });
  return agents.map((a) => {
    const r = report.results.find((x) => x.id === a.id);
    return r ? outcome(a, r.result === "sent" ? "done" : r.result, r.reason) : outcome(a, "failed", "kiln couldn't reach it");
  });
}

/** `f` for each of `agents`, all at once: how it went for each. */
async function each(agents: AgentInfo[], f: (a: AgentInfo) => Promise<unknown>): Promise<Outcome[]> {
  const settled = await Promise.allSettled(agents.map(f));
  return agents.map((a, i) => {
    const s = settled[i];
    return s?.status === "fulfilled" ? outcome(a, "done") : outcome(a, "failed", errorMessage(s?.reason));
  });
}

/** Each agent's goal stack (null for one that couldn't be read). */
const readStacks = (ids: string[]): Promise<Stacks> => Promise.all(ids.map((id) => harnessApi.goals(id).catch(() => null))).then((s) => new Map(ids.map((id, i) => [id, s[i] ?? null])));

/** The selected agents' goal stacks, read again every few seconds while the menu shows them. */
const STACKS = { pollMs: 5000 };
const stacksKey = (ids: string) => `harness/selected-goals/${ids}`;
function Stacked({ ids, children }: { ids: string; children: (stacks: Stacks) => React.ReactNode }) {
  return children(useSuspenseResource(stacksKey(ids), () => readStacks(ids.split(",")), STACKS).data);
}

/** One of the menu's actions: its icon and words, how many of those selected it's for when not all; turned off when it's for none. */
function Act({ icon: Icon, label, k, n, danger, disabled, onSelect }: { icon: LucideIcon; label: string; k: number; n: number; danger?: boolean; disabled: boolean; onSelect: () => void }) {
  return (
    <DropdownMenu.Item className="menu-item" disabled={disabled || k === 0} data-danger={danger || undefined} onSelect={onSelect}>
      <Icon size={14} aria-hidden="true" />
      <span className="flex-1">{label}</span>
      {k > 0 && k < n && <span className="ms-menu-n">{`${k} of ${n}`}</span>}
    </DropdownMenu.Item>
  );
}

/**
 * A line under the head that slides open and shut (its height from nothing and back, as the About
 * card's does), so the list under it moves smoothly rather than jumps; shut, it's out of reach, and
 * `onGone` says it's done sliding (a moment later where motion is paused and nothing slides).
 */
function Slide({ open, onGone, children }: { open: boolean; onGone?: () => void; children: React.ReactNode }) {
  const gone = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el || !onGone) return;
      const t = window.setTimeout(onGone, 400);
      return () => window.clearTimeout(t);
    },
    [onGone],
  );
  return (
    <div
      className="ms-slide"
      data-closing={!open || undefined}
      inert={!open}
      ref={!open ? gone : undefined}
      onTransitionEnd={(e) => {
        if (!open && e.target === e.currentTarget && e.propertyName === "grid-template-rows") onGone?.();
      }}
    >
      <div className="ms-slide-clip">{children}</div>
    </div>
  );
}

/**
 * `open`: agents are being selected (by touch, from Select or a row held, even before the first is
 * picked): the line is there, so the first one picked moves nothing. `onClear` ends selecting. `idle`:
 * what the head's end shows while not selecting (by touch, Select), crossfading with Actions and Clear.
 */
export function BulkBar({ agents, all, open, idle, onGone, onClear }: { agents: AgentInfo[]; all: AgentInfo[]; open: boolean; idle?: React.ReactNode; onGone: () => void; onClear: () => void }) {
  const [ask, setAsk] = React.useState<"move" | "end" | "reach" | null>(null);
  // The ask on show: the one asked, or the last, while it slides shut.
  const [shownAsk, setShownAsk] = React.useState<"move" | "end" | "reach" | null>(null);
  const [busy, setBusy] = React.useState<string | null>(null);
  const [said, setSaid] = React.useState<Result | null>(null);
  // How it went, put away: it slides shut, then goes.
  const [saidGoing, setSaidGoing] = React.useState(false);
  const [outcomes, setOutcomes] = React.useState(false);
  const askGone = React.useCallback(() => setShownAsk(null), []);
  const saidGone = React.useCallback(() => {
    setSaid(null);
    setSaidGoing(false);
  }, []);
  const [keepSet, setKeepSet] = React.useState<{ key: string; on: boolean } | null>(null);
  // Goals resumed or paused again after a try that didn't come back: under the same keys (CP-T3.2).
  const [tries] = React.useState(() => new Tries(clientId));
  // Whatever order they were picked in: the same agents read their goal stacks under the same key.
  const key = agents
    .map((a) => a.id)
    .sort()
    .join(",");
  // Their goal stacks are read for the agents picked a moment ago, so picking more doesn't blink, and as
  // soon as they're picked: the menu has them when it opens.
  const stacksOf = React.useDeferredValue(key);
  if (stacksOf) preload(stacksKey(stacksOf), () => readStacks(stacksOf.split(",")), STACKS);
  // The machines they could move to, read as selecting starts: Move's line opens with them, at its own height.
  if (open) preload("harness/move", askPlaces);
  const n = agents.length;
  const acts = actsOf(agents);
  const disabled = busy !== null;
  // What's asked stays open as others are picked, said for them; once none of them takes it, it's put
  // away (and doesn't come back by itself). How the last action went stays, picking others or not,
  // until it's put away or another one goes.
  const asking = ask === "move" && acts.movable.length > 0 ? "move" : ask === "end" && acts.live.length > 0 ? "end" : ask === "reach" && acts.onRunner.length > 0 ? "reach" : null;
  if (ask !== asking) setAsk(asking);
  if (asking !== null && asking !== shownAsk) setShownAsk(asking);

  const run: Acting = (doing, go) => {
    setBusy(doing);
    setSaidGoing(false);
    setAsk(null);
    setOutcomes(false);
    void go()
      .then(
        (r) => setSaid(r),
        (e: unknown) => setSaid({ line: errorMessage(e), outcomes: [] }),
      )
      .finally(() => setBusy(null));
  };
  const goals = async (action: "start" | "pause", targets: AgentInfo[]) => {
    const asked = tries.keyFor(JSON.stringify({ action, ids: targets.map((a) => a.id) }));
    const outcomes = await each(targets, (a) => harnessApi.goalAction(a.id, action, undefined, `${asked}:${a.id}`));
    if (outcomes.every((o) => o.result === "done")) tries.done();
    invalidate("harness/selected-goals/");
    invalidate("harness/goals/");
    return outcomes;
  };
  const resume = (stacks: Stacks | undefined) => {
    const of = actsOf(agents, stacks);
    const targets = resumable(of);
    run(`Resuming ${count(targets.length, "agent")}…`, async () => {
      const [limits, stacks] = await Promise.all([of.atLimit.length > 0 ? commandEach(of.atLimit, { type: "resume" }) : [], of.goalsPaused.length > 0 ? goals("start", of.goalsPaused) : []]);
      // At a limit with its goals paused too: resumed once both went.
      const byId = new Map<string, Outcome>();
      for (const o of [...limits, ...stacks]) {
        const was = byId.get(o.id);
        byId.set(o.id, was && was.result !== "done" ? was : o);
      }
      return told(
        (k) => `Resumed ${k}`,
        targets.flatMap((a) => byId.get(a.id) ?? []),
      );
    });
  };
  const pause = (stacks: Stacks | undefined) => {
    const targets = actsOf(agents, stacks).goalsRunning;
    run(`Pausing the goals of ${count(targets.length, "agent")}…`, async () => told((k) => `Paused the goals of ${k}: no next goal starts until you resume them`, await goals("pause", targets)));
  };
  const stop = () => run(`Stopping ${count(acts.working.length, "turn")}…`, async () => told((k) => `Stopped the turns of ${k}`, await commandEach(acts.working, { type: "interrupt" })));
  const restart = () => run(`Restarting ${count(acts.exited.length, "agent")}…`, async () => told((k) => `Restarting ${k}`, await commandEach(acts.exited, { type: "restart" })));
  const remove = () =>
    run(`Removing ${count(acts.exited.length, "agent")}…`, async () => {
      const outcomes = await each(acts.exited, (a) => harnessApi.stop(a.id));
      if (acts.exited.length === n && outcomes.every((o) => o.result === "done")) onGone();
      return told((k) => `Removed ${k}`, outcomes);
    });
  const move = (place: Place, plan: Places, now: boolean) => {
    const going = place.takes;
    run(`Moving ${count(going.length, "agent")} to ${place.name}…`, async () => {
      const moved = await each(going, (a) => harnessApi.move(a.id, place.target, now));
      const stay = acts.movable.filter((a) => !going.some((g) => g.id === a.id)).map((a) => outcome(a, "skipped", whyNot(a, place, plan)));
      const later = !now && going.some(busyWith);
      return told((k) => `Moving ${k} to ${place.name}${later ? ", those at work once their turn is over" : ""}`, [...moved, ...stay]);
    });
  };
  const end = (withKids: boolean) => {
    const targets = acts.live;
    run(`Ending ${count(targets.length, "agent")}…`, async () => {
      const ids = targets.map((a) => a.id);
      // With the agents they started, one that another of them started ends with that one.
      const roots = withKids ? topmost(ids, all) : ids;
      const settled = await Promise.allSettled(roots.map((id) => harnessApi.end(id, withKids)));
      const ended = new Set(settled.flatMap((s) => (s.status === "fulfilled" ? s.value.ended : [])));
      const why = new Map(
        roots.map((id, i) => {
          const s = settled[i];
          return [id, s?.status === "rejected" ? errorMessage(s.reason) : "it didn't end"] as const;
        }),
      );
      const outcomes = targets.map((a) => (ended.has(a.id) ? outcome(a, "done") : outcome(a, "failed", why.get(a.id) ?? "it didn't end with the agent that started it")));
      const along = [...ended].filter((id) => !ids.includes(id)).length;
      if (agents.every((a) => ended.has(a.id))) onGone();
      return told((k) => `Ended ${k}${along > 0 ? ` and ${count(along, "agent")} they started` : ""}`, outcomes);
    });
  };
  const reach = (value: string, targets: AgentInfo[], skipped: Outcome[]) =>
    run(`Changing where ${count(targets.length, "agent")} run commands…`, async () =>
      told((k) => `Changed where ${k} run commands: each starts again with it once its turn is over`, [...(await commandEach(targets, { type: "control", id: "reach", value })), ...skipped]),
    );
  // Kept going unless switched off: shown as asked at once (as one agent's menu does), until other agents are picked.
  const keeping = keepSet?.key === key ? keepSet.on : agents.every((a) => !a.keep_off) ? true : agents.every((a) => a.keep_off) ? false : null;
  const keep = (on: boolean) => {
    setKeepSet({ key, on });
    run(`${on ? "Keeping" : "No longer keeping"} ${count(n, "agent")} going…`, async () =>
      told((k) => `${on ? "Keeping" : "No longer keeping"} ${k} going when they stop part-way`, await commandEach(agents, { type: "control", id: "keep_going", value: on })),
    );
  };

  /** Resume and Pause: what they take turns on the goal stacks, read once the agents are picked. */
  const goalItems = (stacks?: Stacks) => {
    const of = actsOf(agents, stacks);
    return (
      <>
        <Act icon={Play} label="Resume" k={resumable(of).length} n={n} disabled={disabled} onSelect={() => resume(stacks)} />
        <Act icon={Pause} label="Pause" k={of.goalsRunning.length} n={n} disabled={disabled} onSelect={() => pause(stacks)} />
      </>
    );
  };

  /** The ask on show, while asked and as it slides shut. */
  const askLine = (k: "move" | "end" | "reach") =>
    k === "move" ? (
      <div className="ms-bulk-ask" role="group" aria-label="Move the selected agents">
        <Loadable label="Loading your machines" fallback={<span className="ms-bulk-note">Looking at your machines…</span>}>
          <MoveAsk agents={acts.movable} disabled={disabled} onMove={move} onCancel={() => setAsk(null)} />
        </Loadable>
      </div>
    ) : k === "end" ? (
      <div className="ms-bulk-ask" role="group" aria-label="End the selected agents">
        <EndAsk agents={acts.live} all={all} disabled={disabled} onEnd={end} onCancel={() => setAsk(null)} />
      </div>
    ) : (
      <div className="ms-bulk-ask" role="group" aria-label="Where the selected agents' commands run">
        <Loadable label="Loading your machines" fallback={<span className="ms-bulk-note">Looking at your machines…</span>}>
          <ReachPick agents={acts.onRunner} disabled={disabled} onChange={reach} onCancel={() => setAsk(null)} />
        </Loadable>
      </div>
    );

  return (
    <>
      {/* The head's end: Select (by touch) and, selecting, Actions and Clear, one over the other, crossfading in place. */}
      <span className="ms-head-end">
        {idle && (
          <span className="ms-head-idle" data-shown={!open || undefined} inert={open} aria-hidden={open || undefined}>
            {idle}
          </span>
        )}
        <div
          className="ms-bulk"
          data-shown={open || undefined}
          inert={!open}
          aria-hidden={!open || undefined}
          role="group"
          aria-label={n > 0 ? `What to do with the ${count(n, "selected agent")}` : "What to do with the agents you select"}
        >
          <DropdownMenu.Root>
            <DropdownMenu.Trigger asChild>
              <Button size="xs" variant="secondary" disabled={disabled || n === 0}>
                Actions
                <ChevronDown aria-hidden="true" />
              </Button>
            </DropdownMenu.Trigger>
            <DropdownMenu.Portal>
              <DropdownMenu.Content className="popover ms-bulk-menu" sideOffset={6} align="end" collisionPadding={12}>
                {stacksOf ? (
                  <React.Suspense fallback={goalItems()}>
                    <Stacked ids={stacksOf}>{goalItems}</Stacked>
                  </React.Suspense>
                ) : (
                  goalItems()
                )}
                <Act icon={ArrowRightLeft} label="Move to…" k={acts.movable.length} n={n} disabled={disabled} onSelect={() => setAsk("move")} />
                <Act icon={Square} label="Stop their turns" k={acts.working.length} n={n} disabled={disabled} onSelect={stop} />
                <Act icon={RotateCcw} label="Restart" k={acts.exited.length} n={n} disabled={disabled} onSelect={restart} />
                <DropdownMenu.Separator className="my-1 h-px bg-white/10" />
                <Act icon={Route} label="Where their commands run…" k={acts.onRunner.length} n={n} disabled={disabled} onSelect={() => setAsk("reach")} />
                <DropdownMenu.CheckboxItem
                  className="menu-item"
                  checked={keeping === null ? "indeterminate" : keeping}
                  disabled={disabled}
                  onSelect={(e) => e.preventDefault()}
                  onCheckedChange={(on) => keep(on === true)}
                >
                  <span className="ms-menu-mark">
                    <DropdownMenu.ItemIndicator>{keeping === null ? <Minus size={14} aria-hidden="true" /> : <Check size={14} aria-hidden="true" />}</DropdownMenu.ItemIndicator>
                  </span>
                  <span className="flex-1">Keep going when they stop part-way</span>
                </DropdownMenu.CheckboxItem>
                <React.Suspense fallback={null}>
                  <MuteItem agents={agents} disabled={disabled} run={run} />
                </React.Suspense>
                <DropdownMenu.Separator className="my-1 h-px bg-white/10" />
                <Act icon={Power} label="End…" k={acts.live.length} n={n} danger disabled={disabled} onSelect={() => setAsk("end")} />
                <Act icon={Trash} label="Remove" k={acts.exited.length} n={n} danger disabled={disabled} onSelect={remove} />
              </DropdownMenu.Content>
            </DropdownMenu.Portal>
          </DropdownMenu.Root>
          <Button size="xs" variant="secondary" onClick={onClear}>
            <X aria-hidden="true" />
            Clear
          </Button>
        </div>
      </span>
      {shownAsk && (
        <Slide open={asking === shownAsk} onGone={askGone}>
          {askLine(shownAsk)}
        </Slide>
      )}
      {/* One line, at it and then how it went, so what it says is read out as it changes. */}
      {(busy ?? said) && (
        <Slide open={busy !== null || !saidGoing} onGone={saidGone}>
          <div className="ms-bulk-said" role="status">
            {busy ? (
              <span className="ms-msg-result">{busy}</span>
            ) : (
              said && (
                <>
                  {said.outcomes.length > 0 ? (
                    <button type="button" className="ms-msg-result" aria-expanded={outcomes} onClick={() => setOutcomes(!outcomes)}>
                      <ChevronRight size={12} className={cn("ms-chev", outcomes && "rotate-90")} aria-hidden="true" />
                      {said.line}
                    </button>
                  ) : (
                    <span className="ms-msg-result">{said.line}</span>
                  )}
                  <button type="button" className="hx-icon" aria-label="Dismiss" onClick={() => setSaidGoing(true)}>
                    <X size={12} />
                  </button>
                </>
              )
            )}
          </div>
        </Slide>
      )}
      {said && said.outcomes.length > 0 && (
        <Slide open={!busy && !saidGoing && outcomes}>
          <ul className="ms-msg-outcomes ms-bulk-outcomes">
            {said.outcomes.map((o) => (
              <li key={o.id} data-result={o.result}>
                <span className="ms-msg-o-name">{o.name}</span>
                <span className="ms-msg-o-where">{o.machine}</span>
                <span className="ms-msg-o-result">{o.result}</span>
                {o.reason && <span className="ms-msg-o-why">{o.reason}</span>}
              </li>
            ))}
          </ul>
        </Slide>
      )}
    </>
  );
}

/** A select in the line, as the pause panel's are. */
function Pick({ label, value, onChange, disabled, children }: { label: string; value: string; onChange: (v: string) => void; disabled?: boolean; children: React.ReactNode }) {
  return (
    <span className="select hx-pick">
      <select aria-label={label} value={value} disabled={disabled} onChange={(e) => onChange(e.target.value)}>
        {children}
      </select>
      <ChevronDown size={12} aria-hidden="true" />
    </span>
  );
}

/** Where the agents that can move go: the machines that take some of them (how many, when not all), once their turns are over or now. */
function MoveAsk({ agents, disabled, onMove, onCancel }: { agents: AgentInfo[]; disabled: boolean; onMove: (place: Place, plan: Places, now: boolean) => void; onCancel: () => void }) {
  const { byTarget, runners } = useSuspenseResource("harness/move", askPlaces).data;
  const plan = placesFor(agents, byTarget, runners);
  const [to, setTo] = React.useState("");
  const place = plan.places.find((p) => p.target === to);
  const working = place?.takes.some(busyWith) ?? false;
  const stay = place
    ? agents.flatMap((a) => {
        const why = whyNot(a, place, plan);
        return why ? [`${agentLabel(a)} (${why})`] : [];
      })
    : [];
  const why = [
    plan.older.length > 0 && `${inWords(plan.older)} ${plan.older.length === 1 ? "runs an older kiln: upgrade it" : "run an older kiln: upgrade them"} in System → Updates to move agents there.`,
    plan.closed.length > 0 && `${inWords(plan.closed)} ${plan.closed.length === 1 ? "is" : "are"} set not to take work from other runners.`,
  ]
    .filter(Boolean)
    .join(" ");
  if (plan.places.length === 0)
    return (
      <>
        <span className="ms-bulk-q">{why || `No other connected machine can take ${agents.length === 1 ? "it" : "them"}.`}</span>
        <Button size="xs" variant="ghost" onClick={onCancel}>
          Close
        </Button>
      </>
    );
  return (
    <>
      <span className="ms-bulk-q">Move {count(agents.length, "agent")} to</span>
      <Pick label="The machine they move to" value={to} onChange={setTo} disabled={disabled}>
        <option value="" disabled>
          Choose a machine
        </option>
        {plan.places.map((p) => (
          <option key={p.target} value={p.target}>
            {p.name}
            {p.takes.length < agents.length ? ` (${p.takes.length} of ${agents.length})` : ""}
          </option>
        ))}
      </Pick>
      {working && (
        <Button size="xs" variant="secondary" disabled={disabled || !place} onClick={() => place && onMove(place, plan, true)} title="Their turns stop here and go on there">
          Move now
        </Button>
      )}
      <Button size="xs" disabled={disabled || !place} onClick={() => place && onMove(place, plan, false)}>
        {working ? "Move after their turns" : "Move"}
      </Button>
      <Button size="xs" variant="ghost" onClick={onCancel}>
        Cancel
      </Button>
      {stay.length > 0 && (
        <p className="ms-bulk-note">
          {stay.length === 1 ? "One stays where it is" : `${stay.length} stay where they are`}: {stay.slice(0, 3).join("; ")}
          {stay.length > 3 ? "; …" : "."}
        </p>
      )}
      {why && <p className="ms-bulk-note">{why}</p>}
    </>
  );
}

/** Ending them, asked: what goes with them, and the agents they started that still run, ending with them unless kept. */
function EndAsk({ agents, all, disabled, onEnd, onCancel }: { agents: AgentInfo[]; all: AgentInfo[]; disabled: boolean; onEnd: (withKids: boolean) => void; onCancel: () => void }) {
  const kids = startedBy(new Set(agents.map((a) => a.id)), all);
  const [withKids, setWithKids] = React.useState(true);
  const one = agents.length === 1;
  const left = agents.reduce((s, a) => s + (a.left_behind ?? 0), 0);
  return (
    <>
      <span className="ms-bulk-q">
        End {count(agents.length, "agent")}? {one ? "Its process stops and it leaves" : "Their processes stop and they leave"} the board
        {left > 0 ? `, with the ${left === 1 ? "shell or monitor" : `${left} shells and monitors`} left running` : ""}. {one ? "Its" : "Their"} changes stay on disk.
      </span>
      {kids.length > 0 && (
        <label className="ms-bulk-check">
          <input type="checkbox" className="check" checked={withKids} onChange={(e) => setWithKids(e.target.checked)} />
          <span>
            End the {count(kids.length, "agent")} {one ? "it" : "they"} started too
            <span className="ms-bulk-names">
              :{" "}
              {kids
                .slice(0, 3)
                .map((k) => agentLabel(k))
                .join(", ")}
              {kids.length > 3 ? ` and ${kids.length - 3} more` : ""}
            </span>
          </span>
        </label>
      )}
      <Button size="xs" variant="danger" disabled={disabled} onClick={() => onEnd(withKids)}>
        End {agents.length}
      </Button>
      <Button size="xs" variant="ghost" onClick={onCancel}>
        Cancel
      </Button>
    </>
  );
}

/** Stands for "as each of them has it", where they don't all have the same. */
const MIXED = "\u0000mixed";

/**
 * Where the commands of the agents on runners run besides their own machines, as one agent's Where
 * commands run… has it; changed, each starts again with it once its turn is over.
 */
function ReachPick({
  agents,
  disabled,
  onChange,
  onCancel,
}: {
  agents: AgentInfo[];
  disabled: boolean;
  onChange: (value: string, targets: AgentInfo[], skipped: Outcome[]) => void;
  onCancel: () => void;
}) {
  const { runners } = useSuspenseResource("harness/move", askPlaces).data;
  const [picked, setPicked] = React.useState<string | null>(null);
  // A runner without a reach identity runs a kiln from before commands could run elsewhere.
  const older = agents.filter((a) => runners.some((r) => `agent:${r.name}` === a.target && !r.fingerprint));
  const can = agents.filter((a) => !older.includes(a));
  if (agents.length > 0 && can.length === 0)
    return <span className="ms-bulk-note">{agents.length === 1 ? "Its runner runs" : "Their runners run"} an older kiln, without commands on other runners: upgrade it in System → Updates.</span>;
  // One wording for one of them or many, so the line keeps its width as they're picked.
  const had = [...new Set(can.map((a) => a.reach ?? ""))];
  const now = had.length === 1 ? (had[0] ?? "") : null;
  const choices = [
    { value: "", label: "Any runner they need" },
    ...reachChoices(
      runners.filter((r) => r.connected && r.reach),
      "",
    ),
    { value: "off", label: "Only their own machines" },
  ];
  // The one they have stays a choice, whether or not that runner is around now.
  if (now !== null && !choices.some((c) => c.value === now)) choices.splice(1, 0, { value: now, label: `Prefer ${now}` });
  const value = picked ?? now ?? MIXED;
  const skipped = older.map((a) => outcome(a, "skipped", "its runner runs an older kiln, without commands on other runners"));
  // None of them on a runner: there, turned off.
  const off = disabled || can.length === 0;
  return (
    <span className="ms-bulk-reach" title={can.length === 0 ? "None of them runs on a runner" : undefined}>
      <span className="ms-bulk-q">Where their commands run</span>
      <Pick label="Where their commands run" value={value} onChange={setPicked} disabled={off}>
        {now === null && (
          <option value={MIXED} disabled>
            As each has it now
          </option>
        )}
        {choices.map((c) => (
          <option key={c.value || "any"} value={c.value}>
            {c.label}
          </option>
        ))}
      </Pick>
      <Button size="xs" disabled={off || picked === null || picked === now} onClick={() => picked !== null && onChange(picked, can, skipped)}>
        Change
      </Button>
      <Button size="xs" variant="ghost" onClick={onCancel}>
        Cancel
      </Button>
    </span>
  );
}

/** Their notifications muted for you on every device, or unmuted when they all are: where push is on here. */
function MuteItem({ agents, disabled, run }: { agents: AgentInfo[]; disabled: boolean; run: Acting }) {
  const push = useRes(pushStateRes);
  const muted = new Set(push.data.muted);
  const on = agents.filter((a) => muted.has(a.id));
  if (!push.data.available && on.length === 0) return null;
  const unmute = on.length === agents.length;
  const targets = unmute ? agents : agents.filter((a) => !muted.has(a.id));
  const go = () => {
    const ids = new Set(targets.map((a) => a.id));
    push.setData((s) => ({ ...s, muted: unmute ? s.muted.filter((m) => !ids.has(m)) : [...new Set([...s.muted, ...ids])] }));
    run(`${unmute ? "Unmuting" : "Muting"} ${count(targets.length, "agent")}…`, async () => {
      const outcomes = await each(targets, (a) => pushApi.mute(a.id, !unmute));
      if (outcomes.some((o) => o.result !== "done")) invalidate("push/state");
      return told((k) => `${unmute ? "Unmuted" : "Muted"} the notifications of ${k}`, outcomes);
    });
  };
  return <Act icon={unmute ? Bell : BellOff} label={unmute ? "Unmute notifications" : "Mute notifications"} k={targets.length} n={agents.length} disabled={disabled} onSelect={go} />;
}
