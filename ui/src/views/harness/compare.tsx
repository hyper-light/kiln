/**
 * The Changes tab's Compare (docs/agents.md, "Compare"; the Changes panel plan 01a194444b1d, step
 * 5): From and To each a point, picked in place from a list that filters as you type. Of this
 * session: Now (the folder as it is, untracked files too), Before it began, and each agent's
 * version (its files as it left them); elsewhere, branches, tags and commits. Between commits,
 * "Only what To adds since they split" (git's three-dot) is on at first; it sits in the head's menu
 * with Swap. Two agents compare plainly, each file saying which of them changed it. A large compare
 * comes as counts, a file's lines once it's opened.
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { ArrowLeftRight, Bot, Check, ChevronDown, Circle, GitBranch, GitCommitHorizontal, History, Tag } from "lucide-react";
import type { Compared, CompareRefs, FileChange } from "@/api/harness";
import { useSuspenseResource } from "@/lib/resource";
import { usePref } from "@/lib/prefs";
import { relativeTime } from "@/lib/format";
import { DiffStat, FileDiffView } from "./diff";
import { FileList, rowOf, sortRows, type Row, type SortKey, type Sub } from "./changes";
import type { ChangedFile } from "./derive";

/** Where an agent's points come from: its machine, asked (`agents.refs`, `agents.compare`). */
export interface CompareSource {
  id: string;
  refs: () => Promise<{ refs: CompareRefs }>;
  compare: (from: string, to: string, split: boolean, path?: string) => Promise<{ compared: Compared }>;
}

type RefsAnswer = { refs: CompareRefs; error?: undefined } | { error: string };
type CompareAnswer = { compared: Compared; error?: undefined } | { error: string };

const why = (e: unknown) => ({ error: e instanceof Error ? e.message : String(e) });

/** Its repository's refs, read through Suspense (kept, and refreshed when shown again). */
function useRefs(source: CompareSource): RefsAnswer {
  return useSuspenseResource(`harness/refs/${source.id}`, () => source.refs().then((a) => a, why)).data;
}

/** The Compare mode's button: on once its machine answers, else turned off with why. */
export function CompareMode({ source, on, onPick }: { source: CompareSource; on: boolean; onPick: () => void }) {
  const a = useRefs(source);
  return (
    <button type="button" role="radio" aria-checked={on} disabled={a.error !== undefined} title={a.error} onClick={onPick}>
      Compare
    </button>
  );
}

/** The agent itself, as a point names it. */
const AGENT = "agent:main";

interface Point {
  /** As `agents.compare` takes it. */
  value: string;
  label: string;
  note?: string;
  side?: string;
  icon: typeof Circle;
}

/** `head`: the tab's mode row, given what the head's menu adds for Compare. `self`: the agent's name; `subs`: its subagents. */
export function CompareView({ source, cwd, head, self, subs }: { source: CompareSource; cwd: string; head: (menu: React.ReactNode) => React.ReactNode; self: string; subs: readonly Sub[] }) {
  return (
    <React.Suspense
      fallback={
        <>
          <div className="cx-head">{head(null)}</div>
          <div className="cx-none">Asking its machine what it has to compare…</div>
        </>
      }
    >
      <ComparePanel source={source} cwd={cwd} head={head} self={self} subs={subs} />
    </React.Suspense>
  );
}

function ComparePanel({ source, cwd, head, self, subs }: { source: CompareSource; cwd: string; head: (menu: React.ReactNode) => React.ReactNode; self: string; subs: readonly Sub[] }) {
  const a = useRefs(source);
  const refs = a.error === undefined ? a.refs : null;
  const [from, setFrom] = React.useState<string | null>(null);
  const [to, setTo] = React.useState("now");
  const [split, setSplit] = React.useState(true);
  const [open, setOpen] = React.useState<"from" | "to" | null>(null);
  const [sel, setSel] = React.useState<string | null>(null);
  const points = React.useMemo(() => pointsOf(refs, self, subs), [refs, self, subs]);
  const fromValue = from ?? (refs?.trunk ? `ref:${refs.trunk}` : "began");
  const asked = React.useDeferredValue(`${fromValue}\u0000${to}\u0000${split}`);
  if (!refs) {
    return (
      <>
        <div className="cx-head">{head(null)}</div>
        <div className="cx-none">{a.error}</div>
      </>
    );
  }
  const labelOf = (v: string): Point => points.flatMap((g) => g.items).find((p) => p.value === v) ?? { value: v, label: v.replace(/^ref:/, ""), icon: GitCommitHorizontal };
  const menu = (
    <>
      <DropdownMenu.Separator className="cx-menu-sep" />
      <DropdownMenu.CheckboxItem className="menu-item" checked={split} onCheckedChange={(on) => setSplit(on === true)}>
        <span className="cx-mark">
          <DropdownMenu.ItemIndicator>
            <Check size={14} aria-hidden="true" />
          </DropdownMenu.ItemIndicator>
        </span>
        <span className="cx-item">
          Only what To adds since they split
          <small>Between commits, as git's three-dot diff</small>
        </span>
      </DropdownMenu.CheckboxItem>
      <DropdownMenu.Item
        className="menu-item"
        onSelect={() => {
          setFrom(to);
          setTo(fromValue);
          setSel(null);
        }}
      >
        <ArrowLeftRight size={14} aria-hidden="true" />
        <span className="cx-item">Swap From and To</span>
      </DropdownMenu.Item>
    </>
  );
  const row = (which: "from" | "to") => {
    const value = which === "from" ? fromValue : to;
    const p = labelOf(value);
    return (
      <div className="cx-hrow" key={which}>
        <span className="cx-lab">{which === "from" ? "From" : "To"}</span>
        <button type="button" className="cx-val" aria-expanded={open === which} aria-label={`${which === "from" ? "From" : "To"}: ${p.label}`} onClick={() => setOpen(open === which ? null : which)}>
          <p.icon size={13} aria-hidden="true" />
          <span>{p.label}</span>
          <ChevronDown size={12} aria-hidden="true" className="cx-chev" />
        </button>
        <span className="cx-total">
          {which === "to" && (
            <React.Suspense fallback={null}>
              <Total source={source} asked={asked} />
            </React.Suspense>
          )}
        </span>
      </div>
    );
  };
  const pick = (which: "from" | "to") => (v: string) => {
    if (which === "from") setFrom(v);
    else setTo(v);
    setOpen(null);
    setSel(null);
  };
  return (
    <>
      <div className="cx-head">
        {head(menu)}
        {row("from")}
        {open === "from" && <Picker points={points} value={fromValue} onPick={pick("from")} />}
        {row("to")}
        {open === "to" && <Picker points={points} value={to} onPick={pick("to")} />}
      </div>
      <React.Suspense fallback={<div className="cx-none">Comparing…</div>}>
        <CompareFiles source={source} asked={asked} cwd={cwd} sel={sel} onSel={setSel} name={(v) => labelOf(v).label} />
      </React.Suspense>
    </>
  );
}

/** What the pickers offer, in groups: this session's points, then its repository's branches, tags and commits. */
function pointsOf(refs: CompareRefs | null, self: string, subs: readonly Sub[]): { label: string; items: Point[] }[] {
  const session: Point[] = [
    { value: "now", label: "Now", note: "the folder as it is, untracked files too", icon: Circle },
    { value: "began", label: "Before it began", note: "the commit it began at", icon: History },
    { value: AGENT, label: self, note: "its files as it left them", icon: Bot },
    ...subs.map((s): Point => ({ value: `agent:${s.id}`, label: s.label, note: s.kind ? `${s.kind} · its files as it left them` : "its files as it left them", icon: Bot })),
  ];
  return [
    { label: "This session", items: session },
    { label: "Branches", items: (refs?.branches ?? []).map((b): Point => ({ value: `ref:${b}`, label: b, note: b === refs?.trunk ? "its trunk" : undefined, icon: GitBranch })) },
    { label: "Tags", items: (refs?.tags ?? []).map((t): Point => ({ value: `ref:${t}`, label: t, icon: Tag })) },
    {
      label: "Commits",
      items: (refs?.commits ?? []).map((c): Point => ({
        value: `ref:${c.sha}`,
        label: c.sha.slice(0, 7),
        note: c.subject,
        side: relativeTime(Math.floor(c.at / 1000)).replace(/ ago$/, ""),
        icon: GitCommitHorizontal,
      })),
    },
  ];
}

/** A point picked in place, from a list that filters as you type. */
function Picker({ points, value, onPick }: { points: { label: string; items: Point[] }[]; value: string; onPick: (v: string) => void }) {
  const [q, setQ] = React.useState("");
  const words = q.trim().toLowerCase();
  const shown = points.map((g) => ({ ...g, items: g.items.filter((p) => !words || `${p.label} ${p.note ?? ""}`.toLowerCase().includes(words)) })).filter((g) => g.items.length > 0);
  return (
    <div className="cx-pick">
      <input className="cx-filter" type="search" placeholder="Filter: a branch, a tag, a commit, an agent" aria-label="Filter the points" value={q} onChange={(e) => setQ(e.target.value)} />
      {shown.length === 0 && <div className="cx-pick-none">Nothing called that.</div>}
      {shown.map((g) => (
        <React.Fragment key={g.label}>
          <div className="cx-menu-label">{g.label}</div>
          {g.items.map((p) => (
            <button key={p.value} type="button" className="cx-ref" aria-pressed={p.value === value} onClick={() => onPick(p.value)}>
              <p.icon size={13} aria-hidden="true" />
              <span className="cx-ref-t">
                {p.label}
                {p.note && <em>{p.note}</em>}
              </span>
              {p.side && <small>{p.side}</small>}
            </button>
          ))}
        </React.Fragment>
      ))}
    </div>
  );
}

const split3 = (asked: string): [string, string, boolean] => {
  const [from = "", to = "", split = "true"] = asked.split("\u0000");
  return [from, to, split === "true"];
};

/** The two points compared, read through Suspense (kept per pair). */
function useCompared(source: CompareSource, asked: string, path?: string): CompareAnswer {
  const [from, to, split] = split3(asked);
  return useSuspenseResource(`harness/compare/${source.id}/${asked}/${path ?? ""}`, () => source.compare(from, to, split, path).then((a) => a, why)).data;
}

function Total({ source, asked }: { source: CompareSource; asked: string }) {
  const a = useCompared(source, asked);
  if (a.error !== undefined || a.compared.files.length === 0) return null;
  return <DiffStat add={a.compared.files.reduce((n, f) => n + f.added, 0)} del={a.compared.files.reduce((n, f) => n + f.removed, 0)} />;
}

const asChanged = (f: FileChange): ChangedFile => ({
  path: f.path,
  tree: f.tree,
  created: Boolean(f.created),
  deleted: Boolean(f.deleted),
  truncated: Boolean(f.truncated),
  add: f.added,
  del: f.removed,
  edits: [{ call: null, diff: { path: f.path, created: f.created, hunks: f.hunks } }],
  place: f.place,
});

function CompareFiles({
  source,
  asked,
  cwd,
  sel,
  onSel,
  name,
}: {
  source: CompareSource;
  asked: string;
  cwd: string;
  sel: string | null;
  onSel: (k: string | null) => void;
  name: (v: string) => string;
}) {
  const [sort] = usePref<SortKey>("kiln.changes.sort", "path");
  const a = useCompared(source, asked);
  if (a.error !== undefined) return <div className="cx-none">{a.error}</div>;
  const c = a.compared;
  const [from, to] = split3(asked);
  const rows: Row[] = sortRows(
    c.files.map((f) => rowOf(asChanged(f), cwd)),
    sort,
  );
  // Two agents: which of them changed each file.
  const line = (r: Row) => {
    const who = c.changed_by?.[r.file.path];
    if (!who) return "";
    return who.length > 1 ? "both changed it" : `changed by ${name(`agent:${who[0] ?? ""}`)}`;
  };
  const counted = c.counts_only ? `Too many files to show at once: ${c.files.length}. Pick one to see its lines.` : "";
  return (
    <>
      {counted && <div className="cx-note">{counted}</div>}
      <FileList
        rows={rows}
        line={line}
        sel={sel}
        onSel={onSel}
        none={from === to ? "They're the same point." : "They're the same."}
        diffOf={c.counts_only ? (r, alone) => (alone ? <OneFile source={source} asked={asked} path={r.file.path} label={r.name} /> : null) : undefined}
      />
    </>
  );
}

/** One file of a large compare: its lines, asked for alone. */
function OneFile({ source, asked, path, label }: { source: CompareSource; asked: string; path: string; label: string }) {
  return (
    <React.Suspense fallback={<div className="hx-dim text-[11px]">Reading its lines…</div>}>
      <OneFileLines source={source} asked={asked} path={path} label={label} />
    </React.Suspense>
  );
}

function OneFileLines({ source, asked, path, label }: { source: CompareSource; asked: string; path: string; label: string }) {
  const a = useCompared(source, asked, path);
  if (a.error !== undefined) return <div className="hx-dim text-[11px]">{a.error}</div>;
  const f = a.compared.files[0];
  if (!f) return null;
  return <FileDiffView diff={{ path: f.path, created: f.created, hunks: f.hunks }} label={label} defaultOpen />;
}
