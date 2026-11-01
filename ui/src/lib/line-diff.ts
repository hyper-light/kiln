/**
 * Two texts' differing lines, as an update draft's Changes view shows them (docs/ci.md W29-R22,
 * pl-7): each line kept, taken out or put in, numbered as it is in its own text, with a few kept
 * lines around each change and the rest folded. Pure: a longest common subsequence over lines.
 */

export interface DiffLine {
  kind: "same" | "del" | "add" | "gap";
  text: string;
  /** Its number in the old text (from 1), or in the new; the other side's null. */
  old: number | null;
  now: number | null;
}

/** Past this many lines a side, every line reads as changed: the table would be too big to make. */
const MAX_LINES = 3000;

export function lineDiff(a: string, b: string, context = 2): DiffLine[] {
  const x = a.split("\n");
  const y = b.split("\n");
  const all: DiffLine[] = [];
  if (x.length > MAX_LINES || y.length > MAX_LINES) {
    x.forEach((t, i) => all.push({ kind: "del", text: t, old: i + 1, now: null }));
    y.forEach((t, j) => all.push({ kind: "add", text: t, old: null, now: j + 1 }));
  } else {
    // lcs[i][j]: the longest common run of x[i..] and y[j..], flattened.
    const w = y.length + 1;
    const lcs = new Uint32Array((x.length + 1) * w);
    for (let i = x.length - 1; i >= 0; i--)
      for (let j = y.length - 1; j >= 0; j--) lcs[i * w + j] = x[i] === y[j] ? (lcs[(i + 1) * w + j + 1] ?? 0) + 1 : Math.max(lcs[(i + 1) * w + j] ?? 0, lcs[i * w + j + 1] ?? 0);
    let [i, j] = [0, 0];
    while (i < x.length || j < y.length) {
      if (i < x.length && j < y.length && x[i] === y[j]) {
        all.push({ kind: "same", text: x[i] ?? "", old: i + 1, now: j + 1 });
        i++;
        j++;
      } else if (i < x.length && (j >= y.length || (lcs[(i + 1) * w + j] ?? 0) >= (lcs[i * w + j + 1] ?? 0))) {
        // What's taken out before what's put in, as diffs read.
        all.push({ kind: "del", text: x[i] ?? "", old: i + 1, now: null });
        i++;
      } else {
        all.push({ kind: "add", text: y[j] ?? "", old: null, now: j + 1 });
        j++;
      }
    }
  }
  // Kept lines near a change stay; the rest fold into one gap each.
  const near = all.map((l, k) => l.kind !== "same" || all.slice(Math.max(0, k - context), k + context + 1).some((m) => m.kind !== "same"));
  const out: DiffLine[] = [];
  all.forEach((l, k) => {
    if (near[k]) out.push(l);
    else if (out.at(-1)?.kind !== "gap") out.push({ kind: "gap", text: "", old: null, now: null });
  });
  return out;
}
