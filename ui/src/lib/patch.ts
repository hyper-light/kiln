/**
 * A unified diff of one file's two texts, as `git apply` and `patch -p1` take it: hunks of the
 * lines removed and added, each with `context` lines around it. Lines are matched by their longest
 * common subsequence between the common start and end; a change too big to compare cheaply is one
 * hunk replacing the middle whole. Empty when the texts are the same.
 */

type Op = { t: " " | "-" | "+"; s: string };

/** Comparing more line pairs than this replaces the changed middle whole. */
const CELLS = 4_000_000;

/** A text's lines, and whether its last one ends with a newline. */
function lines(text: string): { list: string[]; eol: boolean } {
  if (text === "") return { list: [], eol: true };
  const eol = text.endsWith("\n");
  return { list: (eol ? text.slice(0, -1) : text).split("\n"), eol };
}

function ops(a: string[], b: string[]): Op[] {
  let pre = 0;
  while (pre < a.length && pre < b.length && a[pre] === b[pre]) pre++;
  let suf = 0;
  while (suf < a.length - pre && suf < b.length - pre && a[a.length - 1 - suf] === b[b.length - 1 - suf]) suf++;
  const x = a.slice(pre, a.length - suf);
  const y = b.slice(pre, b.length - suf);
  const mid: Op[] = [];
  const [n, m] = [x.length, y.length];
  if (n * m > CELLS) {
    x.forEach((s) => mid.push({ t: "-", s }));
    y.forEach((s) => mid.push({ t: "+", s }));
  } else {
    // How long a common subsequence each pair of suffixes has.
    const w = m + 1;
    const len = new Int32Array((n + 1) * w);
    const at = (i: number, j: number) => len[i * w + j] ?? 0;
    for (let i = n - 1; i >= 0; i--) {
      for (let j = m - 1; j >= 0; j--) len[i * w + j] = x[i] === y[j] ? at(i + 1, j + 1) + 1 : Math.max(at(i + 1, j), at(i, j + 1));
    }
    let [i, j] = [0, 0];
    while (i < n && j < m) {
      if (x[i] === y[j]) {
        mid.push({ t: " ", s: x[i] ?? "" });
        i++;
        j++;
      } else if (at(i + 1, j) >= at(i, j + 1)) {
        mid.push({ t: "-", s: x[i++] ?? "" });
      } else {
        mid.push({ t: "+", s: y[j++] ?? "" });
      }
    }
    while (i < n) mid.push({ t: "-", s: x[i++] ?? "" });
    while (j < m) mid.push({ t: "+", s: y[j++] ?? "" });
  }
  const same = (s: string): Op => ({ t: " ", s });
  return [...a.slice(0, pre).map(same), ...mid, ...a.slice(a.length - suf).map(same)];
}

const NO_EOL = "\\ No newline at end of file";

export function unifiedPatch(path: string, before: string, after: string, context = 3): string {
  if (before === after) return "";
  const a = lines(before);
  const b = lines(after);
  // A last line with no newline after it is another line than the same words with one: marked,
  // it matches only a last line like it.
  const mark = "\u0000";
  if (!a.eol && a.list.length) a.list[a.list.length - 1] += mark;
  if (!b.eol && b.list.length) b.list[b.list.length - 1] += mark;
  // Each op with the old and new line it is at (0-based).
  let [o, n] = [0, 0];
  const rows = ops(a.list, b.list).map((op) => {
    const r = { ...op, s: op.s.replace(mark, ""), o, n };
    if (op.t !== "+") o++;
    if (op.t !== "-") n++;
    return r;
  });
  const spans: [number, number][] = [];
  rows.forEach((r, k) => {
    if (r.t === " ") return;
    const [s, e] = [Math.max(0, k - context), Math.min(rows.length, k + context + 1)];
    const last = spans[spans.length - 1];
    if (last && s <= last[1]) last[1] = Math.max(last[1], e);
    else spans.push([s, e]);
  });
  let out = `--- a/${path}\n+++ b/${path}\n`;
  for (const [s, e] of spans) {
    const hunk = rows.slice(s, e);
    const first = hunk[0];
    if (!first) continue;
    const olds = hunk.filter((r) => r.t !== "+").length;
    const news = hunk.filter((r) => r.t !== "-").length;
    // A side with no lines here starts at the line before it (0 at the top), as diff writes it.
    out += `@@ -${olds ? first.o + 1 : first.o},${olds} +${news ? first.n + 1 : first.n},${news} @@\n`;
    for (const r of hunk) {
      out += `${r.t}${r.s}\n`;
      // The file's last line, without a newline after it: said so, once a side.
      const lastOld = r.t !== "+" && r.o === a.list.length - 1 && !a.eol;
      const lastNew = r.t !== "-" && r.n === b.list.length - 1 && !b.eol;
      if (lastOld || lastNew) out += `${NO_EOL}\n`;
    }
  }
  return out;
}
