/**
 * The Changes tab's Session view (docs/agents.md, "The Changes tab"; the Changes panel plan
 * 01a194444b1d, step 2): its head on one grid, a label at the edge, then its value, then the view's
 * total at the right edge; Scope, By and Sort as menus like Mission control's Actions; and its
 * files as a flat list, each saying who changed it, those outside the project under bands of their
 * own. Commits and Compare come with their own steps: turned off until then, not hidden.
 */
import * as React from "react";
import { DropdownMenu } from "radix-ui";
import { ArrowDownUp, Bot, Check, ChartNoAxesColumn, ChevronDown, Clock, FilePlus2, Folder, Layers, User, Users, type LucideIcon } from "lucide-react";
import type { FilePlace } from "@/api/harness";
import { usePref } from "@/lib/prefs";
import { relativeTime } from "@/lib/format";
import { DiffStat, FileDiffView } from "./diff";
import { relPath, type ChangedFile } from "./derive";
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

type SortKey = "path" | "latest" | "most" | "kind" | "who";

const SORTS: { id: SortKey; label: string; word: string; icon: LucideIcon }[] = [
  { id: "path", label: "Path", word: "Path", icon: Folder },
  { id: "latest", label: "Latest change first", word: "Latest", icon: Clock },
  { id: "most", label: "Most changed first", word: "Most", icon: ChartNoAxesColumn },
  { id: "kind", label: "New, changed, deleted", word: "Kind", icon: FilePlus2 },
  { id: "who", label: "Who changed it", word: "Who", icon: Users },
];

/** The agent itself, as a file's `by` names it beside its subagents' runs. */
const AGENT = "main";

/** A subagent of its harness's: its run's id (its spawning call's), what it was asked, its kind. */
export interface Sub {
  id: string;
  label: string;
  kind?: string;
}

/** A file as the list shows it: where it is, from its folder (or its own working tree's top), and the worktree it's in when that isn't its folder's. */
interface Row {
  key: string;
  file: ChangedFile;
  name: string;
  tree: string | null;
}

function rowOf(f: ChangedFile, cwd: string): Row {
  const elsewhere = f.tree && f.tree !== cwd && !cwd.startsWith(`${f.tree}/`) ? f.tree : null;
  return { key: `${f.tree ?? ""}\u0000${f.path}`, file: f, name: elsewhere ? f.path : relPath(f.path, cwd), tree: elsewhere };
}

/** A worktree as the second line names it: its folder's name, "worktree" when it's one (Claude Code's `.claude/worktrees`, git's). */
function treeWord(tree: string): string {
  const name = tree.replace(/\/+$/, "").split("/").pop() ?? tree;
  return /\/worktrees?\//.test(tree) ? `worktree ${name}` : name;
}

const KIND_RANK = (f: ChangedFile) => (f.created ? 0 : f.deleted ? 2 : 1);

/** `files`: the session's changeset. `self`: the agent's name; `subs`: its harness's subagents, in the order they began. */
export function SessionChanges({ files, cwd, self, subs }: { files: ChangedFile[]; cwd: string; self: string; subs: readonly Sub[] }) {
  const [scope, setScope] = useScope();
  const [sort, setSort] = usePref<SortKey>("kiln.changes.sort", "path");
  const [by, setBy] = React.useState<string>("all");
  const [sel, setSel] = React.useState<string | null>(null);
  const names = React.useMemo(() => new Map(subs.map((s) => [s.id, s.label])), [subs]);
  const who = React.useCallback((id: string) => (id === AGENT ? self : (names.get(id) ?? "A subagent")), [names, self]);
  // Everyone it may be narrowed to: the agent, its subagents as they began, and any run its files name that isn't listed (yet).
  const people = React.useMemo(() => {
    const out: Sub[] = [{ id: AGENT, label: self, kind: "itself" }, ...subs];
    for (const f of files) for (const id of f.by ?? []) if (!out.some((p) => p.id === id)) out.push({ id, label: who(id) });
    return out;
  }, [files, self, subs, who]);
  const scoped = React.useMemo(() => files.filter((f) => inScope(f, scope)), [files, scope]);
  const rows = React.useMemo(() => {
    const mine = by === "all" ? scoped : scoped.filter((f) => (f.by ?? [AGENT]).includes(by));
    const rows = mine.map((f) => rowOf(f, cwd));
    const order: Record<SortKey, (a: Row, b: Row) => number> = {
      path: (a, b) => a.name.localeCompare(b.name),
      latest: (a, b) => (b.file.at ?? 0) - (a.file.at ?? 0) || a.name.localeCompare(b.name),
      most: (a, b) => b.file.add + b.file.del - (a.file.add + a.file.del) || a.name.localeCompare(b.name),
      kind: (a, b) => KIND_RANK(a.file) - KIND_RANK(b.file) || a.name.localeCompare(b.name),
      who: (a, b) => people.findIndex((p) => p.id === (a.file.by ?? [AGENT])[0]) - people.findIndex((p) => p.id === (b.file.by ?? [AGENT])[0]) || a.name.localeCompare(b.name),
    };
    const band = (r: Row) => ["project", "user", "agent"].indexOf(placeOf(r.file));
    return rows.sort((a, b) => band(a) - band(b) || order[sort](a, b));
  }, [scoped, by, cwd, sort, people]);
  const add = rows.reduce((n, r) => n + r.file.add, 0);
  const del = rows.reduce((n, r) => n + r.file.del, 0);
  const picked = sel !== null && rows.some((r) => r.key === sel) ? sel : null;
  const shown = picked ? rows.filter((r) => r.key === picked) : rows;
  const count = (id: string) => scoped.filter((f) => (f.by ?? [AGENT]).includes(id)).length;
  const byLabel = by === "all" ? "Everyone" : who(by);
  const scopeOf = SCOPE[scope];
  const sortOf = SORTS.find((s) => s.id === sort);

  const line = (r: Row): string => {
    const ids = r.file.by ?? [];
    const tree = r.tree ? ` · ${treeWord(r.tree)}` : "";
    if (by === "all") return ids.length ? `${ids.map(who).join(", ")}${tree}` : tree.slice(3);
    const others = ids.filter((id) => id !== by);
    return others.length ? `also changed by ${others.map(who).join(", ")}` : `${who(by)}${tree}`;
  };

  return (
    <>
      <div className="cx-head">
        <div className="cx-mrow">
          <div className="cx-modes" role="radiogroup" aria-label="What it shows">
            <button type="button" role="radio" aria-checked="true">
              Session
            </button>
            <button type="button" role="radio" aria-checked="false" disabled title="Not in this version of kiln yet">
              Commits
            </button>
            <button type="button" role="radio" aria-checked="false" disabled title="Not in this version of kiln yet">
              Compare
            </button>
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
              </DropdownMenu.Content>
            </DropdownMenu.Portal>
          </DropdownMenu.Root>
        </div>
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
                </DropdownMenu.RadioGroup>
              </DropdownMenu.Content>
            </DropdownMenu.Portal>
          </DropdownMenu.Root>
          <span className="cx-total">{rows.length > 0 && <DiffStat add={add} del={del} />}</span>
        </div>
      </div>
      {rows.length === 0 ? (
        <div className="cx-none">{scope === "project" && by === "all" ? "Nothing in this project changed." : "Nothing here at this scope."}</div>
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
                <button type="button" className="cx-file" aria-pressed={picked === r.key} onClick={() => setSel(picked === r.key ? null : r.key)} title={r.tree ? `${r.tree}/${r.name}` : r.name}>
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
        {shown.map((r) =>
          r.file.edits.map((e, i) => (
            <div key={`${r.key}-${i}`}>
              {r.file.edits.length > 1 && (
                <div className="hx-dim mb-1 text-[11px]">
                  edit {i + 1} of {r.file.edits.length}
                  {e.call?.status === "error" ? " · failed" : ""}
                </div>
              )}
              <FileDiffView diff={e.diff} label={r.tree ? `${treeWord(r.tree)}: ${r.name}` : r.name} defaultOpen={shown.length <= 4} />
              {r.file.truncated && <div className="hx-dim mt-1 text-[11px]">Diff shortened — the file changed a lot.</div>}
            </div>
          )),
        )}
      </div>
    </>
  );
}
