/**
 * An agent's goal stack: goals one after another, each going to the agent once the one before is
 * met, as if you gave it /goal again and again yourself. On a harness with goals of its own each
 * goes as its /goal and it says when one is met; on the rest each goes as a message and is done
 * when that turn ends. A goal that isn't met pauses the stack, with why. The server drives it, so
 * it goes on with the console closed.
 *
 * In the page's flow: the goals as a list (what's met, under way, next), a field to add one, and
 * the stack's few actions; editing the goals not yet started is one text box, a goal a line.
 */
import * as React from "react";
import { harnessApi, type GoalItem, type GoalStack } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { errorMessage, useResource } from "@/lib/hooks";
import "./goals.css";

const MARK: Record<GoalItem["status"], string> = { met: "✓", active: "●", pending: "○", unmet: "✕", skipped: "–" };
const WORD: Record<GoalItem["status"], string> = { met: "met", active: "under way", pending: "next", unmet: "not met", skipped: "skipped" };

export function Goals({ agent, harness }: { agent: string; harness?: string }) {
  const res = useResource(() => harnessApi.goals(agent), [agent], { pollMs: 2500 });
  const [adding, setAdding] = React.useState("");
  const [editing, setEditing] = React.useState<string | null>(null);
  const [asking, setAsking] = React.useState(false);
  const [earlier, setEarlier] = React.useState(false);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const stack = res.data ?? null;
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

  const act = async (f: () => Promise<GoalStack>) => {
    setBusy(true);
    setError(null);
    try {
      const s = await f();
      res.setData(() => s);
      return true;
    } catch (e) {
      setError(errorMessage(e));
      return false;
    } finally {
      setBusy(false);
    }
  };
  const add = async () => {
    const lines = adding
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean);
    if (lines.length && (await act(() => harnessApi.addGoals(agent, lines)))) setAdding("");
  };
  const saveEdit = async () => {
    if (editing === null) return;
    const lines = editing
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean);
    // Lines keep the goal that was on them; the rest are new.
    const goals = lines.map((text, i) => ({ id: pending[i]?.id, text }));
    if (await act(() => harnessApi.setGoals(agent, goals))) setEditing(null);
  };

  if (!stack) return res.error ? <p className="in-none">{errorMessage(res.error)}</p> : null;
  const how = (active ?? items.filter((i) => i.how).at(-1))?.how;
  const sent =
    how === "message"
      ? "Each goes as a message; the next once its turn ends."
      : how
        ? `Each goes as ${harness ?? "the harness"}'s own /goal; the next once it's met.`
        : `Each goes to ${harness ?? "the agent"} once the one before is met: as its own /goal where it has one.`;

  return (
    <div className="gs">
      {items.length > 0 && (
        <>
          <p className="gs-state" data-state={stack.state}>
            {stack.state === "running" ? "Running" : stack.state === "done" ? "Done" : "Paused"} · {met} of {items.length} met
            {stack.state === "paused" && stack.reason ? <span className="gs-reason">{stack.reason}</span> : null}
          </p>
          {editing === null ? (
            <ol className="gs-list">
              {(folded > 0 || earlier) && done.length > 3 && (
                <li className="gs-more">
                  <button type="button" className="gs-more-btn" onClick={() => setEarlier(!earlier)}>
                    {earlier ? "Hide the earlier ones" : `${folded} earlier`}
                  </button>
                </li>
              )}
              {shown.map((g) => (
                <li key={g.id} className="gs-item" data-status={g.status}>
                  <span className="gs-mark" aria-hidden="true">
                    {MARK[g.status]}
                  </span>
                  <span className="gs-text">
                    {g.text}
                    <span className="gs-word">
                      {WORD[g.status]}
                      {g.reason && g.status !== "pending" ? ` · ${g.reason}` : ""}
                    </span>
                  </span>
                </li>
              ))}
            </ol>
          ) : (
            <div className="gs-edit">
              <label className="field-label" htmlFor={`gs-edit-${agent}`}>
                Goals not yet started, one a line
              </label>
              <textarea id={`gs-edit-${agent}`} className="gs-area" value={editing} onChange={(e) => setEditing(e.target.value)} rows={Math.max(3, editing.split("\n").length + 1)} />
              <div className="gs-actions">
                <Button size="sm" disabled={busy} onClick={() => void saveEdit()}>
                  Save goals
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setEditing(null)}>
                  Cancel
                </Button>
              </div>
            </div>
          )}
        </>
      )}
      {editing === null && (
        <>
          <div className="gs-add">
            <label className="sr-only" htmlFor={`gs-add-${agent}`}>
              Add a goal
            </label>
            <textarea
              id={`gs-add-${agent}`}
              className="gs-area gs-add-field"
              rows={1}
              value={adding}
              placeholder={items.length ? "Another goal (a line each)" : "A goal: a condition to meet, e.g. the parser tests pass"}
              onChange={(e) => setAdding(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  void add();
                }
              }}
            />
            <Button size="sm" variant="secondary" disabled={busy || !adding.trim()} onClick={() => void add()}>
              Add
            </Button>
          </div>
          <p className="gs-how">{sent}</p>
          {items.length > 0 && (
            <div className="gs-actions">
              {stack.state !== "running" && (pending.length > 0 || active) && (
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
              {stack.state === "running" && (
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
              {items.length > 0 &&
                (asking ? (
                  <span className="gs-confirm" role="group" aria-label="Clear the goals">
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
                ))}
            </div>
          )}
        </>
      )}
      {error && <p className="gs-error">{error}</p>}
    </div>
  );
}
