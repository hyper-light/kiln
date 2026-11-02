/**
 * A Ship item's per-file diffs (docs/ci.md W26-R07, R13): folding like the agent chat's Changes
 * (each file's header folds it; each hunk shows with its context, ⋯ between hunks; a diff over 100
 * lines folds to its first lines and the lines with comments, ⋯ for the gaps), each header's icon
 * opening the whole file in a full-screen view in place (no popup; the file from versions/:n/blob).
 * Threads sit inline on their lines; conflicts sit where they fall; a line's number opens a new
 * thread on it. The code is in its language's colours (tree-sitter, lib/syntax), as in agent chat's
 * diffs; in an agent's change, a name or a line of code tapped opens a card under it asking the
 * agent about it (components/code/about.tsx), as agent chat and the review page do (Ada, 2026-10-29).
 */
import * as React from "react";
import { Maximize2, X } from "lucide-react";
import type { Hunk } from "@/api/harness";
import type { Conflict, Thread } from "@/api/review";
import { AboutCard, AboutContext, cardKey, linesLabel, offsetAt, selecting, useCards, wordAt, type Asked } from "@/components/code/about";
import { Toks } from "@/components/code/highlighted";
import { resolveLanguage, useHighlighter, type Highlight, type Tok } from "@/lib/syntax/browser";
import type { Res } from "@/lib/data";
import { useRes } from "@/lib/data";
import { isImage, type PatchFile } from "@/views/review/patch";
import { ReviewComposer } from "./composer";
import { ConflictBlock } from "./conflicts";
import { anchorOf, fold, linesAt, type Anchor, type Row, type Side } from "./fold";
import type { DraftPlace, LinesOf, Ops } from "./ops";
import { counted } from "./perf";
import { ThreadView } from "./threads";
import "./review.css";

export interface FilesCtx {
  ops: Ops;
  threads: Thread[];
  conflicts: Conflict[];
  rowsByFile: Map<string, Row[]>;
  version: number;
  draft: DraftPlace | null;
  setDraft: (d: DraftPlace | null) => void;
  /** Open a file whole, in place. */
  open: (file: string) => void;
  /** The search's letters: matching lines marked, the rest dimmed. */
  q: string;
  /** The lines a span holds, as a file's rows have them (stable, for the memoized threads). */
  linesOf: LinesOf;
  /** Marks a file viewed, or not, where kiln keeps it (a kiln change's, POST /api/diffs/:key/viewed); left out where nothing keeps it. */
  viewed?: (file: string, on: boolean) => void;
}

/** A file's status in words, as kiln's review page says it: the server's when it says one (git's
 * letter, R100 or C75, or a host's word), else its patch's (Changes plan 01a17bd748ff73d98f09996ecdc6873d, step 2). */
export function statusOf(file: PatchFile, given?: string): string {
  const s = given ?? "";
  // Git's score, zero-padded (R092): a number.
  const n = Number(s.slice(1)) || 0;
  switch (s[0]) {
    case "A":
      return "added";
    case "D":
      return "deleted";
    case "M":
      return "modified";
    case "R":
      return n && n !== 100 ? `renamed ${n}%` : "renamed";
    case "C":
      return n ? `copied ${n}%` : "copied";
    case "T":
      return "type changed";
  }
  if (s === "removed") return "deleted";
  if (s) return s;
  return file.similarity != null && (file.status === "renamed" || file.status === "copied") && file.similarity !== 100 ? `${file.status} ${file.similarity}%` : file.status;
}

/** Threads by the line they end on, in the version shown. */
function byEnd(threads: Thread[], version: number): Map<string, { t: Thread; a: Anchor }[]> {
  const m = new Map<string, { t: Thread; a: Anchor }[]>();
  for (const t of threads) {
    if (t.file == null) continue;
    const a = anchorOf(t, version);
    if (!a) continue;
    const k = `${t.file}|${a.side}:${a.end}`;
    m.set(k, [...(m.get(k) ?? []), { t, a }]);
  }
  return m;
}

export function useThreadMap(threads: Thread[], version: number) {
  return React.useMemo(() => byEnd(threads, version), [threads, version]);
}

/** The lines a span holds, as a file's rows have them (suggestions and the composer's). */
export function linesIn(rows: Row[], side: Side, start: number, end: number) {
  return linesAt(rows, { side, start, end });
}

export function makeLinesOf(rowsByFile: Map<string, Row[]>): LinesOf {
  return (file, side, start, end) => {
    const rows = rowsByFile.get(file);
    return rows ? linesIn(rows, side, start, end) : null;
  };
}

/** One file: its header (path, the change's size, the whole-file icon) folds its diff. Renders only
 * when what it draws changes — its file, its threads' places, or its ctx (W26-R15, AC06). */
export const FileDiff = React.memo(function FileDiff({
  file,
  ctx,
  map,
  meta,
}: {
  file: PatchFile;
  ctx: FilesCtx;
  map: Map<string, { t: Thread; a: Anchor }[]>;
  meta?: { binary?: boolean; generated?: boolean; status?: string; viewed?: boolean };
}) {
  counted("ship/filediff");
  // Viewed folds the file, as on GitHub and kiln's review page; one viewed before opens folded.
  const [viewed, setViewed] = React.useState(meta?.viewed ?? false);
  const [open, setOpen] = React.useState(!(meta?.generated ?? false) && !(meta?.viewed ?? false));
  const status = statusOf(file, meta?.status);
  const rows = ctx.rowsByFile.get(file.path) ?? [];
  const add = rows.filter((r) => r.kind === "add").length;
  const del = rows.filter((r) => r.kind === "del").length;
  const long = rows.length > 100;
  const carries = (r: Row, _i: number) =>
    map.has(`${file.path}|new:${r.n}`) ||
    map.has(`${file.path}|old:${r.o}`) ||
    ctx.conflicts.some((c) => c.file === file.path && c.line === r.n) ||
    (ctx.draft?.kind === "line" && ctx.draft.file === file.path && lineOf(r, ctx.draft.side) === ctx.draft.line);
  const shown = fold(rows, carries);
  const q = ctx.q.trim().toLowerCase();
  // In its language's colours once its grammar is here (lib/syntax); plain till then.
  const hl = useHighlighter(resolveLanguage(null, file.path));
  const toks = React.useMemo(() => tokensOf(hl, file.hunks), [hl, file.hunks]);
  // An agent's change: a name or a line asked about opens a card under it (components/code/about.tsx).
  const asking = useFileAsk(file.path, (side, from, to) => around(file.hunks, side, from, to));
  return (
    <details className="rv-f" open={open} onToggle={(e) => setOpen(e.currentTarget.open)}>
      <summary>
        <span className="rv-path">{file.oldPath && file.oldPath !== file.path ? `${file.oldPath} → ${file.path}` : file.path}</span>
        <span className="rv-stat">
          <span className="rv-chip" data-status={status.split(" ")[0]}>
            {status}
          </span>
          <span className="rv-add">+{add}</span> <span className="rv-del">−{del}</span>
        </span>
        {ctx.viewed && (
          <label className="rv-viewed">
            <input
              type="checkbox"
              className="check"
              checked={viewed}
              onChange={(e) => {
                const on = e.currentTarget.checked;
                setViewed(on);
                setOpen(!on);
                ctx.viewed?.(file.path, on);
              }}
            />
            Viewed
          </label>
        )}
        <button
          type="button"
          className="rv-ib"
          aria-label={`Open ${file.path} whole`}
          title="Open the whole file"
          onClick={(e) => {
            e.preventDefault();
            ctx.open(file.path);
          }}
        >
          <Maximize2 size={13} aria-hidden="true" />
        </button>
      </summary>
      <div className="rv-code">
        {ctx.conflicts
          .filter((c) => c.file === file.path && c.line == null)
          .map((cf) => (
            <ConflictBlock key={cf.id} cf={cf} n={cf.of} diffKey={ctx.ops.key ?? ""} />
          ))}
        {shown.map((x, i) =>
          x < 0 ? (
            <div key={`g${i}`} className="rv-gap" aria-hidden="true">
              ⋯
            </div>
          ) : (
            <Line key={x} file={file} row={rows[x]!} ctx={ctx} map={map} q={q} toks={toks} asking={asking} />
          ),
        )}
        {long && (
          <button type="button" className="rv-more" onClick={() => ctx.open(file.path)}>
            <Maximize2 size={13} aria-hidden="true" />
            <span>{rows.length} lines · open the whole file</span>
          </button>
        )}
      </div>
    </details>
  );
});

function lineOf(r: Row, side: Side): number | null {
  return side === "old" ? r.o : r.n;
}

/** A file's tokens by line number, each side's. */
export interface FileToks {
  old: Map<number, Tok[]>;
  neu: Map<number, Tok[]>;
}

const NO_TOKS: FileToks = { old: new Map(), neu: new Map() };

/** A file's tokens: each hunk's sides read as one piece of code, as the review page's diff reads
 * them, so strings and comments over lines read right; none until its grammar is here. */
export function tokensOf(hl: Highlight | null, hunks: readonly Hunk[]): FileToks {
  if (!hl) return NO_TOKS;
  const old = new Map<number, Tok[]>();
  const neu = new Map<number, Tok[]>();
  for (const h of hunks) {
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
  }
  return { old, neu };
}

/** What a card under a line asks about: lines `from` to `to` of one side. */
interface AskedLines {
  key: string;
  side: Side;
  from: number;
  to: number;
  ask: Asked;
}

/** A file's asking (an agent's change only): code tapped asks about it; the card open and those sliding shut. */
interface FileAsk {
  ask?: (side: Side, line: number | null, text: string, e: React.MouseEvent<HTMLElement>) => void;
  cards: { c: AskedLines; closing: boolean }[];
  close: () => void;
  gone: (key: string) => void;
}

/**
 * Asking about a file's code, where it's an agent's (AboutContext: components/code/about.tsx): a
 * name tapped, or a line, opens a card under it; another line of the same side, while lines are
 * asked about, takes in the lines between; the same again closes it. `around` gives the code the
 * agent is shown for lines `from` to `to` of a side.
 */
function useFileAsk(path: string, around: (side: Side, from: number, to: number) => string): FileAsk {
  const about = React.useContext(AboutContext);
  const cards = useCards<AskedLines>();
  return {
    ask: about
      ? (side, line, text, e) => {
          if (line == null || selecting()) return;
          const off = offsetAt(e.currentTarget, e.clientX, e.clientY);
          const word = off === null ? null : wordAt(text, off);
          const selection = word ? text.slice(word.start, word.end) : undefined;
          cards.update((prev) => {
            let [from, to] = [line, line];
            if (!selection && prev && !prev.ask.selection && prev.side === side && (line < prev.from || line > prev.to)) [from, to] = [Math.min(prev.from, line), Math.max(prev.to, line)];
            else if (prev && prev.side === side && prev.from === line && prev.to === line && prev.ask.selection === selection) return null;
            const label = selection ?? linesLabel(from, to);
            const where = `${path.split("/").pop() ?? path} · ${side === "old" ? "old " : ""}${linesLabel(from, to)}`;
            return { key: cardKey(`${side}${from}-${to}`), side, from, to, ask: { path, line: from, end: to > from ? to : undefined, selection, code: around(side, from, to), label, where } };
          });
        }
      : undefined,
    cards: [...cards.closing.map((c) => ({ c, closing: true })), ...(cards.open ? [{ c: cards.open, closing: false }] : [])],
    close: () => cards.update(() => null),
    gone: cards.gone,
  };
}

/** A hunk's lines as a diff's (`+`, `-`, ` `) from line `from` to `to` of `side`, with up to ten either side: what the agent is shown. */
function around(hunks: readonly Hunk[], side: Side, from: number, to: number): string {
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

/** A line's number clicked, as on kiln's review page: a comment starts there; shift-click another on
 * the same side of the file takes the lines between; the same line again closes it. */
export function pickLine(draft: DraftPlace | null, file: string, side: Side, line: number, extend: boolean): DraftPlace | null {
  if (draft?.kind === "line" && draft.file === file && draft.side === side) {
    const start = draft.start ?? draft.line;
    if (extend) {
      const [a, b] = [Math.min(start, line), Math.max(draft.line, line)];
      return a === b ? { kind: "line", file, side, line: b } : { kind: "line", file, side, start: a, line: b };
    }
    if (start === line && draft.line === line) return null;
  }
  return { kind: "line", file, side, line };
}

/** One diff line in its language's colours, a card asking about it under it, its threads and conflicts, and its draft's composer. */
function Line({ file, row, ctx, map, q, toks, asking }: { file: PatchFile; row: Row; ctx: FilesCtx; map: Map<string, { t: Thread; a: Anchor }[]>; q: string; toks: FileToks; asking: FileAsk }) {
  const side: Side = row.kind === "del" ? "old" : "new";
  const n = lineOf(row, side);
  const ask = asking.ask;
  const asked = n != null && asking.cards.some(({ c, closing }) => !closing && c.side === side && n >= c.from && n <= c.to);
  const cards = n == null ? [] : asking.cards.filter(({ c }) => c.side === side && c.to === n);
  const here = n == null ? [] : (map.get(`${file.path}|${side}:${n}`) ?? []);
  const cfs = n == null ? [] : ctx.conflicts.filter((c) => c.file === file.path && c.line === n);
  const d = ctx.draft?.kind === "line" && ctx.draft.file === file.path && ctx.draft.side === side ? ctx.draft : null;
  const drafting = d != null && d.line === n;
  const from = d?.start ?? d?.line ?? 0;
  const picked = d != null && n != null && n >= from && n <= d.line;
  const hit = q !== "" && row.text.toLowerCase().includes(q);
  const dim = q !== "" && !hit;
  const sign = row.kind === "add" ? "+" : row.kind === "del" ? "−" : " ";
  return (
    <>
      <div
        className={`rv-l${row.kind === "add" ? " a" : row.kind === "del" ? " d" : ""}${hit ? " hit" : ""}`}
        data-dim={dim || undefined}
        data-picked={picked || undefined}
        data-asked={asked || undefined}
        data-rvl={`${file.path}|${side}:${n}`}
      >
        <button
          type="button"
          className="rv-ln"
          aria-label={`Comment on line ${n}`}
          disabled={n == null || !ctx.ops.kiln}
          onClick={(e) => ctx.setDraft(pickLine(ctx.draft, file.path, side, n!, e.shiftKey))}
        >
          {n ?? ""}
        </button>
        <code>
          {sign}{" "}
          <span className="rv-t" data-ask={ask ? "" : undefined} onClick={ask ? (e) => ask(side, n, row.text, e) : undefined}>
            <Toks text={row.text} toks={(side === "old" ? toks.old : toks.neu).get(n ?? -1)} />
          </span>
        </code>
      </div>
      {cards.map(({ c, closing }) => (
        <AboutCard key={c.key} ask={c.ask} closing={closing} onClose={asking.close} onGone={() => asking.gone(c.key)} />
      ))}
      {here.map(({ t }) => (
        <ThreadView key={t.id} t={t} ops={ctx.ops} linesOf={ctx.linesOf} draft={ctx.draft?.kind === "reply" && ctx.draft.thread === t.id} setDraft={ctx.setDraft} />
      ))}
      {cfs.map((cf) => (
        <ConflictBlock key={cf.id} cf={cf} n={cf.of} diffKey={ctx.ops.key ?? ""} />
      ))}
      {drafting && (
        <div className="rv-thread">
          <ReviewComposer
            at={{ item: ctx.ops.item, file: file.path, line: n, side }}
            lines={linesIn(ctx.rowsByFile.get(file.path) ?? [], side, from, n!)}
            placeholder={from < n! ? `Comment on lines ${from}–${n}` : `Comment on line ${n}`}
            onSend={(body, kind) => ctx.ops.commentAt(file.path, n!, side, body, kind ?? "question", from < n! ? from : undefined).then(() => ctx.setDraft(null))}
            onCancel={() => ctx.setDraft(null)}
            autoFocus
          />
        </div>
      )}
    </>
  );
}

/** The whole file in place of the file list (no popup), its lines numbered, threads where they sit. */
export function WholeFile({ file, ctx, map, text }: { file: PatchFile; ctx: FilesCtx; map: Map<string, { t: Thread; a: Anchor }[]>; text: Res<string> }) {
  return (
    <React.Suspense fallback={<div className="rv-fullh">…</div>}>
      <WholeBody file={file} ctx={ctx} map={map} text={text} />
    </React.Suspense>
  );
}

function WholeBody({ file, ctx, map, text }: { file: PatchFile; ctx: FilesCtx; map: Map<string, { t: Thread; a: Anchor }[]>; text: Res<string> }) {
  const body = useResText(text);
  const rows = React.useMemo<Row[]>(() => {
    const added = new Set<number>();
    for (const r of ctx.rowsByFile.get(file.path) ?? []) if (r.kind === "add" && r.n != null) added.add(r.n);
    return body.split("\n").map((t, i) => ({ o: null, n: i + 1, kind: added.has(i + 1) ? "add" : "ctx", text: t }));
  }, [body, ctx.rowsByFile, file.path]);
  const all = React.useMemo(() => new Map(ctx.rowsByFile).set(file.path, rows), [ctx.rowsByFile, file.path, rows]);
  const linesOf = React.useMemo(() => makeLinesOf(all), [all]);
  const whole = React.useMemo(() => ({ ...ctx, rowsByFile: all, linesOf }), [ctx, all, linesOf]);
  const q = ctx.q.trim().toLowerCase();
  // The whole file read as one piece of code, in its colours; asked about, its own lines around it.
  const hl = useHighlighter(resolveLanguage(null, file.path, body));
  const toks = React.useMemo<FileToks>(() => ({ old: new Map(), neu: new Map((hl?.(body) ?? []).map((t, i): [number, Tok[]] => [i + 1, t])) }), [hl, body]);
  const asking = useFileAsk(file.path, (_side, from, to) =>
    body
      .split("\n")
      .slice(Math.max(0, from - 11), to + 10)
      .join("\n"),
  );
  return (
    <div className="rv-full" role="region" aria-label={file.path} tabIndex={-1} ref={(el) => el?.focus({ preventScroll: true })} onKeyDown={(e) => e.key === "Escape" && ctx.open("")}>
      <div className="rv-fullh">
        <span className="rv-path">{file.path}</span>
        <button type="button" className="rv-ib" aria-label="Close" title="Close" onClick={() => ctx.open("")}>
          <X size={15} aria-hidden="true" />
        </button>
      </div>
      <div className="rv-fullb rv-code">
        {rows.map((r, i) => (
          <Line key={i} file={file} row={r} ctx={whole} map={map} q={q} toks={toks} asking={asking} />
        ))}
      </div>
    </div>
  );
}

function useResText(r: Res<string>): string {
  return useRes(r).data;
}

/** A file's image, in the same in-place view (binary files have no text). */
export function WholeImage({ file, src, close }: { file: string; src: string; close: () => void }) {
  return (
    <div className="rv-full" role="region" aria-label={file}>
      <div className="rv-fullh">
        <span className="rv-path">{file}</span>
        <button type="button" className="rv-ib" aria-label="Close" title="Close" onClick={close}>
          <X size={15} aria-hidden="true" />
        </button>
      </div>
      <div className="rv-fullb rv-img">{isImage(file) ? <img src={src} alt={file} /> : <p className="rv-sub">Binary file, not shown.</p>}</div>
    </div>
  );
}
