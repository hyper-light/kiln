/**
 * A plan's steps as you read and run them: their graph, as they'll run (what waits for what), each
 * step its number and title. Pick steps by tapping them: what each picked step is shows under the
 * graph (what to do, examples, acceptance criteria, tests; a plain step's own words), and Run takes
 * just those.
 */
import type { Plan } from "@/api/plans";
import { Button } from "@/components/ui/button";
import { Prose } from "@/views/harness/parts";
import { effectiveNeeds, PlanGraph } from "./graph";
import { itemDetail, stepMarkdown } from "./steps";

/** Steps as words: "step 2", "steps 2 and 3", "steps 1, 2 and 4". */
export function stepsWords(n: readonly number[]): string {
  if (n.length <= 1) return n.length ? `step ${n[0]}` : "";
  return `steps ${n.slice(0, -1).join(", ")} and ${n[n.length - 1]}`;
}

export function PlanSteps({ plan, picked, onPick, onRun }: { plan: Plan; picked: readonly number[]; onPick: (n: number[]) => void; onRun: () => void }) {
  const items = plan.items ?? [];
  const titles = items.length ? items.map((it) => it.title || "Untitled step") : plan.steps;
  if (titles.length === 0) return null;
  // Steps that say what they wait for run as that says; else each after the one before.
  const waits = effectiveNeeds(titles.map((_, i) => items[i]?.needs));
  const steps = titles.map((title, i) => ({ n: i + 1, title, needs: waits[i] ?? [] }));
  const pick = (n: number) => onPick(picked.includes(n) ? picked.filter((x) => x !== n) : [...picked, n].sort((a, b) => a - b));
  return (
    <section className="plan-section" aria-label="Steps">
      <div className="sv-top">
        <h2 className="plan-h">Steps</h2>
        {picked.length > 0 ? (
          <span className="sv-picked">
            {stepsWords(picked)} picked ·{" "}
            <button type="button" className="sv-link" onClick={() => onPick([])}>
              clear
            </button>
          </span>
        ) : (
          <span className="plan-dim sv-hint">Pick steps to see what they are and run just those</span>
        )}
      </div>
      <PlanGraph label="Its steps, as they'll run" steps={steps} picked={picked} onPick={pick} />
      {picked.length > 0 && (
        <div className="sv-picked-steps">
          <ol className="sv-details">
            {picked.map((n) => {
              const it = items[n - 1];
              const detail = it ? itemDetail(it) : "";
              return (
                <li key={n} className="sv-detail">
                  <span className="sv-n">{n}</span>
                  <div className="sv-body">
                    {it ? (
                      <>
                        <p className="sv-title">{it.title || "Untitled step"}</p>
                        {detail && <Prose text={detail} />}
                      </>
                    ) : (
                      <Prose text={stepMarkdown(plan.body, n) || (plan.steps[n - 1] ?? "")} />
                    )}
                  </div>
                </li>
              );
            })}
          </ol>
          <Button size="sm" className="sv-run" onClick={onRun}>
            Run {stepsWords(picked)}
          </Button>
        </div>
      )}
    </section>
  );
}
