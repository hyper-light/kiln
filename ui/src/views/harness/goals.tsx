/**
 * An agent's goal stack, in its own panel under the Plan tab's plans: goals that run one after
 * another on this agent, as if you gave it /goal again and again yourself. Each goes as its
 * harness's own /goal (or, where it has none, as a message: the agent ends its reply with GOAL MET
 * when it's met); when one is met the next starts. Until then the agent is kept on it: whenever it
 * stops short of it, or loses it when it starts again, the goal is sent again. The server drives
 * it, so it goes on with the console closed and across restarts.
 *
 * Drawn as a chain: each goal a numbered link (met, under way, next, not met), joined to the next,
 * the one under way ringed in the turning light and its words in the holographic shimmer; under a
 * goal, its steps (its plan's, or the parts it was split into) on its line, as goal-plan.tsx draws
 * them; one field to add a goal; the stack's few actions; editing the goals not yet met (the one
 * under way first) is one text box, a goal a line. Cleared goals can be brought back. A stack
 * paused for a while says when it resumes by itself.
 */
import * as React from "react";
import { ListTree } from "lucide-react";
import { harnessApi, type GoalItem, type GoalStack } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { harnessSessionsRes, useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { ApiError } from "@/api/client";
import { planHref, plansApi, runHref } from "@/api/plans";
import { ResearchLine } from "@/views/research/research";
import { effectiveNeeds } from "@/views/plans/graph-layout";
import { StandTitle, type Stand } from "@/views/plans/status";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { Link } from "@/lib/router";
import { canPlan, fromLines, stackReads, startWords, stepStand, stoppedBy, toGo } from "./goal-stack";
import { GoalPlan } from "./goal-plan";
import { machineWord, type GoalStep } from "./goal-plan-layout";
import "./goals.css";

const WORD: Record<GoalItem["status"], string> = { met: "met", active: "under way", pending: "waiting", unmet: "not met", skipped: "skipped" };

/** Longer than this, a goal's words fold to their first lines. */
const LONG_GOAL = 220;

/** A goal's words, in the prism's shimmer while it's worked on: a long one folds to its first lines, opened on asking. */
function GoalWords({ text, stand }: { text: string; stand: Stand }) {
  const [open, setOpen] = React.useState(false);
  const long = text.length > LONG_GOAL;
  return (
    <>
      <span className="gs-words" data-folded={long && !open ? "" : undefined}>
        <StandTitle stand={stand}>{text}</StandTitle>
      </span>
      {long && (
        <button type="button" className="gs-fold" aria-expanded={open} onClick={() => setOpen(!open)}>
          {open ? "Less" : "More"}
        </button>
      )}
    </>
  );
}

/** A goal's parts at any depth: its own, and theirs (a plan step's criteria and tests). */
function partsOf(g: GoalItem, items: GoalItem[]): GoalItem[] {
  const ids = new Set([g.id]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const i of items)
      if (i.part_of && ids.has(i.part_of) && !ids.has(i.id)) {
        ids.add(i.id);
        grew = true;
      }
  }
  return items.filter((i) => i.id !== g.id && ids.has(i.id));
}

/** "gone" for a plan or run that isn't there any more. */
const unlessGone = (e: unknown): "gone" => {
  if (e instanceof ApiError && (e.status === 404 || e.status === 400)) return "gone";
  throw e;
};

const usePlan = (plan: string) => useSuspenseResource(`plans/${plan}`, (s) => plansApi.get(plan, s).catch(unlessGone)).data;

/** A plan run as it goes, read again every few seconds (none without a run). */
const useRunView = (run: string | undefined) =>
  useSuspenseResource(
    run ? `plans/runs/${run}` : "plans/runs/none",
    (s) => (run ? plansApi.runView(run, s).catch(unlessGone) : Promise.resolve(null)),
    run ? { on: ["harness", "harness_closed", "plan_stack"], pollMs: 4000 } : {},
  ).data;

const PART_STANDS: Record<GoalItem["status"], Stand> = { pending: "waiting", active: "working", met: "met", unmet: "unmet", skipped: "skipped" };

/** Where a part stands: under way while you've paused the stack, paused. */
const partStand = (p: GoalItem | undefined, paused: boolean): Stand => (p?.status === "active" && paused ? "paused" : PART_STANDS[p?.status ?? "pending"]);

/** How many of a part's own parts (a plan step's acceptance criteria and tests) are met. */
function checksOf(part: GoalItem | undefined, items: GoalItem[]): GoalStep["checks"] {
  const own = part ? items.filter((i) => i.part_of === part.id) : [];
  return own.length ? { met: own.filter((i) => i.status === "met").length, of: own.length } : undefined;
}

/** Who an agent is, at a step: its harness and its machine ("Devin · fedora"). */
const whoOf = (a: { kind_name: string; target: string } | undefined) => (a ? `${a.kind_name} · ${machineWord(a.target)}` : undefined);

/**
 * A planned goal's plan and its steps as they stand: its parts on this stack (each step a part),
 * or its run, an agent a step, saying who's at each. Being planned (none of its steps on the stack
 * yet, nor its run), its one step is the planning.
 */
function usePlanned(goal: GoalItem, items: GoalItem[], paused: boolean, agent: string) {
  const p = usePlan(goal.planned_as ?? "");
  const run = items.find((i) => i.part_of === goal.id && i.waits_on)?.waits_on;
  const view = useRunView(run);
  const sessions = useRes(harnessSessionsRes).data;
  const own = whoOf(sessions.find((x) => x.id === agent));
  if (!p || p === "gone") return null;
  const planning = p.phase === "planning" && !run && !items.some((i) => i.part_of === goal.id && i.part_kind === "plan");
  if (planning) {
    const part = items.find((i) => i.part_of === goal.id && i.part_kind === "planning");
    const steps: GoalStep[] = [{ n: 1, title: "Research it inside and out, and write its steps", needs: [], stand: partStand(part, paused), who: own }];
    return { plan: p, run, steps, planning };
  }
  const titles = p.items?.length ? p.items.map((it) => it.title || "Untitled step") : p.steps;
  const waits = effectiveNeeds(titles.map((_, i) => p.items?.[i]?.needs));
  const ran = view && view !== "gone" ? view.steps : null;
  const steps: GoalStep[] = titles.map((title, i) => {
    const n = i + 1;
    const r = ran?.find((x) => x.n === n);
    if (r) {
      const who = whoOf(sessions.find((x) => x.id === r.agent_id)) ?? (r.agent ? `${r.agent.kind} · ${machineWord(r.agent.machine)}` : undefined);
      return { n, title, needs: waits[i] ?? [], stand: r.stand, who };
    }
    const part = items.find((x) => x.part_of === goal.id && x.part_kind === "plan" && x.step?.n === n);
    return { n, title, needs: waits[i] ?? [], stand: stepStand(goal, items, n, paused) ?? "waiting", who: own, checks: checksOf(part, items) };
  });
  return { plan: p, run, steps, planning };
}

/** A planned goal, in its line: how many of its plan's steps are met (its plan to open), its run, its research. */
function PlannedWords({ goal, items, paused, agent }: { goal: GoalItem; items: GoalItem[]; paused: boolean; agent: string }) {
  const planned = usePlanned(goal, items, paused, agent);
  if (!planned) return null;
  const { plan, run, steps, planning } = planned;
  const met = steps.filter((s) => s.stand === "met").length;
  return (
    <>
      {" · "}
      <Link to={planHref(plan.id)}>{planning ? "being planned" : `${met} of ${steps.length} step${steps.length === 1 ? "" : "s"} met`}</Link>
      {run && (
        <>
          {" · "}
          <Link to={runHref(run)}>its run</Link>
        </>
      )}
      {plan.research && (
        <>
          {" · "}
          <ResearchLine id={plan.research} />
        </>
      )}
    </>
  );
}

/** A planned goal's steps, under it on its line. */
function PlannedSteps({ goal, items, paused, agent }: { goal: GoalItem; items: GoalItem[]; paused: boolean; agent: string }) {
  const planned = usePlanned(goal, items, paused, agent);
  if (!planned?.steps.length) return null;
  return <GoalPlan steps={planned.steps} label={`Its plan, “${planned.plan.title}”`} />;
}

/** What a plan step's goal says before its criterion or test: its step and plan, said by the goal already. */
const STEP_SAID = /^"[^"]*" \(step \d+ of the plan "[^"]*"\), /;

/** The parts a goal was split into (or a plan step's criteria and tests), under it on its line, each in turn. */
function PartSteps({ parts, items, paused, agent }: { parts: GoalItem[]; items: GoalItem[]; paused: boolean; agent: string }) {
  const own = whoOf(useRes(harnessSessionsRes).data.find((x) => x.id === agent));
  const steps: GoalStep[] = parts.map((p, i) => ({
    n: i + 1,
    title: p.text.replace(STEP_SAID, ""),
    needs: i ? [i] : [],
    stand: partStand(p, paused),
    who: own,
    checks: checksOf(p, items),
  }));
  return <GoalPlan steps={steps} label="Its parts" />;
}

/** An agent's name, from the agents kiln knows (its id's start until they're in). */
function AgentLabel({ id }: { id: string }) {
  return (
    <React.Suspense fallback={<>agent {id.slice(0, 8)}</>}>
      <AgentName id={id} />
    </React.Suspense>
  );
}

function AgentName({ id }: { id: string }) {
  const name = useRes(harnessSessionsRes).data.find((a) => a.id === id)?.name;
  return <>{name || `agent ${id.slice(0, 8)}`}</>;
}

/** When the goal under way last went to the agent: sent, or sent again (how many times in a row). */
function sentWords(g: GoalItem): string {
  if (!g.reminded_at) return "";
  const when = relativeTime(g.reminded_at / 1000);
  return g.reminders ? ` · sent again ${g.reminders}×, last ${when}` : ` · sent ${when}`;
}

/**
 * Each agent's last goal stack: a read that fails keeps showing it (with none yet, the panel says
 * why), and one asked for before a change made here came back doesn't undo the change.
 */
const reads = stackReads(() => performance.now());

function readGoals(agent: string): Promise<{ stack: GoalStack | null; error: unknown }> {
  const took = reads.ask(agent);
  return harnessApi.goals(agent).then(
    (stack) => ({ stack: took(stack), error: null }),
    (error: unknown) => ({ stack: reads.last(agent), error }),
  );
}

export function Goals({ agent, harness, working }: { agent: string; harness?: string; working?: boolean }) {
  // Another agent's: these stay until its goals are in.
  const shown = React.useDeferredValue(agent);
  return (
    <Loadable label="Loading the goal stack" fallback={<GoalPanel agent={agent} harness={harness} stack={null} />}>
      {/* Its own: an edit or a goal half typed for one agent never goes to another. */}
      <AgentGoals key={shown} agent={shown} harness={harness} working={working} />
    </Loadable>
  );
}

/** The agent's goal stack, read again every few seconds while it shows. */
function AgentGoals({ agent, harness, working }: { agent: string; harness?: string; working?: boolean }) {
  const res = useSuspenseResource(`harness/goals/${agent}`, () => readGoals(agent), { pollMs: 2500 });
  return (
    <GoalPanel
      agent={agent}
      harness={harness}
      working={working}
      stack={res.data.stack}
      error={res.data.error}
      onStack={(s) => {
        reads.changed(agent, s);
        res.setData(() => ({ stack: s, error: null }));
      }}
    />
  );
}

/** Where a goal stands, as its node shows it: under way and the agent at work, working; under way while it's
 * idle, or to come, only sketched. */
function standOf(g: GoalItem, paused: boolean, working: boolean): Stand {
  if (g.status === "met") return "met";
  if (g.status === "unmet") return "unmet";
  if (g.status === "skipped") return "skipped";
  if (g.status === "active") return paused ? "paused" : working ? "working" : "waiting";
  return "waiting";
}

function GoalPanel({
  agent,
  harness,
  working = false,
  stack,
  error: readError,
  onStack,
}: {
  agent: string;
  harness?: string;
  /** The agent is at work now. */
  working?: boolean;
  /** None until it's read. */
  stack: GoalStack | null;
  /** Why it couldn't be read. */
  error?: unknown;
  onStack?: (s: GoalStack) => void;
}) {
  const [adding, setAdding] = React.useState("");
  // The edit box's words, and the goals it began with (what its lines go back to).
  const [editing, setEditing] = React.useState<{ text: string; goals: GoalItem[] } | null>(null);
  const [asking, setAsking] = React.useState(false);
  const [earlier, setEarlier] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const items = stack?.items ?? [];
  // The stack's own goals: their parts are drawn under them, as their steps.
  const goals = items.filter((i) => !i.part_of);
  // Each goal's number in the stack, as it's spoken of.
  const numbers = new Map(goals.map((g, i) => [g.id, i + 1]));
  const pending = items.filter((i) => i.status === "pending");
  const active = items.find((i) => i.status === "active");
  const met = goals.filter((i) => i.status === "met").length;
  const cleared = stack?.cleared ?? 0;
  const paused = stack?.state === "paused";
  // Finished ones beyond the last few fold away.
  const done = goals.filter((i) => i.status !== "pending" && i.status !== "active");
  const folded = earlier ? 0 : Math.max(0, done.length - 3);
  const shown = goals.filter((i) => !done.slice(0, folded).includes(i));
  // The goal that stopped the stack, to try again first (none while a goal is under way: that one resumes).
  const unmet = stoppedBy(stack);
  const startWord = startWords(stack);
  const who = harness ?? "the agent";
  const asMessage = (active ?? items.filter((i) => i.how).at(-1))?.how === "message";

  const act = async (f: () => Promise<GoalStack>) => {
    setBusy(true);
    setError(null);
    try {
      const s = await f();
      onStack?.(s);
      return true;
    } catch (e) {
      setError(errorMessage(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  const add = async () => {
    const text = adding.trim();
    // A stack that was done goes on with it.
    if (text && (await act(() => harnessApi.addGoals(agent, [text], stack?.state === "done")))) setAdding("");
  };
  const saveEdit = async () => {
    if (editing === null) return;
    const { goals, dropped } = fromLines(editing.text, editing.goals);
    if (
      await act(async () => {
        const s = await harnessApi.setGoals(agent, goals);
        // The goal under way, its line taken out: skipped, if it's still the one under way.
        return dropped && s.items.some((i) => i.id === dropped.id && i.status === "active") ? harnessApi.goalAction(agent, "skip") : s;
      })
    )
      setEditing(null);
  };

  // Not read yet (the stand-in while it loads), or it couldn't be: not "no goals", which it may have.
  const state = !stack ? (readError ? "unread" : "reading") : items.length === 0 ? "empty" : stack.state;
  const chip =
    state === "running"
      ? `Running · ${met} of ${goals.length} met`
      : state === "done"
        ? `Done · ${met} of ${goals.length} met`
        : state === "paused"
          ? `Paused · ${met} of ${goals.length} met`
          : state === "reading"
            ? "Reading…"
            : state === "unread"
              ? "Couldn't read it"
              : "No goals yet";

  return (
    <section className="gs-panel" aria-label="Goal stack">
      <header className="gs-top">
        <h3 className="gs-h">Goal stack</h3>
        <span className="gs-chip" data-state={state}>
          {chip}
        </span>
      </header>
      <p className="gs-what">
        {asMessage
          ? `Goals run one after another on this agent: each goes to ${who} as a message, and the next starts once it says the goal is met (GOAL MET).`
          : `Goals run one after another on this agent: each goes to ${who} as its own /goal, and the next starts once it's met.`}{" "}
        Until then kiln keeps it on the goal: whenever it stops short, or loses the goal when it starts again, kiln sends it again.
      </p>
      {/* Big goals planned first (docs/goals.md): a person's setting, on unless turned off. */}
      <label className="gs-option">
        <input type="checkbox" className="check" checked={stack?.plan_big ?? true} disabled={busy} onChange={(e) => void act(() => harnessApi.goalSettings(agent, { plan_big: e.target.checked }))} />
        <span>
          Plan big goals first
          <span className="gs-option-hint">A goal longer than a couple of sentences is researched and planned before it&rsquo;s worked on, its steps run under it.</span>
        </span>
      </label>
      {!stack && readError ? <p className="gs-error">{errorMessage(readError)}</p> : null}
      {stack?.state === "paused" && stack.reason && (
        <p className="gs-reason">
          {stack.reason}
          {stack.resume_at ? ` · resumes by itself ${relativeTime(stack.resume_at / 1000)}` : ""}
        </p>
      )}

      {editing !== null ? (
        <div className="gs-edit">
          <label className="field-label" htmlFor={`gs-edit-${agent}`}>
            Goals not yet met, one a line
          </label>
          <div className="field">
            <textarea
              id={`gs-edit-${agent}`}
              value={editing.text}
              onChange={(e) => {
                const text = e.target.value;
                setEditing((cur) => cur && { ...cur, text });
              }}
              rows={Math.max(3, editing.text.split("\n").length + 1)}
            />
          </div>
          <div className="gs-actions">
            <Button size="sm" disabled={busy} onClick={() => void saveEdit()}>
              Save goals
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setEditing(null)}>
              Cancel
            </Button>
          </div>
        </div>
      ) : (
        <>
          {goals.length > 0 && (
            <ol className="gs-chain">
              {folded > 0 || (earlier && done.length > 3) ? (
                <li className="gs-more">
                  <button type="button" className="gs-more-btn" onClick={() => setEarlier(!earlier)}>
                    {earlier ? "Hide the earlier ones" : `${folded} earlier`}
                  </button>
                </li>
              ) : null}
              {shown.map((g) => {
                // A goal whose parts come first is under way while they are (its steps, or its run's agents, at work).
                const doing = g.status === "pending" ? partsOf(g, items).find((i) => i.status === "active") : undefined;
                const status = doing ? "active" : g.status;
                // The goal under way while you've paused the stack: paused, not under way.
                const held = status === "active" && stack?.state === "paused";
                // One handed to another agent (a program's track's worker): under way there.
                const away = g.status === "pending" && !doing ? g.on : undefined;
                const stand = standOf({ ...g, status }, held, doing?.waits_on ? true : working && !g.waits_on);
                return (
                  <li key={g.id} className="gs-link" data-status={held ? "paused" : away ? "active" : status}>
                    {/* Its number in the stack, ringed by where it stands: the one at work in the turning light. */}
                    <span className="gs-node" data-stand={stand} data-wide={(numbers.get(g.id) ?? 0) > 9 ? "" : undefined} title={WORD[status]}>
                      {numbers.get(g.id)}
                    </span>
                    <span className="gs-text">
                      {/* A plan's step as a goal reads "title — description — Done when: …": its title first, the rest under it. */}
                      <GoalWords text={g.text.split(" — ")[0] ?? g.text} stand={stand} />
                      {g.text.includes(" — ") && <span className="gs-detail">{g.text.split(" — ").slice(1).join(" — ")}</span>}
                      <span className="gs-word">
                        {held ? "paused" : away ? (
                          <>
                            under way on{" "}
                            <Link to={`/agents/${encodeURIComponent(away)}`}>
                              <AgentLabel id={away} />
                            </Link>
                          </>
                        ) : (
                          WORD[status]
                        )}
                        {g.planned_as ? (
                          <Loadable label="its plan" fallback={null}>
                            <PlannedWords goal={g} items={items} paused={paused} agent={agent} />
                          </Loadable>
                        ) : g.status === "active" && !held && !g.waits_on ? (
                          sentWords(g)
                        ) : (
                          ""
                        )}
                        {g.reason && g.status !== "pending" ? ` · ${g.reason}` : ""}
                        {g.waits_on && (
                          <>
                            {" · waits for "}
                            <Link to={runHref(g.waits_on)}>its run</Link>
                          </>
                        )}
                      </span>
                    </span>
                    {/* What can be done with it: a goal of its own still to go becomes a plan. */}
                    {canPlan(g) && (
                      <div className="gs-acts">
                        <Button
                          size="xs"
                          variant="secondary"
                          disabled={busy}
                          onClick={() =>
                            void act(async () => {
                              await harnessApi.planGoal(agent, g.id);
                              return harnessApi.goals(agent);
                            })
                          }
                        >
                          <ListTree aria-hidden="true" /> Make a plan
                        </Button>
                      </div>
                    )}
                    {/* Its steps under it, on its line: its plan's, or the parts it was split into. */}
                    {g.planned_as ? (
                      <Loadable label="its plan" fallback={null}>
                        <PlannedSteps goal={g} items={items} paused={paused} agent={agent} />
                      </Loadable>
                    ) : items.some((i) => i.part_of === g.id) ? (
                      <Loadable label="its parts" fallback={null}>
                        <PartSteps parts={items.filter((i) => i.part_of === g.id)} items={items} paused={paused} agent={agent} />
                      </Loadable>
                    ) : null}
                  </li>
                );
              })}
            </ol>
          )}
          <form
            className="gs-add"
            onSubmit={(e) => {
              e.preventDefault();
              void add();
            }}
          >
            <label className="sr-only" htmlFor={`gs-add-${agent}`}>
              Add a goal
            </label>
            <div className="field">
              <input id={`gs-add-${agent}`} value={adding} placeholder={items.length ? "Add the next goal" : "Add a goal"} onChange={(e) => setAdding(e.target.value)} />
            </div>
            <Button size="sm" variant="secondary" type="submit" disabled={busy || !adding.trim() || !onStack}>
              Add
            </Button>
          </form>
          <p className="field-hint gs-hint">A condition it can meet, like &ldquo;the parser tests pass&rdquo;. A plan&rsquo;s steps go here too: run it as Goals.</p>
          {(items.length > 0 || cleared > 0) && (
            <div className="gs-actions">
              {startWord && (
                <Button size="sm" disabled={busy} onClick={() => void act(() => harnessApi.goalAction(agent, "start"))}>
                  {startWord}
                </Button>
              )}
              {unmet && (
                <Button
                  size="sm"
                  variant="secondary"
                  disabled={busy}
                  onClick={() =>
                    void act(async () => {
                      await harnessApi.setGoals(agent, [{ text: unmet.text }, ...pending.map((p) => ({ id: p.id, text: p.text }))]);
                      return harnessApi.goalAction(agent, "start");
                    })
                  }
                >
                  Try it again
                </Button>
              )}
              {stack?.state === "running" && (
                <Button size="sm" variant="secondary" disabled={busy} onClick={() => void act(() => harnessApi.goalAction(agent, "pause"))}>
                  Pause
                </Button>
              )}
              {active && (
                <Button size="sm" variant="ghost" disabled={busy} onClick={() => void act(() => harnessApi.goalAction(agent, "skip"))}>
                  Skip this one
                </Button>
              )}
              {(pending.length > 0 || active) && (
                <Button
                  size="sm"
                  variant="ghost"
                  disabled={busy}
                  onClick={() => {
                    const goals = toGo(items);
                    setEditing({ text: goals.map((g) => g.text).join("\n"), goals });
                  }}
                >
                  Edit
                </Button>
              )}
              {cleared > 0 && (
                <Button size="sm" variant="secondary" disabled={busy} onClick={() => void act(() => harnessApi.goalAction(agent, "restore"))}>
                  Bring back {cleared} cleared {cleared === 1 ? "goal" : "goals"}
                </Button>
              )}
              {items.length === 0 ? null : asking ? (
                <span className="gs-confirm" role="group" aria-label="Clear the goal stack">
                  Clear every goal here?
                  <Button
                    size="sm"
                    variant="danger"
                    disabled={busy}
                    onClick={() => {
                      setAsking(false);
                      void act(() => harnessApi.goalAction(agent, "clear"));
                    }}
                  >
                    Clear
                  </Button>
                  <Button size="sm" variant="ghost" onClick={() => setAsking(false)}>
                    Keep them
                  </Button>
                </span>
              ) : (
                <Button size="sm" variant="ghost" onClick={() => setAsking(true)}>
                  Clear
                </Button>
              )}
            </div>
          )}
        </>
      )}
      {error && <p className="gs-error">{error}</p>}
    </section>
  );
}
