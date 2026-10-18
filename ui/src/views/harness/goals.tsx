/**
 * An agent's goal stack, in its own panel under the Plan tab's plans: goals that run one after
 * another on this agent, as if you gave it /goal again and again yourself. Each goes as its
 * harness's own /goal (or, where it has none, as a message: the agent ends its reply with GOAL MET
 * when it's met); when one is met the next starts. Until then the agent is kept on it: whenever it
 * stops short of it, or loses it when it starts again, the goal is sent again. The server drives
 * it, so it goes on with the console closed and across restarts.
 *
 * Drawn as a chain: each goal a numbered link (met, under way, next, not met), joined to the next;
 * one field to add a goal; the stack's few actions; editing the goals not yet started is one text
 * box, a goal a line.
 */
import * as React from "react";
import { harnessApi, type GoalItem, type GoalStack } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Loadable } from "@/components/ui/loading";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { useSuspenseResource } from "@/lib/resource";
import "./goals.css";

const WORD: Record<GoalItem["status"], string> = { met: "met", active: "under way", pending: "waiting", unmet: "not met", skipped: "skipped" };

/** When the goal under way last went to the agent: sent, or sent again (how many times in a row). */
function sentWords(g: GoalItem): string {
  if (!g.reminded_at) return "";
  const when = relativeTime(g.reminded_at / 1000);
  return g.reminders ? ` · sent again ${g.reminders}×, last ${when}` : ` · sent ${when}`;
}

/** Each agent's last goal stack: a read that fails keeps showing it (with none yet, the panel says why). */
const lastStack = new Map<string, GoalStack>();

function readGoals(agent: string): Promise<{ stack: GoalStack | null; error: unknown }> {
  return harnessApi.goals(agent).then(
    (stack) => {
      lastStack.set(agent, stack);
      return { stack, error: null };
    },
    (error: unknown) => ({ stack: lastStack.get(agent) ?? null, error }),
  );
}

export function Goals({ agent, harness }: { agent: string; harness?: string }) {
  // Another agent's: these stay until its goals are in.
  const shown = React.useDeferredValue(agent);
  return (
    <Loadable label="Loading the goal stack" fallback={<GoalPanel agent={agent} harness={harness} stack={null} />}>
      <AgentGoals agent={shown} harness={harness} />
    </Loadable>
  );
}

/** The agent's goal stack, read again every few seconds while it shows. */
function AgentGoals({ agent, harness }: { agent: string; harness?: string }) {
  const res = useSuspenseResource(`harness/goals/${agent}`, () => readGoals(agent), { pollMs: 2500 });
  return (
    <GoalPanel
      agent={agent}
      harness={harness}
      stack={res.data.stack}
      error={res.data.error}
      onStack={(s) => {
        lastStack.set(agent, s);
        res.setData(() => ({ stack: s, error: null }));
      }}
    />
  );
}

function GoalPanel({
  agent,
  harness,
  stack,
  error: readError,
  onStack,
}: {
  agent: string;
  harness?: string;
  /** None until it's read. */
  stack: GoalStack | null;
  /** Why it couldn't be read. */
  error?: unknown;
  onStack?: (s: GoalStack) => void;
}) {
  const [adding, setAdding] = React.useState("");
  const [editing, setEditing] = React.useState<string | null>(null);
  const [asking, setAsking] = React.useState(false);
  const [earlier, setEarlier] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const items = stack?.items ?? [];
  const pending = items.filter((i) => i.status === "pending");
  const active = items.find((i) => i.status === "active");
  const met = items.filter((i) => i.status === "met").length;
  // Finished ones beyond the last few fold away.
  const done = items.filter((i) => i.status !== "pending" && i.status !== "active");
  const folded = earlier ? 0 : Math.max(0, done.length - 3);
  const shown = items.filter((i) => !done.slice(0, folded).includes(i));
  // The goal that stopped the stack, to try again first.
  const unmet = stack?.state === "paused" ? [...items].reverse().find((i) => i.status === "unmet" || i.status === "skipped") : undefined;
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
    if (text && (await act(() => harnessApi.addGoals(agent, [text])))) setAdding("");
  };
  const saveEdit = async () => {
    if (editing === null) return;
    const lines = editing
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean);
    // Lines keep the goal that was on them; the rest are new.
    if (
      await act(() =>
        harnessApi.setGoals(
          agent,
          lines.map((text, i) => ({ id: pending[i]?.id, text })),
        ),
      )
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
      {stack?.state === "paused" && stack.reason && <p className="gs-reason">{stack.reason}</p>}

      {editing !== null ? (
        <div className="gs-edit">
          <label className="field-label" htmlFor={`gs-edit-${agent}`}>
            Goals not yet started, one a line
          </label>
          <div className="field">
            <textarea id={`gs-edit-${agent}`} value={editing} onChange={(e) => setEditing(e.target.value)} rows={Math.max(3, editing.split("\n").length + 1)} />
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
                  <li key={g.id} className="gs-link" data-status={held ? "paused" : g.status}>
                    <span className="gs-node" aria-hidden="true">
                      {g.status === "met" ? "✓" : g.status === "unmet" ? "✕" : g.status === "skipped" ? "–" : items.indexOf(g) + 1}
                    </span>
                    <span className="gs-text">
                      {/* A plan's step as a goal reads "title — description — Done when: …": its title first, the rest under it. */}
                      {g.text.split(" — ")[0]}
                      {g.text.includes(" — ") && <span className="gs-detail">{g.text.split(" — ").slice(1).join(" — ")}</span>}
                      <span className="gs-word">
                        {held ? "paused" : WORD[g.status]}
                        {g.status === "active" && !held ? sentWords(g) : ""}
                        {g.reason && g.status !== "pending" ? ` · ${g.reason}` : ""}
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
          {items.length > 0 && (
            <div className="gs-actions">
              {stack?.state !== "running" && (pending.length > 0 || active) && (
                <Button size="sm" disabled={busy} onClick={() => void act(() => harnessApi.goalAction(agent, "start"))}>
                  {unmet ? "Go on to the next" : met > 0 || active ? "Resume" : "Start"}
                </Button>
              )}
              {unmet && !active && (
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
              {pending.length > 0 && (
                <Button size="sm" variant="ghost" disabled={busy} onClick={() => setEditing(pending.map((p) => p.text).join("\n"))}>
                  Edit
                </Button>
              )}
              {asking ? (
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
