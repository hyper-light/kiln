/**
 * The Ship dropdown's file diffs as rows (docs/ci.md W26-R07): a patch file's hunks read flat
 * (each row its old and new number, its sign and text, and its hunk), a gap between hunks, and the
 * fold of a diff over 100 lines to its first lines and the lines that carry something (a thread, a
 * conflict, the draft's place), with gaps between. Pure: the view maps these to elements.
 */
import type { PatchFile } from "@/views/review/patch";

export type RowKind = "ctx" | "del" | "add";

/** One line of a file's diff: where it stands on each side, its sign and its text. */
export interface Row {
  o: number | null;
  n: number | null;
  kind: RowKind;
  text: string;
  /** The hunk it's in, from 0 (a patch's rows); a whole file's rows have none. */
  h?: number;
}

/** A file's hunk lines as rows, in the patch's order. */
export function rowsOf(file: Pick<PatchFile, "hunks">): Row[] {
  const out: Row[] = [];
  file.hunks.forEach((hunk, h) => {
    let o = hunk.old_start;
    let n = hunk.new_start;
    for (const l of hunk.lines) {
      const t = l.charAt(0);
      if (t === "\\") continue;
      if (t === "-") out.push({ o: o++, n: null, kind: "del", text: l.slice(1), h });
      else if (t === "+") out.push({ o: null, n: n++, kind: "add", text: l.slice(1), h });
      else out.push({ o: o++, n: n++, kind: "ctx", text: l.slice(1), h });
    }
  });
  return out;
}

/** A diff over this many lines folds to its first lines and the lines that carry something. */
export const FOLD_LINES = 100;
/** How many of a folded diff's first lines stay. */
export const FIRST_LINES = 12;

/**
 * What of `rows` shows: each kept row's index, and a negative number for a gap, `-x` with x the rows
 * folded away there, or 1 where only the patch leaves lines out (between its hunks). Every hunk
 * shows with its context, a gap between hunks; a diff over 100 lines keeps its first lines and the
 * lines that carry something.
 */
export function fold(rows: readonly Row[], carries: (row: Row, index: number) => boolean): number[] {
  const long = rows.length > FOLD_LINES;
  const out: number[] = [];
  let gap = 0;
  let last: Row | null = null;
  rows.forEach((r, i) => {
    if (long && i >= FIRST_LINES && !carries(r, i)) {
      gap++;
      return;
    }
    if (gap) out.push(-gap);
    else if (last && last.h !== r.h) out.push(-1);
    gap = 0;
    last = r;
    out.push(i);
  });
  if (gap) out.push(-gap);
  return out;
}

/** The `side:line` key a thread's end is under (old side: the old number; else the new). */
export type Side = "old" | "new";

export function keyOf(side: Side, line: number | null): string | null {
  return line == null ? null : `${side}:${line}`;
}

/** Where a thread sits in the version shown: its side and the line it ends on (null: not here). */
export interface Anchor {
  side: Side;
  start: number;
  end: number;
}

export function anchorOf(
  t: { version: number; side: Side; start_line: number | null; end_line: number | null; now: { version: number; start_line: number; end_line: number } | null },
  version: number,
): Anchor | null {
  if (t.start_line == null) return null;
  if (t.version === version) return { side: t.side, start: t.start_line, end: t.end_line ?? t.start_line };
  if (t.now && t.now.version === version) return { side: t.side, start: t.now.start_line, end: t.now.end_line };
  return null;
}

/** The lines a thread is on, as the rows shown have them (its side's text, by its own numbers). */
export function linesAt(rows: readonly Row[], a: Anchor): { start: number; text: string[] } | null {
  const mine = rows.filter((r) => (a.side === "old" ? r.o !== null && r.o >= a.start && r.o <= a.end : r.n !== null && r.n >= a.start && r.n <= a.end));
  if (!mine.length) return null;
  return { start: a.start, text: mine.map((r) => r.text) };
}
