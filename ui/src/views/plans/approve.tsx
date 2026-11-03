/**
 * A plan its agent's goals wait on your review of goes one way: approved, its steps go to that agent
 * as its goal's parts (docs/plans.md, "Reviewing a plan"). Where plans are run (a plan's page, a
 * saved plan in a Plan tab), such a plan has Approve in Run's place, never a run of its own beside
 * those steps; it's rejected or answered in your own words where it's reviewed, its agent's Plan tab.
 */
import * as React from "react";
import { clientId, type AgentInfo } from "@/api/harness";
import { plansApi, type Plan } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { harnessSessionsRes, useRes } from "@/lib/data";
import { errorMessage } from "@/lib/hooks";
import { invalidate } from "@/lib/resource";
import { Link } from "@/lib/router";
import { Tries } from "@/lib/tries";
import { agentLabel } from "@/views/harness/derive";

/** The agent whose goals wait on your review of plan `id`, while they do (as the agents list has it, kept up to date by the server's events). */
export function useWaitsOn(id: string): AgentInfo | undefined {
  return useRes(harnessSessionsRes).data.find((a) => a.held?.plan === id);
}

/** Approved here: whose goals went on, with how many of its steps. */
export interface Approved {
  agent: string;
  name: string;
  steps: number;
}

const stepWords = (n: number) => `${n} step${n === 1 ? "" : "s"}`;

/**
 * In Run's place while `a`'s goals wait on your review of `plan`: what approving does, and Approve
 * (for the version shown). `here`: its review is on this page, so it isn't linked. Approved, it
 * says so (`approved`, kept by whoever shows this, as the agent's goals go on).
 */
export function ApproveInstead({
  plan,
  a,
  approved,
  onApproved,
  here,
  dirty,
}: {
  plan: Plan;
  a?: AgentInfo;
  approved: Approved | null;
  onApproved: (x: Approved) => void;
  here?: boolean;
  dirty?: boolean;
}) {
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  // Asked again (its answer lost, or it failed), the same approval under its key (CP-T3.2).
  const [tries] = React.useState(() => new Tries(clientId));
  const steps = plan.items.length || plan.steps.length;
  if (approved)
    return (
      <p className="plan-ok">
        Approved: its {stepWords(approved.steps)} go to {approved.name} as its goal&rsquo;s parts · <Link to={`/agents/${encodeURIComponent(approved.agent)}`}>open the agent</Link>
      </p>
    );
  if (!a) return null;
  const name = agentLabel(a);
  const approve = () => {
    setBusy(true);
    setError(null);
    void plansApi.review(plan.id, { answer: "approve", version: plan.version, key: tries.keyFor(`approve ${plan.id} v${plan.version}`) }).then(
      () => {
        tries.done();
        invalidate(`plans/${plan.id}`);
        invalidate(`harness/goals/${a.id}`);
        onApproved({ agent: a.id, name, steps });
      },
      (e: unknown) => {
        setError(errorMessage(e));
        setBusy(false);
      },
    );
  };
  return (
    <>
      <p className="plan-dim">
        {name}&rsquo;s goals wait on your review of this plan. Approved, its {stepWords(steps)} go to {name} as its goal&rsquo;s parts: it runs no other way.
        {!here && (
          <>
            {" "}
            To reject it or ask for changes, <Link to={`/agents/${encodeURIComponent(a.id)}?tab=plan`}>review it in its Plan tab</Link>.
          </>
        )}
      </p>
      <div className="plan-actions">
        <Button size="sm" disabled={busy || dirty} onClick={approve}>
          {busy ? "Approving…" : "Approve"}
        </Button>
        {dirty && <span className="plan-dim">Save your changes first: what&rsquo;s approved is the saved version.</span>}
        {error && <span className="plan-error">{error}</span>}
      </div>
    </>
  );
}
