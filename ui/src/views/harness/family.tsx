/** Flat Mission Control rows, plus the End dialog's handling of related agents. */
import * as React from "react";
import "./family.css";
import { ArrowUpRight } from "lucide-react";
import { Link } from "@/lib/router";
import { harnessApi, type SubagentBrief } from "@/api/harness";
import { useSuspenseResource } from "@/lib/resource";
import { taskDot } from "./derive";

const TASK_WORDS: Record<string, string> = { running: "working", pending: "queued", completed: "finished", failed: "failed", paused: "paused", waiting: "needs you", stopped: "stopped", killed: "stopped", cancelled: "cancelled" };

/** Native subagents open their existing thread; they have no independent session controls. */
export function NativeTask({ agent, task }: { agent: string; task: SubagentBrief }) {
  return (
    <li className="fam-task">
      <Link to={`/agents/${encodeURIComponent(agent)}?task=${encodeURIComponent(task.id)}`} className="fam-task-link">
        <span className="fam-task-name">{task.description || "Subagent"}</span>
        <span className="fam-task-status">
          <i className="dot" data-status={taskDot(task)} aria-hidden="true" />
          {TASK_WORDS[task.status] ?? task.status}
        </span>
        <ArrowUpRight size={16} aria-hidden="true" />
      </Link>
    </li>
  );
}

/** An agent's live children, as the server knows them (theirs too). */
function useChildren(id: string) {
  return useSuspenseResource(`harness/children/${id}`, () => harnessApi.children(id)).data.children;
}

/** In the End dialog: the agents it started that still run, and whether they end with it. */
function EndsWith({ id, on, onChange }: { id: string; on: boolean; onChange: (on: boolean) => void }) {
  const kids = useChildren(id);
  if (kids.length === 0) return null;
  const names = kids.map((k) => k.name).join(", ");
  return (
    <span className="fam-end">
      <span className="fam-end-what">
        It started {kids.length === 1 ? "an agent that's" : `${kids.length} agents that are`} still running: {names}.
      </span>
      <label className="fam-end-check">
        <input type="checkbox" className="check" checked={on} onChange={(e) => onChange(e.target.checked)} />
        End {kids.length === 1 ? "it" : "them"} too
      </label>
    </span>
  );
}

/** What ending agent `id` ends with it: the line for its End dialog, and the end itself. */
export function useEndWithChildren(id: string) {
  const [withKids, setWithKids] = React.useState(true);
  const note = (
    <React.Suspense fallback={null}>
      <EndsWith id={id} on={withKids} onChange={setWithKids} />
    </React.Suspense>
  );
  return { note, end: () => harnessApi.end(id, withKids) };
}
