/**
 * A repository's land queue (W14-R08, W21-R09): what waits to land on its trunk, the batch under
 * test, and what was ejected and why. Your own entries can be taken out.
 */
import * as React from "react";
import { Link, useSearchParams } from "@/lib/router";
import { api } from "@/api/client";
import { reviewApi, whoSaid, type QueueEntry } from "@/api/review";
import { Button } from "@/components/ui/button";
import { Loadable, Loading } from "@/components/ui/loading";
import { PageIntro, SelectField } from "@/components/ui/misc";
import { StatusDot } from "@/components/ui/status";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { relativeTime } from "@/lib/format";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import "./review.css";

const TONE: Record<string, string> = { queued: "queued", testing: "running", landed: "succeeded", ejected: "failed", cancelled: "cancelled" };

export default function QueuePage() {
  return (
    <div className="container-console page rv-page">
      <title>Land queue · Review · kiln</title>
      <PageIntro index={4} label="Repositories / Review / Land queue" title="Land queue." lead="What lands next on each trunk, the batch under test, and what was sent back." />
      <Loadable label="Loading the queue" fallback={<Loading label="Loading the queue" block />}>
        <QueueBody />
      </Loadable>
    </div>
  );
}

function QueueBody() {
  const repos = useRes({ key: "review/repos", fetch: (s) => api.repos(s) }).data;
  const [params, setParams] = useSearchParams();
  const repo = params.get("repo") ?? repos[0]?.name ?? "";
  return (
    <>
      <div className="rv-bar">
        <SelectField
          label="Repository"
          value={repo}
          onChange={(v) => {
            const n = new URLSearchParams(params);
            n.set("repo", v);
            setParams(n, { replace: true });
          }}
          options={repos.map((r) => ({ value: r.name, label: r.name }))}
        />
      </div>
      {repo ? (
        <Loadable label="Loading the queue" fallback={<Loading label="Loading the queue" block />}>
          <Entries repo={repo} />
        </Loadable>
      ) : (
        <p className="rv-sub">kiln hosts no repositories yet.</p>
      )}
    </>
  );
}

function Entries({ repo }: { repo: string }) {
  const entries: QueueEntry[] = useRes({ key: `review/queue/${repo}`, fetch: (s) => reviewApi.queue(repo, s), opts: { on: ["queue", "landed"] } }).data;
  const [busy, setBusy] = React.useState<string | null>(null);
  if (entries.length === 0) return <p className="rv-sub">Nothing is waiting to land on {repo}.</p>;
  const out = async (id: string) => {
    setBusy(id);
    try {
      await reviewApi.dequeue(id);
      invalidate(`review/queue/${repo}`);
    } catch (e) {
      toast.error("It stays in the queue", errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  return (
    <ul className="rv-inbox-list">
      {entries.map((e) => (
        <li key={e.id} className="rv-queue-row">
          <StatusDot status={TONE[e.state] ?? e.state} title={e.state} />
          <div className="min-w-0 flex-1">
            <p className="rv-inbox-what">
              {e.diffs.map((d, i) => (
                <React.Fragment key={d}>
                  {i > 0 && ", "}
                  <Link to={`/review/${d}`}>{d}</Link>
                </React.Fragment>
              ))}{" "}
              <span className="rv-sub">
                onto {e.trunk} · {e.strategy.replace(/-/g, " ")} · {e.state}
                {e.batch ? " · in the batch under test" : ""}
              </span>
            </p>
            <p className="rv-sub">
              {whoSaid(e.enqueued_by)} · {relativeTime(e.created_at)}
              {e.reason ? ` · ${e.reason}` : ""}
            </p>
          </div>
          {(e.state === "queued" || e.state === "testing") && (
            <Button size="xs" variant="ghost" disabled={busy !== null} onClick={() => void out(e.id)}>
              Take it out
            </Button>
          )}
        </li>
      ))}
    </ul>
  );
}
