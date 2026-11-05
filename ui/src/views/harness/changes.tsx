/**
 * The Changes tab's Session view (docs/agents.md, "The Changes tab"; the Changes panel plan
 * 01a194444b1d, step 2): its head on one grid, a label at the edge, then its value, then the view's
 * total at the right edge; Scope, By and Sort as menus like Mission control's Actions; and its
 * files as a flat list, each saying who changed it, those outside the project under bands of their
 * own. Commits is its branch's commits (commits.tsx); Compare comes with its own step, turned off
 * until then, not hidden. Narrowed to one who changed them, a file another changed too shows only
 * their part, as the session answers it (step 3, "One subagent's own edits"), or their edits one
 * by one from the conversation where it can't, saying why.
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { ArrowDownUp, Bot, Check, ChartNoAxesColumn, ChevronDown, Clock, FilePlus2, Folder, Layers, User, Users, type LucideIcon } from "lucide-react";
import { sessionStore, type FileChange, type FilePlace, type HEvent } from "@/api/harness";
import { usePref } from "@/lib/prefs";
import { relativeTime } from "@/lib/format";
import { DiffStat, FileDiffView } from "./diff";
import { Whole } from "./whole";
import { diffStats, relPath, type ChangedFile } from "./derive";
import { CommitsMode, CommitsView, type CommitsSource } from "./commits";
import { CompareMode, CompareView, type CompareSource } from "./compare";
import "./changes.css";

/** Whose files a scope shows: this project's; everything changed for the person; the agents' own too. */
export type Scope = "project" | "user" | "all";

const SCOPE_PLACES: Record<Scope, readonly FilePlace[]> = { project: ["project"], user: ["project", "user"], all: ["project", "user", "agent"] };

const SCOPE: Record<Scope, { label: string; note: string; icon: LucideIcon }> = {
  project: { label: "Project", note: "Its folder and its repository's worktrees", icon: Folder },
  user: { label: "User", note: "And everything else it changed for you", icon: User },
  all: { label: "All", note: "And the agents' own: memory, caches, ~/.claude, ~/.codex", icon: Layers },
};
const SCOPES: Scope[] = ["project", "user", "all"];

/** The scope this browser shows, Project at first; the tab's count follows it. */
export function useScope(): [Scope, (s: Scope) => void] {
  return usePref<Scope>("kiln.changes.scope", "project");
}

/** Whose a file is: one from a runner that doesn't say is this project's. */
export const placeOf = (f: { place?: FilePlace }): FilePlace => f.place ?? "project";

export const inScope = (f: { place?: FilePlace }, scope: Scope): boolean => SCOPE_PLACES[scope].includes(placeOf(f));

/** The band a file outside the project is listed under. */
const BAND: Record<FilePlace, string | null> = { project: null, user: "Elsewhere", agent: "The agents' own" };

export type SortKey = "path" | "latest" | "most" | "kind" | "who";

const SORTS: { id: SortKey; label: string; word: string; icon: LucideIcon }[] = [
  { id: "path", label: "Path", word: "Path", icon: Folder },
  { id: "latest", label: "Latest change first", word: "Latest", icon: Clock },
  { id: "most", label: "Most changed first", word: "Most", icon: ChartNoAxesColumn },
  { id: "kind", label: "New, changed, deleted", word: "Kind", icon: FilePlus2 },
  { id: "who", label: "Who changed it", word: "Who", icon: Users },
];

/** The agent itself, as a file's `by` names it beside its subagents' runs. */
const AGENT = "main";

/** An agent it started through kiln, as By's value names it (its own session's id after it). */
const STARTED = "started:";

/** No session to listen to: By isn't on an agent it started. */
const nothingToHear = () => () => undefined;

/** The server's cumulative changeset as the file list and diffs show it (one diff per file). */
export function fromServer(files: readonly FileChange[]): ChangedFile[] {
  return files.map((f) => ({
    path: f.path,
    tree: f.tree,
    created: Boolean(f.created),
    add: f.added,
    del: f.removed,
    edits: [{ call: null, diff: { path: f.path, created: f.created, hunks: f.hunks } }],
    deleted: Boolean(f.deleted),
    truncated: Boolean(f.truncated),
    by: f.by,
    place: f.place,
    at: f.at,
  }));
}

/** A subagent of its harness's: its run's id (its spawning call's), what it was asked, its kind. */
export interface Sub {
  id: string;
  label: string;
  kind?: string;
}

/** Where one's own part of a file comes from: its session, asked (`own_edits`), and the conversation where it can't say. */
export interface OwnSource {
  /** The session's id. */
  id: string;
  /** Its session answers (its features have "own_edits"). */
  answers: boolean;
  /** Its answer for `by` (null: none came). */
  ask: (by: string) => Promise<unknown>;
  /** Its events as they are now: their edits, one by one. */
  events: () => readonly HEvent[];
}

/** One's own part of each file another changed too, as its session answers it (docs/agents.md, "One subagent's own edits"). */
interface OwnReply {
  by: string;
  files: { path: string; tree?: string | null; kept: boolean; why?: string; edits?: FileChange[] }[];
}

/** Answers kept by what they're for (the session, who, and the files as they are), the latest few. */
const ANSWERS = new Map<string, Promise<OwnReply | null>>();

function answerFor(own: OwnSource, by: string, stamp: string): Promise<OwnReply | null> {
  const key = `${own.id}\u0000${by}\u0000${stamp}`;
  let p = ANSWERS.get(key);
  if (!p) {
    p = own.ask(by).then((v) => {
      const r = v as Partial<OwnReply> | null;
      return r && r.by === by && Array.isArray(r.files) ? (r as OwnReply) : null;
    });
    ANSWERS.set(key, p);
    for (const k of [...ANSWERS.keys()].slice(0, Math.max(0, ANSWERS.size - 16))) ANSWERS.delete(k);
  }
  return p;
}

/** What a file's part stands on: who changed it, how much, and when. */
const stampOf = (f: ChangedFile) => `${f.tree ?? ""}\u0000${f.path}\u0000${f.add}\u0000${f.del}\u0000${f.at ?? 0}\u0000${(f.by ?? []).join(",")}`;

/** Why a file shows one's edits one by one rather than their part as a whole. */
const ONE_BY_ONE: Record<string, string> = {
  cap: "Its edits one by one: this session keeps no more copies to compare them by (64 MiB at most).",
  unknown: "Its edits one by one: kiln didn't see how it found this file.",
  silent: "Its edits one by one: its machine didn't answer.",
  cannot: "Its edits one by one: its machine's kiln can't single out its part yet.",
};

/** `by`'s edits of `f` as the conversation shows them, one by one. */
function editsFrom(events: readonly HEvent[], by: string, f: ChangedFile, cwd: string): ChangedFile["edits"] {
  const abs = (p: string) => (p.startsWith("/") ? p : `${cwd.replace(/\/+$/, "")}/${p}`);
  const want = f.tree ? `${f.tree.replace(/\/+$/, "")}/${f.path}` : abs(f.path);
  const out: ChangedFile["edits"] = [];
  for (const e of events) {
    if (e.kind !== "tool" || e.tool !== "edit" || e.status === "denied" || (e.parent ?? AGENT) !== by) continue;
    for (const d of e.diffs ?? []) if (abs(d.path) === want || (!d.path.startsWith("/") && want.endsWith(`/${d.path}`))) out.push({ call: e, diff: d });
  }
  return out;
}

/** `f` as `by` changed it alone, with why it's shown edit by edit where it is. `reply`: undefined while it's asked (`pending`: the session's change shows till then). */
function partOf(f: ChangedFile, by: string, reply: OwnReply | null | undefined, own: OwnSource | undefined, cwd: string): { file: ChangedFile; note?: string; pending?: boolean } {
  const theirs = (why: string) => {
    const edits = own ? editsFrom(own.events(), by, f, cwd) : [];
    const { add, del } = diffStats(edits.map((e) => e.diff));
    return { file: { ...f, add, del, truncated: false, edits }, note: ONE_BY_ONE[why] ?? ONE_BY_ONE.unknown };
  };
  if (!own) return { file: f };
  if (!own.answers) return theirs("cannot");
  if (reply === undefined) return { file: f, pending: true };
  if (reply === null) return theirs("silent");
  const it = reply.files.find((o) => (o.tree ?? "") === (f.tree ?? "") && o.path === f.path);
  if (!it) return { file: f };
  if (!it.kept) return theirs(it.why ?? "unknown");
  const edits = it.edits ?? [];
  return {
    file: {
      ...f,
      created: edits[0]?.created ?? false,
      deleted: edits.at(-1)?.deleted ?? false,
      truncated: edits.some((e) => e.truncated),
      add: edits.reduce((n, e) => n + e.added, 0),
      del: edits.reduce((n, e) => n + e.removed, 0),
      edits: edits.map((e) => ({ call: null, diff: { path: f.path, created: e.created, hunks: e.hunks } })),
    },
  };
}

/** The answer for `by` about `stamp`'s files, read through Suspense; none to ask, nothing. */
function Asked({ own, by, stamp, children }: { own: OwnSource | undefined; by: string; stamp: string; children: (reply: OwnReply | null | undefined) => React.ReactNode }) {
  const reply = own?.answers && stamp ? React.use(answerFor(own, by, stamp)) : undefined;
  return children(reply);
}

/** A file as the list shows it: where it is, from its folder (or its own working tree's top), and the worktree it's in when that isn't its folder's. */
export interface Row {
  key: string;
  file: ChangedFile;
  name: string;
  tree: string | null;
}

export function rowOf(f: ChangedFile, cwd: string): Row {
  const elsewhere = f.tree && f.tree !== cwd && !cwd.startsWith(`${f.tree}/`) ? f.tree : null;
  return { key: `${f.tree ?? ""}\u0000${f.path}`, file: f, name: elsewhere ? f.path : relPath(f.path, cwd), tree: elsewhere };
}

/** A worktree as the second line names it: its folder's name, "worktree" when it's one (Claude Code's `.claude/worktrees`, git's). */
function treeWord(tree: string): string {
  const name = tree.replace(/\/+$/, "").split("/").pop() ?? tree;
  return /\/worktrees?\//.test(tree) ? `worktree ${name}` : name;
}

const KIND_RANK = (f: ChangedFile) => (f.created ? 0 : f.deleted ? 2 : 1);

/** `rows` in `sort`'s order, those outside the project after it under their bands; `who`: where a row's first changer is among those who changed them (Who). */
export function sortRows(rows: Row[], sort: SortKey, who?: (r: Row) => number): Row[] {
  const order: Record<SortKey, (a: Row, b: Row) => number> = {
    path: (a, b) => a.name.localeCompare(b.name),
    latest: (a, b) => (b.file.at ?? 0) - (a.file.at ?? 0) || a.name.localeCompare(b.name),
    most: (a, b) => b.file.add + b.file.del - (a.file.add + a.file.del) || a.name.localeCompare(b.name),
    kind: (a, b) => KIND_RANK(a.file) - KIND_RANK(b.file) || a.name.localeCompare(b.name),
    who: (a, b) => (who ? who(a) - who(b) : 0) || a.name.localeCompare(b.name),
  };
  const band = (r: Row) => ["project", "user", "agent"].indexOf(placeOf(r.file));
  return rows.sort((a, b) => band(a) - band(b) || order[sort](a, b));
}

/** What the tab shows: the session's changes, picked commits', or two points compared. */
export type Mode = "session" | "commits" | "compare";

/** The head's first row: what it shows, in the composer's mode words, and Sort at its right edge, its menu with what the view adds (`menu`). Commits and Compare are turned off, saying why, where its machine can't answer them (`commits`, `compare`, read through Suspense). */
export function ModeRow({ mode, onMode, commits, compare, menu }: { mode: Mode; onMode: (m: Mode) => void; commits?: React.ReactNode; compare?: React.ReactNode; menu?: React.ReactNode }) {
  const [sort, setSort] = usePref<SortKey>("kiln.changes.sort", "path");
  const sortOf = SORTS.find((s) => s.id === sort);
  return (
    <div className="cx-mrow">
      <div className="cx-modes" role="radiogroup" aria-label="What it shows">
        <button type="button" role="radio" aria-checked={mode === "session"} onClick={() => onMode("session")}>
          Session
        </button>
        {commits ?? (
          <button type="button" role="radio" aria-checked="false" disabled title="Not in this version of kiln yet">
            Commits
          </button>
        )}
        {compare ?? (
          <button type="button" role="radio" aria-checked="false" disabled title="Not in this version of kiln yet">
            Compare
          </button>
        )}
      </div>
      <DropdownMenu.Root>
        <DropdownMenu.Trigger asChild>
          <button type="button" className="cx-view" aria-label={`Sort: ${sortOf?.label ?? "Path"}`}>
            <ArrowDownUp size={12} aria-hidden="true" />
            {sortOf?.word ?? "Path"}
          </button>
        </DropdownMenu.Trigger>
        <DropdownMenu.Portal>
          <DropdownMenu.Content className="popover cx-menu" sideOffset={6} align="end" collisionPadding={12}>
            <DropdownMenu.Label className="cx-menu-label">Sort</DropdownMenu.Label>
            <DropdownMenu.RadioGroup value={sort} onValueChange={(v) => setSort(v as SortKey)}>
              {SORTS.map((s) => (
                <DropdownMenu.RadioItem key={s.id} value={s.id} className="menu-item">
                  <s.icon size={14} aria-hidden="true" />
                  <span className="flex-1">{s.label}</span>
                  <span className="cx-mark">
                    <DropdownMenu.ItemIndicator>
                      <Check size={14} aria-hidden="true" />
                    </DropdownMenu.ItemIndicator>
                  </span>
                </DropdownMenu.RadioItem>
              ))}
            </DropdownMenu.RadioGroup>
            {menu}
          </DropdownMenu.Content>
        </DropdownMenu.Portal>
      </DropdownMenu.Root>
    </div>
  );
}

/** The files a view shows, a flat list, each saying on the line under it what `line` says, and below them the picked one's diff (every one's, none picked). `notes`: what's said under a file's diffs; `pending`: files whose numbers wait for an answer; `diffOf`: a file's diff where the view reads it itself (`alone`: it's the one picked). */
export function FileList({
  rows,
  line,
  sel,
  onSel,
  none,
  notes,
  pending,
  diffOf,
}: {
  rows: Row[];
  line: (r: Row) => string;
  sel: string | null;
  onSel: (key: string | null) => void;
  none: string;
  notes?: Map<string, string | undefined>;
  pending?: Set<string>;
  diffOf?: (r: Row, alone: boolean) => React.ReactNode;
}) {
  const picked = sel !== null && rows.some((r) => r.key === sel) ? sel : null;
  const shown = picked ? rows.filter((r) => r.key === picked) : rows;
  return (
    <>
      {rows.length === 0 ? (
        <div className="cx-none">{none}</div>
      ) : (
        <div className="cx-files">
          {rows.map((r, i) => {
            const band = BAND[placeOf(r.file)];
            const slash = r.name.lastIndexOf("/");
            const when = r.file.at ? relativeTime(Math.floor(r.file.at / 1000)).replace(/ ago$/, "") : "";
            const words = line(r);
            return (
              <React.Fragment key={r.key}>
                {band && placeOf(r.file) !== placeOf(rows[i - 1]?.file ?? { place: "project" }) && <div className="cx-band">{band}</div>}
                <button
                  type="button"
                  className="cx-file"
                  aria-pressed={picked === r.key}
                  data-pending={pending?.has(r.key) || undefined}
                  onClick={() => onSel(picked === r.key ? null : r.key)}
                  title={r.tree ? `${r.tree}/${r.name}` : r.name}
                >
                  <span className="cx-path">
                    {slash > 0 && (
                      <span className="cx-path-dir">
                        <span dir="ltr">{r.name.slice(0, slash + 1)}</span>
                      </span>
                    )}
                    <span className="cx-path-name">{r.name.slice(slash + 1)}</span>
                    {r.file.created && <span className="hx-tag">new</span>}
                    {r.file.deleted && <span className="hx-tag hx-tag-bad">deleted</span>}
                  </span>
                  <DiffStat add={r.file.add} del={r.file.del} />
                  {(words || when) && (
                    <>
                      <span className="cx-by">{words}</span>
                      <span className="cx-when">{when}</span>
                    </>
                  )}
                </button>
              </React.Fragment>
            );
          })}
        </div>
      )}
      <div className="hx-stack">
        {shown.map((r) => (
          <React.Fragment key={r.key}>
            {diffOf && diffOf(r, picked !== null)}
            {!diffOf &&
              r.file.edits.map((e, i) => (
                <div key={`${r.key}-${i}`}>
                  {r.file.edits.length > 1 && (
                    <div className="hx-dim mb-1 text-[11px]">
                      edit {i + 1} of {r.file.edits.length}
                      {e.call?.status === "error" ? " · failed" : ""}
                    </div>
                  )}
                  {e.call ? (
                    // A lean call's lines come once it shows (whole.tsx).
                    <Whole e={e.call} fallback={<FileDiffView diff={e.diff} label={r.tree ? `${treeWord(r.tree)}: ${r.name}` : r.name} defaultOpen={false} />}>
                      {(w) => (
                        <FileDiffView diff={w.diffs?.[(e.call?.diffs ?? []).indexOf(e.diff)] ?? e.diff} label={r.tree ? `${treeWord(r.tree)}: ${r.name}` : r.name} defaultOpen={shown.length <= 4} />
                      )}
                    </Whole>
                  ) : (
                    <FileDiffView diff={e.diff} label={r.tree ? `${treeWord(r.tree)}: ${r.name}` : r.name} defaultOpen={shown.length <= 4} />
                  )}
                  {r.file.truncated && <div className="hx-dim mt-1 text-[11px]">Diff shortened — the file changed a lot.</div>}
                </div>
              ))}
            {notes?.get(r.key) && <div className="hx-dim mt-1 text-[11px]">{notes.get(r.key)}</div>}
          </React.Fragment>
        ))}
      </div>
    </>
  );
}

/** `files`: the session's changeset. `self`: the agent's name; `subs`: its harness's subagents, in the order they began. `own`: where one's own part of a file comes from; `commits`, its branch's commits. */
export function SessionChanges({
  files,
  cwd,
  self,
  subs,
  own,
  commits,
  compare,
  started = [],
}: {
  files: ChangedFile[];
  cwd: string;
  self: string;
  subs: readonly Sub[];
  own?: OwnSource;
  commits?: CommitsSource;
  compare?: CompareSource;
  started?: readonly Sub[];
}) {
  const [mode, setMode] = React.useState<Mode>("session");
  const [scope, setScope] = useScope();
  const [sort] = usePref<SortKey>("kiln.changes.sort", "path");
  const [by, setBy] = React.useState<string>("all");
  const [sel, setSel] = React.useState<string | null>(null);
  // An agent it started: its own changes, read from its own session, at the same scope.
  const startedOne = React.useMemo(
    () => (by.startsWith(STARTED) ? (started.find((a) => `${STARTED}${a.id}` === by) ?? { id: by.slice(STARTED.length), label: "An agent it started" }) : null),
    [by, started],
  );
  const theirStore = startedOne ? sessionStore(startedOne.id) : null;
  const theirChanges = React.useSyncExternalStore(
    theirStore?.subscribe ?? nothingToHear,
    () => theirStore?.get().changes ?? null,
    () => null,
  );
  const theirReady = React.useSyncExternalStore(
    theirStore?.subscribe ?? nothingToHear,
    () => theirStore?.get().ready ?? true,
    () => true,
  );
  const theirs = React.useMemo(() => (startedOne ? fromServer(theirChanges ?? []) : null), [startedOne, theirChanges]);
  const names = React.useMemo(() => new Map(subs.map((s) => [s.id, s.label])), [subs]);
  const who = React.useCallback((id: string) => (id === AGENT ? self : (names.get(id) ?? "A subagent")), [names, self]);
  // Everyone it may be narrowed to: the agent, its subagents as they began, and any run its files name that isn't listed (yet).
  const people = React.useMemo(() => {
    const out: Sub[] = [{ id: AGENT, label: self, kind: "itself" }, ...subs];
    for (const f of files) for (const id of f.by ?? []) if (!out.some((p) => p.id === id)) out.push({ id, label: who(id) });
    return out;
  }, [files, self, subs, who]);
  const scoped = React.useMemo(() => files.filter((f) => inScope(f, scope)), [files, scope]);
  const mine = React.useMemo(() => (theirs ? theirs.filter((f) => inScope(f, scope)) : by === "all" ? scoped : scoped.filter((f) => (f.by ?? [AGENT]).includes(by))), [theirs, scoped, scope, by]);
  // Narrowed to one of them: what of the files another changed too is theirs, asked of the session; the answer before it holds while the next is asked.
  const shared = by === "all" || theirs ? [] : mine.filter((f) => (f.by?.length ?? 0) > 1);
  const stamp = own?.answers && shared.length > 0 ? shared.map(stampOf).join("\u0001") : "";
  const askedBy = React.useDeferredValue(by);
  const asked = React.useDeferredValue(stamp);
  const sorted = (reply: OwnReply | null | undefined) => {
    const parts = new Map<string, string | undefined>();
    const pending = new Set<string>();
    const rows = mine.map((f) => {
      if (by === "all" || theirs || (f.by?.length ?? 0) < 2) return rowOf(f, cwd);
      const p = partOf(f, by, reply === undefined || reply?.by === by ? reply : undefined, own, cwd);
      const r = rowOf(p.file, cwd);
      parts.set(r.key, p.note);
      if (p.pending) pending.add(r.key);
      return r;
    });
    const first = (r: Row) => people.findIndex((p) => p.id === (r.file.by ?? [AGENT])[0]);
    return { rows: sortRows(rows, sort, first), notes: parts, pending };
  };
  const count = (id: string) => scoped.filter((f) => (f.by ?? [AGENT]).includes(id)).length;
  const byLabel = by === "all" ? "Everyone" : startedOne ? startedOne.label : who(by);
  const scopeOf = SCOPE[scope];
  // Commits: on once its machine has listed them (each read through Suspense), else turned off with why.
  const commitsMode = commits && (
    <React.Suspense
      fallback={
        <button type="button" role="radio" aria-checked="false" disabled title="Asking its machine for its commits">
          Commits
        </button>
      }
    >
      <CommitsMode source={commits} on={mode === "commits"} onPick={() => setMode("commits")} />
    </React.Suspense>
  );
  // Compare: on once its machine has said what it has to compare.
  const compareMode = compare && (
    <React.Suspense
      fallback={
        <button type="button" role="radio" aria-checked="false" disabled title="Asking its machine what it has to compare">
          Compare
        </button>
      }
    >
      <CompareMode source={compare} on={mode === "compare"} onPick={() => setMode("compare")} />
    </React.Suspense>
  );
  const head = (menu: React.ReactNode) => <ModeRow mode={mode} onMode={setMode} commits={commitsMode} compare={compareMode} menu={menu} />;
  const modeRow = head(null);
  if (mode === "commits" && commits) return <CommitsView source={commits} cwd={cwd} head={modeRow} />;
  if (mode === "compare" && compare) return <CompareView source={compare} cwd={cwd} head={head} self={self} subs={subs} />;

  const line = (r: Row): string => {
    const ids = r.file.by ?? [];
    const tree = r.tree ? ` · ${treeWord(r.tree)}` : "";
    if (startedOne) return `${startedOne.label}${tree}`;
    if (by === "all") return ids.length ? `${ids.map(who).join(", ")}${tree}` : tree.slice(3);
    const others = ids.filter((id) => id !== by);
    return others.length ? `also changed by ${others.map(who).join(", ")}` : `${who(by)}${tree}`;
  };

  /** What shows what one did alone: as the session's answer has it once it's in, the session's change till then. */
  const withOwn = (show: (reply: OwnReply | null | undefined) => React.ReactNode) => (
    <React.Suspense fallback={show(undefined)}>
      <Asked own={own} by={askedBy} stamp={asked}>
        {show}
      </Asked>
    </React.Suspense>
  );

  const list = ({ rows, notes, pending }: ReturnType<typeof sorted>) => (
    <FileList
      rows={rows}
      line={line}
      sel={sel}
      onSel={setSel}
      none={startedOne && !theirReady ? "Asking it for its changes…" : scope === "project" && by === "all" ? "Nothing in this project changed." : "Nothing here at this scope."}
      notes={notes}
      pending={pending}
    />
  );

  return (
    <>
      <div className="cx-head">
        {modeRow}
        <div className="cx-hrow">
          <span className="cx-lab">Scope</span>
          <DropdownMenu.Root>
            <DropdownMenu.Trigger asChild>
              <button type="button" className="cx-val" aria-label={`Scope: ${scopeOf.label}`}>
                <scopeOf.icon size={13} aria-hidden="true" />
                <span>{scopeOf.label}</span>
                <ChevronDown size={12} aria-hidden="true" className="cx-chev" />
              </button>
            </DropdownMenu.Trigger>
            <DropdownMenu.Portal>
              <DropdownMenu.Content className="popover cx-menu" sideOffset={6} align="start" collisionPadding={12}>
                <DropdownMenu.RadioGroup value={scope} onValueChange={(v) => setScope(v as Scope)}>
                  {SCOPES.map((id) => {
                    const s = SCOPE[id];
                    return (
                      <DropdownMenu.RadioItem key={id} value={id} className="menu-item">
                        <span className="cx-mark">
                          <DropdownMenu.ItemIndicator>
                            <Check size={14} aria-hidden="true" />
                          </DropdownMenu.ItemIndicator>
                        </span>
                        <s.icon size={14} aria-hidden="true" />
                        <span className="cx-item">
                          {s.label}
                          <small>{s.note}</small>
                        </span>
                        <span className="cx-n">{files.filter((f) => inScope(f, id)).length} files</span>
                      </DropdownMenu.RadioItem>
                    );
                  })}
                </DropdownMenu.RadioGroup>
              </DropdownMenu.Content>
            </DropdownMenu.Portal>
          </DropdownMenu.Root>
        </div>
        <div className="cx-hrow">
          <span className="cx-lab">By</span>
          <DropdownMenu.Root>
            <DropdownMenu.Trigger asChild>
              <button type="button" className="cx-val" aria-label={`By: ${byLabel}`}>
                {by === "all" ? <Users size={13} aria-hidden="true" /> : <Bot size={13} aria-hidden="true" />}
                <span>{byLabel}</span>
                <ChevronDown size={12} aria-hidden="true" className="cx-chev" />
              </button>
            </DropdownMenu.Trigger>
            <DropdownMenu.Portal>
              <DropdownMenu.Content className="popover cx-menu" sideOffset={6} align="start" collisionPadding={12}>
                <DropdownMenu.RadioGroup value={by} onValueChange={setBy}>
                  <DropdownMenu.RadioItem value="all" className="menu-item">
                    <span className="cx-mark">
                      <DropdownMenu.ItemIndicator>
                        <Check size={14} aria-hidden="true" />
                      </DropdownMenu.ItemIndicator>
                    </span>
                    <Users size={14} aria-hidden="true" />
                    <span className="cx-item">Everyone</span>
                    <span className="cx-n">{scoped.length} files</span>
                  </DropdownMenu.RadioItem>
                  {people.map((p, i) => {
                    const n = count(p.id);
                    return (
                      <React.Fragment key={p.id}>
                        {i === 1 && <DropdownMenu.Label className="cx-menu-label">Its subagents</DropdownMenu.Label>}
                        <DropdownMenu.RadioItem value={p.id} className="menu-item" disabled={n === 0}>
                          <span className="cx-mark">
                            <DropdownMenu.ItemIndicator>
                              <Check size={14} aria-hidden="true" />
                            </DropdownMenu.ItemIndicator>
                          </span>
                          <Bot size={14} aria-hidden="true" />
                          <span className="cx-item">
                            {p.label}
                            {p.kind && <small>{n === 0 && p.id !== AGENT ? `${p.kind} · changed nothing here` : p.kind}</small>}
                          </span>
                          <span className="cx-n">{n === 1 ? "1 file" : `${n} files`}</span>
                        </DropdownMenu.RadioItem>
                      </React.Fragment>
                    );
                  })}
                  {started.length > 0 && <DropdownMenu.Label className="cx-menu-label">Agents it started</DropdownMenu.Label>}
                  {started.map((a) => (
                    <DropdownMenu.RadioItem key={a.id} value={`${STARTED}${a.id}`} className="menu-item">
                      <span className="cx-mark">
                        <DropdownMenu.ItemIndicator>
                          <Check size={14} aria-hidden="true" />
                        </DropdownMenu.ItemIndicator>
                      </span>
                      <Bot size={14} aria-hidden="true" />
                      <span className="cx-item">
                        {a.label}
                        <small>{a.kind ? `${a.kind} · a session of its own` : "a session of its own"}</small>
                      </span>
                      <span className="cx-n" />
                    </DropdownMenu.RadioItem>
                  ))}
                </DropdownMenu.RadioGroup>
              </DropdownMenu.Content>
            </DropdownMenu.Portal>
          </DropdownMenu.Root>
          <span className="cx-total">
            {withOwn((reply) => {
              const { rows, pending } = sorted(reply);
              return (
                rows.length > 0 && <DiffStat add={rows.reduce((n, r) => n + r.file.add, 0)} del={rows.reduce((n, r) => n + r.file.del, 0)} className={pending.size > 0 ? "cx-pending" : undefined} />
              );
            })}
          </span>
        </div>
      </div>
      {withOwn((reply) => list(sorted(reply)))}
    </>
  );
}
