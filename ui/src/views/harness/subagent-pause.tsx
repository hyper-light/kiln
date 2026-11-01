/**
 * A subagent's pause, wherever its row or thread is (Mission control's tree, the composer's, over
 * its thread): Pause while it goes, Resume its own pause (`Command::Pause`). One kiln started is
 * paused whole, its processes held where they are (and the subagents kiln started under it); one of
 * its harness's own is held at its next tool call, by its agent's session. Off where it can't be: it
 * ended, its harness can't hold it, or a pause over it holds it.
 */
import * as React from "react";
import { Pause, Play } from "lucide-react";
import { harnessApi } from "@/api/harness";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import type { SubagentNode } from "./subagent-tree";

export function PauseButton({ n, className, size = 14 }: { n: Pick<SubagentNode, "kind" | "session" | "task" | "name" | "paused" | "pausable">; className?: string; size?: number }) {
  const [busy, setBusy] = React.useState(false);
  const own = n.paused === "own";
  const go = () => {
    setBusy(true);
    harnessApi
      .command(n.session, { type: "pause", on: !own, ...(n.kind === "harness" && n.task ? { task: n.task } : {}) })
      .catch((e: unknown) => toast.error(own ? `Couldn't resume ${n.name}` : `Couldn't pause ${n.name}`, errorMessage(e)))
      .finally(() => setBusy(false));
  };
  return (
    <button type="button" className={className} disabled={!n.pausable || busy} aria-label={`${own ? "Resume" : "Pause"} ${n.name}`} onClick={go}>
      {own ? <Play size={size} aria-hidden="true" /> : <Pause size={size} aria-hidden="true" />}
    </button>
  );
}
