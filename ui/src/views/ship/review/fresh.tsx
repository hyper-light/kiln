/**
 * How fresh a change is on its trunk (docs/ci.md W26-R16), under the Files section's gate row: the
 * planner's words ("replays clean on main@4c1e2a"), where a replay's checks come from ("checks from
 * v4": a replay runs none of its own), and Update, which moves a change request's branch to its
 * replay. Update stays in place: disabled, saying why, when it can't go.
 */
import * as React from "react";
import { GitBranch, RefreshCw } from "lucide-react";
import { freshApi, type Freshness } from "@/api/review";
import { invalidate } from "@/lib/resource";
import "./review.css";

const said = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function FreshRow({ fresh, diffKey }: { fresh: Freshness; diffKey: string }) {
  const [busy, start] = React.useTransition();
  const [err, setErr] = React.useState<string | null>(null);
  const [moved, setMoved] = React.useState<string | null>(null);
  const update = () =>
    start(async () => {
      setErr(null);
      try {
        const r = await freshApi.update(diffKey);
        setMoved(`${r.branch} moved to v${r.version}`);
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
            <b>{fresh.words}</b>
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
