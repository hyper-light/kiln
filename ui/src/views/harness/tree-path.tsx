/**
 * A subagent tree row's piece of the path (subagent-tree.ts `RowPath`), drawn where its row puts
 * it (Mission control: in its checkbox's column; by the composer: beside its pick box): its lanes'
 * lines, the curve into its mark from its parent's lane, and its mark, in its stand's colour. The
 * piece of a row that needs you is tan. Its lines run the whole of its row and over the line above
 * it, so the lanes go on unbroken from row to row. Only a ring at work moves.
 */
import * as React from "react";
import type { NodeStand, RowPath } from "./subagent-tree";
import "./tree-path.css";

/** Joins a ring at work to the page's clock, where every other one is in its turn (as status-glyph.tsx's `phaseLock` does Ship's). */
function phaseLock(el: Element | null) {
  if (!el || typeof el.getAnimations !== "function") return;
  for (const a of el.getAnimations({ subtree: true })) if (a.playState === "running" && a.startTime !== 0) a.startTime = 0;
}

/** The prism a ring at work turns through (status-glyph.tsx's). */
const PRISM = [
  ["#b8d9d2", 0],
  ["#a6c4ed", 0.33],
  ["#b8a6d5", 0.66],
  ["#d6a2aa", 1],
] as const;

/**
 * Its mark: an open ring at work (turning through the prism, as Ship's running glyph does), two
 * rings needing you, bars paused, a closed ring with its centre done, broken failed or stopped.
 */
function Mark({ x, y, stand, prism }: { x: number; y: number; stand: NodeStand; prism: string }) {
  const ring = <circle className="tp-bg" cx={x} cy={y} r={stand === "needs" ? 8 : 6} />;
  switch (stand) {
    case "working":
      return (
        <>
          <defs>
            <linearGradient id={prism} x1="0" x2="1" y1="0" y2="1">
              {PRISM.map(([c, at]) => (
                <stop key={at} offset={at} stopColor={c} />
              ))}
            </linearGradient>
          </defs>
          {ring}
          <circle className="tp-mark tp-turn" cx={x} cy={y} r={4.5} strokeDasharray="20 8.3" style={{ stroke: `url(#${prism})` }} />
        </>
      );
    case "needs":
      return (
        <>
          {ring}
          <circle className="tp-mark tp-outer" cx={x} cy={y} r={7} />
          <circle className="tp-mark" cx={x} cy={y} r={4} />
        </>
      );
    case "paused":
      return (
        <>
          {ring}
          <circle className="tp-mark" cx={x} cy={y} r={4.5} />
          <path className="tp-mark tp-bars" d={`M${x - 1.4} ${y - 1.9}v3.8M${x + 1.4} ${y - 1.9}v3.8`} />
        </>
      );
    case "done":
      return (
        <>
          {ring}
          <circle className="tp-mark" cx={x} cy={y} r={4.5} />
          <circle className="tp-dot" cx={x} cy={y} r={1.6} />
        </>
      );
    default:
      return (
        <>
          {ring}
          <circle className="tp-mark" cx={x} cy={y} r={4.5} strokeDasharray="3 3" />
        </>
      );
  }
}

/**
 * The row's piece, `width` wide: lanes from `x0`, `dx` apart (a tree deeper than fits keeps to its
 * last lane), its mark `y` down from the row's top, where its name's line is.
 */
export function TreePath({ path, stand, x0, dx, y, width, className }: { path: RowPath; stand: NodeStand; x0: number; dx: number; y: number; width: number; className?: string }) {
  const prism = `tp${React.useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  // A ring that comes to work joins every other one where it is in its turn (the page's clock).
  const lock = React.useCallback((el: SVGSVGElement | null) => (stand === "working" ? phaseLock(el) : undefined), [stand]);
  const most = Math.max(0, Math.floor((width - x0 - 5) / dx));
  const X = (lane: number) => x0 + Math.min(lane, most) * dx;
  const lines = (hot: boolean) => (
    <>
      {path.pieces
        .filter((p) => p.hot === hot)
        .map((p, i) => (
          <line key={`${hot}-${i}`} className={hot ? "tp-hot" : "tp-line"} x1={X(p.lane)} x2={X(p.lane)} y1={p.from === "top" ? -1 : y} y2={p.to === "bottom" ? "100%" : y} />
        ))}
      {path.curve && path.curve.hot === hot && (
        <path className={hot ? "tp-hot" : "tp-line"} d={`M${X(path.curve.from)} -1 C${X(path.curve.from)} ${y * 0.75}, ${X(path.curve.to) - 4} ${y}, ${X(path.curve.to)} ${y}`} />
      )}
    </>
  );
  return (
    <svg ref={lock} className={`tp${className ? ` ${className}` : ""}`} width={width} data-stand={stand} aria-hidden="true">
      {lines(false)}
      {lines(true)}
      <Mark x={X(path.lane)} y={y} stand={stand} prism={prism} />
    </svg>
  );
}
