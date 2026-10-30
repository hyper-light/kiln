/**
 * How fresh a change is on its trunk (docs/ci.md W26-R16), under the Files section's gate row: the
 * planner's words ("replays clean on main@4c1e2a"), where a replay's checks come from ("checks from
 * v4": a replay runs none of its own), and Update: a change request's branch moved to its replay, a
 * host's pull request updated on its host. Update stays in place: disabled, saying why, when it
 * can't go; once pressed, what it did takes the words' place, so nothing moves.
 */
import * as React from "react";
import { GitBranch, RefreshCw } from "lucide-react";
import type { Freshness } from "@/api/review";
import { invalidate } from "@/lib/resource";
import "./review.css";

const said = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function FreshRow({ fresh, update: go }: { fresh: Freshness; /** Update, and what it did in words. */ update: () => Promise<string> }) {
  const [busy, start] = React.useTransition();
  const [err, setErr] = React.useState<string | null>(null);
  const [moved, setMoved] = React.useState<string | null>(null);
  const update = () =>
    start(async () => {
      setErr(null);
      try {
        setMoved(await go());
        invalidate("review/");
        invalidate("ship/");
      } catch (e) {
        setErr(said(e));
      }
    });
  const why = moved ?? fresh.update.why_not ?? undefined;
  return (
    <>
      <div className="rv-bar" data-fresh="">
        <span className="rv-ks">
          <span className="rv-k" data-k="fresh" title={`Tip ${fresh.tip}`}>
            <RefreshCw size={12} aria-hidden="true" />
            <b>{moved ?? fresh.words}</b>
          </span>
          {fresh.checks_from != null && (
            <span className="rv-k" data-k="checks" title="A replay runs no checks of its own">
              <b>checks from v{fresh.checks_from}</b>
            </span>
          )}
        </span>
        <button type="button" className="rv-btn" disabled={busy || !fresh.update.can || moved != null} title={why} aria-label={why ? `Update: ${why}` : "Update"} onClick={update}>
          <GitBranch size={13} aria-hidden="true" />
          <span>Update</span>
        </button>
      </div>
      {err && (
        <p className="rvc-error" role="alert">
          {err}
        </p>
      )}
    </>
  );
}
