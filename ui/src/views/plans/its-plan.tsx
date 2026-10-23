/**
 * An agent's own plan, in its Plan tab, as the tree of a plan's steps: its to-do list, one after
 * another, each where it stands (the one under way in the prism's light and shimmer); or, keeping
 * none, the plan it proposed (plan mode), kept by kiln as one of your plans, its steps as they'll
 * run. Its words are there on asking, and its page runs its steps. A plan with no steps to draw
 * shows as its words; one from before kiln kept them, as its words to keep.
 */
import * as React from "react";
import { planHref, plansApi, type Plan } from "@/api/plans";
import { Link } from "@/lib/router";
import { useSuspenseResource } from "@/lib/resource";
import { Prose } from "@/views/harness/parts";
import { effectiveNeeds, PlanGraph, StepBar } from "./graph";
import type { GraphStep } from "./graph-layout";
import { SavePlan } from "./save";
import type { Stand } from "./status";
import "./plans.css";

/**
 * The plan agent `session` proposed, as kiln keeps it (`words`: its latest, as its conversation has
 * them). `tracked`: it keeps a to-do list, drawn above as how far it's come: the plan is a line.
 */
export function ProposedPlan({ session, words, tracked }: { session: string; words: string; tracked: boolean }) {
  // Asked again when it proposes another (kept within moments), and now and then.
  const kept = useSuspenseResource(`plans/proposed/${session}/${words.length}`, (s) => plansApi.proposedBy(session, s), { pollMs: 5000 }).data;
  const p = kept[0];
  if (p) return <KeptPlan p={p} tracked={tracked} />;
  if (!words) return null;
  return (
    <div className="hx-stack">
      <Prose text={words} className="hx-plan-text" />
      <SavePlan body={words} />
    </div>
  );
}

function KeptPlan({ p, tracked }: { p: Plan; tracked: boolean }) {
  const [open, setOpen] = React.useState(false);
  const titles = p.items?.length ? p.items.map((it) => it.title || "Untitled step") : p.steps;
  const waits = effectiveNeeds(titles.map((_, i) => p.items?.[i]?.needs));
  const steps: GraphStep[] = titles.map((title, i) => ({ n: i + 1, title, needs: waits[i] ?? [] }));
  // No steps to draw: its words, as it wrote them.
  if (!tracked && steps.length === 0)
    return (
      <div className="ip">
        <p className="ip-head">
          Kept by kiln as <Link to={planHref(p.id)}>{p.title}</Link>
        </p>
        <Prose text={p.body} className="hx-plan-text" />
      </div>
    );
  return (
    <div className="ip">
      {!tracked && <PlanGraph label={`Its plan, “${p.title}”`} steps={steps} />}
      <p className="ip-head">
        <Link to={planHref(p.id)}>{p.title}</Link>
        <span className="ip-meta"> · kept by kiln · </span>
        <button type="button" className="ip-words" aria-expanded={open} onClick={() => setOpen(!open)}>
          {open ? "hide its words" : "its words"}
        </button>
      </p>
      {open && <Prose text={p.body} className="hx-plan-text" />}
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
