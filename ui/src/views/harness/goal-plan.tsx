/**
 * A goal's steps, under it on its stack (goal-plan-layout.ts): each in the legend's glyph on the
 * goal's line (status.tsx: met closed in mint, waiting sketched, not met broken in amber, skipped
 * dashed in pink, stopped faint, needs you the amber rings), the steps that run at once on one
 * branch beside it, where each stands in a word at the right, and who's at one that's worked on or
 * needs you under it. The one at work has the light turning in its glyph and its words in the
 * holographic shimmer. Only that moves.
 */
import * as React from "react";
import { TitleText } from "@/views/plans/graph";
import { headline } from "@/views/plans/graph-layout";
import { STAND_WORDS, StandTitle, StepGlyph } from "@/views/plans/status";
import { goalPlanRows, shortTitle, type GoalStep } from "./goal-plan-layout";

export function GoalPlan({ steps, label }: { steps: GoalStep[]; label: string }) {
  const [open, setOpen] = React.useState(false);
  return (
    <ol className="gp" aria-label={label}>
      {goalPlanRows(steps, open).map((r, i) => {
        if (r.kind === "fork")
          return (
            <li key={`fork-${i}`} className="gp-fork">
              <svg className="gp-curve" viewBox="0 0 54 22" preserveAspectRatio="none" aria-hidden="true">
                <path d="M11 0 C11 11, 43 11, 43 22" />
              </svg>
              <span className="gp-count">{r.count} at once</span>
            </li>
          );
        if (r.kind === "join")
          return (
            <li key={`join-${i}`} className="gp-join" aria-hidden="true">
              <svg className="gp-curve" viewBox="0 0 54 14" preserveAspectRatio="none">
                <path d="M43 0 C43 7, 11 7, 11 14" />
              </svg>
            </li>
          );
        if (r.kind === "folded")
          return (
            <li key="folded" className="gp-row" data-lane="main" data-stand="met">
              <span className="gp-glyph">
                <StepGlyph stand="met" />
              </span>
              <span className="gp-text">
                <span className="gp-title">
                  {r.count} steps met{" "}
                  <button type="button" className="gp-show" onClick={() => setOpen(true)}>
                    show them
                  </button>
                </span>
              </span>
              <span className="gp-word">{STAND_WORDS.met}</span>
            </li>
          );
        const s = r.step;
        return (
          <li key={s.n} className="gp-row" data-lane={r.lane} data-stand={s.stand}>
            <span className="gp-glyph">
              <StepGlyph stand={s.stand} />
            </span>
            <span className="gp-text">
              <span className="gp-title">
                <StandTitle stand={s.stand} flow={`${s.n}:${s.title}`}>
                  <TitleText parts={headline(s.title)} />
                </StandTitle>
              </span>
              {(s.stand === "working" || s.stand === "needs_you") && s.who && <span className="gp-sub">{s.who}</span>}
              {s.checks && (
                <span className="gp-sub">
                  {s.checks.met} of {s.checks.of} checks met
                </span>
              )}
              {r.after.length > 0 && <span className="gp-sub">after {r.after.map((a) => `“${shortTitle(a.title)}”`).join(", ")}</span>}
              {r.start && <span className="gp-sub">from the start</span>}
            </span>
            <span className="gp-word">{STAND_WORDS[s.stand]}</span>
          </li>
        );
      })}
    </ol>
  );
}
