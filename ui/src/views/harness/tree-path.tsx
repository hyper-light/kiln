/**
 * A subagent tree row's piece of the path (subagent-tree.ts `RowPath`), drawn where its row puts
 * it (Mission control: in its checkbox's column; by the composer: beside its pick box): its lanes'
 * lines, the curve into its mark from its parent's lane, and its mark, in its stand's colour. The
 * piece of a row that needs you is tan; nothing in it moves.
 */
import type { NodeStand, RowPath } from "./subagent-tree";
import "./tree-path.css";

/** Its mark: an open ring at work, two rings needing you, bars paused, a closed ring with its centre done, broken failed or stopped. */
function Mark({ x, y, stand }: { x: number; y: number; stand: NodeStand }) {
  const ring = <circle className="tp-bg" cx={x} cy={y} r={stand === "needs" ? 8 : 6} />;
  switch (stand) {
    case "working":
      return (
        <>
          {ring}
          <circle className="tp-mark" cx={x} cy={y} r={4.5} strokeDasharray="20 8.3" />
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
  const most = Math.max(0, Math.floor((width - x0 - 5) / dx));
  const X = (lane: number) => x0 + Math.min(lane, most) * dx;
  const lines = (hot: boolean) => (
    <>
      {path.pieces
        .filter((p) => p.hot === hot)
        .map((p, i) => (
          <line key={`${hot}-${i}`} className={hot ? "tp-hot" : "tp-line"} x1={X(p.lane)} x2={X(p.lane)} y1={p.from === "top" ? 0 : y} y2={p.to === "bottom" ? "100%" : y} />
        ))}
      {path.curve && path.curve.hot === hot && (
        <path className={hot ? "tp-hot" : "tp-line"} d={`M${X(path.curve.from)} 0 C${X(path.curve.from)} ${y * 0.75}, ${X(path.curve.to) - 4} ${y}, ${X(path.curve.to)} ${y}`} />
      )}
    </>
  );
  return (
    <svg className={`tp${className ? ` ${className}` : ""}`} width={width} data-stand={stand} aria-hidden="true">
      {lines(false)}
      {lines(true)}
      <Mark x={X(path.lane)} y={y} stand={stand} />
    </svg>
  );
}
