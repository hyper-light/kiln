/**
 * How a filter field's chosen chips share its one line (facet-box.tsx): as many as fit, room kept to
 * type, the rest folded into one "+N" chip after them. Widths in, a fold out: nothing here reads the
 * page (tests/facet-fold.test.ts).
 */

/**
 * What the line shows: its first `shown` chips (the first cut to `cut` px when given). The rest fold
 * into "+N"; when not even one chip fits, one chip counts them all: "4 filters", or "+4" when the
 * words would leave no room to type.
 */
export interface Fold {
  shown: number;
  cut?: number;
  count?: "words" | "plus";
}

/** Every chip shown: what a field shows before it's measured. */
export const UNFOLDED: Fold = { shown: Number.POSITIVE_INFINITY };

/** A first chip cut narrower than this no longer reads (about eight letters): the count says it instead. */
export const MIN_CUT = 96;
/** The only chip, cut down to leave room to type, keeps at least this much. */
export const MIN_ONE = 64;

export function foldChips({
  line,
  chips,
  more,
  count,
  typed,
  gap,
}: {
  /** The line's width. */
  line: number;
  /** Each chip's width, in order. */
  chips: number[];
  /** The "+N" chip's width at its widest N. */
  more: number;
  /** The width of the chip that counts them all ("4 filters"). */
  count: number;
  /** Room kept to type (typingRoom). */
  typed: number;
  /** Between two chips. */
  gap: number;
}): Fold {
  const all = chips.length;
  const run = (n: number) => chips.slice(0, n).reduce((s, w) => s + w + gap, 0);
  if (all === 0 || run(all) + typed <= line) return { shown: all };
  if (all === 1) return { shown: 1, cut: Math.max(MIN_ONE, Math.floor(line - typed - gap)) };
  for (let n = all - 1; n >= 1; n--) if (run(n) + more + gap + typed <= line) return { shown: n };
  const room = Math.floor(line - more - gap - typed - gap);
  if (room >= MIN_CUT) return { shown: 1, cut: room };
  return { shown: 0, count: count + gap + typed <= line ? "words" : "plus" };
}

/** Room kept to type on a folded line: the words typed (at least 72 px, at most 60% of the line), else a caret's worth. */
export const typingRoom = (line: number, words: number, typing: boolean) => (typing ? Math.min(Math.max(words + 14, 72), line * 0.6) : 16);

export const sameFold = (a: Fold, b: Fold) => a.shown === b.shown && a.cut === b.cut && a.count === b.count;
