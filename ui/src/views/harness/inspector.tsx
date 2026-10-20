/**
 * Beside the conversation, three places: everything the agent changed as one reviewable changeset;
 * its plan, subagents and the commands it ran; and what it works with — MCP servers, skills, usage.
 */
import * as React from "react";
import { Bot, ChevronRight, Square } from "lucide-react";
import { useSession, type Command, type FileChange as ServerChange, type HEvent, type Meta, type SessionStore, type State, type Task } from "@/api/harness";
import { cn } from "@/lib/utils";
import { changesetOf, duration, editsOf, money, relPath, runsOf, taskDot, taskLive, tokens, useSameList, type FileChange, type ToolEvent } from "./derive";
import { DiffStat, FileDiffView } from "./diff";
import { Output, Prose } from "./parts";
import { LimitMeter, StatusList } from "./limits";
import { McpPanel, SkillsPanel } from "./tools";
import { TaskElapsed, TodoList } from "./timeline";
import { Background, isBackground } from "./background";
import { PlansHere } from "@/views/plans/here";
import { PlanSourceContext, SavePlan, todosAsPlan } from "@/views/plans/save";
import { Goals } from "./goals";
import { LivePages } from "./live-pages";
import { Loadable } from "@/components/ui/loading";
import { ChangeBar } from "./change-bar";
import { WorkspaceBar } from "./workspace-bar";
import { SubagentOutput } from "./task-output";

const NO_EVENTS: HEvent[] = [];

/** Three places beside the conversation: what it changed, its plan (and goals, and saved plans to run), and its tools (what it has running, subagents, commands it ran, MCP, skills, usage). */
export type InspectorTab = "changes" | "plan" | "tools";

const ToolSectionOpen = React.createContext(true);

/** Large tool inventories start folded; hiding them keeps searches and unfinished forms intact. */
function ToolSection({ title, count, children }: { title: string; count?: number; children: React.ReactNode }) {
  const [open, setOpen] = React.useState(false);
  const bodyId = React.useId();
  return (
    <section className="in-fold" data-open={open || undefined}>
      <h3 className="in-sec-head">
        <button type="button" className="in-sec-toggle" aria-expanded={open} aria-controls={bodyId} onClick={() => setOpen(!open)}>
          <ChevronRight size={12} className={cn("hx-chev", open && "rotate-90")} aria-hidden="true" />
          {title}
          {count !== undefined && <span className="in-sec-count">{count}</span>}
        </button>
      </h3>
      <div id={bodyId} hidden={!open}><ToolSectionOpen.Provider value={open}>{children}</ToolSectionOpen.Provider></div>
    </section>
  );
}

/** The server's cumulative changeset as the file list and diffs show it (one diff per file). */
function fromServer(files: ServerChange[]): FileChange[] {
  return files.map((f) => ({
    path: f.path,
    created: Boolean(f.created),
    add: f.added,
    del: f.removed,
    edits: [{ call: null, diff: { path: f.path, created: f.created, hunks: f.hunks } }],
    deleted: Boolean(f.deleted),
    truncated: Boolean(f.truncated),
  }));
}

/** `events`: only its edits matter (the changeset is worked out from them when the server sends none). `agentId`: its change request goes above them, and `volumeOver`, the folder its workspace of its own is laid over. */
export const Changes = React.memo(function Changes({
  events,
  changes,
  cwd,
  agentId,
  volumeOver,
}: {
  events: HEvent[];
  changes: ServerChange[] | null;
  cwd: string;
  agentId?: string;
  volumeOver?: string;
}) {
  const local = React.useMemo(() => (changes ? null : changesetOf(events)), [events, changes]);
  const files = React.useMemo(() => (changes ? fromServer(changes) : (local ?? [])), [changes, local]);
  const [sel, setSel] = React.useState<string | null>(null);
  const shown: FileChange[] = sel ? files.filter((f) => f.path === sel) : files;
  const add = files.reduce((n, f) => n + f.add, 0);
  const del = files.reduce((n, f) => n + f.del, 0);
  const wsBar = agentId && volumeOver ? <WorkspaceBar agentId={agentId} over={volumeOver} /> : null;
  if (files.length === 0)
    return (
      <div className="hx-changes">
        {wsBar}
        <div className="hx-empty">No changes yet.</div>
      </div>
    );
  return (
    <div className="hx-changes">
      {wsBar}
      {agentId && (
        <Loadable label="Loading its change request" fallback={<div className="hx-change-bar" aria-busy="true" />}>
          <ChangeBar agentId={agentId} />
        </Loadable>
      )}
      <div className="hx-files">
        <button type="button" className="hx-file" aria-pressed={sel === null} onClick={() => setSel(null)}>
          <span className="hx-file-name">All files · {files.length}</span>
          <DiffStat add={add} del={del} />
        </button>
        {files.map((f) => {
          const rel = relPath(f.path, cwd);
          const slash = rel.lastIndexOf("/");
          return (
            <button key={f.path} type="button" className="hx-file" aria-pressed={sel === f.path} onClick={() => setSel(sel === f.path ? null : f.path)} title={rel}>
              <span className="hx-file-name">
                {slash > 0 && <span className="hx-dim">{rel.slice(0, slash + 1)}</span>}
                {rel.slice(slash + 1)}
                {f.created && <span className="hx-tag ml-1.5">new</span>}
                {f.deleted && <span className="hx-tag hx-tag-bad ml-1.5">deleted</span>}
                {f.edits.length > 1 && <span className="hx-dim ml-1.5">×{f.edits.length}</span>}
              </span>
              <DiffStat add={f.add} del={f.del} />
            </button>
          );
        })}
      </div>
      <div className="hx-stack">
        {shown.map((f) =>
          f.edits.map((e, i) => (
            <div key={`${f.path}-${i}`}>
              {f.edits.length > 1 && (
                <div className="hx-dim mb-1 text-[11px]">
                  edit {i + 1} of {f.edits.length}
                  {e.call?.status === "error" ? " · failed" : ""}
                </div>
              )}
              <FileDiffView diff={e.diff} label={relPath(f.path, cwd)} defaultOpen={shown.length <= 4} />
              {f.truncated && <div className="hx-dim mt-1 text-[11px]">Diff shortened — the file changed a lot.</div>}
            </div>
          )),
        )}
      </div>
    </div>
  );
});

/** `runs`: the commands it ran, in order. */
const Runs = React.memo(function Runs({ runs: ran }: { runs: ToolEvent[] }) {
  const runs = React.useMemo(() => ran.slice().reverse(), [ran]);
  const [open, setOpen] = React.useState<string | null>(null);
  if (runs.length === 0) return <div className="hx-empty">No commands yet.</div>;
  return (
    <ul className="hx-runs">
      {runs.map((c: ToolEvent) => {
        const failed = c.status === "error" || (c.exit_code !== undefined && c.exit_code !== 0);
        const isOpen = open === c.id;
        return (
          <li key={c.id} className="hx-run" data-failed={failed || undefined}>
            <button type="button" className="hx-run-head" aria-expanded={isOpen} onClick={() => setOpen(isOpen ? null : c.id)}>
              <i className="dot" data-status={c.status === "running" ? "running" : failed ? "failed" : c.status === "done" ? "succeeded" : "cancelled"} aria-hidden="true" />
              <code className="hx-trunc">{c.title}</code>
              <span className="hx-run-meta">
                {c.exit_code !== undefined && <span className={failed ? "hx-bad" : "hx-good"}>{c.exit_code}</span>}
                {c.ended ? <span className="hx-meta-num">{duration(c.ended - c.started)}</span> : null}
              </span>
            </button>
            {isOpen && <Output text={c.output ?? ""} truncated={c.truncated} error={failed} />}
          </li>
        );
      })}
    </ul>
  );
});

function TaskRow({ t, depth, canStop, store, onOpen }: { t: Task; depth: number; canStop: boolean; store: SessionStore; onOpen?: (toolId: string) => void }) {
  const live = taskLive(t);
  const [outputOpen, setOutputOpen] = React.useState(false);
  const sectionOpen = React.useContext(ToolSectionOpen);
  const outputId = React.useId();
  return (
    <li className="hx-task" style={{ paddingLeft: depth * 14 }}>
      <div className="hx-task-head">
        <i className="dot" data-status={taskDot(t)} aria-hidden="true" />
        <Bot size={12} className="text-[var(--color-subtle)]" />
        <button type="button" className="hx-task-title" disabled={!onOpen || !t.tool_id} onClick={() => t.tool_id && onOpen?.(t.tool_id)}>
          {t.description}
        </button>
        {canStop && live && t.stoppable !== false && (
          <button type="button" className="hx-icon" aria-label={`Stop ${t.description}`} onClick={() => store.send({ type: "stop_task", task: t.id })}>
            <Square size={10} fill="currentColor" />
          </button>
        )}
      </div>
      <div className="hx-task-meta">
        {t.kind && <span>{t.kind}</span>}
        {t.background && <span>background</span>}
        <span>{t.status}</span>
        {t.tool_uses ? <span>{t.tool_uses} steps</span> : null}
        {t.tokens ? <span>{tokens(t.tokens)} tokens</span> : null}
        <TaskElapsed task={t} />
        {live && t.last_tool && <span className="hx-trunc">{t.last_tool}</span>}
        <button type="button" className="hx-link" aria-expanded={outputOpen} aria-controls={outputId} onClick={() => setOutputOpen(!outputOpen)}>
          {outputOpen ? "Hide output" : "Output"}
        </button>
      </div>
      <div id={outputId} hidden={!outputOpen}>
        {outputOpen && <SubagentOutput agent={store.id} task={t} visible={sectionOpen} />}
      </div>
    </li>
  );
}

export function Subagents({ state, meta, store, onOpen }: { state: State | null; meta: Meta | null; store: SessionStore; onOpen?: (toolId: string) => void }) {
  const tasks = (state?.tasks ?? []).filter((t) => !isBackground(t));
  if (tasks.length === 0) return <div className="hx-empty">No subagents yet.</div>;
  const canStop = meta?.features.includes("stop_task") ?? false;
  // Children under their parents.
  const kids = new Map<string, Task[]>();
  const roots: Task[] = [];
  for (const t of tasks) {
    if (t.parent_task && tasks.some((p) => p.id === t.parent_task)) kids.set(t.parent_task, [...(kids.get(t.parent_task) ?? []), t]);
    else roots.push(t);
  }
  const rows: React.ReactNode[] = [];
  const walk = (t: Task, d: number) => {
    rows.push(<TaskRow key={t.id} t={t} depth={d} canStop={canStop} store={store} onOpen={onOpen} />);
    for (const k of kids.get(t.id) ?? []) walk(k, d + 1);
  };
  roots.forEach((t) => walk(t, 0));
  return <ul className="hx-tasks">{rows}</ul>;
}

function Status({ state }: { state: State | null }) {
  const limits = state?.limits ?? [];
  const items = state?.harness_status ?? [];
  return (
    <div className="hx-stack">
      {limits.length > 0 && (
        <section className="hx-stack">
          <div className="hx-eyebrow">Plan usage</div>
          {limits.map((l) => (
            <LimitMeter key={l.id} limit={l} />
          ))}
        </section>
      )}
      <section className="hx-stack">
        <div className="hx-eyebrow">This session</div>
        <Usage state={state} />
      </section>
      {items.length > 0 && <StatusList items={items} />}
      {limits.length === 0 && items.length === 0 && <div className="hx-dim text-[12px]">This harness hasn't reported its status or plan limits yet.</div>}
    </div>
  );
}

function Usage({ state }: { state: State | null }) {
  const u = state?.usage;
  if (!u) return null;
  const ctx = u.context_tokens && u.context_window ? Math.min(100, (u.context_tokens / u.context_window) * 100) : null;
  return (
    <div className="hx-usage">
      <div className="hx-kv">
        <span>Cost</span>
        <b>{money(u.cost_usd)}</b>
      </div>
      <div className="hx-kv">
        <span>Turns</span>
        <b>{u.turns}</b>
      </div>
      <div className="hx-kv">
        <span>Tokens in / out</span>
        <b>
          {tokens(u.input_tokens)} / {tokens(u.output_tokens)}
        </b>
      </div>
      <div className="hx-kv">
        <span>From cache</span>
        <b>{tokens(u.cache_read_tokens)}</b>
      </div>
      {ctx !== null && (
        <div className="hx-ctx">
          <div className="hx-kv">
            <span>Context</span>
            <b>
              {Math.round(ctx)}% · {tokens(u.context_tokens)} of {tokens(u.context_window)}
            </b>
          </div>
          <div className="hx-ctx-bar">
            <span style={{ width: `${ctx}%` }} data-high={ctx > 80 || undefined} />
          </div>
        </div>
      )}
    </div>
  );
}

export function Inspector({
  tab,
  onTab,
  store,
  cwd,
  onOpenTask,
  className,
  hideTabs,
  actions,
  agentId,
  harness,
  send,
}: {
  /** The agent, and how to send it commands (background tasks: output, stop). */
  agentId?: string;
  /** The agent's harness, by name ("Claude Code"). */
  harness?: string;
  send?: (c: Command) => void;
  /** The page shows its own tabs (phones). */
  hideTabs?: boolean;
  /** Desktop side tray controls, beside its tabs. */
  actions?: React.ReactNode;
  tab: InspectorTab;
  onTab: (t: InspectorTab) => void;
  /** The agent's live session: what's shown here is read from it. */
  store: SessionStore;
  cwd: string;
  onOpenTask?: (toolId: string) => void;
  className?: string;
}) {
  const events = useSession(store, (s) => (tab === "changes" && s.changes ? NO_EVENTS : s.events));
  const changes = useSession(store, (s) => s.changes);
  // The folder its workspace of its own is laid over, once made (a string: Changes stays memoized).
  const volumeOver = useSession(store, (s) => (s.info?.workspace?.made ? (s.info.workspace.over ?? "") : undefined));
  const state = useSession(store, (s) => s.state);
  const meta = useSession(store, (s) => s.meta);
  // What's worked out from the conversation stays as it was while it streams, unless it's what changed.
  const edits = useSameList(React.useMemo(() => (changes ? NO_EVENTS : editsOf(events)), [changes, events]));
  const fileCount = React.useMemo(() => (changes ? changes.length : changesetOf(edits).length), [changes, edits]);
  const runs = useSameList(React.useMemo(() => (tab === "tools" ? runsOf(events) : []), [events, tab]));
  const todos = state?.todos ?? [];
  const done = todos.filter((t) => t.status === "completed").length;
  // The plan it last proposed (plan mode), to read and keep.
  const proposed = React.useMemo(() => {
    if (tab !== "plan") return "";
    const e = events.findLast((e) => !e.parent && e.kind === "tool" && e.tool === "plan");
    return e?.kind === "tool" ? ((e.input as { plan?: string } | null)?.plan ?? e.output ?? "") : "";
  }, [events, tab]);
  const tasks = (state?.tasks ?? []).filter((t) => !isBackground(t));
  const mcp = state?.mcp ?? [];
  const tabs: { id: InspectorTab; label: string; count?: string }[] = [
    { id: "changes", label: "Changes", count: fileCount ? String(fileCount) : undefined },
    { id: "plan", label: "Plan", count: todos.length ? `${done}/${todos.length}` : undefined },
    { id: "tools", label: "Tools", count: tasks.length ? String(tasks.length) : mcp.length ? `${mcp.filter((m) => m.status === "connected").length}/${mcp.length}` : undefined },
  ];
  return (
    <div className={cn("hx-inspector", className)}>
      <div className="hx-inspector-head" hidden={hideTabs}>
        <div className="hx-tabs" role="tablist">
          {tabs.map((t) => (
            <button key={t.id} type="button" role="tab" aria-selected={tab === t.id} className="hx-tab" onClick={() => onTab(t.id)}>
              {t.label}
              {t.count && <span className="hx-tab-count">{t.count}</span>}
            </button>
          ))}
        </div>
        {actions}
      </div>
      <div className="hx-inspector-body">
        {tab === "changes" && <Changes events={edits} changes={changes} cwd={cwd} agentId={agentId} volumeOver={volumeOver} />}
        {tab === "plan" && (
          <div className="in-secs">
            <PlanSourceContext.Provider value={agentId ? { session: agentId } : null}>
              <section>
                <h3 className="in-sec-head">Its plan{todos.length > 0 && <span>{done} of {todos.length} done</span>}</h3>
                {todos.length > 0 ? (
                  <div className="hx-stack">
                    <div className="hx-progress">
                      <span style={{ width: `${(done / todos.length) * 100}%` }} />
                    </div>
                    <TodoList todos={todos} />
                    <SavePlan body={todosAsPlan(todos)} title={todos[0]?.text.slice(0, 80)} />
                  </div>
                ) : proposed ? (
                  <div className="hx-stack">
                    <Prose text={proposed} className="hx-plan-text" />
                    <SavePlan body={proposed} />
                  </div>
                ) : (
                  <p className="in-none">None yet: the plan it writes (or its to-do list) shows here, to keep as one of your plans.</p>
                )}
              </section>
              {todos.length > 0 && proposed && (
                <section>
                  <h3 className="in-sec-head">Proposed plan</h3>
                  <Prose text={proposed} className="hx-plan-text" />
                  <SavePlan body={proposed} />
                </section>
              )}
              {agentId && (
                <section>
                  <h3 className="in-sec-head">Your plans</h3>
                  <PlansHere session={agentId} />
                </section>
              )}
              {agentId && <Goals agent={agentId} harness={harness} />}
            </PlanSourceContext.Provider>
          </div>
        )}
        {tab === "tools" && (
          <div className="in-secs">
            {/* What it has going: shells and monitors it left running, subagents; what it works with; then
                every command it ran, last, since that list only grows. */}
            {agentId && send && <Background state={state} agent={agentId} send={send} />}
            {agentId && (
              <section>
                <h3 className="in-sec-head">Live pages</h3>
                <LivePages session={agentId} />
              </section>
            )}
            {tasks.length > 0 && (
              <ToolSection title="Subagents" count={tasks.length}>
                <Subagents state={state} meta={meta} store={store} onOpen={onOpenTask} />
              </ToolSection>
            )}
            <ToolSection title="MCP servers" count={state?.mcp?.length}>
              <McpPanel state={state} store={store} />
            </ToolSection>
            <ToolSection title="Skills" count={state?.skills?.length}>
              <SkillsPanel state={state} store={store} />
            </ToolSection>
            <section>
              <h3 className="in-sec-head">Usage and status</h3>
              <Status state={state} />
            </section>
            {runs.length > 0 && (
              <section>
                <h3 className="in-sec-head">Commands it ran<span>{runs.length}</span></h3>
                <Runs runs={runs} />
              </section>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
