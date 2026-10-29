/**
 * The Ship dropdown's file diffs as rows (docs/ci.md W26-R07): a patch file's hunks read flat
 * (each row its old and new number, its sign and text), and the fold of a diff over 100 lines to
 * its first lines and the lines that carry something (a thread, a conflict, the draft's place),
 * with gaps between. Pure: the view maps these to elements.
 */
import type { PatchFile } from "@/views/review/patch";

export type RowKind = "ctx" | "del" | "add";

/** One line of a file's diff: where it stands on each side, its sign and its text. */
export interface Row {
  o: number | null;
  n: number | null;
  kind: RowKind;
  text: string;
}

/** A file's hunk lines as rows, in the patch's order. */
export function rowsOf(file: Pick<PatchFile, "hunks">): Row[] {
  const out: Row[] = [];
  for (const h of file.hunks) {
    let o = h.old_start;
    let n = h.new_start;
    for (const l of h.lines) {
      const t = l.charAt(0);
      if (t === "\\") continue;
      if (t === "-") out.push({ o: o++, n: null, kind: "del", text: l.slice(1) });
      else if (t === "+") out.push({ o: null, n: n++, kind: "add", text: l.slice(1) });
      else out.push({ o: o++, n: n++, kind: "ctx", text: l.slice(1) });
    }
  }
  return out;
}

/** A diff over this many lines folds to its first lines and the lines that carry something. */
export const FOLD_LINES = 100;
/** How many of a folded diff's first lines stay. */
export const FIRST_LINES = 12;

/**
 * What of `rows` shows when the diff is long: each kept row's index, a negative number marking a
 * gap of `-x` rows folded away. Short diffs show every row.
 */
export function fold(rows: readonly Row[], carries: (row: Row, index: number) => boolean): number[] {
  if (rows.length <= FOLD_LINES) return rows.map((_, i) => i);
  const keep = new Set<number>();
  for (let i = 0; i < FIRST_LINES && i < rows.length; i++) keep.add(i);
  rows.forEach((r, i) => {
    if (carries(r, i)) keep.add(i);
  });
  const out: number[] = [];
  let gap = 0;
  rows.forEach((_, i) => {
    if (keep.has(i)) {
      if (gap) {
        out.push(-gap);
        gap = 0;
      }
      out.push(i);
    } else gap++;
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
