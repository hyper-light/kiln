/**
 * The inspector's Plan and Tools tabs (inspector.tsx), in a chunk of their own: neither is in a
 * conversation's first view (Changes is), so their code (plans, research, goals, background tasks,
 * tools, usage) comes once a tab is opened or pointed at (goal 8, plan 01a1a832ebfd step 6).
 */
import * as React from "react";
import { Bot, Square } from "lucide-react";
import type { AgentInfo, Command, Meta, SessionStore, State, Task, Todo } from "@/api/harness";
import { agentLabel, duration, money, taskDot, taskLive, tokens, type ToolEvent } from "./derive";
import { kilnStatus } from "./tasks";
import { Output } from "./parts";
import { Prose } from "./prose";
import { LimitMeter, StatusList } from "./limits";
import { McpPanel, SkillsPanel } from "./tools";
import { TaskElapsed } from "./timeline";
import { Background, isBackground } from "./background";
import { PlansSection } from "@/views/plans/browser";
import { PlanSourceContext, SavePlan, todosAsPlan } from "@/views/plans/save";
import { ProposedPlan, TodoSteps } from "@/views/plans/its-plan";
import { ReviewHere } from "@/views/plans/review";
import { ResearchSection } from "@/views/research/research";
import { Goals } from "./goals";
import { LivePages } from "./live-pages";
import { Loadable } from "@/components/ui/loading";
import { Fold, FoldOpen } from "./fold";
import { Whole } from "./whole";
import { SubagentOutput } from "./task-output";
import type { useReviewWaits } from "@/views/plans/review";

const NO_AGENTS: readonly AgentInfo[] = [];

/** Large tool inventories start folded; hiding them keeps searches and unfinished forms intact. */
/** A section that folds, closed until opened (what's in it is long, or read only once it's open). */
function ToolSection({ id, title, count, children }: { id: string; title: string; count?: number; children: React.ReactNode }) {
  return (
    <Fold id={id} title={title} count={count} defaultOpen={false}>
      {children}
    </Fold>
  );
}

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
            {isOpen && (
              // Lean, its output comes once it's opened (whole.tsx).
              <Whole e={c} fallback={<Output text="" truncated={c.truncated} error={failed} />}>
                {(w) => <Output text={w.output ?? ""} truncated={w.truncated} error={failed} />}
              </Whole>
            )}
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

/** The Plan tab: a plan waiting on your review, the plan under way and every plan kept, what agents found out, its goals. */
export function PlanTab({
  agentId,
  store,
  send,
  todos,
  reviewing,
  proposed,
  harness,
  working,
  onOpenTask,
}: {
  agentId?: string;
  store: SessionStore;
  send?: (c: Command) => void;
  todos: Todo[];
  reviewing: ReturnType<typeof useReviewWaits>;
  proposed: string;
  harness?: string;
  working: boolean;
  onOpenTask?: (toolId: string) => void;
}) {
  return (
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
              {todos.length === 0 && !proposed && <p className="in-none">None yet: the plan it proposes shows here, kept by kiln as one of your plans, and its to-do list as it works through it.</p>}
            </>
          }
        />
        {/* What agents found out, graded by kiln (research.tsx): to search, a card each, in brief until opened. */}
        {agentId && <ResearchSection agent={agentId} />}
        {agentId && <Goals agent={agentId} harness={harness} working={working} store={store} onOpenTask={onOpenTask} />}
      </PlanSourceContext.Provider>
    </div>
  );
}

/** The Tools tab: what it has going, subagents, MCP servers, skills, usage, the commands it ran. */
export function ToolsTab({
  agentId,
  send,
  state,
  subagents,
  meta,
  store,
  onOpenTask,
  kiln,
  runs,
}: {
  agentId?: string;
  send?: (c: Command) => void;
  state: State | null;
  subagents: number;
  meta: Meta | null;
  store: SessionStore;
  onOpenTask?: (toolId: string) => void;
  kiln: readonly AgentInfo[];
  runs: ToolEvent[];
}) {
  return (
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
  );
}
