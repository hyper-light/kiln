/**
 * Whether an agent waits on a plan's review: the conversation and its inspector ask on every render;
 * the review itself (review.tsx) loads when its tab opens (goal 8).
 */
import { useSession, type SessionStore } from "@/api/harness";
import { harnessSessionsRes, useRes } from "@/lib/data";
import { stillAsked, useAnswered as useRequestsAnswered } from "@/views/harness/answered";

/** What waits on your review of `store`'s agent's plans, if anything (its Plan tab says so): a plan it proposed, or one its goals wait on. */
export function useReviewWaits(store: SessionStore): "proposed" | "goal" | null {
  const held = Boolean(useRes(harnessSessionsRes).data.find((a) => a.id === store.id)?.held?.plan);
  const asked = useSession(store, (s) => s.state?.approvals);
  const done = useRequestsAnswered();
  if (asked && stillAsked(asked, done).some((a) => a.tool === "plan" && !a.agent && Boolean(a.plan))) return "proposed";
  return held ? "goal" : null;
}
