/**
 * A plan's steps as their graph (docs/plans.md, "One agent per step"): in rounds, each step in the
 * round after the latest step it waits for, so a round's steps run side by side; a line from each
 * step it waits for. Running, each step shows where it stands (status.tsx): lines into a step at
 * work in the prism, settled ones quiet, the rest only sketched. Nothing moves along them.
 *
 * Down, a step a row at every width (a round's steps one under another), each the width of the graph: a line where a step waits for the one above it, and what else it waits for said on it
 * ("after 2, 3"). The lines are drawn from where the steps are laid out, again whenever that
 * changes (a ref's observer: no effect). Given `onPick`, steps are picked by tapping them.
 */
import * as React from "react";
import { Link } from "@/lib/router";
import { blocked, curve, headline, rounds, type Box, type GraphStep, type TitlePart } from "./graph-layout";
import { STAND_WORDS, StandTitle, StepGlyph, type Stand } from "./status";

export { effectiveNeeds, type GraphStep } from "./graph-layout";

interface Line {
  from: number;
  to: number;
  d: string;
}

/** What the graph draws between its steps: the lines, and for each step one of whose lines would run behind another step, every step it waits for ("after 2, 3"). */
interface Drawn {
  lines: Line[];
  after: Record<number, number[]>;
}

/** The lines between steps, from where `el` lays them out. */
function lines(el: HTMLElement, steps: GraphStep[]): Drawn {
  const box = el.getBoundingClientRect();
  const out: Line[] = [];
  const after: Record<number, number[]> = {};
  // Each step's box, from the graph's top left.
  const boxes = new Map<number, Box>();
  for (const s of steps) {
    const r = el.querySelector<HTMLElement>(`[data-n="${s.n}"]`)?.getBoundingClientRect();
    if (r) boxes.set(s.n, { left: r.left - box.left, top: r.top - box.top, right: r.right - box.left, bottom: r.bottom - box.top });
  }
  for (const s of steps)
    for (const d of s.needs) {
      const a = boxes.get(d);
      const b = boxes.get(s.n);
      if (!a || !b) continue;
      const across = b.left > a.right - 4;
      // One above the other, as a phone lays them: straight down from its mark.
      const column = Math.abs(a.left - b.left) < 4 && Math.abs(a.right - b.right) < 4;
      const [x1, y1, x2, y2] = across
        ? [a.right, (a.top + a.bottom) / 2, b.left, (b.top + b.bottom) / 2]
        : column
          ? [a.left + MARK_X, a.bottom, b.left + MARK_X, b.top]
          : [(a.left + a.right) / 2, a.bottom, (b.left + b.right) / 2, b.top];
      // A line that would run behind another step (a round between, a phone's column) isn't drawn:
      // the step says everything it waits for instead.
      const others = [...boxes].filter(([n]) => n !== d && n !== s.n).map(([, x]) => x);
      if (blocked(curve(x1, y1, x2, y2, across), others)) {
        after[s.n] = s.needs;
        continue;
      }
      const r = (v: number) => Math.round(v * 10) / 10;
      out.push({
        from: d,
        to: s.n,
        d: across
          ? `M${r(x1)} ${r(y1)} C${r(x1 + 18)} ${r(y1)}, ${r(x2 - 18)} ${r(y2)}, ${r(x2)} ${r(y2)}`
          : column
            ? `M${r(x1)} ${r(y1)} V${r(y2)}`
            : `M${r(x1)} ${r(y1)} C${r(x1)} ${r(y1 + 12)}, ${r(x2)} ${r(y2 - 12)}, ${r(x2)} ${r(y2)}`,
      });
    }
  return { lines: out, after };
}

/** Where a step's mark is across its box (its padding and half the mark): a line down a column runs from it. */
const MARK_X = 18;


/** A step's title: code as code, a long name broken where its parts meet. */
export function TitleText({ parts }: { parts: TitlePart[] }) {
  return parts.map((p, i) =>
    p.code ? (
      <code key={i} className="ptree-code">
        {p.text.split(/(?<=[/_.:])/).flatMap((x, j) => (j ? [<wbr key={j} />, x] : [x]))}
      </code>
    ) : (
      <React.Fragment key={i}>{p.text}</React.Fragment>
    ),
  );
}

export function PlanGraph({ steps, label, picked, onPick }: { steps: GraphStep[]; label: string; picked?: readonly number[]; onPick?: (n: number) => void }) {
  const shape = steps.map((s) => `${s.n}:${s.needs.join(",")}`).join(";");
  const [drawn, setDrawn] = React.useState<Drawn>({ lines: [], after: {} });

  // The lines follow the layout: drawn when it's there, and whenever its size changes.
  const ref = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el) return;
      let last = "";
      const draw = () => {
        const next = lines(el, steps);
        const key = `${next.lines.map((l) => l.d).join("|")}/${JSON.stringify(next.after)}`;
        if (key !== last) {
          last = key;
          setDrawn(next);
        }
      };
      const watch = new ResizeObserver(draw);
      watch.observe(el);
      el.querySelectorAll(".ptree-node").forEach((n) => watch.observe(n));
      draw();
      return () => watch.disconnect();
    },
    // Steps the same shape keep their lines; what each stands for is read as it's drawn.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [shape],
  );
  const stand = new Map(steps.map((s) => [s.n, s.stand]));
  const lineState = (l: Line) => (stand.get(l.from) === "met" ? (stand.get(l.to) === "working" ? "live" : "met") : "waits");
  const gradient = `ptree-prism-${React.useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  return (
    <div className="ptree" ref={ref} role="group" aria-label={label}>
      <svg className="ptree-lines" aria-hidden="true">
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
        {drawn.lines.map((l) => (
          <path key={`${l.from}-${l.to}`} d={l.d} data-s={lineState(l)} style={lineState(l) === "live" ? { stroke: `url(#${gradient})` } : undefined} />
        ))}
      </svg>
      <div className="ptree-rounds">
        {rounds(steps).map((r, i) => (
          <div key={i} className="ptree-round">
            {r.map((s) => {
              const st: Stand = s.stand ?? "waiting";
              const body = (
                <>
                  <StepGlyph stand={st} />
                  <span className="ptree-text">
                    <span className="ptree-title">
                      <span className="ptree-n">{s.n}</span>
                      <StandTitle stand={st} flow={`${s.n}:${s.title}`}>
                        {s.title ? <TitleText parts={headline(s.title)} /> : "Untitled step"}
                      </StandTitle>
                    </span>
                    {s.stand && <span className="ptree-word">{STAND_WORDS[st]}</span>}
                    {drawn.after[s.n]?.length ? <span className="ptree-after">after {drawn.after[s.n]?.join(", ")}</span> : null}
                    {s.who && <span className="ptree-who">{s.who}</span>}
                  </span>
                </>
              );
              return s.href ? (
                <Link key={s.n} to={s.href} className="ptree-node" data-n={s.n} data-stand={s.stand}>
                  {body}
                </Link>
              ) : onPick ? (
                <button key={s.n} type="button" className="ptree-node" data-n={s.n} data-stand={s.stand} aria-pressed={picked?.includes(s.n) ?? false} onClick={() => onPick(s.n)}>
                  {body}
                </button>
              ) : (
                <div key={s.n} className="ptree-node" data-n={s.n} data-stand={s.stand}>
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
    <div className="ptree-bar" aria-hidden="true">
      {stands.map((s, i) => (
        <i key={i} data-s={s} />
      ))}
    </div>
  );
}
