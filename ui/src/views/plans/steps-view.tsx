/**
 * A plan's steps as you read and run them: its graph (what waits for what), then each step as a
 * card: its number and title, what it waits for, and what it is (what to do, examples, acceptance
 * criteria, tests), folded until asked. Pick steps to run just those (Run it, below, takes them);
 * a step's own Run picks it alone.
 */
import * as React from "react";
import type { Plan } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { Prose } from "@/views/harness/parts";
import { effectiveNeeds, PlanGraph } from "./graph";
import { itemDetail } from "./steps";

/** Steps as words: "step 2", "steps 2 and 3", "steps 1, 2 and 4". */
export function stepsWords(n: readonly number[]): string {
  if (n.length <= 1) return n.length ? `step ${n[0]}` : "";
  return `steps ${n.slice(0, -1).join(", ")} and ${n[n.length - 1]}`;
}

export function PlanSteps({ plan, picked, onPick, onRun }: { plan: Plan; picked: readonly number[]; onPick: (n: number[]) => void; onRun: (n: number) => void }) {
  const items = plan.items ?? [];
  const titles = items.length ? items.map((it) => it.title || "Untitled step") : plan.steps;
  if (titles.length === 0) return null;
  // Steps that say what they wait for run as that says; else each after the one before.
  const said = items.some((it) => it.needs?.length);
  const waits = effectiveNeeds(titles.map((_, i) => items[i]?.needs));
  const steps = titles.map((title, i) => ({ n: i + 1, title, needs: waits[i] ?? [] }));
  const pick = (n: number, on: boolean) => onPick(on ? [...picked.filter((x) => x !== n), n].sort((a, b) => a - b) : picked.filter((x) => x !== n));
  return (
    <section className="plan-section" aria-label="Steps">
      <div className="sv-top">
        <h2 className="plan-h">Steps</h2>
        {picked.length > 0 ? (
          <span className="sv-picked">
            {stepsWords(picked)} picked to run ·{" "}
            <button type="button" className="sv-link" onClick={() => onPick([])}>
              clear
            </button>
          </span>
        ) : (
          <span className="plan-dim sv-hint">Pick steps to run just those</span>
        )}
      </div>
      {steps.length > 1 && said && <PlanGraph label="Its steps, as they'll run" steps={steps} />}
      <ol className="sv-list">
        {steps.map((s, i) => (
          <StepCard
            key={s.n}
            n={s.n}
            title={s.title}
            after={said ? s.needs : []}
            detail={items[i] ? itemDetail(items[i]) : ""}
            picked={picked.includes(s.n)}
            onPick={(on) => pick(s.n, on)}
            onRun={() => onRun(s.n)}
          />
        ))}
      </ol>
    </section>
  );
}

function StepCard({
  n,
  title,
  after,
  detail,
  picked,
  onPick,
  onRun,
}: {
  n: number;
  title: string;
  after: number[];
  detail: string;
  picked: boolean;
  onPick: (on: boolean) => void;
  onRun: () => void;
}) {
  const [open, setOpen] = React.useState(false);
  return (
    <li className="sv-step" data-picked={picked || undefined}>
      <input type="checkbox" className="check sv-check" checked={picked} onChange={(e) => onPick(e.target.checked)} aria-label={`Pick step ${n}`} />
      <div className="sv-body">
        <p className="sv-title">
          <span className="sv-n">{n}</span>
          {title}
        </p>
        {after.length > 0 && <p className="sv-after">After {stepsWords(after)}</p>}
        {detail && (
          <>
            {open && (
              <div className="sv-detail">
                <Prose text={detail} />
              </div>
            )}
            <button type="button" className="sv-link" aria-expanded={open} onClick={() => setOpen(!open)}>
              {open ? "Less" : "What it is"}
            </button>
          </>
        )}
      </div>
      <Button size="xs" variant="secondary" onClick={onRun}>
        Run
      </Button>
    </li>
  );
}
