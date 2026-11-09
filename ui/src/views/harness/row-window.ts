/** W21-R02: bounded rows over the current canonical projection, without estimated spacers. */
export type WindowPart = { type: "rows"; from: number; to: number } | { type: "gap"; from: number; to: number };

export interface RowWindow {
  from: number;
  to: number;
  parts: WindowPart[];
}

/** Bounded position hints only, backward compatible with the original outer-part positions. */
export interface RowPosition {
  id: string;
  offset: number;
  row?: true;
  seq?: number;
  neighbors?: readonly { id: string; row?: true }[];
}

/** A deleted row restores to a concrete adjacent survivor, never a guessed pixel height. */
export function survivingPosition(position: RowPosition, has: (id: string, row?: true) => boolean): RowPosition | null {
  if (has(position.id, position.row)) return position;
  for (const neighbor of position.neighbors?.slice(0, 2) ?? []) if (has(neighbor.id, neighbor.row)) return { ...position, id: neighbor.id, row: neighbor.row };
  return null;
}

/**
 * A keyed page plus rows currently being interacted with. Pins are an explicit soft-bound
 * exception: selecting text or focusing an expanded control must never discard its DOM.
 * Nothing is copied or retained outside the supplied current row keys.
 */
export function rowWindow(keys: readonly string[], size: number, anchor: string | null, pins: ReadonlySet<string> = new Set()): RowWindow {
  const count = Math.max(1, Math.floor(Number.isFinite(size) ? size : 1));
  const at = anchor === null ? -1 : keys.indexOf(anchor);
  const from = at < 0 ? Math.max(0, keys.length - count) : at;
  const to = Math.min(keys.length, from + count);
  const ranges: { from: number; to: number }[] = [];
  if (from < to) ranges.push({ from, to });
  for (let i = 0; i < keys.length; i++) if ((i < from || i >= to) && pins.has(keys[i]!)) ranges.push({ from: i, to: i + 1 });
  ranges.sort((a, b) => a.from - b.from);
  const merged: { from: number; to: number }[] = [];
  for (const range of ranges) {
    const last = merged.at(-1);
    if (last && range.from <= last.to) last.to = Math.max(last.to, range.to);
    else merged.push({ ...range });
  }
  const parts: WindowPart[] = [];
  let end = 0;
  for (const range of merged) {
    if (range.from > end) parts.push({ type: "gap", from: end, to: range.from });
    parts.push({ type: "rows", ...range });
    end = range.to;
  }
  if (end < keys.length) parts.push({ type: "gap", from: end, to: keys.length });
  return { from, to, parts };
}

/** A page beginning just before a target, preserving its stable key across history prepends. */
export function revealRow(keys: readonly string[], target: string, before = 3): string | null {
  const at = keys.indexOf(target);
  return at < 0 ? null : (keys[Math.max(0, at - before)] ?? null);
}

/** The adjacent page inside a gap; every row remains reachable without an unbounded expand. */
export function pageInGap(keys: readonly string[], gap: { from: number; to: number }, size: number, from: number): string | null {
  return keys[gap.to <= from ? Math.max(gap.from, gap.to - size) : gap.from] ?? null;
}
