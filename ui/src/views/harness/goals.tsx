/**
 * An agent's goal stack, in its own panel under the Plan tab's plans: goals that run one after
 * another on this agent, as if you gave it /goal again and again yourself. Each goes as its
 * harness's own /goal (or, where it has none, as a message: the agent ends its reply with GOAL MET
 * when it's met); when one is met the next starts. Until then the agent is kept on it: whenever it
 * stops short of it, or loses it when it starts again, the goal is sent again. The server drives
 * it, so it goes on with the console closed and across restarts.
 *
 * Drawn as a chain: each goal a numbered link (met, under way, next, not met), joined to the next,
 * a goal's parts (it was split into them: they come first) as smaller links before it; one field
 * to add a goal; the stack's few actions; editing the goals not yet met (the one under way first) is
 * one text box, a goal a line. Cleared goals can be brought back. A stack paused for a while says
 * when it resumes by itself.
 */
import * as React from "react";
import { harnessApi, type GoalItem, type GoalStack } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { relativeTime } from "@/lib/format";
import { Link } from "@/lib/router";
import { planHref, runHref } from "@/api/plans";
import { StandTitle, StepGlyph, type Stand } from "@/views/plans/status";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import { fromLines, stackReads, startWords, stoppedBy, toGo } from "./goal-stack";
import "./goals.css";

const WORD: Record<GoalItem["status"], string> = { met: "met", active: "under way", pending: "waiting", unmet: "not met", skipped: "skipped" };

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

/** How deep a goal is among parts: 0 a goal, 1 a part, 2 a part's part. */
function depthOf(g: GoalItem, items: GoalItem[]): number {
  let d = 0;
  let at: GoalItem | undefined = g;
  while (at?.part_of && d < 3) {
    const up: string = at.part_of;
    at = items.find((i) => i.id === up);
    d += 1;
  }
  return d;
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
  const pending = items.filter((i) => i.status === "pending");
  const active = items.find((i) => i.status === "active");
  const met = items.filter((i) => i.status === "met").length;
  const cleared = stack?.cleared ?? 0;
  // Finished ones beyond the last few fold away.
  const done = items.filter((i) => i.status !== "pending" && i.status !== "active");
  const folded = earlier ? 0 : Math.max(0, done.length - 3);
  const shown = items.filter((i) => !done.slice(0, folded).includes(i));
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
      ? `Running · ${met} of ${items.length} met`
      : state === "done"
        ? `Done · ${met} of ${items.length} met`
        : state === "paused"
          ? `Paused · ${met} of ${items.length} met`
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
          {items.length > 0 && (
            <ol className="gs-chain">
              {folded > 0 || (earlier && done.length > 3) ? (
                <li className="gs-more">
                  <button type="button" className="gs-more-btn" onClick={() => setEarlier(!earlier)}>
                    {earlier ? "Hide the earlier ones" : `${folded} earlier`}
                  </button>
                </li>
              ) : null}
              {shown.map((g) => {
                // The goal under way while you've paused the stack: paused, not under way.
                const held = g.status === "active" && stack?.state === "paused";
                return (
                  <li key={g.id} className="gs-link" data-status={held ? "paused" : g.status} data-part={g.part_of ? "" : undefined} data-depth={g.part_of ? depthOf(g, items) : undefined}>
                    <span className="gs-node" title={WORD[g.status]}>
                      <StepGlyph stand={standOf(g, held, working && !g.waits_on)} />
                    </span>
                    <span className="gs-text">
                      {/* A plan's step as a goal reads "title — description — Done when: …": its title first, the rest under it. */}
                      <StandTitle stand={standOf(g, held, working && !g.waits_on)}>{g.text.split(" — ")[0] ?? g.text}</StandTitle>
                      {g.text.includes(" — ") && <span className="gs-detail">{g.text.split(" — ").slice(1).join(" — ")}</span>}
                      <span className="gs-word">
                        {held ? "paused" : WORD[g.status]}
                        {g.status === "active" && !held && !g.waits_on ? sentWords(g) : ""}
                        {g.reason && g.status !== "pending" ? ` · ${g.reason}` : ""}
                        {g.planned_as && (
                          <>
                            {" · "}
                            <Link to={planHref(g.planned_as)}>planned</Link>: met once every step of its plan is
                          </>
                        )}
                        {g.waits_on && (
                          <>
                            {" · waits for "}
                            <Link to={runHref(g.waits_on)}>its run</Link>
                          </>
                        )}
                      </span>
                    </span>
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
