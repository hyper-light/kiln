/**
 * kiln's diff view: one file's change, made to be read. Its header (the path, the size of the
 * change) stays in view while you scroll through the file. Hunks are unified on narrow containers
 * and side by side on wide ones where the caller allows it; long lines soft-wrap under a hanging
 * indent instead of scrolling sideways; the words that changed in an edited line are picked out;
 * long unchanged runs fold away behind a control that opens them in place. Which layout applies is
 * the diff's own width's call (container queries in diff.css), not the viewport's.
 */
import * as React from "react";
import { flushSync } from "react-dom";
import { ChevronRight, ChevronsUpDown, FileDiff as FileDiffIcon, FilePlus2 } from "lucide-react";
import type { FileDiff, Hunk } from "@/api/harness";
import { cn } from "@/lib/utils";
import { diffStats } from "./derive";
import "./diff.css";

/* ------------------------------------------------------------------ model */

export type Kind = "ctx" | "del" | "add";
/** [start, end) of a run of changed characters in a line. */
export type Span = [number, number];

export interface Line {
  kind: Kind;
  /** Its number in the old file (unchanged and deleted lines) and in the new one (unchanged and added). */
  o: number | null;
  n: number | null;
  text: string;
  /** Where the line's wrapped rows start, in columns: just past its own indentation. */
  hang: number;
  /** The words that changed, when the line is an edit of a line on the other side. */
  marks?: Span[];
}

/** One row of a change block side by side: a deletion's index, an addition's, or nothing. */
export type Pair = [number | null, number | null];

/**
 * A hunk is runs of unchanged lines around blocks of changes. A block's pairs line each deletion up
 * with the addition it became (or with nothing), the way the two sides show it side by side.
 */
export type Seg = { t: "ctx"; lines: Line[] } | { t: "chg"; dels: Line[]; adds: Line[]; pairs: Pair[] };

export interface HunkModel {
  segs: Seg[];
  /** Where it sits in the file, for the hairline above it. */
  where: string;
  /** It starts at the top of the file, so nothing above it went unshown. */
  top: boolean;
}

/**
 * Words (a flag or a kebab-case part keeps its hyphens: "--push", "-t", "-stat"), runs of
 * whitespace, the common operators, and otherwise single marks of punctuation.
 */
const TOKEN = /--?[\p{L}_][\p{L}\p{N}_]*|[\p{L}\p{N}_]+|\s+|->|=>|::|--+|\+\+|&&|\|\||[=!<>]=|[^\p{L}\p{N}_\s]/gu;
/** Past this many token pairs, comparing two lines falls back to what lies between their common ends. */
const MAX_CELLS = 40_000;
/** Past this many line pairs, a block pairs its lines in order instead of by likeness. */
const MAX_PAIRS = 400;
/** How alike two lines must be (the share of their non-blank characters in common) to read as one line edited. */
const ALIKE = 0.4;
/** Unchanged lines kept beside a change when the run around them folds. */
const KEEP = 3;
/** Fewer hidden lines than this aren't worth a control. */
const MIN_FOLD = 4;

interface Toks {
  text: string;
  /** Each token, and the offset it starts at. */
  s: string[];
  at: number[];
}

function tokenize(text: string): Toks {
  const s: string[] = [];
  const at: number[] = [];
  for (const m of text.matchAll(TOKEN)) {
    s.push(m[0]);
    at.push(m.index ?? 0);
  }
  return { text, s, at };
}

interface Likeness {
  /** The share of the two lines' non-blank characters they have in common. */
  sim: number;
  /** What changed in each. */
  a: Span[];
  b: Span[];
}

/**
 * The tokens that changed, as runs of characters: neighbours join across whitespace, and a run
 * sheds the whitespace at its ends (unless whitespace is all that changed there).
 */
function changed(t: Toks, keep: Uint8Array): Span[] {
  const out: Span[] = [];
  t.s.forEach((s, i) => {
    if (keep[i]) return;
    const from = t.at[i] ?? 0;
    const to = from + s.length;
    const last = out[out.length - 1];
    if (last && !/\S/.test(t.text.slice(last[1], from))) last[1] = to;
    else out.push([from, to]);
  });
  const blank = (k: number) => /\s/.test(t.text.charAt(k));
  return out.map(([from, to]): Span => {
    let s = from;
    let e = to;
    while (s < e && blank(s)) s++;
    while (e > s && blank(e - 1)) e--;
    return s < e ? [s, e] : [from, to];
  });
}

/** Lines too long to compare word by word: what lies between their common start and end. */
function compareEnds(a: string, b: string): Likeness {
  const max = Math.min(a.length, b.length);
  let p = 0;
  while (p < max && a[p] === b[p]) p++;
  let s = 0;
  while (s < max - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;
  const span = (t: string): Span[] => (p + s < t.length ? [[p, t.length - s]] : []);
  return { sim: a.length + b.length ? (2 * (p + s)) / (a.length + b.length) : 1, a: span(a), b: span(b) };
}

/** Two lines compared word by word (a longest common subsequence of their tokens). */
function compare(a: Toks, b: Toks): Likeness {
  const n = a.s.length;
  const m = b.s.length;
  if (n * m > MAX_CELLS) return compareEnds(a.text, b.text);
  // run(i, j): the length of the longest common subsequence of a's tokens from i and b's from j.
  const w = m + 1;
  const lcs = new Uint16Array((n + 1) * w);
  const run = (i: number, j: number) => lcs[i * w + j] ?? 0;
  for (let i = n - 1; i >= 0; i--)
    for (let j = m - 1; j >= 0; j--) lcs[i * w + j] = a.s[i] === b.s[j] ? run(i + 1, j + 1) + 1 : Math.max(run(i + 1, j), run(i, j + 1));
  const ka = new Uint8Array(n);
  const kb = new Uint8Array(m);
  for (let i = 0, j = 0; i < n && j < m; ) {
    if (a.s[i] === b.s[j]) {
      ka[i++] = 1;
      kb[j++] = 1;
    } else if (run(i + 1, j) >= run(i, j + 1)) i++;
    else j++;
  }
  let same = 0;
  let all = 0;
  const count = (t: Toks, keep: Uint8Array) =>
    t.s.forEach((s, i) => {
      if (!/\S/.test(s)) return;
      all += s.length;
      if (keep[i]) same += s.length;
    });
  count(a, ka);
  count(b, kb);
  return { sim: all ? same / all : 1, a: changed(a, ka), b: changed(b, kb) };
}

/**
 * Lines a block's deletions up with the additions they became, and marks what changed in each
 * pair. Pairs are chosen by likeness (the most of it, in order) so an inserted or dropped line
 * doesn't shift every pair after it; lines alike in nothing sit side by side unmarked.
 */
function pair(dels: Line[], adds: Line[]): Pair[] {
  const d = dels.length;
  const a = adds.length;
  const td = dels.map((l) => tokenize(l.text));
  const ta = adds.map((l) => tokenize(l.text));
  const like = (i: number, j: number): Likeness | null => {
    const x = td[i];
    const y = ta[j];
    if (!x || !y) return null;
    const k = compare(x, y);
    return k.sim >= ALIKE ? k : null;
  };
  const take = (i: number, j: number, k: Likeness) => {
    const x = dels[i];
    const y = adds[j];
    if (!x || !y) return;
    x.marks = k.a;
    y.marks = k.b;
  };
  const inOrder = (from: number, toD: number, toA: number): Pair[] =>
    Array.from({ length: Math.max(toD, toA) - from }, (_, k): Pair => [from + k < toD ? from + k : null, from + k < toA ? from + k : null]);

  if (d === 0 || a === 0 || d * a > MAX_PAIRS) {
    for (let i = 0; i < Math.min(d, a); i++) {
      const k = like(i, i);
      if (k) take(i, i, k);
    }
    return inOrder(0, d, a);
  }

  const ks: (Likeness | null)[] = [];
  for (let i = 0; i < d; i++) for (let j = 0; j < a; j++) ks.push(like(i, j));
  const kAt = (i: number, j: number) => ks[i * a + j] ?? null;
  // most(i, j): the most likeness a pairing of the deletions from i with the additions from j keeps.
  const w = a + 1;
  const best = new Float64Array((d + 1) * w);
  const most = (i: number, j: number) => best[i * w + j] ?? 0;
  for (let i = d - 1; i >= 0; i--)
    for (let j = a - 1; j >= 0; j--) {
      const k = kAt(i, j);
      best[i * w + j] = Math.max(most(i + 1, j), most(i, j + 1), k ? k.sim + most(i + 1, j + 1) : 0);
    }
  const matches: [number, number][] = [];
  for (let i = 0, j = 0; i < d && j < a; ) {
    const k = kAt(i, j);
    if (k && most(i, j) === k.sim + most(i + 1, j + 1)) {
      take(i, j, k);
      matches.push([i++, j++]);
    } else if (most(i + 1, j) >= most(i, j + 1)) i++;
    else j++;
  }
  const pairs: Pair[] = [];
  let i = 0;
  let j = 0;
  for (const [mi, mj] of [...matches, [d, a] as [number, number]]) {
    // Between two matches, what's left of each side sits side by side, in order.
    for (let k = 0; i + k < mi || j + k < mj; k++) pairs.push([i + k < mi ? i + k : null, j + k < mj ? j + k : null]);
    if (mi < d && mj < a) pairs.push([mi, mj]);
    i = mi + 1;
    j = mj + 1;
  }
  return pairs;
}

/**
 * Where a line's wrapped rows start, in columns: its indentation (a tab runs to the next stop of
 * four) plus two. Browsers count tab stops from where wrapped rows start, so a line with a tab in
 * it hangs by whole stops, and its tabs still land where the file means them to.
 */
function hangOf(text: string): number {
  let col = 0;
  let i = 0;
  for (; i < text.length; i++) {
    const ch = text[i];
    if (ch === " ") col++;
    else if (ch === "\t") col += 4 - (col % 4);
    else break;
  }
  if (i === text.length) return 2;
  return text.includes("\t") ? Math.ceil((col + 2) / 4) * 4 : col + 2;
}

function line(kind: Kind, o: number | null, n: number | null, text: string): Line {
  return { kind, o, n, text, hang: hangOf(text) };
}

/** A hunk as rows: its unchanged runs and its change blocks, each deletion paired with what it became (the review page reads it too). */
export function hunkOf(h: Hunk): HunkModel {
  const segs: Seg[] = [];
  let o = h.old_start;
  let n = h.new_start;
  const ls = h.lines;
  for (let i = 0; i < ls.length; ) {
    const tag = (ls[i] ?? "").charAt(0);
    if (tag === "-" || tag === "+") {
      // A block runs to the next unchanged line; its deletions and additions keep their own order.
      const dels: Line[] = [];
      const adds: Line[] = [];
      for (; i < ls.length; i++) {
        const l = ls[i] ?? "";
        const t = l.charAt(0);
        if (t === "-") dels.push(line("del", o++, null, l.slice(1)));
        else if (t === "+") adds.push(line("add", null, n++, l.slice(1)));
        else if (t !== "\\") break;
      }
      segs.push({ t: "chg", dels, adds, pairs: pair(dels, adds) });
      continue;
    }
    // "\ No newline at end of file" says nothing worth a row.
    if (tag !== "\\") {
      const l = line("ctx", o++, n++, (ls[i] ?? "").slice(1));
      const last = segs[segs.length - 1];
      if (last?.t === "ctx") last.lines.push(l);
      else segs.push({ t: "ctx", lines: [l] });
    }
    i++;
  }
  const end = h.new_start + h.new_lines - 1;
  return {
    segs,
    where: h.new_lines === 0 ? `at line ${h.old_start}` : end > h.new_start ? `lines ${h.new_start}–${end}` : `line ${h.new_start}`,
    top: h.old_start <= 1 && h.new_start <= 1,
  };
}

/** How a run of unchanged lines folds: how many to keep above and below the fold, or null to show it all. */
function foldOf(len: number, si: number, segs: number): { head: number; tail: number } | null {
  const alone = segs === 1;
  const head = si === 0 && !alone ? 0 : KEEP;
  const tail = si === segs - 1 && !alone ? 0 : KEEP;
  return len - head - tail >= MIN_FOLD ? { head, tail } : null;
}

/* ------------------------------------------------------------------ view */

type Vars = React.CSSProperties & Record<`--${string}`, number | string>;
/** The row a line takes side by side (unified, rows simply follow the DOM). */
const at = (sr: number): Vars => ({ "--sr": sr });
const SIGN: Record<Kind, string> = { ctx: "", del: "−", add: "+" };

/** A "." after a call or an index, before a name: where a long chain of calls wraps best. */
const SEAM = /(?<=[)\]?])\.(?=[\p{L}_])/gu;
/** The hyphens that start a flag or a custom property ("--push", "-t", "var(--line)"). */
const FLAG = /(?<=^|[\s"'=([{,])--?(?=[\p{L}_])/gu;

/**
 * Where a soft-wrapped line may and may not break, past its indentation: "may" before a chain's
 * dots (a <wbr>, so a long chain wraps at its links rather than mid-name); "may not" inside "->"
 * or after a flag's hyphens (diff.css glues them with a word joiner). Neither is ever copied.
 */
function seamsOf(text: string, from: number): { at: number; glue: boolean }[] {
  const out: { at: number; glue: boolean }[] = [];
  for (const m of text.matchAll(SEAM)) if ((m.index ?? 0) > from) out.push({ at: m.index ?? 0, glue: false });
  for (const m of text.matchAll(FLAG)) if ((m.index ?? 0) >= from) out.push({ at: (m.index ?? 0) + m[0].length, glue: true });
  for (let i = text.indexOf("->", from); i !== -1; i = text.indexOf("->", i + 2)) out.push({ at: i + 1, glue: true });
  return out.sort((a, b) => a.at - b.at);
}

/**
 * A line's text: its changed words marked, its seams in place, and its indentation kept from
 * wrapping (white-space: pre), or a long first word would wrap whole and leave its first row empty.
 */
function Words({ text, marks }: { text: string; marks?: Span[] }) {
  const ind = text.length - text.trimStart().length;
  const seams = seamsOf(text, ind);
  const runs: { s: number; e: number; mark: boolean }[] = [];
  let from = 0;
  for (const [s, e] of marks ?? []) {
    if (s > from) runs.push({ s: from, e: s, mark: false });
    runs.push({ s, e, mark: true });
    from = e;
  }
  if (from < text.length) runs.push({ s: from, e: text.length, mark: false });
  let k = 0;
  return (
    <>
      {runs.map((r, i) => {
        const parts: React.ReactNode[] = [];
        let at = r.s;
        if (at < ind) {
          at = Math.min(ind, r.e);
          parts.push(
            <span key="ind" className="hx-ind">
              {text.slice(r.s, at)}
            </span>,
          );
        }
        for (let p = seams[k]; p !== undefined && p.at < r.e; p = seams[++k]) {
          if (p.at > at) parts.push(text.slice(at, p.at));
          parts.push(p.glue ? <span key={p.at} className="hx-glue" /> : <wbr key={p.at} />);
          at = p.at;
        }
        if (at < r.e) parts.push(text.slice(at, r.e));
        return r.mark ? <mark key={i}>{parts}</mark> : <React.Fragment key={i}>{parts}</React.Fragment>;
      })}
    </>
  );
}

const Row = React.memo(function Row({ line, sr, split, unfolded, first }: { line: Line; sr: number; split: boolean; unfolded?: boolean; first?: string }) {
  const hang: Vars | undefined = line.hang === 2 ? undefined : { "--hang": line.hang };
  return (
    <div
      className="hx-row"
      data-kind={line.kind}
      data-unfolded={unfolded || undefined}
      data-fold-start={first}
      tabIndex={first ? -1 : undefined}
      style={split ? at(sr) : undefined}
    >
      <span className="hx-no" data-side="o" aria-hidden="true">
        {line.o ?? ""}
      </span>
      <span className="hx-no" data-side="n" aria-hidden="true">
        {line.n ?? ""}
      </span>
      <span className="hx-mk">{SIGN[line.kind]}</span>
      <code className="hx-code" style={hang}>
        <Words text={line.text} marks={line.marks} />
      </code>
      {split && line.kind === "ctx" && (
        // Side by side, an unchanged line shows on both sides.
        <code className="hx-code hx-dup" style={hang} aria-hidden="true">
          <Words text={line.text} />
        </code>
      )}
    </div>
  );
});

function HunkRows({ h, hi, split, unfolded, onUnfold }: { h: HunkModel; hi: number; split: boolean; unfolded: ReadonlySet<string>; onUnfold: (id: string) => void }) {
  const out: React.ReactNode[] = [];
  let sr = 1;
  h.segs.forEach((seg, si) => {
    if (seg.t === "ctx") {
      const id = `${hi}.${si}`;
      const len = seg.lines.length;
      const fold = foldOf(len, si, h.segs.length);
      const open = unfolded.has(id);
      seg.lines.forEach((l, k) => {
        const folded = fold !== null && k >= fold.head && k < len - fold.tail;
        if (folded && !open) {
          if (k === fold.head) {
            const count = len - fold.head - fold.tail;
            const r = sr++;
            out.push(
              <button key={`${si}f`} type="button" className="hx-fold" style={split ? at(r) : undefined} onClick={() => onUnfold(id)}>
                <ChevronsUpDown size={12} className="hx-fold-icon" aria-hidden="true" />
                <span className="hx-fold-label">Show {count} unchanged lines</span>
              </button>,
            );
          }
          return;
        }
        out.push(<Row key={`${si}.${k}`} line={l} sr={sr++} split={split} unfolded={folded} first={folded && k === fold?.head ? id : undefined} />);
      });
      return;
    }
    // Unified: the block's deletions, then its additions. Side by side: each in its pair's row.
    const top = sr;
    const rowD: number[] = [];
    const rowA: number[] = [];
    const fills: React.ReactNode[] = [];
    seg.pairs.forEach(([d, a], k) => {
      if (d !== null) rowD[d] = top + k;
      else if (split) fills.push(<span key={`${si}l${k}`} className="hx-fill" data-side="l" style={at(top + k)} />);
      if (a !== null) rowA[a] = top + k;
      else if (split) fills.push(<span key={`${si}r${k}`} className="hx-fill" data-side="r" style={at(top + k)} />);
    });
    seg.dels.forEach((l, k) => out.push(<Row key={`${si}d${k}`} line={l} sr={rowD[k] ?? top} split={split} />));
    seg.adds.forEach((l, k) => out.push(<Row key={`${si}a${k}`} line={l} sr={rowA[k] ?? top} split={split} />));
    out.push(...fills);
    sr += seg.pairs.length;
  });
  return <div className="hx-hunk">{out}</div>;
}

export function DiffStat({ add, del, className }: { add: number; del: number; className?: string }) {
  return (
    <span className={cn("hx-stat", className)}>
      <span className="hx-add">+{add}</span>
      <span className="hx-del">−{del}</span>
    </span>
  );
}

/** One file's change: a header (path, size of the change) and its hunks. */
export function FileDiffView({
  diff,
  label,
  defaultOpen = true,
  layout = "auto",
  reveal = 0,
}: {
  /** Find shows a match in it: it opens (a new number each time one is shown). */
  reveal?: number;
  diff: FileDiff;
  /** What to call the file (a path relative to the agent's folder). */
  label?: string;
  defaultOpen?: boolean;
  /**
   * auto: side by side when the diff is at least 760px wide. split: side by side from 640px.
   * unified: never side by side. Narrower than 640px, every diff is unified.
   */
  layout?: "auto" | "unified" | "split";
}) {
  const [open, setOpen] = React.useState(defaultOpen);
  const [revealed, setRevealed] = React.useState(0);
  if (reveal && reveal !== revealed) {
    setRevealed(reveal);
    setOpen(true);
  }
  const [unfolded, setUnfolded] = React.useState<ReadonlySet<string>>(() => new Set());
  const body = React.useRef<HTMLDivElement>(null);
  const hunks = diff.hunks;
  const model = React.useMemo(() => {
    let last = 1;
    for (const h of hunks) last = Math.max(last, h.old_start + h.old_lines - 1, h.new_start + h.new_lines - 1);
    const stats = diffStats([{ hunks }]);
    const only = stats.del > 0 && stats.add === 0 ? "del" : stats.add > 0 && stats.del === 0 ? "add" : undefined;
    return { hunks: open ? hunks.map(hunkOf) : [], digits: Math.max(2, String(last).length), only, stats };
  }, [hunks, open]);
  // Side by side only helps when lines changed on both sides (not in a new file or a pure deletion,
  // whose other column of numbers goes, too).
  const split = layout !== "unified" && !model.only ? (layout === "split" ? "on" : "auto") : undefined;

  const unfold = React.useCallback((id: string) => {
    flushSync(() => setUnfolded((u) => new Set(u).add(id)));
    // The control that opened a fold is gone: keep focus where it was, on the first line it showed.
    body.current?.querySelector<HTMLElement>(`[data-fold-start="${id}"]`)?.focus({ preventScroll: true });
  }, []);

  const name = label ?? diff.path;
  const cut = name.lastIndexOf("/") + 1;
  return (
    <div className="hx-diff" data-split={split} data-only={model.only} style={{ "--digits": model.digits } as Vars}>
      <button type="button" className="hx-diff-head" aria-expanded={open} onClick={() => setOpen(!open)}>
        <ChevronRight size={13} className={cn("hx-chev", open && "rotate-90")} />
        {diff.created ? <FilePlus2 size={13} className="hx-diff-icon hx-diff-icon-new" /> : <FileDiffIcon size={13} className="hx-diff-icon" />}
        <span className="hx-path" title={name}>
          {cut > 0 && (
            <span className="hx-path-dir">
              <span dir="ltr">{name.slice(0, cut)}</span>
            </span>
          )}
          <span className="hx-path-name">{name.slice(cut)}</span>
        </span>
        {diff.created && <span className="hx-tag">new</span>}
        <DiffStat add={model.stats.add} del={model.stats.del} className="ml-auto" />
      </button>
      {open && (
        <div ref={body} className="hx-diff-body">
          {model.hunks.length === 0 ? (
            <div className="hx-diff-empty">No line changes.</div>
          ) : (
            model.hunks.map((h, hi) => (
              <React.Fragment key={hi}>
                {(hi > 0 || !h.top) && (
                  <div className="hx-sep">
                    <span className="hx-sep-label">{h.where}</span>
                  </div>
                )}
                <HunkRows h={h} hi={hi} split={split !== undefined} unfolded={unfolded} onUnfold={unfold} />
              </React.Fragment>
            ))
          )}
        </div>
      )}
    </div>
  );
}
