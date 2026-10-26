/**
 * Where an agent's change request stands, in a word (docs/agentic.md C1–C4): from work not up for
 * review yet, through its checks and its review, to merged or closed.
 */
import type { AgentChange } from "@/api/changes";

export interface ChangeState {
  key: "pending" | "pushed" | "checking" | "failed" | "fixing" | "changes" | "review" | "approved" | "merged" | "closed";
  word: string;
  /** The status dot that shows it (`.dot[data-status]`). */
  dot: "pending" | "running" | "failed" | "timed_out" | "succeeded" | "cancelled";
}

/** What checks say while they haven't finished. */
const UNDER_WAY = new Set(["running", "queued", "pending", "in_progress", "waiting", "requested", "started"]);

/**
 * Its state. None yet: pending. While it's open, what it waits on first: failing checks (fixing,
 * when the agent fixes them by itself), changes asked for, checks still running; then its pull
 * request not opened yet (pushed), approved, or waiting for a review.
 */
export function changeState(c: AgentChange | undefined): ChangeState {
  if (!c) return { key: "pending", word: "Pending", dot: "pending" };
  if (c.state === "landed") return { key: "merged", word: "Merged", dot: "succeeded" };
  if (c.state === "closed") return { key: "closed", word: "Closed", dot: "cancelled" };
  if (c.checks === "failed") return c.fix_checks ? { key: "fixing", word: "Fixing", dot: "running" } : { key: "failed", word: "Failed", dot: "failed" };
  if (c.review === "changes-requested") return { key: "changes", word: "Changes requested", dot: "timed_out" };
  if (c.checks && UNDER_WAY.has(c.checks)) return { key: "checking", word: "Checking", dot: "running" };
  if (!c.change) return { key: "pushed", word: "Pushed", dot: "pending" };
  if (c.review === "approved") return { key: "approved", word: "Approved", dot: "succeeded" };
  return { key: "review", word: "In review", dot: "pending" };
}
