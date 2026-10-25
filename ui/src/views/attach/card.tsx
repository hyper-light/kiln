/**
 * A run's incident card (docs/ci.md W20-R10): what an attached agent was woken for in this run,
 * where that stands and what it found, with its agent a click away. Nothing when no attachment
 * woke for the run, or when the incidents can't be read.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { attachApi, type IncidentBrief } from "@/api/attach";
import { relativeTime } from "@/lib/format";
import { useSuspenseResource } from "@/lib/resource";
import { Quiet } from "@/views/harness/change-bar";
import "./attach.css";

const STATE: Record<IncidentBrief["state"], string> = { open: "its agent is on it", reported: "reported", resolved: "resolved", waiting: "waiting for you", dismissed: "dismissed" };

function Cards({ run }: { run: string }) {
  const list = useSuspenseResource(`incidents/run/${run}`, (s) => attachApi.ofRun(run, s).catch(() => [] as IncidentBrief[]), { on: ["incident"] }).data;
  if (list.length === 0) return null;
  return (
    <section className="attach-cards" aria-label="Incidents">
      {list.map((i) => (
        <div key={i.id} className="attach-card" data-state={i.state}>
          <div className="attach-row-head">
            <span className="attach-name">{i.name}</span>
            <span className="attach-state">
              {STATE[i.state]}
              {i.class ? ` · ${i.class}` : ""}
            </span>
          </div>
          <div className="attach-incident-title">{i.title}</div>
          {i.report && <div className="attach-report">{i.report}</div>}
          {i.why && i.state === "waiting" && <div className="attach-latest">{i.why}.</div>}
          <div className="attach-meta">
            {i.times > 1 ? `${i.times} times · ` : ""}since {relativeTime(i.at)}
            {i.links.length ? ` · ${i.links.join(", ")}` : ""}
            {i.agent && (
              <>
                {" · "}
                <Link to={`/agents/${encodeURIComponent(i.agent)}`}>Open its agent</Link>
              </>
            )}
          </div>
        </div>
      ))}
    </section>
  );
}

/** The incidents run `run` is part of, as cards. */
export function IncidentCard({ run }: { run: string }) {
  return (
    <Quiet>
      <React.Suspense fallback={null}>
        <Cards run={run} />
      </React.Suspense>
    </Quiet>
  );
}
