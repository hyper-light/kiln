/** Attention plan 01a199e1b177752e84dad5dc990e6a15: one activity classification for the list and chat header. */
import type { AgentInfo, State, Status } from "@/api/harness";
import { taskLive } from "./derive";
import { idleWorkFinished, subagentsAtWork, subagentTasksAtWork } from "./tasks";

export type ChatStand = "needs" | "crashed" | "paused" | "working" | "finished" | "ready" | "offline" | "ended";
type ChatInfo = Pick<AgentInfo, "status" | "approvals" | "held" | "exit" | "pause" | "frozen" | "queued" | "room_wait" | "tasks_running" | "left_behind" | "subagents">;

/** Requests stay visible; pauses hold live work; queued input and room admission are work still to do. */
export function chatStand(a: ChatInfo, stateHasWork = false, subagents = subagentsAtWork(a.subagents).length): ChatStand {
  if (a.approvals.length > 0 || a.status === "waiting" || a.held) return "needs";
  if (a.status === "exited") return a.exit?.again_at != null ? "working" : a.exit?.error ? "crashed" : "ended";
  if (a.status === "disconnected") return "offline";
  if (a.frozen != null || (a.pause && a.status !== "running")) return "paused";
  if (a.status === "running" || a.status === "starting" || a.queued > 0 || a.room_wait != null || subagents > 0) return "working";
  return idleWorkFinished(a, stateHasWork) ? "finished" : "ready";
}

export type BorderState = "active" | "needs" | "stopped" | "finished" | "ready" | "disconnected";

/** The socket's State owns optional live flags, including their removal; Info is the current roster's ownership view. */
export function turnSignal(status: Status, conn: string, state: State | null, info: AgentInfo | null, kiln: number, family: AgentInfo | null): { tone: BorderState; label: string } {
  if (status === "disconnected" || (conn !== "open" && status !== "exited")) return { tone: "disconnected", label: conn === "reconnecting" ? "Reconnecting…" : "Disconnected" };
  const subs = (state ? subagentTasksAtWork(state.tasks).length : subagentsAtWork(info?.subagents).length) + kiln;
  const pendingDelivery = state && Object.values(state.delivery).some((delivery) => delivery === "sent" || delivery === "queued");
  const activity: ChatInfo = {
    status,
    approvals: state ? state.approvals : (info?.approvals ?? []),
    held: info?.held,
    exit: state ? state.exit : info?.exit,
    pause: state ? state.pause : info?.pause,
    frozen: state ? state.frozen : info?.frozen,
    queued: Math.max(info?.queued ?? 0, state?.queue.length ?? 0, pendingDelivery ? 1 : 0),
    room_wait: state ? state.room_wait : info?.room_wait,
    tasks_running: family?.tasks_running ?? 0,
    left_behind: Math.max(family?.left_behind ?? 0, state?.left_behind ?? 0),
  };
  const stand = chatStand(activity, family === null || state?.tasks.some(taskLive), subs);
  switch (stand) {
    case "needs":
      return { tone: "needs", label: "Waiting for you" };
    case "paused":
      return { tone: "stopped", label: activity.frozen != null ? "Paused" : "Paused at a usage limit" };
    case "working":
      if (status === "exited") return { tone: "active", label: "Starting again…" };
      if (status === "starting") return { tone: "active", label: (state ? state.harness_session : info?.harness_session) ? "Resuming…" : "Starting…" };
      if (status === "idle" && activity.room_wait != null) return { tone: "active", label: "Waiting for room" };
      if (status === "idle" && activity.queued > 0) return { tone: "active", label: "Message queued" };
      return { tone: "active", label: status === "idle" && subs > 0 ? `Working · ${subs} subagent${subs === 1 ? "" : "s"}` : "Working" };
    case "crashed":
    case "ended":
      return { tone: "stopped", label: "Stopped" };
    case "offline":
      return { tone: "disconnected", label: "Disconnected" };
    case "finished":
    case "ready":
      return { tone: stand, label: stand === "finished" ? "Finished" : "Ready" };
  }
}
