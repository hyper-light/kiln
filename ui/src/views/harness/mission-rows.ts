/** List-only metadata lets native threads share Mission Control's filters and ordering. */
import type { AgentInfo, SubagentBrief } from "@/api/harness";

export type MissionStand = "needs" | "crashed" | "paused" | "working" | "finished" | "ready" | "offline" | "ended";
export type NativeRow = { agent: string; task: SubagentBrief };
export type MissionRow = { a: AgentInfo; stand: MissionStand; native?: NativeRow };

function nativeState(status: string): { status: AgentInfo["status"]; stand: MissionStand } {
  switch (status) {
    case "running": return { status: "running", stand: "working" };
    case "pending": return { status: "starting", stand: "working" };
    case "waiting": return { status: "waiting", stand: "needs" };
    case "paused": return { status: "idle", stand: "paused" };
    case "completed": return { status: "idle", stand: "finished" };
    case "failed": return { status: "exited", stand: "crashed" };
    case "idle": return { status: "idle", stand: "ready" };
    case "disconnected": return { status: "disconnected", stand: "offline" };
    default: return { status: "exited", stand: "ended" };
  }
}

/** Native view keys never identify executors; navigate and act only through `native`. */
export function missionRows(managed: { a: AgentInfo; stand: MissionStand }[]): MissionRow[] {
  return managed.flatMap((row): MissionRow[] => [
    row,
    ...(row.a.subagents ?? []).map((task): MissionRow => {
      const owner = row.a;
      const { status, stand } = nativeState(task.status);
      const name = task.description.trim() || "Subagent";
      return {
        native: { agent: owner.id, task },
        stand,
        a: {
          id: `native:${encodeURIComponent(owner.id)}:${encodeURIComponent(task.id)}`,
          name,
          title: name,
          status,
          created_at: task.started,
          last_activity: task.ended ?? task.started,
          target: owner.target,
          kind: owner.kind,
          kind_name: owner.kind_name,
          cwd: owner.cwd,
          model: owner.model,
          mode: owner.mode,
          approvals: [],
          queued: 0,
          tasks_running: 0,
          cost_usd: 0,
        },
      };
    }),
  ]);
}
