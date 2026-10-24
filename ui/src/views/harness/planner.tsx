/**
 * A goal being planned now, by a subagent of the agent's (Make a plan, Plan it now), as the agent's
 * session shows it (goal-stack.ts `plannerOf`): asked for, at work (its last calls, a line each,
 * cut to fit, the older fainter), or stopped with the plan not ready (why, and planning it again).
 * Under the planning step, on the goal's line; the step's own glyph turns while the subagent works.
 */
import type * as React from "react";
import { relativeTime } from "@/lib/format";
import { callWords, type Planner } from "./goal-stack";
import { TaskElapsed } from "./timeline";

/** `again`: the subagent stopped short, what plans it again (shown by why). */
type Props = { p: Planner; agent: string; onOpen?: (toolId: string) => void; again?: React.ReactNode };

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
  const { p, again } = props;
  return (
    <div className="gp-live" data-kind={p.kind}>
      {p.kind === "ended" && p.task.summary ? (
        <p className="gp-live-why">{p.task.summary}</p>
      ) : (
        p.kind !== "asked" &&
        p.calls.length > 0 && (
          <ol className="gp-tail" aria-label="The subagent's last calls">
            {p.calls.map((c, i) => (
              <li key={c.id} data-age={p.calls.length - 1 - i}>
                {callWords(c)}
              </li>
            ))}
          </ol>
        )
      )}
      <Meta {...props} />
      {again && <span className="gp-again">{again}</span>}
    </div>
  );
}
