/**
 * A goal being planned now, by a subagent of the agent's (Make a plan, Plan it now), as the agent's
 * session shows it (goal-stack.ts `plannerOf`): how far it has got in dots (the research, the steps,
 * ready), its latest call (cut to fit), who's at it; or, stopped with the plan not ready, why and
 * planning it again. Under the planning step, on the goal's line; the step's own glyph turns while
 * the subagent works, and the dot of the stage it's at.
 */
import type * as React from "react";
import { relativeTime } from "@/lib/format";
import { callWords, type Planner } from "./goal-stack";
import { TaskElapsed } from "./timeline";

/** `wrote`: the plan has its steps; `again`: the subagent stopped short, what plans it again (shown by why). */
type Props = { p: Planner; agent: string; wrote: boolean; onOpen?: (toolId: string) => void; again?: React.ReactNode };

/** How far the planning has got, in the console's status dots: the research, then the steps, then ready. */
function Dots({ p, wrote }: { p: Planner; wrote: boolean }) {
  const at = p.kind === "asked" ? "next" : p.kind === "ended" ? "stopped" : "now";
  const stages = [
    { word: "Research", at: wrote ? "done" : at },
    { word: "Steps", at: wrote ? at : "later" },
    { word: "Ready", at: "later" },
  ];
  const dot: Record<string, string | undefined> = { done: "succeeded", now: "running", next: "queued", later: "skipped" };
  return (
    <ol className="gp-dots" aria-label="How far the planning has got">
      {stages.map((s) => (
        <li key={s.word} data-at={s.at}>
          <i className="dot" data-status={dot[s.at]} aria-hidden="true" />
          {s.word}
        </li>
      ))}
    </ol>
  );
}

/** Who's at it, for how long, and the subagent to open. */
function Meta({ p, agent, onOpen }: Props) {
  if (p.kind === "asked")
    return (
      <span className="gp-live-meta">
        Asked {agent} for a subagent · {relativeTime(p.at / 1000)}
      </span>
    );
  const { task } = p;
  return (
    <span className="gp-live-meta">
      {p.kind === "planning" ? "Subagent" : `Subagent stopped ${relativeTime((task.ended ?? task.started) / 1000)}`}
      {task.tool_uses ? ` · ${task.tool_uses} calls` : ""}
      {p.kind === "planning" && (
        <>
          {" · "}
          <TaskElapsed task={task} />
        </>
      )}
      {onOpen && task.tool_id && (
        <>
          {" · "}
          <button type="button" className="gp-open" onClick={() => onOpen(task.tool_id!)}>
            Open
          </button>
        </>
      )}
    </span>
  );
}

export function PlannerLive(props: Props) {
  const { p, wrote, again } = props;
  const last = p.kind === "asked" ? undefined : p.calls.at(-1);
  return (
    <div className="gp-live" data-kind={p.kind}>
      <Dots p={p} wrote={wrote} />
      {p.kind === "ended" && p.task.summary ? <p className="gp-live-why">{p.task.summary}</p> : last && <p className="gp-last">{callWords(last)}</p>}
      <Meta {...props} />
      {again && <span className="gp-again">{again}</span>}
    </div>
  );
}
