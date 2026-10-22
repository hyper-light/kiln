/**
 * A plan's steps as their graph (docs/plans.md, "One agent per step"): in rounds, each step in the
 * round after the latest step it waits for, so a round's steps run side by side; a line from each
 * step it waits for. Running, each step shows where it stands (status.tsx): lines into a step at
 * work in the prism, settled ones quiet, the rest only sketched. Nothing moves along them.
 *
 * Wide, the rounds run across; narrow (a phone, a side pane), down, two steps a row. The lines are
 * drawn from where the steps are laid out, again whenever that changes (a ref's observer: no
 * effect).
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { rounds, type GraphStep } from "./graph-layout";
import { STAND_WORDS, StandTitle, StepGlyph, type Stand } from "./status";

export { effectiveNeeds, type GraphStep } from "./graph-layout";

interface Line {
  from: number;
  to: number;
  d: string;
}

/** The lines between steps, from where `el` lays them out. */
function lines(el: HTMLElement, steps: GraphStep[]): Line[] {
  const box = el.getBoundingClientRect();
  const out: Line[] = [];
  for (const s of steps)
    for (const d of s.needs) {
      const a = el.querySelector<HTMLElement>(`[data-n="${d}"]`)?.getBoundingClientRect();
      const b = el.querySelector<HTMLElement>(`[data-n="${s.n}"]`)?.getBoundingClientRect();
      if (!a || !b) continue;
      const across = b.left > a.right - 4;
      const [x1, y1, x2, y2] = across
        ? [a.right - box.left, a.top + a.height / 2 - box.top, b.left - box.left, b.top + b.height / 2 - box.top]
        : [a.left + a.width / 2 - box.left, a.bottom - box.top, b.left + b.width / 2 - box.left, b.top - box.top];
      const r = (v: number) => Math.round(v * 10) / 10;
      out.push({
        from: d,
        to: s.n,
        d: across ? `M${r(x1)} ${r(y1)} C${r(x1 + 18)} ${r(y1)}, ${r(x2 - 18)} ${r(y2)}, ${r(x2)} ${r(y2)}` : `M${r(x1)} ${r(y1)} C${r(x1)} ${r(y1 + 12)}, ${r(x2)} ${r(y2 - 12)}, ${r(x2)} ${r(y2)}`,
      });
    }
  return out;
}

export function PlanGraph({ steps, label }: { steps: GraphStep[]; label: string }) {
  const shape = steps.map((s) => `${s.n}:${s.needs.join(",")}`).join(";");
  const [drawn, setDrawn] = React.useState<Line[]>([]);
  // The lines follow the layout: drawn when it's there, and whenever its size changes.
  const ref = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el) return;
      let last = "";
      const draw = () => {
        const next = lines(el, steps);
        const key = next.map((l) => l.d).join("|");
        if (key !== last) {
          last = key;
          setDrawn(next);
        }
      };
      const watch = new ResizeObserver(draw);
      watch.observe(el);
      el.querySelectorAll(".pg-node").forEach((n) => watch.observe(n));
      draw();
      return () => watch.disconnect();
    },
    // Steps the same shape keep their lines; what each stands for is read as it's drawn.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [shape],
  );
  const stand = new Map(steps.map((s) => [s.n, s.stand]));
  const lineState = (l: Line) => (stand.get(l.from) === "met" ? (stand.get(l.to) === "working" ? "live" : "met") : "waits");
  const gradient = `pg-prism-${React.useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  return (
    <div className="pg" ref={ref} role="group" aria-label={label}>
      <svg className="pg-lines" aria-hidden="true">
        <defs>
          {/* Across the whole graph: a line flat across has no height for a gradient of its own. */}
          <linearGradient id={gradient} gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="100%" y2="100%">
            <stop offset="0" stopColor="#b8d9d2" />
            <stop offset="0.3" stopColor="#a6c4ed" />
            <stop offset="0.55" stopColor="#b8a6d5" />
            <stop offset="0.78" stopColor="#d6a2aa" />
            <stop offset="1" stopColor="#d8cbb0" />
          </linearGradient>
        </defs>
        {drawn.map((l) => (
          <path key={`${l.from}-${l.to}`} d={l.d} data-s={lineState(l)} style={lineState(l) === "live" ? { stroke: `url(#${gradient})` } : undefined} />
        ))}
      </svg>
      <div className="pg-rounds">
        {rounds(steps).map((r, i) => (
          <div key={i} className="pg-round">
            {r.map((s) => {
              const st: Stand = s.stand ?? "waiting";
              const body = (
                <>
                  <StepGlyph stand={st} />
                  <span className="pg-text">
                    <span className="pg-title">
                      <span className="pg-n">{s.n}</span>
                      <StandTitle stand={st}>{s.title || "Untitled step"}</StandTitle>
                    </span>
                    {s.stand && <span className="pg-word">{STAND_WORDS[st]}</span>}
                    {s.who && <span className="pg-who">{s.who}</span>}
                  </span>
                </>
              );
              return s.href ? (
                <Link key={s.n} to={s.href} className="pg-node" data-n={s.n} data-stand={s.stand}>
                  {body}
                </Link>
              ) : (
                <div key={s.n} className="pg-node" data-n={s.n} data-stand={s.stand}>
                  {body}
                </div>
              );
            })}
          </div>
        ))}
      </div>
    </div>
  );
}

/** How far a run has come: a segment a step, in where it stands. */
export function StepBar({ stands }: { stands: Stand[] }) {
  return (
    <div className="pg-bar" aria-hidden="true">
      {stands.map((s, i) => (
        <i key={i} data-s={s} />
      ))}
    </div>
  );
}
