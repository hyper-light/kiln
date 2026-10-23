/**
 * An agent's own plan, in its Plan tab: its to-do list drawn as a plan's steps are, the tree of them
 * one after another, each where it stands (the one under way in the prism's light and shimmer); and
 * the plan it proposed (plan mode) in its own words, kept by kiln as one of your plans, whose page
 * runs its steps. A plan from before kiln kept them shows as its words, to keep.
 */
import { planHref, plansApi, type Plan } from "@/api/plans";
import { Link } from "@/lib/router";
import { useSuspenseResource } from "@/lib/resource";
import { Prose } from "@/views/harness/parts";
import { PlanGraph, StepBar } from "./graph";
import type { GraphStep } from "./graph-layout";
import { SavePlan } from "./save";
import type { Stand } from "./status";
import "./plans.css";

/** The plan agent `session` proposed, as kiln keeps it (`words`: its latest, as its conversation has them). */
export function ProposedPlan({ session, words }: { session: string; words: string }) {
  // Asked again when it proposes another (kept within moments), and now and then.
  const kept = useSuspenseResource(`plans/proposed/${session}/${words.length}`, (s) => plansApi.proposedBy(session, s), { pollMs: 5000 }).data;
  const p = kept[0];
  if (p) return <KeptPlan p={p} />;
  if (!words) return null;
  return (
    <div className="hx-stack">
      <Prose text={words} className="hx-plan-text" />
      <SavePlan body={words} />
    </div>
  );
}

function KeptPlan({ p }: { p: Plan }) {
  return (
    <div className="ip">
      <p className="ip-head">
        Kept by kiln as <Link to={planHref(p.id)}>{p.title}</Link>
        <span className="ip-meta">, where its steps run</span>
      </p>
      <Prose text={p.body} className="hx-plan-text" />
    </div>
  );
}

const TODO_STAND: Record<string, Stand> = { completed: "met", in_progress: "working" };

/** A to-do list as the tree of a plan's steps, each after the one before: done met, the one under way working, the rest waiting. */
export function TodoSteps({ todos }: { todos: readonly { text: string; status: string }[] }) {
  const steps: GraphStep[] = todos.map((t, i) => ({ n: i + 1, title: t.text, needs: i ? [i] : [], stand: TODO_STAND[t.status] ?? "waiting" }));
  return (
    <div className="ip-todos">
      <StepBar stands={steps.map((s) => s.stand ?? "waiting")} />
      <PlanGraph label="Its to-do list" steps={steps} />
    </div>
  );
}
