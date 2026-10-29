/**
 * One file of a diff under review (W14-R06): its header (where it is, how it changed, viewed), and
 * its hunks, unified or side by side, syntax-highlighted (tree-sitter, lib/syntax), the words that
 * changed in an edited line picked out, blocks that moved marked with where they went, unchanged
 * runs folded, and threads in place under the lines they're about. A line's number is where a
 * comment starts: click it, shift-click another to take a range.
 *
 * Each hunk is laid out only as it nears the screen (`content-visibility: auto`), so a diff of
 * thousands of lines scrolls as smoothly as a short one.
 */
import * as React from "react";
import { ChevronRight, MessageSquarePlus } from "lucide-react";
import type { Hunk } from "@/api/harness";
import { reviewApi, whoSaid, type ChangedFile, type Thread } from "@/api/review";
import { Button } from "@/components/ui/button";
import { relativeTime } from "@/lib/format";
import { resolveLanguage, useHighlighter, type Highlight, type Tok } from "@/lib/syntax/browser";
import { AboutCard, AboutContext, cardKey, linesLabel, offsetAt, selecting, useCards, wordAt, type Asked } from "@/components/code/about";
import { hunkOf, type Line, type Seg, type Span } from "@/views/harness/diff";
import { ignoringWhitespace, isImage, placeOf, type MovedTo, type PatchFile, type Place } from "./patch";
import "@/components/code/syntax.css";

/** Unchanged lines kept beside a change when a run folds; fewer hidden than MIN_FOLD aren't worth a control. */
const KEEP = 3;
const MIN_FOLD = 4;

export type Side = "old" | "new";

/** Lines picked for a comment: a side and a range on it. */
export interface Pick {
  side: Side;
  start: number;
  end: number;
}

export interface Moves {
  del: Map<Place, MovedTo>;
  add: Map<Place, MovedTo>;
}

export interface FileProps {
  idx: number;
  file: PatchFile;
  meta: ChangedFile | undefined;
  diffKey: string;
  version: number;
  split: boolean;
  whitespace: boolean;
  moves: Moves;
  /** Every file's path by its index (for "moved to …"). */
  pathOf: (idx: number) => string;
  /** This file's threads. */
  threads: Thread[];
  onViewed: (path: string, on: boolean) => void;
  /** A new thread on lines of this file (draft unless `publish`). */
  onComment: (pick: Pick, body: string, publish: boolean) => Promise<void>;
  onReply: (thread: Thread, body: string, publish: boolean) => Promise<void>;
  /** None: threads can't be resolved from here. */
  onResolve?: (thread: Thread, resolved: boolean) => Promise<void>;
  /** Comments go out at once, and files aren't ticked off (a host's pull request: W23-R06). */
  now?: boolean;
}

/** Where a thread sits in the version shown: its side and the line it ends on (null: outdated here). */
export function anchorOf(t: Thread, version: number): { side: Side; start: number; end: number } | null {
  if (t.start_line == null) return null;
  if (t.version === version) return { side: t.side, start: t.start_line, end: t.end_line ?? t.start_line };
  if (t.now && t.now.version === version) return { side: t.side, start: t.now.start_line, end: t.now.end_line };
  return null;
}

/** A line's text in runs: each a token class (or none), changed or not. */
function pieces(text: string, toks: Tok[] | undefined, marks: Span[] | undefined): { s: number; e: number; cls: string; mark: boolean }[] {
  const cuts = new Set<number>([0, text.length]);
  for (const [s, e] of toks ?? []) {
    cuts.add(s);
    cuts.add(e);
  }
  for (const [s, e] of marks ?? []) {
    cuts.add(s);
    cuts.add(e);
  }
  const at = [...cuts].filter((c) => c >= 0 && c <= text.length).sort((a, b) => a - b);
  const out: { s: number; e: number; cls: string; mark: boolean }[] = [];
  for (let i = 0; i + 1 < at.length; i++) {
    const s = at[i] ?? 0;
    const e = at[i + 1] ?? s;
    if (e <= s) continue;
    const tok = (toks ?? []).find(([ts, te]) => ts <= s && e <= te);
    const mark = (marks ?? []).some(([ms, me]) => ms <= s && e <= me);
    out.push({ s, e, cls: tok?.[2] ?? "", mark });
  }
  return out;
}

function Code({ text, toks, marks, onClick }: { text: string; toks?: Tok[]; marks?: Span[]; onClick?: React.MouseEventHandler<HTMLElement> }) {
  if (!text)
    return (
      <code className="rv-code" onClick={onClick}>
        {"​"}
      </code>
    );
  return (
    <code className="rv-code" onClick={onClick}>
      {pieces(text, toks, marks).map((p) => {
        const s = text.slice(p.s, p.e);
        const cls = [p.cls, p.mark ? "rv-mark" : ""].filter(Boolean).join(" ");
        return cls ? (
          <span key={p.s} className={cls}>
            {s}
          </span>
        ) : (
          <React.Fragment key={p.s}>{s}</React.Fragment>
        );
      })}
    </code>
  );
}

/** Each side's tokens by line number, for one hunk: each side read as one piece of code, so strings and comments over lines read right. */
function tokensOf(hl: Highlight | null, h: Hunk): { old: Map<number, Tok[]>; neu: Map<number, Tok[]> } {
  const old = new Map<number, Tok[]>();
  const neu = new Map<number, Tok[]>();
  if (!hl) return { old, neu };
  const oldLines: string[] = [];
  const oldNums: number[] = [];
  const newLines: string[] = [];
  const newNums: number[] = [];
  let o = h.old_start;
  let n = h.new_start;
  for (const l of h.lines) {
    const t = l.charAt(0);
    if (t === "\\") continue;
    const text = l.slice(1);
    if (t !== "+") {
      oldLines.push(text);
      oldNums.push(o++);
    }
    if (t !== "-") {
      newLines.push(text);
      newNums.push(n++);
    }
  }
  if (oldLines.length) hl(oldLines.join("\n"))?.forEach((toks, i) => old.set(oldNums[i] ?? -1, toks));
  if (newLines.length) hl(newLines.join("\n"))?.forEach((toks, i) => neu.set(newNums[i] ?? -1, toks));
  return { old, neu };
}

/** Which lines of a hunk a fold hides, run by run: `ctx` runs keep KEEP lines beside each change. */
function foldsOf(segs: Seg[]): Map<number, { head: number; tail: number }> {
  const out = new Map<number, { head: number; tail: number }>();
  segs.forEach((seg, si) => {
    if (seg.t !== "ctx") return;
    const alone = segs.length === 1;
    const head = si === 0 && !alone ? 0 : KEEP;
    const tail = si === segs.length - 1 && !alone ? 0 : KEEP;
    if (seg.lines.length - head - tail >= MIN_FOLD) out.set(si, { head, tail });
  });
  return out;
}

export function FileDiff(p: FileProps) {
  const { file, meta } = p;
  const [open, setOpen] = React.useState(!(meta?.generated ?? false) && !(meta?.viewed ?? false));
  const viewed = meta?.viewed ?? false;
  const added = meta?.added ?? file.hunks.reduce((a, h) => a + h.lines.filter((l) => l.startsWith("+")).length, 0);
  const deleted = meta?.deleted ?? file.hunks.reduce((a, h) => a + h.lines.filter((l) => l.startsWith("-")).length, 0);
  const outdated = p.threads.filter((t) => t.start_line != null && !anchorOf(t, p.version));
  return (
    <section className="rv-file" id={`rv-file-${p.idx}`} data-viewed={viewed || undefined}>
      <header className="rv-file-head">
        <button type="button" className="rv-file-toggle" aria-expanded={open} onClick={() => setOpen(!open)}>
          <ChevronRight size={14} className="rv-chev" aria-hidden="true" />
          <span className="rv-path">
            {file.oldPath && file.oldPath !== file.path ? (
              <>
                <span className="rv-path-old">{file.oldPath}</span> → {file.path}
              </>
            ) : (
              file.path
            )}
          </span>
        </button>
        <span className="rv-file-facts">
          {file.status !== "modified" && (
            <span className="rv-chip" data-status={file.status}>
              {file.status}
              {file.similarity != null && file.status !== "added" && file.status !== "deleted" ? ` ${file.similarity}%` : ""}
            </span>
          )}
          {meta?.generated && <span className="rv-chip">generated</span>}
          {file.modeChange && <span className="rv-chip">{file.modeChange}</span>}
          {!file.binary && (
            <span className="rv-stat">
              <b>+{added}</b> <i>−{deleted}</i>
            </span>
          )}
          {!p.now && (
            <label className="rv-viewed">
              <input
                type="checkbox"
                className="check"
                checked={viewed}
                onChange={(e) => {
                  p.onViewed(file.path, e.target.checked);
                  setOpen(!e.target.checked);
                }}
              />
              Viewed
            </label>
          )}
        </span>
      </header>
      {open && (
        <div className="rv-file-body">
          {outdated.length > 0 && (
            <div className="rv-outdated">
              <p className="rv-sub">
                {outdated.length} thread{outdated.length === 1 ? "" : "s"} on code this version no longer has
              </p>
              {outdated.map((t) => (
                <ThreadView key={t.id} t={t} onReply={p.onReply} onResolve={p.onResolve} now={p.now} />
              ))}
            </div>
          )}
          {file.binary ? (
            <BinaryBody p={p} />
          ) : file.hunks.length === 0 ? (
            <p className="rv-sub rv-pad">{file.modeChange ? "Only its mode changed." : file.status === "renamed" ? "Moved as it was." : "No line changes."}</p>
          ) : (
            <Hunks p={p} />
          )}
        </div>
      )}
    </section>
  );
}

function BinaryBody({ p }: { p: FileProps }) {
  const { file } = p;
  if (!isImage(file.path)) return <p className="rv-sub rv-pad">Binary file, not shown.</p>;
  const src = (side: Side, path: string) => reviewApi.blobUrl(p.diffKey, p.version, path, side);
  return (
    <div className="rv-images">
      {file.status !== "added" && (
        <figure>
          <figcaption className="rv-sub">Before</figcaption>
          <img src={src("old", file.oldPath ?? file.path)} alt={`${file.oldPath ?? file.path} before`} loading="lazy" />
        </figure>
      )}
      {file.status !== "deleted" && (
        <figure>
          <figcaption className="rv-sub">After</figcaption>
          <img src={src("new", file.path)} alt={`${file.path} after`} loading="lazy" />
        </figure>
      )}
    </div>
  );
}

/** The file's hunks, in their language's colours (once its grammar is here), with the lines picked for a comment and the threads under their lines. */
function Hunks({ p }: { p: FileProps }) {
  const [pick, setPick] = React.useState<Pick | null>(null);
  const [unfolded, setUnfolded] = React.useState<ReadonlySet<string>>(new Set());
  const hl = useHighlighter(resolveLanguage(null, p.file.path));
  const hunks = React.useMemo(() => p.file.hunks.map((h) => (p.whitespace ? ignoringWhitespace(h) : h)), [p.file.hunks, p.whitespace]);
  const models = React.useMemo(() => hunks.map((h) => ({ m: hunkOf(h), tok: tokensOf(hl, h) })), [hunks, hl]);
  // Threads by where they end: `old:12`, `new:40`.
  const byEnd = React.useMemo(() => {
    const m = new Map<string, Thread[]>();
    for (const t of p.threads) {
      const a = anchorOf(t, p.version);
      if (!a) continue;
      const k = `${a.side}:${a.end}`;
      m.set(k, [...(m.get(k) ?? []), t]);
    }
    return m;
  }, [p.threads, p.version]);
  const choose = (side: Side, line: number, extend: boolean) => {
    if (extend && pick && pick.side === side) setPick({ side, start: Math.min(pick.start, line), end: Math.max(pick.end, line) });
    else if (pick && pick.side === side && pick.start === line && pick.end === line) setPick(null);
    else setPick({ side, start: line, end: line });
  };
  // An agent's diff: a name or a line asked about opens a card under it (components/code/about.tsx).
  const about = React.useContext(AboutContext);
  const cards = useCards<AskedLines>();
  const ask = about
    ? (side: Side, line: number | null, text: string, e: React.MouseEvent<HTMLElement>) => {
        if (line == null || selecting()) return;
        const off = offsetAt(e.currentTarget, e.clientX, e.clientY);
        const word = off === null ? null : wordAt(text, off);
        const selection = word ? text.slice(word.start, word.end) : undefined;
        cards.update((prev) => {
          let [from, to] = [line, line];
          // Another line, while lines of the same side are asked about: the lines between are taken in.
          if (!selection && prev && !prev.ask.selection && prev.side === side && (line < prev.from || line > prev.to)) [from, to] = [Math.min(prev.from, line), Math.max(prev.to, line)];
          else if (prev && prev.side === side && prev.from === line && prev.to === line && prev.ask.selection === selection) return null;
          const label = selection ?? linesLabel(from, to);
          const where = `${p.file.path.split("/").pop() ?? p.file.path} · ${side === "old" ? "old " : ""}${linesLabel(from, to)}`;
          return {
            key: cardKey(`${side}${from}-${to}`),
            side,
            from,
            to,
            ask: { path: p.file.path, line: from, end: to > from ? to : undefined, selection, code: around(hunks, side, from, to), label, where },
          };
        });
      }
    : undefined;
  const ctx: RowCtx = {
    p,
    pick,
    choose,
    byEnd,
    close: () => setPick(null),
    ask,
    asked: cards.open,
    cards: [...cards.closing.map((c) => ({ c, closing: true })), ...(cards.open ? [{ c: cards.open, closing: false }] : [])],
    closeAsk: () => cards.update(() => null),
    goneAsk: cards.gone,
  };
  return (
    <div className="rv-hunks" data-split={p.split || undefined}>
      {models.map(({ m, tok }, hi) => {
        const folds = foldsOf(m.segs);
        // The hunk's rows in order, each with the lines it holds (`new:12`), so a chunk with a
        // thread or the lines picked in it is drawn at once.
        const items: Item[] = [];
        const row = (key: string, node: React.ReactNode, at: (string | null)[]) => items.push({ key, node, at: at.filter((a): a is string => a != null) });
        const k = (side: Side, line: number | null) => (line == null ? null : `${side}:${line}`);
        m.segs.forEach((seg, si) => {
          if (seg.t === "ctx") {
            const fold = folds.get(si);
            const key = `${hi}:${si}`;
            const ctxRow = (l: Line) => row(`c${l.n}`, <CtxRow l={l} tok={tok} c={ctx} />, [k("new", l.n)]);
            if (!fold || unfolded.has(key)) return seg.lines.forEach(ctxRow);
            const hidden = seg.lines.length - fold.head - fold.tail;
            seg.lines.slice(0, fold.head).forEach(ctxRow);
            row(
              `f${key}`,
              <button type="button" className="rv-fold" onClick={() => setUnfolded(new Set([...unfolded, key]))}>
                Show {hidden} unchanged line{hidden === 1 ? "" : "s"}
              </button>,
              [],
            );
            seg.lines.slice(seg.lines.length - fold.tail).forEach(ctxRow);
            return;
          }
          if (p.split)
            return seg.pairs.forEach(([di, ai], pi) => {
              const d = di == null ? undefined : seg.dels[di];
              const a = ai == null ? undefined : seg.adds[ai];
              row(`s${si}-${pi}`, <SplitRow d={d} a={a} tok={tok} c={ctx} />, [k("old", d?.o ?? null), k("new", a?.n ?? null)]);
            });
          seg.dels.forEach((l) => row(`d${l.o}`, <ChgRow l={l} tok={tok} c={ctx} />, [k("old", l.o)]));
          seg.adds.forEach((l) => row(`a${l.n}`, <ChgRow l={l} tok={tok} c={ctx} />, [k("new", l.n)]));
        });
        const pickAt = pick ? `${pick.side}:${pick.end}` : null;
        const chunks: Item[][] = [];
        for (let i = 0; i < items.length; i += CHUNK) chunks.push(items.slice(i, i + CHUNK));
        return (
          <div key={hi} className="rv-hunk" style={{ containIntrinsicSize: `auto ${Math.max(1, items.length) * ROW_PX + 26}px` }}>
            <div className="rv-hunk-head">{m.where}</div>
            {chunks.length <= 2
              ? items.map((it) => <React.Fragment key={it.key}>{it.node}</React.Fragment>)
              : chunks.map((c, ci) => (
                  <Chunk key={ci} rows={c.length} eager={ci === 0 || c.some((it) => it.at.some((a) => byEnd.has(a) || a === pickAt))}>
                    {c.map((it) => (
                      <React.Fragment key={it.key}>{it.node}</React.Fragment>
                    ))}
                  </Chunk>
                ))}
          </div>
        );
      })}
    </div>
  );
}

/** A row of a hunk, as a chunk holds it: the lines it's on (`old:12`, `new:40`). */
interface Item {
  key: string;
  node: React.ReactNode;
  at: string[];
}

/** Rows a chunk holds; past two chunks a hunk is drawn chunk by chunk, as each nears the screen. */
const CHUNK = 60;
/** A row's height (12px Geist Mono at a 1.6 line height), as a chunk not drawn yet holds its place. */
const ROW_PX = 19.2;

/**
 * Part of a long hunk: a placeholder of its height until it comes within a couple of screens,
 * then its rows, for good. A huge diff so draws only what's near you, and scrolls as a short one.
 */
function Chunk({ rows, eager, children }: { rows: number; eager: boolean; children: React.ReactNode }) {
  const [near, setNear] = React.useState(eager);
  const watch = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el || near) return;
      const io = new IntersectionObserver(
        (es) => {
          if (es.some((e) => e.isIntersecting)) {
            setNear(true);
            io.disconnect();
          }
        },
        { rootMargin: "1600px 0px" },
      );
      io.observe(el);
      return () => io.disconnect();
    },
    [near],
  );
  if (near || eager)
    return (
      <div className="rv-chunk" style={{ containIntrinsicSize: `auto ${rows * ROW_PX}px` }}>
        {children}
      </div>
    );
  return <div ref={watch} className="rv-chunk-hold" style={{ height: rows * ROW_PX }} aria-hidden="true" />;
}

interface RowCtx {
  p: FileProps;
  pick: Pick | null;
  choose: (side: Side, line: number, extend: boolean) => void;
  byEnd: Map<string, Thread[]>;
  close: () => void;
  /** Asking about a name or lines (an agent's diff only: components/code/about.tsx), what's asked about, and closing its card. */
  ask?: (side: Side, line: number | null, text: string, e: React.MouseEvent<HTMLElement>) => void;
  asked: AskedLines | null;
  /** The open card and those sliding shut. */
  cards: { c: AskedLines; closing: boolean }[];
  closeAsk: () => void;
  goneAsk: (key: string) => void;
}

/** What a card under a line asks about: lines `from` to `to` of one side. */
interface AskedLines {
  key: string;
  side: Side;
  from: number;
  to: number;
  ask: Asked;
}

/** A line's code, tapped: asking about the name there, or the line. */
const tap = (c: RowCtx, side: Side, line: number | null, text: string) => (c.ask ? (e: React.MouseEvent<HTMLElement>) => c.ask?.(side, line, text, e) : undefined);
const asking = (c: RowCtx, side: Side, line: number | null) => line != null && c.asked != null && c.asked.side === side && line >= c.asked.from && line <= c.asked.to;

/** A hunk's lines as a diff's (`+`, `-`, ` `) from line `from` to `to` of `side`, with up to ten either side. */
function around(hunks: Hunk[], side: Side, from: number, to: number): string {
  for (const h of hunks) {
    let [o, n] = [h.old_start, h.new_start];
    const at: number[] = [];
    h.lines.forEach((l, i) => {
      const c = l.charAt(0);
      if (c === "\\") return;
      const old = c === "+" ? null : o++;
      const neu = c === "-" ? null : n++;
      const mine = side === "new" ? neu : old;
      if (mine != null && mine >= from && mine <= to) at.push(i);
    });
    if (at.length) return h.lines.slice(Math.max(0, (at[0] ?? 0) - 10), (at[at.length - 1] ?? 0) + 11).join("\n");
  }
  return "";
}

type Toks = { old: Map<number, Tok[]>; neu: Map<number, Tok[]> };

const picked = (pick: Pick | null, side: Side, line: number | null) => line != null && pick != null && pick.side === side && line >= pick.start && line <= pick.end;

function Num({ side, line, c }: { side: Side; line: number | null; c: RowCtx }) {
  if (line == null) return <span className="rv-num" aria-hidden="true" />;
  return (
    <button
      type="button"
      className="rv-num"
      data-side={side}
      id={`rv-f${c.p.idx}-${side === "old" ? "o" : "n"}${line}`}
      aria-label={`Comment on ${side === "old" ? "old " : ""}line ${line}`}
      onClick={(e) => c.choose(side, line, e.shiftKey)}
    >
      {line}
    </button>
  );
}

/**
 * What comes under a line: a card asking about the lines ending there (a band of its own, sliding
 * open and shut), the composer of a comment picked to end there, and the threads that end there.
 */
function Under({ side, line, c }: { side: Side; line: number | null; c: RowCtx }) {
  if (line == null) return null;
  const threads = c.byEnd.get(`${side}:${line}`) ?? [];
  const composing = c.pick != null && c.pick.side === side && c.pick.end === line;
  const cards = c.cards.filter(({ c: k }) => k.side === side && k.to === line);
  if (!composing && threads.length === 0 && cards.length === 0) return null;
  return (
    <>
      {cards.map(({ c: k, closing }) => (
        <AboutCard key={k.key} ask={k.ask} closing={closing} onClose={c.closeAsk} onGone={() => c.goneAsk(k.key)} />
      ))}
      {(composing || threads.length > 0) && (
        <div className="rv-under">
          {threads.map((t) => (
            <ThreadView key={t.id} t={t} onReply={c.p.onReply} onResolve={c.p.onResolve} now={c.p.now} />
          ))}
          {composing && c.pick && <NewComment pick={c.pick} onDone={c.close} onSend={c.p.onComment} now={c.p.now} />}
        </div>
      )}
    </>
  );
}

/** Said once per moved run, on its first line: where the block went (or came from). */
function Moved({ to, c, side, line }: { to: MovedTo | undefined; c: RowCtx; side: Side; line: number | null }) {
  if (!to || line == null) return null;
  const prev = (side === "old" ? c.p.moves.del : c.p.moves.add).get(placeOf(c.p.idx, side === "old" ? "o" : "n", line - 1));
  if (prev && prev.file === to.file && prev.line === to.line - 1) return null;
  const here = to.file === c.p.idx;
  const where = here ? `line ${to.line}` : `${c.p.pathOf(to.file).split("/").pop() ?? ""}:${to.line}`;
  return (
    <a
      className="rv-moved"
      href={`#rv-f${to.file}-${side === "old" ? "n" : "o"}${to.line}`}
      title={side === "old" ? `Moved to ${c.p.pathOf(to.file)}, line ${to.line}` : `Moved from ${c.p.pathOf(to.file)}, line ${to.line}`}
    >
      {side === "old" ? "moved to" : "moved from"} {where}
    </a>
  );
}

function CtxRow({ l, tok, c }: { l: Line; tok: Toks; c: RowCtx }) {
  const sel = picked(c.pick, "new", l.n);
  if (c.p.split)
    return (
      <>
        <div className="rv-row" data-kind="ctx" data-sel={sel || undefined}>
          <div className="rv-half" data-asked={asking(c, "old", l.o) || undefined} style={{ "--hang": l.hang } as React.CSSProperties}>
            <Num side="old" line={l.o} c={c} />
            <Code text={l.text} toks={tok.old.get(l.o ?? -1)} onClick={tap(c, "old", l.o, l.text)} />
          </div>
          <div className="rv-half" data-asked={asking(c, "new", l.n) || undefined} style={{ "--hang": l.hang } as React.CSSProperties}>
            <Num side="new" line={l.n} c={c} />
            <Code text={l.text} toks={tok.neu.get(l.n ?? -1)} onClick={tap(c, "new", l.n, l.text)} />
          </div>
        </div>
        <Under side="new" line={l.n} c={c} />
      </>
    );
  return (
    <>
      <div className="rv-row" data-kind="ctx" data-sel={sel || undefined} data-asked={asking(c, "new", l.n) || undefined} style={{ "--hang": l.hang } as React.CSSProperties}>
        <Num side="old" line={l.o} c={c} />
        <Num side="new" line={l.n} c={c} />
        <span className="rv-sign" />
        <Code text={l.text} toks={tok.neu.get(l.n ?? -1)} onClick={tap(c, "new", l.n, l.text)} />
      </div>
      <Under side="new" line={l.n} c={c} />
    </>
  );
}

function ChgRow({ l, tok, c }: { l: Line; tok: Toks; c: RowCtx }) {
  const del = l.kind === "del";
  const side: Side = del ? "old" : "new";
  const line = del ? l.o : l.n;
  const moved = line == null ? undefined : (del ? c.p.moves.del : c.p.moves.add).get(placeOf(c.p.idx, del ? "o" : "n", line));
  return (
    <>
      <div
        className="rv-row"
        data-kind={l.kind}
        data-sel={picked(c.pick, side, line) || undefined}
        data-asked={asking(c, side, line) || undefined}
        data-moved={moved ? true : undefined}
        style={{ "--hang": l.hang } as React.CSSProperties}
      >
        <Num side="old" line={del ? l.o : null} c={c} />
        <Num side="new" line={del ? null : l.n} c={c} />
        <span className="rv-sign">{del ? "−" : "+"}</span>
        <Code text={l.text} toks={(del ? tok.old : tok.neu).get(line ?? -1)} marks={l.marks} onClick={tap(c, side, line, l.text)} />
        <Moved to={moved} c={c} side={side} line={line} />
      </div>
      <Under side={side} line={line} c={c} />
    </>
  );
}

function SplitRow({ d, a, tok, c }: { d: Line | undefined; a: Line | undefined; tok: Toks; c: RowCtx }) {
  const dm = d?.o == null ? undefined : c.p.moves.del.get(placeOf(c.p.idx, "o", d.o));
  const am = a?.n == null ? undefined : c.p.moves.add.get(placeOf(c.p.idx, "n", a.n));
  return (
    <>
      <div className="rv-row" data-kind="pair">
        <div
          className="rv-half"
          data-kind={d ? "del" : "none"}
          data-sel={picked(c.pick, "old", d?.o ?? null) || undefined}
          data-asked={asking(c, "old", d?.o ?? null) || undefined}
          data-moved={dm ? true : undefined}
          style={{ "--hang": d?.hang ?? 0 } as React.CSSProperties}
        >
          <Num side="old" line={d?.o ?? null} c={c} />
          {d && <Code text={d.text} toks={tok.old.get(d.o ?? -1)} marks={d.marks} onClick={tap(c, "old", d.o, d.text)} />}
          <Moved to={dm} c={c} side="old" line={d?.o ?? null} />
        </div>
        <div
          className="rv-half"
          data-kind={a ? "add" : "none"}
          data-sel={picked(c.pick, "new", a?.n ?? null) || undefined}
          data-asked={asking(c, "new", a?.n ?? null) || undefined}
          data-moved={am ? true : undefined}
          style={{ "--hang": a?.hang ?? 0 } as React.CSSProperties}
        >
          <Num side="new" line={a?.n ?? null} c={c} />
          {a && <Code text={a.text} toks={tok.neu.get(a.n ?? -1)} marks={a.marks} onClick={tap(c, "new", a.n, a.text)} />}
          <Moved to={am} c={c} side="new" line={a?.n ?? null} />
        </div>
      </div>
      <Under side="old" line={d?.o ?? null} c={c} />
      <Under side="new" line={a?.n ?? null} c={c} />
    </>
  );
}

/* ------------------------------------------------------------------ threads */

function Composer({
  placeholder,
  onSend,
  onCancel,
  autoFocus,
  now,
}: {
  placeholder: string;
  onSend: (body: string, publish: boolean) => Promise<void>;
  onCancel?: () => void;
  autoFocus?: boolean;
  now?: boolean;
}) {
  const [text, setText] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  const send = async (publish: boolean) => {
    if (!text.trim()) return;
    setBusy(true);
    setError(null);
    try {
      await onSend(text.trim(), publish);
      setText("");
      onCancel?.();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="rv-compose">
      <textarea
        className="rv-compose-input"
        rows={3}
        value={text}
        placeholder={placeholder}
        autoFocus={autoFocus}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) void send(!!now);
          if (e.key === "Escape") onCancel?.();
        }}
      />
      {error && (
        <p className="st-note" data-tone="error">
          {error}
        </p>
      )}
      <div className="rv-compose-acts">
        {now ? (
          <Button size="xs" disabled={busy || !text.trim()} onClick={() => void send(true)}>
            Comment
          </Button>
        ) : (
          <>
            <Button size="xs" disabled={busy || !text.trim()} onClick={() => void send(false)}>
              Add to your review
            </Button>
            <Button size="xs" variant="secondary" disabled={busy || !text.trim()} onClick={() => void send(true)}>
              Comment now
            </Button>
          </>
        )}
        {onCancel && (
          <Button size="xs" variant="ghost" disabled={busy} onClick={onCancel}>
            Cancel
          </Button>
        )}
      </div>
    </div>
  );
}

function NewComment({ pick, onSend, onDone, now }: { pick: Pick; onSend: FileProps["onComment"]; onDone: () => void; now?: boolean }) {
  const lines = pick.start === pick.end ? `line ${pick.start}` : `lines ${pick.start}–${pick.end}`;
  return (
    <div className="rv-thread rv-new">
      <p className="rv-sub">
        <MessageSquarePlus size={13} aria-hidden="true" /> On {pick.side === "old" ? "the old " : ""}
        {lines}
      </p>
      <Composer placeholder="Say what should change, or ask" autoFocus now={now} onCancel={onDone} onSend={(body, publish) => onSend(pick, body, publish)} />
    </div>
  );
}

export function ThreadView({ t, onReply, onResolve, now }: { t: Thread; onReply: FileProps["onReply"]; onResolve?: FileProps["onResolve"]; now?: boolean }) {
  const [replying, setReplying] = React.useState(false);
  const resolved = t.state === "resolved";
  const [open, setOpen] = React.useState(!resolved);
  return (
    <div className="rv-thread" id={`rv-t-${t.id}`} data-resolved={resolved || undefined} data-outdated={t.outdated || undefined}>
      {!open ? (
        <button type="button" className="rv-thread-folded" onClick={() => setOpen(true)}>
          <ChevronRight size={13} aria-hidden="true" /> Resolved: {whoSaid(t.comments[0]?.author ?? t.created_by)} · {t.comments.length} comment{t.comments.length === 1 ? "" : "s"}
        </button>
      ) : (
        <>
          {t.comments.map((cm) => (
            <div key={cm.id} className="rv-comment">
              <p className="rv-comment-who">
                <b>{whoSaid(cm.author)}</b> <span className="rv-sub">{relativeTime(cm.created_at)}</span>
                {cm.draft && <span className="rv-chip">draft · goes out with your review</span>}
              </p>
              <p className="rv-comment-body">{cm.body}</p>
              {cm.suggestion != null && (
                <div className="rv-suggestion">
                  <p className="rv-sub">Suggested change</p>
                  <pre>{cm.suggestion}</pre>
                </div>
              )}
            </div>
          ))}
          <div className="rv-thread-acts">
            {!replying && (
              <Button size="xs" variant="ghost" onClick={() => setReplying(true)}>
                Reply
              </Button>
            )}
            {onResolve && (
              <Button size="xs" variant="ghost" onClick={() => void onResolve(t, !resolved).then(() => setOpen(resolved))}>
                {resolved ? "Reopen" : "Resolve"}
              </Button>
            )}
          </div>
          {replying && <Composer placeholder="Reply" autoFocus now={now} onCancel={() => setReplying(false)} onSend={(body, publish) => onReply(t, body, publish)} />}
        </>
      )}
    </div>
  );
}
