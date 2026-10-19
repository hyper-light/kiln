/**
 * A stack of diffs (W14-R03, W21-R09): bottom to top above its trunk, each with where it stands;
 * landing one lands it and every diff below it, in order.
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";
import { reviewApi, STATE_WORD, whoSaid, type Stack } from "@/api/review";
import { Button } from "@/components/ui/button";
import { Loadable, Loading } from "@/components/ui/loading";
import { toast } from "@/components/ui/toast";
import { useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import "./review.css";

export default function StackPage() {
  const { id = "" } = useParams();
  return (
    <div className="container-console page rv-page">
      <title>Stack · Review · kiln</title>
      <Loadable label="Loading the stack" fallback={<Loading label="Loading the stack" block />}>
        <StackBody id={id} />
      </Loadable>
    </div>
  );
}

function StackBody({ id }: { id: string }) {
  const s: Stack = useRes({ key: `review/stack/${id}`, fetch: (sig) => reviewApi.stack(id, sig), opts: { on: ["diff", "diff_version", "review", "landed", "queue"] } }).data;
  const [busy, setBusy] = React.useState<string | null>(null);
  const land = async (name: string) => {
    setBusy(name);
    try {
      await reviewApi.land(name);
      toast({ title: `${name} and the diffs below it are in the land queue`, tone: "running" });
      invalidate(`review/stack/${id}`);
    } catch (e) {
      toast.error(`${name} can't land yet`, errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  // Top first, as it reads: the newest work above what it builds on.
  const top = [...s.diffs].reverse();
  return (
    <>
      <header className="rv-head">
        <p className="rv-eyebrow">
          <Link to="/repos">Repositories</Link> / <Link to="/review">Review</Link> / {s.repo} / stack
        </p>
        <h1 className="rv-title">
          {s.diffs.length} diff{s.diffs.length === 1 ? "" : "s"} onto {s.trunk}
        </h1>
      </header>
      <ol className="rv-stack">
        {top.map((d) => (
          <li key={d.id} className="rv-stack-row">
            <Link to={`/review/${d.name}`} className="rv-inbox-row">
              <span className="rv-inbox-what">
                <b>{d.name}</b> {d.title}
              </span>
              <span className="rv-sub">
                <span className="rv-state" data-state={d.state}>
                  {STATE_WORD[d.state] ?? d.state}
                </span>{" "}
                · {whoSaid(d.author)}
              </span>
            </Link>
            {d.state !== "landed" && d.state !== "abandoned" && (
              <Button size="xs" variant="ghost" disabled={busy !== null} onClick={() => void land(d.name)}>
                {busy === d.name ? "Landing…" : "Land to here"}
              </Button>
            )}
          </li>
        ))}
        <li className="rv-stack-trunk rv-sub">{s.trunk}</li>
      </ol>
    </>
  );
}
