/** Ship's read-only review intent path (W26-R15), independent of the diff rendering code. */
import { agentFixesRes, commitFilesRes, commentsRes, conflictsRes, diffRes, filesRes, hostPullFreshRes, hostPullRes, inboxRes, readinessRes, verdictsRes } from "@/api/review";
import type { ShipCommit } from "@/api/ship";
import { warm } from "@/lib/data";

/** A host's pull request's number, from its change's name ("!6", "#12"). */
export const pullNumber = (name: string) => Number(name.replace(/\D/g, ""));

/** Start every review key at once, before any Suspense read can wait for another. In particular,
 * landing readiness must not wait for the diff, comments, files and conflicts to finish first. */
export function warmReview(item: ShipCommit) {
  const ch = item.change;
  if (ch && ch.host === "kiln")
    warm(inboxRes, diffRes(ch.name), commentsRes(ch.name), filesRes(ch.name, ch.revision), conflictsRes(ch.name), readinessRes(ch.name), verdictsRes(ch.name), agentFixesRes(item.repo));
  else if (ch && ch.host) warm(inboxRes, hostPullRes(item.repo, pullNumber(ch.name)), hostPullFreshRes(item.repo, pullNumber(ch.name)), agentFixesRes(item.repo));
  else warm(inboxRes, commitFilesRes(item.repo, item.sha), agentFixesRes(item.repo));
}
