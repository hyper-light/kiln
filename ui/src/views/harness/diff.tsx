/**
 * kiln's diff view: one file's hunks, side by side when there is room and unified when there is
 * not, with line numbers and the changed part of each edited line picked out.
 */
import * as React from "react";
import { ChevronRight, FilePlus2, FileDiff as FileDiffIcon } from "lucide-react";
import type { FileDiff, Hunk } from "@/api/harness";
import { cn } from "@/lib/utils";
import { diffStats } from "./derive";

type Side = { no: number | null; text: string; kind: "ctx" | "del" | "add" | "none"; mark?: [number, number] };
type Row = { left: Side; right: Side } | { gap: string };

/** The changed middle of a line edited in place: [start, end) in each of the two strings. */
function changedSpan(a: string, b: string): [[number, number], [number, number]] {
  let p = 0;
  const max = Math.min(a.length, b.length);
  while (p < max && a[p] === b[p]) p++;
  let s = 0;
  while (s < max - p && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;
  return [
    [p, a.length - s],
    [p, b.length - s],
  ];
}

function rowsOf(h: Hunk): Row[] {
  const rows: Row[] = [];
  let o = h.old_start;
  let n = h.new_start;
  const lines = h.lines;
  let i = 0;
  while (i < lines.length) {
    const l = lines[i] ?? "";
    const tag = l.charAt(0);
    if (tag === "-" || tag === "+") {
      const dels: string[] = [];
      const adds: string[] = [];
      while (i < lines.length && (lines[i] ?? "").startsWith("-")) dels.push((lines[i++] ?? "").slice(1));
      while (i < lines.length && (lines[i] ?? "").startsWith("+")) adds.push((lines[i++] ?? "").slice(1));
      const k = Math.max(dels.length, adds.length);
      for (let j = 0; j < k; j++) {
        const d = dels[j];
        const a = adds[j];
        const span = d !== undefined && a !== undefined ? changedSpan(d, a) : null;
        rows.push({
          left: d !== undefined ? { no: o++, text: d, kind: "del", mark: span?.[0] } : { no: null, text: "", kind: "none" },
          right: a !== undefined ? { no: n++, text: a, kind: "add", mark: span?.[1] } : { no: null, text: "", kind: "none" },
        });
      }
      continue;
    }
    if (tag === "\\") {
      i++;
      continue;
    }
    const text = l.slice(1);
    rows.push({ left: { no: o++, text, kind: "ctx" }, right: { no: n++, text, kind: "ctx" } });
    i++;
  }
  return rows;
}

function Code({ side }: { side: Side }) {
  const m = side.mark;
  // Only worth marking when part of the line stayed the same.
  if (!m || (m[0] === 0 && m[1] === side.text.length) || m[1] <= m[0]) return <>{side.text || " "}</>;
  return (
    <>
      {side.text.slice(0, m[0])}
      <mark>{side.text.slice(m[0], m[1])}</mark>
      {side.text.slice(m[1])}
    </>
  );
}

function SplitHunk({ h }: { h: Hunk }) {
  const rows = React.useMemo(() => rowsOf(h), [h]);
  return (
    <div className="hx-diff-split">
      {rows.map((r, i) =>
        "gap" in r ? null : (
          <React.Fragment key={i}>
            <span className="hx-ln" data-kind={r.left.kind}>
              {r.left.no ?? ""}
            </span>
            <code className="hx-code" data-kind={r.left.kind}>
              <Code side={r.left} />
            </code>
            <span className="hx-ln" data-kind={r.right.kind}>
              {r.right.no ?? ""}
            </span>
            <code className="hx-code" data-kind={r.right.kind}>
              <Code side={r.right} />
            </code>
          </React.Fragment>
        ),
      )}
    </div>
  );
}

function UnifiedHunk({ h }: { h: Hunk }) {
  const rows = React.useMemo(() => rowsOf(h), [h]);
  const lines: { side: Side; o: number | null; n: number | null }[] = [];
  // Unified order: a changed block's deletions, then its additions.
  let block: { dels: typeof lines; adds: typeof lines } | null = null;
  const flush = () => {
    if (block) lines.push(...block.dels, ...block.adds);
    block = null;
  };
  for (const r of rows) {
    if ("gap" in r) continue;
    if (r.left.kind === "ctx") {
      flush();
      lines.push({ side: r.left, o: r.left.no, n: r.right.no });
      continue;
    }
    block ??= { dels: [], adds: [] };
    if (r.left.kind === "del") block.dels.push({ side: r.left, o: r.left.no, n: null });
    if (r.right.kind === "add") block.adds.push({ side: r.right, o: null, n: r.right.no });
  }
  flush();
  return (
    <div className="hx-diff-unified">
      {lines.map((l, i) => (
        <React.Fragment key={i}>
          <span className="hx-ln" data-kind={l.side.kind}>
            {l.o ?? ""}
          </span>
          <span className="hx-ln" data-kind={l.side.kind}>
            {l.n ?? ""}
          </span>
          <code className="hx-code" data-kind={l.side.kind}>
            <span className="hx-sign" aria-hidden="true">
              {l.side.kind === "del" ? "−" : l.side.kind === "add" ? "+" : " "}
            </span>
            <Code side={l.side} />
          </code>
        </React.Fragment>
      ))}
    </div>
  );
}

/** Wide enough for two columns of code. */
function useWide(ref: React.RefObject<HTMLElement | null>, min = 760) {
  const [wide, setWide] = React.useState(false);
  React.useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const ro = new ResizeObserver(([e]) => setWide((e?.contentRect.width ?? 0) >= min));
    ro.observe(el);
    return () => ro.disconnect();
  }, [ref, min]);
  return wide;
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
}: {
  diff: FileDiff;
  /** What to call the file (a path relative to the agent's folder). */
  label?: string;
  defaultOpen?: boolean;
  layout?: "auto" | "unified" | "split";
}) {
  const ref = React.useRef<HTMLDivElement>(null);
  const wide = useWide(ref);
  const [open, setOpen] = React.useState(defaultOpen);
  const split = layout === "split" || (layout === "auto" && wide);
  const s = diffStats([diff]);
  return (
    <div ref={ref} className="hx-diff">
      <button type="button" className="hx-diff-head" aria-expanded={open} onClick={() => setOpen(!open)}>
        <ChevronRight size={13} className={cn("hx-chev", open && "rotate-90")} />
        {diff.created ? <FilePlus2 size={13} className="text-[var(--color-ok)]" /> : <FileDiffIcon size={13} className="text-[var(--color-secondary)]" />}
        <span className="hx-path">{label ?? diff.path}</span>
        {diff.created && <span className="hx-tag">new</span>}
        <DiffStat add={s.add} del={s.del} className="ml-auto" />
      </button>
      {open && (
        <div className="hx-diff-body">
          {diff.hunks.map((h, i) => (
            <div key={i} className="hx-hunk">
              <div className="hx-hunk-head">
                {h.new_lines > 0 ? `lines ${h.new_start}–${h.new_start + Math.max(0, h.new_lines - 1)}` : `at line ${h.old_start}`}
              </div>
              {split ? <SplitHunk h={h} /> : <UnifiedHunk h={h} />}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
