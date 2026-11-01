/**
 * Beside the conversation, three places: everything the agent changed as one reviewable changeset;
 * its plan, subagents and the commands it ran; and what it works with — MCP servers, skills, usage.
 */
import * as React from "react";
import { Bot, Square } from "lucide-react";
import { useSession, type AgentInfo, type Command, type FileChange as ServerChange, type HEvent, type Meta, type SessionStore, type State, type Task } from "@/api/harness";
import { cn } from "@/lib/utils";
import { agentLabel, changesetOf, duration, editsOf, money, runsOf, taskDot, taskLive, tokens, useSameList, type ChangedFile, type ToolEvent } from "./derive";
import { kilnStatus } from "./tasks";
import { Output, Prose } from "./parts";
import { LimitMeter, StatusList } from "./limits";
import { McpPanel, SkillsPanel } from "./tools";
import { TaskElapsed } from "./timeline";
import { Background, isBackground } from "./background";
import { PlansSection } from "@/views/plans/browser";
import { PlanSourceContext, SavePlan, todosAsPlan } from "@/views/plans/save";
import { ProposedPlan, TodoSteps } from "@/views/plans/its-plan";
import { ReviewHere, useReviewWaits } from "@/views/plans/review";
import { ResearchSection } from "@/views/research/research";
import { Goals } from "./goals";
import { LivePages } from "./live-pages";
import { Loadable } from "@/components/ui/loading";
import { ChangeBar } from "./change-bar";
import { Fold, FoldOpen } from "./fold";
import { WorkspaceBar } from "./workspace-bar";
import { SubagentOutput } from "./task-output";
import { SessionChanges, inScope, useScope, type OwnSource, type Sub } from "./changes";

const NO_EVENTS: HEvent[] = [];
const NO_AGENTS: readonly AgentInfo[] = [];

/** Three places beside the conversation: what it changed, its plan (and goals, and saved plans to run), and its tools (what it has running, subagents, commands it ran, MCP, skills, usage). */
export type InspectorTab = "changes" | "plan" | "tools";

/** Large tool inventories start folded; hiding them keeps searches and unfinished forms intact. */
/** A section that folds, closed until opened (what's in it is long, or read only once it's open). */
function ToolSection({ id, title, count, children }: { id: string; title: string; count?: number; children: React.ReactNode }) {
  return (
    <Fold id={id} title={title} count={count} defaultOpen={false}>
      {children}
    </Fold>
  );
}

/** The server's cumulative changeset as the file list and diffs show it (one diff per file). */
function fromServer(files: ServerChange[]): ChangedFile[] {
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

/** Its harness's subagents (their runs' ids, what each was asked, its kind), the same list while they stay the same: what the Changes tab's By names. */
function useSubs(tasks: readonly Task[] | undefined): readonly Sub[] {
  const subs = (tasks ?? []).filter((t) => !isBackground(t) && t.tool_id).map((t): Sub => ({ id: t.tool_id ?? t.id, label: t.description, kind: t.kind || undefined }));
  const key = subs.map((s) => `${s.id}\u0000${s.label}\u0000${s.kind ?? ""}`).join("\u0001");
  // The same subagents, by what they are, keep the same list (as useSameList keeps its).
  const [kept] = React.useState(() => ({ key, subs }));
  if (kept.key !== key) {
    kept.key = key;
    kept.subs = subs;
  }
  return kept.subs;
}

/** `events`: only its edits matter (the changeset is worked out from them when the server sends none). `agentId`: its change request goes above them, and `volumeOver`, the folder its workspace of its own is laid over. `self`: the agent's name, and `subs` its harness's subagents, for who changed each file; `own`, where what one of them changed alone comes from. */
export const Changes = React.memo(function Changes({
  events,
  changes,
  cwd,
  agentId,
  volumeOver,
  self,
  subs,
  own,
}: {
  events: HEvent[];
  changes: ServerChange[] | null;
  cwd: string;
  agentId?: string;
  volumeOver?: string;
  self: string;
  subs: readonly Sub[];
  own?: OwnSource;
}) {
  const local = React.useMemo(() => (changes ? null : changesetOf(events)), [events, changes]);
  const files = React.useMemo(() => (changes ? fromServer(changes) : (local ?? [])), [changes, local]);
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
        <Loadable label="Loading its change request" fallback={<div className="hx-cr" aria-busy="true" />}>
          <ChangeBar agentId={agentId} />
        </Loadable>
      )}
      <SessionChanges files={files} cwd={cwd} self={self} subs={subs} own={own} />
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
  const sectionOpen = React.useContext(FoldOpen);
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

/** A subagent kiln started for one of its goals or plan steps (a session of its own): where it stands, what it's at; it opens its thread here. */
function KilnRow({ a, onOpen }: { a: AgentInfo; onOpen?: (id: string) => void }) {
  const status = kilnStatus(a);
  return (
    <li className="hx-task">
      <div className="hx-task-head">
        <i className="dot" data-status={status === "running" ? "running" : status === "failed" ? "failed" : status === "completed" ? "succeeded" : "pending"} aria-hidden="true" />
        <Bot size={12} className="text-[var(--color-subtle)]" />
        <button type="button" className="hx-task-title" disabled={!onOpen} onClick={() => onOpen?.(a.id)}>
          {agentLabel(a)}
        </button>
      </div>
      <div className="hx-task-meta">
        <span>{a.kind_name}</span>
        <span>{status}</span>
        {status === "running" && a.doing && <span className="hx-trunc">{a.doing}</span>}
      </div>
    </li>
  );
}

export function Subagents({
  state,
  meta,
  store,
  onOpen,
  kiln = NO_AGENTS,
}: {
  state: State | null;
  meta: Meta | null;
  store: SessionStore;
  onOpen?: (toolId: string) => void;
  kiln?: readonly AgentInfo[];
}) {
  const tasks = (state?.tasks ?? []).filter((t) => !isBackground(t));
  if (tasks.length === 0 && kiln.length === 0) return <div className="hx-empty">No subagents yet.</div>;
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
  for (const a of kiln) rows.push(<KilnRow key={a.id} a={a} onOpen={onOpen} />);
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
  kiln = NO_AGENTS,
}: {
  /** The subagents kiln started for its goals or plan steps (sessions of their own), listed with its harness's own. */
  kiln?: readonly AgentInfo[];
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
  // Who changed each file: the agent, by its name, and its harness's subagents.
  const self = useSession(store, (s) => (s.info ? agentLabel(s.info) : "The agent"));
  const subs = useSubs(state?.tasks);
  const [scope] = useScope();
  // At work now: the goal under way shimmers.
  const working = useSession(store, (s) => s.info?.status === "running");
  const meta = useSession(store, (s) => s.meta);
  // What one who changed its files did alone: its session answers, or its events show it edit by edit.
  const answers = meta?.features.includes("own_edits") ?? false;
  const own = React.useMemo<OwnSource>(
    () => ({ id: store.id, answers, ask: (by) => store.request((client) => ({ type: "own_edits", client, by }), 8000), events: () => store.get().events }),
    [store, answers],
  );
  // What's worked out from the conversation stays as it was while it streams, unless it's what changed.
  const edits = useSameList(React.useMemo(() => (changes ? NO_EVENTS : editsOf(events)), [changes, events]));
  // What the Changes tab counts is what its scope shows: this project's files at first.
  const fileCount = React.useMemo(() => (changes ? changes.filter((f) => inScope(f, scope)).length : changesetOf(edits).length), [changes, edits, scope]);
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
  const subagents = tasks.length + kiln.length;
  const mcp = state?.mcp ?? [];
  // A plan waiting on your review: its tab says so, and it's first in it.
  const reviewing = useReviewWaits(store);
  const tabs: { id: InspectorTab; label: string; count?: string; waits?: boolean }[] = [
    { id: "changes", label: "Changes", count: fileCount ? String(fileCount) : undefined },
    { id: "plan", label: "Plan", count: todos.length ? `${done}/${todos.length}` : undefined, waits: reviewing !== null },
    { id: "tools", label: "Tools", count: subagents ? String(subagents) : mcp.length ? `${mcp.filter((m) => m.status === "connected").length}/${mcp.length}` : undefined },
  ];
  return (
    <div className={cn("hx-inspector", className)}>
      <div className="hx-inspector-head" hidden={hideTabs}>
        <div className="hx-tabs" role="tablist">
          {tabs.map((t) => (
            <button key={t.id} type="button" role="tab" aria-selected={tab === t.id} className="hx-tab" onClick={() => onTab(t.id)}>
              {t.label}
              {t.count && <span className="hx-tab-count">{t.count}</span>}
              {t.waits && <i className="pr-tab-dot" role="img" aria-label="a plan waits on your review" />}
            </button>
          ))}
        </div>
        {actions}
      </div>
      <div className="hx-inspector-body">
        {tab === "changes" && <Changes events={edits} changes={changes} cwd={cwd} agentId={agentId} volumeOver={volumeOver} self={self} subs={subs} own={own} />}
        {tab === "plan" && (
          <div className="in-secs">
            <PlanSourceContext.Provider value={agentId ? { session: agentId } : null}>
              {/* A plan waiting on you (review.tsx): read and answered here, first. */}
              {agentId && <ReviewHere agent={agentId} store={store} send={send} />}
              {/* The plan under way as it moves, then every plan kept, to find and recall (browser.tsx). */}
              <PlansSection
                session={agentId}
                active={
                  <>
                    {/* Its to-do list, drawn as a plan's steps are: how far it's come. */}
                    {todos.length > 0 && (
                      <div className="hx-stack">
                        <TodoSteps todos={todos} />
                        <SavePlan body={todosAsPlan(todos)} title={todos[0]?.text.slice(0, 80)} />
                      </div>
                    )}
                    {/* The plan it proposed (plan mode), in its words, kept by kiln; while it waits on you, the review above is it. */}
                    {reviewing === "proposed" && send ? (
                      <p className="in-none">Its plan waits on your review, above.</p>
                    ) : agentId ? (
                      <Loadable label="its plan" fallback={null}>
                        <ProposedPlan session={agentId} words={proposed} tracked={todos.length > 0} />
                      </Loadable>
                    ) : (
                      proposed && (
                        <div className="hx-stack">
                          <Prose text={proposed} className="hx-plan-text" />
                          <SavePlan body={proposed} />
                        </div>
                      )
                    )}
                    {todos.length === 0 && !proposed && (
                      <p className="in-none">None yet: the plan it proposes shows here, kept by kiln as one of your plans, and its to-do list as it works through it.</p>
                    )}
                  </>
                }
              />
              {/* What agents found out, graded by kiln (research.tsx): to search, a card each, in brief until opened. */}
              {agentId && <ResearchSection agent={agentId} />}
              {agentId && <Goals agent={agentId} harness={harness} working={working} store={store} onOpenTask={onOpenTask} />}
            </PlanSourceContext.Provider>
          </div>
        )}
        {tab === "tools" && (
          <div className="in-secs">
            {/* What it has going: shells and monitors it left running, subagents; what it works with; then
                every command it ran, last, since that list only grows. */}
            {agentId && send && <Background state={state} agent={agentId} send={send} />}
            {agentId && (
              <Fold id="live-pages" title="Live pages">
                <LivePages session={agentId} />
              </Fold>
            )}
            {subagents > 0 && (
              <ToolSection id="subagents" title="Subagents" count={subagents}>
                <Subagents state={state} meta={meta} store={store} onOpen={onOpenTask} kiln={kiln} />
              </ToolSection>
            )}
            <ToolSection id="mcp" title="MCP servers" count={state?.mcp?.length}>
              <McpPanel state={state} store={store} />
            </ToolSection>
            <ToolSection id="skills" title="Skills" count={state?.skills?.length}>
              <SkillsPanel state={state} store={store} />
            </ToolSection>
            <Fold id="usage" title="Usage and status">
              <Status state={state} />
            </Fold>
            {runs.length > 0 && (
              <Fold id="commands" title="Commands it ran" count={runs.length}>
                <Runs runs={runs} />
              </Fold>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
