/**
 * An agent's own plan, in its Plan tab: the plan it proposed (plan mode) as kiln keeps it, its
 * steps as cards to pick or run (its plan's page takes them), its words folded under them; and its
 * to-do list, each item as a step stands, the one under way in the prism's shimmer. A plan from
 * before kiln kept them shows as its words, to keep.
 */
import * as React from "react";
import { planHref, plansApi, type Plan } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { Link, useNavigate } from "@/lib/router";
import { useSuspenseResource } from "@/lib/resource";
import { Prose } from "@/views/harness/parts";
import { SavePlan } from "./save";
import { StandTitle, StepGlyph, type Stand } from "./status";
import { PlanSteps, stepsWords } from "./steps-view";
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
  const navigate = useNavigate();
  const [picked, setPicked] = React.useState<number[]>([]);
  const [words, setWords] = React.useState(false);
  // Run on its page: the steps picked there, its Run it open.
  const run = (n: number[]) => navigate(`${planHref(p.id)}?pick=${n.join(",")}`);
  return (
    <div className="ip">
      <p className="ip-head">
        <Link to={planHref(p.id)}>{p.title}</Link>
        <span className="ip-meta"> · v{p.version}, kept by kiln</span>
      </p>
      <PlanSteps plan={p} picked={picked} onPick={setPicked} onRun={(n) => run([n])} />
      {picked.length > 0 && (
        <div className="ip-run">
          <Button size="xs" onClick={() => run(picked)}>
            Run {stepsWords(picked)}
          </Button>
        </div>
      )}
      <button type="button" className="sv-link" aria-expanded={words} onClick={() => setWords(!words)}>
        {words ? "Hide its words" : "Its words"}
      </button>
      {words && <Prose text={p.body} className="hx-plan-text" />}
    </div>
  );
}

/** A to-do list, each item as a step stands: done met, the one under way working, the rest waiting. */
export function TodoSteps({ todos }: { todos: readonly { text: string; status: string }[] }) {
  return (
    <ol className="ip-todos">
      {todos.map((t, i) => {
        const stand: Stand = t.status === "completed" ? "met" : t.status === "in_progress" ? "working" : "waiting";
        return (
          <li key={i} className="ip-todo" data-stand={stand}>
            <StepGlyph stand={stand} />
            <span className="ip-todo-text">
              <StandTitle stand={stand}>{t.text}</StandTitle>
            </span>
          </li>
        );
      })}
    </ol>
  );
}
