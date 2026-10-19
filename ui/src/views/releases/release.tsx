/**
 * One release: what it does and where, how far it has come, each gate's result with its evidence,
 * people's answers, and what its provider last said. While it waits for approval the answer is
 * here; while it is on its way it can be cancelled (one being delivered is reverted in git).
 */
import * as React from "react";
import { Link, useParams } from "@/lib/router";
import { CircleCheck, CircleX } from "lucide-react";

import { boardHref, deployApi, describe, inFlight, releaseAppHref, releaseHref, STATE_WORDS, type Release } from "@/api/deploy";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { Loadable } from "@/components/ui/loading";
import { Crumbs, Facts, Panel } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage } from "@/lib/hooks";
import { invalidate, useSuspenseResource } from "@/lib/resource";
import { RelTime } from "@/views/shared";
import { Answer, DEPLOY_EVENTS, ReleaseState } from "./index";
import "@/views/deployments/deployments.css";
import "./releases.css";

function Head({ r }: { r: Release | null; id: number }) {
  return (
    <header className="cd-head">
      <Crumbs
        index={3}
        items={[{ label: "Deployments", to: "/deployments" }, { label: "Releases", to: boardHref }, ...(r ? [{ label: r.app, to: releaseAppHref(r.app) }] : []), { label: r ? `#${r.id}` : "…" }]}
      />
      {r && (
        <>
          <h1 className="cd-title">
            {describe(r)} <span className="rel-to">→ {r.environment}</span>
          </h1>
          <p className="cd-note">
            <ReleaseState r={r} />
            {r.message && <span className="rel-msg-inline"> · {r.message}</span>}
          </p>
        </>
      )}
    </header>
  );
}

function Gates({ r }: { r: Release }) {
  if (!r.gates?.length) return null;
  return (
    <Panel eyebrow="Gates" title="What it waited for" description="Each gate's result, as the release last saw it, with what it rests on.">
      <ul className="rel-gates">
        {r.gates.map((g) => (
          <li key={g.gate} data-passed={g.passed}>
            {g.passed ? <CircleCheck aria-label="passed" /> : <CircleX aria-label="not passed" />}
            <div className="min-w-0">
              <span className="rel-gate-name">{g.gate}</span>
              <span className="rel-gate-detail">{g.detail}</span>
            </div>
          </li>
        ))}
      </ul>
    </Panel>
  );
}

function Answers({ r }: { r: Release }) {
  const waiting = r.state === "waiting_approval";
  if (!waiting && !r.approvals?.length) return null;
  return (
    <Panel eyebrow="Approvals" title={waiting ? "It waits for an answer" : "Answers"} actions={waiting ? <Answer r={r} /> : undefined}>
      {r.approvals?.length ? (
        <ul className="rel-answers">
          {r.approvals.map((a) => (
            <li key={`${a.by}-${a.at}`}>
              <span data-approved={a.approved}>{a.approved ? "approved" : "rejected"}</span> by {a.by} · <RelTime ts={a.at} />
              {a.note && <q>{a.note}</q>}
            </li>
          ))}
        </ul>
      ) : (
        <p className="rel-quiet">No answer yet.</p>
      )}
    </Panel>
  );
}

function ReleaseView({ id }: { id: number }) {
  const r = useSuspenseResource(`deploy/release/${id}`, (s) => deployApi.release(id, s), { on: DEPLOY_EVENTS }).data;
  const [asking, setAsking] = React.useState(false);
  const cancellable = inFlight(r.state) && r.state !== "committing";
  const cancel = async () => {
    try {
      await deployApi.cancel(r.id);
      toast({ title: `Cancelled release ${r.id}`, description: r.state === "delivering" ? "Its commit is reverted, and its provider goes back to what ran before." : undefined, tone: "info" });
      invalidate("deploy/");
    } catch (e) {
      toast({ title: "Not cancelled", description: errorMessage(e), tone: "failed" });
    }
  };
  const facts = [
    { label: "App", value: <Link to={releaseAppHref(r.app)}>{r.app}</Link> },
    { label: "Environment", value: r.environment },
    { label: "Kind", value: r.kind },
    ...(r.version ? [{ label: "Version", value: <code>{r.version}</code> }] : []),
    { label: "Asked by", value: `${r.requested_by}${r.origin === "git" ? " (a commit kiln didn't make)" : ""}` },
    { label: "Asked", value: <RelTime ts={r.created_at} /> },
    ...(r.committed_at ? [{ label: "Committed", value: <RelTime ts={r.committed_at} /> }] : []),
    ...(r.ended_at ? [{ label: "Ended", value: <RelTime ts={r.ended_at} /> }] : []),
    ...(r.commit ? [{ label: "Commit", value: <code title={r.commit}>{r.commit.slice(0, 12)}</code> }] : []),
    ...(r.promoted_from ? [{ label: "Promotes", value: <Link to={releaseHref(r.promoted_from)}>release {r.promoted_from}</Link> }] : []),
    ...(r.rollback_of ? [{ label: "Back to", value: <Link to={releaseHref(r.rollback_of)}>release {r.rollback_of}</Link> }] : []),
    ...(r.rolled_back_by ? [{ label: "Rolled back by", value: <Link to={releaseHref(r.rolled_back_by)}>release {r.rolled_back_by}</Link> }] : []),
  ];
  return (
    <>
      <Head r={r} id={id} />
      <div className="rel-grid">
        <Panel
          eyebrow={`Release ${r.id}`}
          title={STATE_WORDS[r.state]}
          actions={
            cancellable ? (
              <Button size="sm" variant="danger" onClick={() => setAsking(true)}>
                Cancel
              </Button>
            ) : undefined
          }
        >
          <Facts items={facts} />
          {r.artifacts && Object.keys(r.artifacts).length > 0 && (
            <ul className="rel-artifacts">
              {Object.entries(r.artifacts).map(([name, a]) => (
                <li key={name}>
                  <span>{name}</span> <code>{a.digest ? `${a.repository}@${a.digest}` : `${a.repository}:${a.tag}`}</code>
                </li>
              ))}
            </ul>
          )}
        </Panel>
        <Answers r={r} />
        <Gates r={r} />
        {r.values && (
          <Panel eyebrow="Values" title="The environment's values.yaml it sets">
            <pre className="rel-pre">{r.values}</pre>
          </Panel>
        )}
        {r.provider && (
          <Panel eyebrow="Provider" title="What its provider last said">
            <pre className="rel-pre">{JSON.stringify(r.provider, null, 2)}</pre>
          </Panel>
        )}
      </div>
      <ConfirmDialog
        open={asking}
        onOpenChange={setAsking}
        title={`Cancel release ${r.id}?`}
        description={r.state === "delivering" ? "It is being delivered: its commit is reverted, and its provider goes back to what ran before." : "It hasn't been committed: nothing changes."}
        confirmLabel="Cancel the release"
        danger
        onConfirm={cancel}
      />
    </>
  );
}

export default function ReleasePage() {
  const id = Number(useParams().id);
  return (
    <div className="container-console page cd-page">
      <title>{`Release ${id} · kiln`}</title>
      <Loadable label="the release" fallback={<Head r={null} id={id} />}>
        <ReleaseView id={id} />
      </Loadable>
    </div>
  );
}
