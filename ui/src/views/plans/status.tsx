/**
 * Where a step stands, drawn as the agent console draws its work (signals.tsx), so a plan's steps
 * and a stack's goals read like the conversation they come from: working, hyperlight's mark with
 * its spectral light turning and its title in the holographic shimmer; needing a person, the
 * optic held in amber; met, the loop closed; not met, the loop broken; skipped, dashed; stopped,
 * faint; waiting, only sketched, still. Only work in progress moves, and only by what the GPU
 * moves.
 */
import { Beam, Optic, Shimmer, type BeamStatus } from "@/views/harness/signals";

/** Where a step (or a goal) stands, as its glyph shows it. */
export type Stand = "waiting" | "working" | "needs_you" | "yours" | "met" | "unmet" | "stopped" | "skipped" | "paused";

export const STAND_WORDS: Record<Stand, string> = {
  waiting: "Waiting",
  working: "Working",
  needs_you: "Needs you",
  yours: "Taken over",
  met: "Met",
  unmet: "Not met",
  stopped: "Stopped",
  skipped: "Skipped",
  paused: "Paused",
};

const BEAM: Record<Exclude<Stand, "needs_you" | "yours">, BeamStatus> = {
  waiting: "pending",
  paused: "pending",
  working: "running",
  met: "done",
  unmet: "error",
  stopped: "cancelled",
  skipped: "denied",
};

/** A step's glyph. In a graph or a stack a waiting one is only sketched, still (plans.css): among many, nothing breathes. */
export function StepGlyph({ stand, className }: { stand: Stand; className?: string }) {
  if (stand === "needs_you" || stand === "yours") return <Optic state="waiting" className={className} />;
  return <Beam status={BEAM[stand]} className={className} />;
}

/** A step's title: in the holographic shimmer while it's worked on, painted into its own letters as a running subagent's are. */
export function StandTitle({ stand, children }: { stand: Stand; children: string }) {
  return <Shimmer on={stand === "working"}>{children}</Shimmer>;
}
