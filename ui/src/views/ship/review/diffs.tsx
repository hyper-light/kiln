/**
 * A Ship item's per-file diffs (docs/ci.md W26-R07, R13): folding like the agent chat's Changes
 * (each file's header folds it; a diff over 100 lines folds to its first lines and the lines with
 * comments, ⋯ for the gaps), each header's icon opening the whole file in a full-screen view in
 * place (no popup; the file from versions/:n/blob). Threads sit inline on their lines; conflicts
 * sit where they fall; a line's number opens a new thread on it.
 */
import * as React from "react";
import { Maximize2, X } from "lucide-react";
import type { Conflict, Thread } from "@/api/review";
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
  const n = s.slice(1);
  switch (s[0]) {
    case "A":
      return "added";
    case "D":
      return "deleted";
    case "M":
      return "modified";
    case "R":
      return n && n !== "100" ? `renamed ${n}%` : "renamed";
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
            <Line key={x} file={file} row={rows[x]!} ctx={ctx} map={map} q={q} />
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

/** One diff line, its threads and conflicts under it, and its draft's composer. */
function Line({ file, row, ctx, map, q }: { file: PatchFile; row: Row; ctx: FilesCtx; map: Map<string, { t: Thread; a: Anchor }[]>; q: string }) {
  const side: Side = row.kind === "del" ? "old" : "new";
  const n = lineOf(row, side);
  const here = n == null ? [] : (map.get(`${file.path}|${side}:${n}`) ?? []);
  const cfs = n == null ? [] : ctx.conflicts.filter((c) => c.file === file.path && c.line === n);
  const drafting = ctx.draft?.kind === "line" && ctx.draft.file === file.path && ctx.draft.side === side && ctx.draft.line === n;
  const hit = q !== "" && row.text.toLowerCase().includes(q);
  const dim = q !== "" && !hit;
  const sign = row.kind === "add" ? "+" : row.kind === "del" ? "−" : " ";
  return (
    <>
      <div className={`rv-l${row.kind === "add" ? " a" : row.kind === "del" ? " d" : ""}${hit ? " hit" : ""}`} data-dim={dim || undefined} data-rvl={`${file.path}|${side}:${n}`}>
        <button
          type="button"
          className="rv-ln"
          aria-label={`Comment on line ${n}`}
          disabled={n == null || !ctx.ops.kiln}
          onClick={() => ctx.setDraft(drafting ? null : { kind: "line", file: file.path, line: n!, side })}
        >
          {n ?? ""}
        </button>
        <code>
          {sign} {row.text}
        </code>
      </div>
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
            lines={linesIn(ctx.rowsByFile.get(file.path) ?? [], side, n!, n!)}
            placeholder={`Comment on line ${n}`}
            onSend={(body, kind) => ctx.ops.commentAt(file.path, n!, side, body, kind ?? "question").then(() => ctx.setDraft(null))}
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
          <Line key={i} file={file} row={r} ctx={whole} map={map} q={q} />
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
