/**
 * Beside the conversation, three places: everything the agent changed as one reviewable changeset;
 * its plan, subagents and the commands it ran; and what it works with — MCP servers, skills, usage.
 */
import * as React from "react";
import { harnessApi, useSession, type AgentInfo, type Command, type FileChange as ServerChange, type HEvent, type SessionStore, type Task } from "@/api/harness";
import { cn } from "@/lib/utils";
import { agentLabel, changesetOf, editsOf, runsOf, useSameList } from "./derive";
import { isBackground } from "./background";
import { useReviewWaits } from "@/views/plans/review";
import { Loadable } from "@/components/ui/loading";
import { Hold } from "@/components/ui/hold";
import { ChangeBar } from "./change-bar";
import { WorkspaceBar } from "./workspace-bar";
import { BodiesContext } from "./whole";
import { SessionChanges, fromServer, inScope, useScope, type OwnSource, type Sub } from "./changes";
import type { CommitsSource } from "./commits";
import type { CompareSource } from "./compare";

const NO_EVENTS: HEvent[] = [];
const NO_AGENTS: readonly AgentInfo[] = [];

/** The Plan and Tools tabs' code, asked for as a tab is pointed at or opened (goal 8 step 6: not in the first view). */
const loadTabs = () => import("./inspector-tabs");
const PlanTab = React.lazy(() => loadTabs().then((m) => ({ default: m.PlanTab })));
const ToolsTab = React.lazy(() => loadTabs().then((m) => ({ default: m.ToolsTab })));

/** Three places beside the conversation: what it changed, its plan (and goals, and saved plans to run), and its tools (what it has running, subagents, commands it ran, MCP, skills, usage). */
export type InspectorTab = "changes" | "plan" | "tools";

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
  started,
}: {
  events: HEvent[];
  changes: ServerChange[] | null;
  cwd: string;
  agentId?: string;
  volumeOver?: string;
  self: string;
  subs: readonly Sub[];
  own?: OwnSource;
  started?: readonly Sub[];
}) {
  const local = React.useMemo(() => (changes ? null : changesetOf(events)), [events, changes]);
  const files = React.useMemo(() => (changes ? fromServer(changes) : (local ?? [])), [changes, local]);
  // Its branch's commits, asked of its machine (Commits).
  const commits = React.useMemo<CommitsSource | undefined>(() => (agentId ? { id: agentId, ask: (picks, branch) => harnessApi.commits(agentId, picks, branch) } : undefined), [agentId]);
  // What it has to compare, and two points compared, asked of its machine (Compare).
  const compare = React.useMemo<CompareSource | undefined>(
    () => (agentId ? { id: agentId, refs: () => harnessApi.refs(agentId), compare: (from, to, split, path) => harnessApi.compare(agentId, from, to, split, path) } : undefined),
    [agentId],
  );
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
      <SessionChanges files={files} cwd={cwd} self={self} subs={subs} own={own} commits={commits} compare={compare} started={started} />
    </div>
  );
});

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
  started,
}: {
  /** The subagents kiln started for its goals or plan steps (sessions of their own), listed with its harness's own. */
  kiln?: readonly AgentInfo[];
  /** Every agent it started through kiln: the Changes tab's By lists them, each read from its own session. */
  started?: readonly Sub[];
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
    <BodiesContext.Provider value={store}>
      <div className={cn("hx-inspector", className)}>
        <div className="hx-inspector-head" hidden={hideTabs}>
          <div className="hx-tabs" role="tablist">
            {tabs.map((t) => (
              <button
                key={t.id}
                type="button"
                role="tab"
                aria-selected={tab === t.id}
                className="hx-tab"
                onClick={() => onTab(t.id)}
                onPointerEnter={t.id === "changes" ? undefined : loadTabs}
                onFocus={t.id === "changes" ? undefined : loadTabs}
              >
                {t.label}
                {t.count && <span className="hx-tab-count">{t.count}</span>}
                {t.waits && <i className="pr-tab-dot" role="img" aria-label="a plan waits on your review" />}
              </button>
            ))}
          </div>
          {actions}
        </div>
        <div className="hx-inspector-body">
          {tab === "changes" && <Changes events={edits} changes={changes} cwd={cwd} agentId={agentId} volumeOver={volumeOver} self={self} subs={subs} own={own} started={started} />}
          {tab === "plan" && (
            <Loadable label="Loading the plan" fallback={<Hold label="Loading the plan" />}>
              <PlanTab agentId={agentId} store={store} send={send} todos={todos} reviewing={reviewing} proposed={proposed} harness={harness} working={working} onOpenTask={onOpenTask} />
            </Loadable>
          )}
          {tab === "tools" && (
            <Loadable label="Loading its tools" fallback={<Hold label="Loading its tools" />}>
              <ToolsTab agentId={agentId} send={send} state={state} subagents={subagents} meta={meta} store={store} onOpenTask={onOpenTask} kiln={kiln} runs={runs} />
            </Loadable>
          )}
        </div>
      </div>
    </BodiesContext.Provider>
  );
}
