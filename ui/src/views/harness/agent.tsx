/**
 * One agent: the conversation (what you asked, what it did, what it answered), what's waiting on
 * you, and the composer — with, beside it, what it changed, its plan, and what it works with. On a
 * phone those become four tabs.
 *
 * The page reads the agent's live session in parts (`useSession`): the conversation, the head, the
 * dock and the side each render when their own part changes, and typing renders only the composer.
 * Until the first snapshot is in, the page's frame shows with the loader where the conversation goes.
 */
import * as React from "react";
import "./harness.css";
import "./console.css";
import { Link, Navigate, useNavigate, useParams, useSearchParams } from "@/lib/router";
import { ArrowLeft, ChevronRight, GitFork, Pause, RotateCcw, Shield, X } from "lucide-react";
import { clientId, harnessApi, lastTurnFrom, sessionStore, useSession, useSessionReady, useStopAt } from "@/api/harness";
import type { AgentInfo, Approval, Attachment, Command, FileChange, HEvent, Meta, SendMode, SessionStore, Task, Todo } from "@/api/harness";
import { harnessSessionsRes, useRes } from "@/lib/data";
import { kilnSubagentsOf } from "./tasks";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { toast } from "@/components/ui/toast";
import { errorMessage, useLocalStorage, useMediaQuery } from "@/lib/hooks";
import { AccountDialog } from "./account-card";
import { Composer, type ToMark } from "./composer";
import { useAgentActions } from "./actions";
import { MoveDialog, movable } from "./move";
import { SettingsSheet, useAttachments, type Attachments } from "./controls";
import { agentLabel, changesetOf, editsOf, useSameList } from "./derive";
import { useListener } from "./dom";
import { FindBar, findIn, type FindNow } from "./find";
import { EarlierMatches } from "./said";
import { AgentHeader, useTurnState, type BorderState } from "./head";
import { Inspector, type InspectorTab } from "./inspector";
import { DesktopLayout, INSPECTOR_COLLAPSED } from "./desktop";
import { AskedLine } from "./asked";
import { PausedLine } from "./pause";
import { useMarkSeen } from "./seen";
import { ActivityLine } from "./background";
import { ThreadFromContext, Timeline, type ReplyTarget } from "./timeline";
import { BesideTerminals, openBeside as openTerminal } from "./beside";
import { useDrafts } from "./state";
import { PlanSourceContext } from "@/views/plans/save";
import { useReviewWaits } from "@/views/plans/review";
import { revealReview } from "@/views/plans/reveal";
import { useEndWithChildren } from "./family";
import { LoadingMark, PrismLabel } from "./signals";
import { asAnswered, markAnswered, stillAsked, unmark, useAnswered } from "./answered";
import { liveRoot, subagentRows, subagentTree, takesMessage, threadCrumbs, threadFrom, topOf, type Crumb, type SubagentNode, type ThreadFrom } from "./subagent-tree";
import { AboutContext } from "@/components/code/about";

type PhoneTab = "work" | InspectorTab;

const NO_EVENTS: HEvent[] = [];
const NO_TODOS: Todo[] = [];

/** The composer's field, to type in: the field itself, not the marked copy drawn under it (compose-field.tsx), which has its class too. */
const composerField = () => document.querySelector<HTMLTextAreaElement>("textarea.hx-compose-input");

/** What a message to it can carry. */
const acceptsOf = (meta: Meta | null) => {
  const f = meta?.features ?? [];
  return f.includes("attachments") ? "any" : f.includes("images") ? "images" : "none";
};

export default function AgentPage() {
  const { id = "" } = useParams();
  // Each agent its own page: what's open, found or chosen on one doesn't carry over to the next.
  return <AgentConsole key={id} id={id} />;
}

function AgentConsole({ id }: { id: string }) {
  const store = sessionStore(id);
  return (
    <React.Suspense fallback={<AgentLoading id={id} />}>
      <InItsAgentsChat id={id}>
        <Agent id={id} store={store} />
      </InItsAgentsChat>
    </React.Suspense>
  );
}

/**
 * A subagent kiln started for an agent's goal or plan step opens in that agent's chat, its thread
 * shown there, as its harness's own subagents' are (`?own=1`: its page of its own, for what's only
 * there). One whose agent has gone is a page of its own.
 */
function InItsAgentsChat({ id, children }: { id: string; children: React.ReactNode }) {
  const [params] = useSearchParams();
  const all = useRes(harnessSessionsRes).data;
  const me = all.find((a) => a.id === id);
  const main = me?.subagent && me.parent && params.get("own") !== "1" ? all.find((a) => a.id === me.parent) : undefined;
  return main ? <Navigate to={`/agents/${main.id}?task=${encodeURIComponent(id)}`} replace /> : <>{children}</>;
}

function Agent({ id, store }: { id: string; store: SessionStore }) {
  useSessionReady(store);
  useMarkSeen(id);
  const navigate = useNavigate();
  const info = useSession(store, (s) => s.info);
  // Code in its conversation can be asked about (components/code/about.tsx): by a helper of its, or, with subagents off, it.
  const busy = useSession(store, (s) => s.state?.turn_started != null);
  const about = React.useMemo(() => ({ id, name: info?.name, subagents: info?.max_subagents !== 0, busy, store }), [id, info?.name, info?.max_subagents, busy, store]);
  const gone = useSession(store, (s) => s.conn === "gone");
  const cwd = useSession(store, (s) => s.state?.cwd || s.info?.cwd || "");
  const accepts = useSession(store, (s) => acceptsOf(s.meta));
  const [params, setParams] = useSearchParams();
  const taskId = params.get("task");
  const taskInfo = useSession(store, (s) => (taskId ? s.state?.tasks.find((t) => t.id === taskId || t.tool_id === taskId) : undefined));
  // The subagents kiln started for its goals or plan steps: its, as its harness's own are.
  const everyone = useRes(harnessSessionsRes).data;
  const kiln = React.useMemo(() => kilnSubagentsOf(everyone, id), [everyone, id]);
  // The head's pause button's state: the composer's border wears it, whichever thread it writes to.
  const turn = useTurnState(store, kiln);
  // One of those, its thread shown here: a session of its own.
  const sub = taskId ? (kiln.find((a) => a.id === taskId) ?? null) : null;
  // Mission Control links by the stable task id; the timeline groups its events by spawn call.
  const task = !sub && taskId ? taskInfo?.tool_id || taskId : null;
  const wide = useMediaQuery("(min-width: 1100px)");
  const [trayCollapsed, setTrayCollapsed] = useLocalStorage(INSPECTOR_COLLAPSED, false);
  // `?tab=plan`: opened on its plans (the way back from one of them, or its research).
  const [tab, setTab] = React.useState<InspectorTab>(() => (params.get("tab") === "plan" ? "plan" : "changes"));
  const [phoneTab, setPhoneTab] = React.useState<PhoneTab>(() => (params.get("tab") === "plan" ? "plan" : "work"));
  const [confirmStop, setConfirmStop] = React.useState(false);
  const [settings, setSettings] = React.useState(false);
  /** /login or /logout: the harness's account on its machine, opened in place over the composer. */
  const [account, setAccount] = React.useState<"login" | "logout" | null>(null);
  /** A terminal beside it (its shell, its harness's own screen), asked for from the composer. */
  const openBeside = React.useCallback(
    (o: { what: "shell" | "screen"; command?: string }) => {
      openTerminal(id, o.what, o.command);
      setPhoneTab("work");
    },
    [id],
  );
  /** What the terminal hands to the composer goes after what's written there. */
  const hand = React.useCallback(
    (text: string) => {
      const d = useDrafts.getState();
      const was = d.drafts[id] ?? "";
      d.setDraft(id, was.trim() ? `${was.replace(/\s+$/, "")}\n\n${text}` : text);
      composerField()?.focus();
    },
    [id],
  );
  // /login and /logout open the account in the conversation, below what was said: brought into view once.
  // Find in the conversation (Ctrl/⌘F, or the head's button): the words, and the match shown. A
  // match found from Mission Control (`?find=`) opens it already looking.
  const [findOpen, setFindOpen] = React.useState(() => Boolean(params.get("find")));
  const [findQ, setFindQ] = React.useState(() => params.get("find") ?? "");
  const [findAt, setFindAt] = React.useState<FindNow | null>(null);
  const findInput = React.useRef<HTMLInputElement>(null);
  const openFind = React.useCallback(() => {
    setFindOpen(true);
    setPhoneTab("work");
    requestAnimationFrame(() => {
      findInput.current?.focus();
      findInput.current?.select();
    });
  }, []);
  useListener<KeyboardEvent>(window, "keydown", (e) => {
    if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey || e.key.toLowerCase() !== "f") return;
    // Asked again from the find bar: the browser's own find.
    if (document.activeElement === findInput.current) return;
    e.preventDefault();
    openFind();
  });
  const attachments = useAttachments(accepts);
  const [dragging, setDragging] = React.useState(false);
  // What the next message answers (a message's ⋯ → Reply to): the composer quotes it.
  const [reply, setReply] = React.useState<ReplyTarget | null>(null);
  // Screenshots pasted anywhere on the page attach to the next message (text pastes are left alone).
  useListener<ClipboardEvent>(document, "paste", (e) => {
    const files = e.clipboardData?.files;
    if (!files?.length || !attachments.enabled) return;
    const t = e.target as HTMLElement | null;
    if (t?.closest("input:not(.hx-compose-input), textarea:not(.hx-compose-input)")) return;
    e.preventDefault();
    void attachments.add(files);
  });

  const send = React.useCallback(
    (c: Command) => {
      // An answer shows as given at once (answered.ts); back if it couldn't go.
      if (c.type === "approve") markAnswered([c.id]);
      if (store.send(c)) return;
      if (c.type === "approve") unmark([c.id]);
      toast.error("Not connected", "The agent's machine is reconnecting; try again in a moment.");
    },
    [store],
  );
  const openTask = React.useCallback(
    (toolId: string) => {
      const child = store.get().state?.tasks.find((t) => t.tool_id === toolId || t.id === toolId);
      setParams((previous) => {
        previous.set("task", child?.id ?? toolId);
        return previous;
      });
      setPhoneTab("work");
    },
    [store, setParams],
  );
  const mainThread = () =>
    setParams((previous) => {
      previous.delete("task");
      return previous;
    });
  const taskName = taskInfo?.description;
  const path = useThreadPath(store, id, info ?? undefined, sub, taskInfo?.id ?? taskId, taskName);
  // Who started the thread shown, for its timeline: a key, so its turns only render again when that changes.
  const fromKey = JSON.stringify(path ? threadFrom(path.crumbs, path.here, { via: sub ? null : taskInfo?.via, session: sub ? id : undefined }) : null);
  const from = React.useMemo(() => JSON.parse(fromKey) as ThreadFrom | null, [fromKey]);

  // The matches, in the thread shown; a new search starts at the newest, and the one shown stays
  // shown as the conversation grows. (The conversation is read here only while finding.)
  // A subagent kiln started, its thread shown: found in its own session's conversation.
  const subId = sub?.id;
  const thread = React.useMemo(() => (subId ? sessionStore(subId) : store), [subId, store]);
  const findEvents = useSession(thread, (s) => (findOpen ? s.events : NO_EVENTS));
  const dropped = useSession(thread, (s) => (findOpen ? (s.state?.dropped ?? 0) : 0));
  const q = React.useDeferredValue(findQ);
  const matches = React.useMemo(() => (findOpen ? findIn(findEvents, subId ? undefined : (task ?? undefined), q) : []), [findOpen, findEvents, subId, task, q]);
  const [searched, setSearched] = React.useState("");
  if (findOpen && q !== searched) {
    setSearched(q);
    const newest = matches.at(-1);
    setFindAt(newest ? { id: newest, n: (findAt?.n ?? 0) + 1 } : null);
  }
  const findIndex = findAt ? matches.indexOf(findAt.id) : -1;
  const stepFind = (by: -1 | 1) => {
    if (matches.length === 0) return;
    const next = findIndex < 0 ? (by < 0 ? matches.length - 1 : 0) : (findIndex + by + matches.length) % matches.length;
    setFindAt({ id: matches[next] ?? "", n: (findAt?.n ?? 0) + 1 });
  };
  const closeFind = () => {
    setFindOpen(false);
    setFindAt(null);
    setSearched("");
    composerField()?.focus();
  };
  const find = React.useMemo(() => (findOpen ? { q, now: findAt } : undefined), [findOpen, q, findAt]);
  const showTools = React.useCallback(() => {
    setTrayCollapsed(false);
    setTab("tools");
    setPhoneTab("tools");
  }, [setTrayCollapsed]);
  const showPlan = () => {
    setTrayCollapsed(false);
    setTab("plan");
    setPhoneTab("plan");
  };
  /** A plan it proposes, reviewed where its plans are: its Plan tab, the review at its top. */
  const reviewPlan = () => {
    showPlan();
    revealReview();
  };
  /** /model, /effort, /permissions: that setting of the head's run settings, opened in place. */
  const openSetting = (label: "Model" | "Effort" | "Permissions") => {
    const b = document.querySelector<HTMLButtonElement>(`.ag-head button.rs-v[aria-label^="${label}:"]`);
    if (!b) return;
    if (b.getAttribute("aria-expanded") !== "true") b.click();
    b.focus({ preventScroll: true });
    b.scrollIntoView({ block: "nearest" });
  };
  /** A slash command that opens one of kiln's panels. */
  const openView = (view: string) => {
    if (view === "settings") setSettings(true);
    else if (view === "login" || view === "logout") setAccount(view);
    else if (view === "model" || view === "effort" || view === "permissions") openSetting(view === "model" ? "Model" : view === "effort" ? "Effort" : "Permissions");
    else if (view === "terminal") openBeside({ what: "shell" });
    else if (view === "screen") openBeside({ what: "screen" });
    else if (view === "goals") showPlan();
    else if (view === "tasks") showTools();
    else if (view === "changes") {
      setTrayCollapsed(false);
      setTab("changes");
      setPhoneTab("changes");
    } else if (view === "new_agent") navigate("/agents?new=1");
    else showTools();
  };

  // Ending it ends the agents it started too, unless the dialog says otherwise.
  const ending = useEndWithChildren(id);
  const stop = async () => {
    try {
      await ending.end();
      navigate("/agents");
    } catch (e) {
      toast.error("Couldn't stop the agent", errorMessage(e));
    }
  };

  const footer = React.useMemo(() => <>{!task && <BesideTerminals agent={id} info={info} onInsert={hand} />}</>, [info, task, id, hand]);
  const planSource = React.useMemo(
    () => (info ? { session: info.id, harness: info.kind, machine: info.target === "local" ? null : info.target.replace(/^agent:/, ""), cwd, agent: info.name } : null),
    [info, cwd],
  );

  const work = (
    <div className="hx-main">
      {findOpen && (
        <FindBar
          q={findQ}
          onQ={setFindQ}
          index={findIndex < 0 ? Math.max(0, matches.length - 1) : findIndex}
          total={matches.length}
          onStep={stepFind}
          onClose={closeFind}
          partial={dropped ? findEvents.length : undefined}
          inputRef={findInput}
        />
      )}
      {findOpen && dropped > 0 && <EarlierMatches agent={subId ?? id} q={findQ} before={findEvents[0]?.at} />}
      {path && <ThreadCrumbs crumbs={path.crumbs} sub={sub} onClose={mainThread} />}
      <ThreadFromContext.Provider value={from}>
        {sub ? (
          <React.Suspense fallback={<LoadingMark />}>
            <KilnThread key={sub.id} id={sub.id} onTerminal={openBeside} find={find} turn={turn} />
          </React.Suspense>
        ) : (
          <>
            <PlanSourceContext.Provider value={planSource}>
              <AboutContext.Provider value={task ? null : about}>
                <Timeline
                  key={task ?? "main"}
                  store={store}
                  agent={id}
                  cwd={cwd}
                  parent={task ?? undefined}
                  onOpenTask={openTask}
                  onCommand={send}
                  onReply={setReply}
                  agentLabel={task ? "Subagent" : info?.kind_name}
                  agentName={info?.name}
                  onDecide={send}
                  target={info?.target}
                  kind={info?.kind}
                  accountOpen={account !== null}
                  find={find}
                  footer={footer}
                  onReviewPlan={task ? undefined : reviewPlan}
                />
              </AboutContext.Provider>
            </PlanSourceContext.Provider>
            <Dock
              store={store}
              id={id}
              attachments={attachments}
              toMain={task !== null}
              task={task}
              kiln={kiln}
              turn={turn}
              onView={openView}
              onTerminal={openBeside}
              onOpenTask={openTask}
              onRemove={() => setConfirmStop(true)}
              send={send}
              reply={reply}
              onClearReply={() => setReply(null)}
            />
          </>
        )}
      </ThreadFromContext.Provider>
    </div>
  );

  const hasFiles = (e: React.DragEvent) => [...e.dataTransfer.types].includes("Files");
  return (
    <div
      className="hx-agent ag-ready"
      onDragEnter={(e) => {
        if (hasFiles(e)) setDragging(true);
      }}
      onDragOver={(e) => {
        if (hasFiles(e)) e.preventDefault();
      }}
      onDragLeave={(e) => {
        if (e.currentTarget === e.target || !e.currentTarget.contains(e.relatedTarget as Node | null)) setDragging(false);
      }}
      onDrop={(e) => {
        setDragging(false);
        if (!hasFiles(e)) return;
        e.preventDefault();
        if (attachments.enabled) void attachments.add(e.dataTransfer.files);
        else toast.error("This agent can't take attachments", `${info?.kind_name ?? "Its harness"} doesn't accept files through kiln yet.`);
      }}
    >
      <title>{info ? `${agentLabel(info)} · Agents · kiln` : "Agent · kiln"}</title>
      {account && info && <AccountDialog key={account} target={info.target} kind={info.kind} intent={account} onClose={() => setAccount(null)} />}
      {dragging && (
        <div className="hx-dropzone" aria-hidden="true">
          <span>{attachments.enabled ? "Drop screenshots or files to attach" : "This agent can't take attachments"}</span>
        </div>
      )}
      <Head store={store} cwd={cwd} send={send} onUsage={showTools} onSettings={() => setSettings(true)} onFind={openFind} />

      {gone ? (
        <div className="hx-empty mt-16">
          This agent has ended.{" "}
          <Link to="/agents" className="text-link">
            Back to all agents
          </Link>
        </div>
      ) : wide ? (
        <DesktopLayout
          collapsed={trayCollapsed}
          onCollapsed={setTrayCollapsed}
          side={(actions) => (
            <AboutContext.Provider value={about}>
              <Inspector tab={tab} onTab={setTab} store={store} cwd={cwd} onOpenTask={openTask} agentId={id} harness={info?.kind_name} send={send} kiln={kiln} className="hx-side" actions={actions} />
            </AboutContext.Provider>
          )}
        >
          {work}
        </DesktopLayout>
      ) : (
        <div className="hx-agent-phone">
          <PhoneTabs store={store} tab={phoneTab} onTab={setPhoneTab} />
          {phoneTab === "work" ? (
            work
          ) : (
            <AboutContext.Provider value={about}>
              <Inspector
                tab={phoneTab}
                onTab={(t) => setPhoneTab(t)}
                store={store}
                cwd={cwd}
                onOpenTask={openTask}
                agentId={id}
                harness={info?.kind_name}
                send={send}
                kiln={kiln}
                hideTabs
                className="hx-side hx-side-phone"
              />
            </AboutContext.Provider>
          )}
        </div>
      )}

      <Settings store={store} open={settings} onOpenChange={setSettings} send={send} />
      <ConfirmDialog
        open={confirmStop}
        onOpenChange={setConfirmStop}
        title="End this agent?"
        description={
          <>
            Its process stops on the machine. Its changes stay on disk; the conversation can be resumed later from its harness.
            {confirmStop && ending.note}
          </>
        }
        confirmLabel="End agent"
        danger
        onConfirm={stop}
      />
    </div>
  );
}

/** The head: who it is, how it's doing, how it runs, and what can be done to it. */
function Head({ store, cwd, send, onUsage, onSettings, onFind }: { store: SessionStore; cwd: string; send: (c: Command) => void; onUsage: () => void; onSettings: () => void; onFind: () => void }) {
  const navigate = useNavigate();
  const info = useSession(store, (s) => s.info);
  const state = useSession(store, (s) => s.state);
  const meta = useSession(store, (s) => s.meta);
  const conn = useSession(store, (s) => s.conn);
  const stopAt = useStopAt(store);
  const lastFrom = useSession(store, lastTurnFrom);
  const status = state?.status ?? info?.status ?? "starting";
  const actions = useAgentActions(
    {
      id: store.id,
      name: info?.name ?? "this agent",
      status,
      kind: info?.kind ?? "claude",
      kind_name: info?.kind_name ?? "its harness",
      target: info?.target ?? "local",
      cwd,
      harness_session: state?.harness_session ?? info?.harness_session,
      left_behind: state?.left_behind ?? info?.left_behind,
      // Its live state says where its commands run (absent: anywhere it needs).
      reach: state ? state.reach : info?.reach,
      keep_off: info?.keep_off,
    },
    { onGone: () => navigate("/agents") },
  );
  return (
    <>
      <AgentHeader
        info={info}
        state={state}
        meta={meta}
        status={status}
        conn={conn}
        cwd={cwd}
        actions={actions}
        send={send}
        onBackground={onUsage}
        onSettings={onSettings}
        onFind={onFind}
        stopAt={stopAt}
        onStop={() => store.stop()}
        onForceStop={() => store.stop(true)}
        lastTurnFrom={lastFrom}
      />
      {actions.dialogs}
    </>
  );
}

/**
 * The thread shown, when it's a subagent's: its whole path, from the agent at the top through each
 * subagent it's under to it (subagent-tree.ts `threadCrumbs`), and where it is in the tree. Its own
 * agent's subagents as its live state has them; the rest as listed. Not in the tree (not listed
 * yet, or no longer): this page's agent, then it.
 */
function useThreadPath(
  store: SessionStore,
  id: string,
  info: AgentInfo | undefined,
  sub: AgentInfo | null,
  task: string | null,
  name: string | undefined,
): { crumbs: Crumb[]; here?: SubagentNode } | null {
  const open = sub !== null || task !== null;
  const everyone = useRes(harnessSessionsRes).data;
  const done = useAnswered();
  // Read only while a thread is open: the main thread's page doesn't render again for its tasks.
  const tasks = useSession(store, (s) => (open ? (s.state?.tasks ?? NO_TASKS) : NO_TASKS));
  const asks = useSession(store, (s) => (open ? (s.state?.approvals ?? NO_ASKS) : NO_ASKS));
  return React.useMemo(() => {
    if (!open) return null;
    const live = (a: AgentInfo) => liveRoot(a, tasks, asks, done);
    const list = everyone.some((a) => a.id === id) ? everyone.map((a) => (a.id === id ? live(a) : a)) : info ? [...everyone, live(info)] : everyone;
    const key = sub ? sub.id : `${id}/${task ?? ""}`;
    const top = topOf(list, id);
    const nodes = top ? subagentTree(top, list) : [];
    const crumbs = top ? threadCrumbs(top, nodes, key) : null;
    return {
      crumbs: crumbs ?? [
        { key: id, name: info ? agentLabel(info) : "Main thread", href: `/agents/${id}${info?.subagent ? "?own=1" : ""}` },
        { key, name: sub ? agentLabel(sub) : (name ?? "Subagent"), href: "" },
      ],
      here: nodes.find((n) => n.key === key),
    };
  }, [open, everyone, id, info, sub, task, name, tasks, asks, done]);
}

/**
 * Over a subagent's thread: its whole path, earlier names giving way first, each earlier one where
 * its thread opens (the first its agent's main thread); then pause, branch and its permissions;
 * its page of its own (one kiln started); and back to the main thread.
 */
function ThreadCrumbs({ crumbs, sub, onClose }: { crumbs: readonly Crumb[]; sub: AgentInfo | null; onClose: () => void }) {
  const last = crumbs.length - 1;
  return (
    <div className="hx-crumbs">
      <nav className="hx-path" aria-label="Path">
        {crumbs.map((c, i) => (
          <React.Fragment key={c.key}>
            {i > 0 && <ChevronRight size={12} className="hx-path-sep" />}
            {i < last ? (
              <Link to={c.href} className="hx-crumb" title={c.name}>
                {c.name}
              </Link>
            ) : (
              <span className="hx-here" title={c.name} aria-current="page">
                {c.name}
              </span>
            )}
          </React.Fragment>
        ))}
      </nav>
      <button type="button" className="hx-icon" aria-label="Pause" disabled>
        <Pause size={14} />
      </button>
      <button type="button" className="hx-icon" aria-label="Branch" disabled>
        <GitFork size={14} />
      </button>
      <button type="button" className="hx-icon" aria-label="Permissions" disabled>
        <Shield size={14} />
      </button>
      {/* What's only on its page of its own (its changes, its settings). */}
      {sub && (
        <Link to={`/agents/${sub.id}?own=1`} className="hx-link">
          Its page
        </Link>
      )}
      <button type="button" className="hx-icon" aria-label="Back to the main thread" onClick={onClose}>
        <X size={13} />
      </button>
    </div>
  );
}

/**
 * A subagent kiln started for one of the agent's goals or plan steps (a session of its own, on the
 * agent's machine with its settings), in the agent's chat as its harness's own subagents are: its
 * thread where the conversation is, what it asks you answered there, and the composer writing to
 * it. What's only on its page of its own (its settings, its own subagents' threads) opens there.
 */
function KilnThread({
  id,
  onTerminal,
  find,
  turn,
}: {
  id: string;
  onTerminal: (o: { what: "shell" | "screen"; command?: string }) => void;
  find?: { q: string; now: FindNow | null };
  /** The agent's pause button's state, on the composer's border. */
  turn: BorderState;
}) {
  const store = sessionStore(id);
  useSessionReady(store);
  const navigate = useNavigate();
  const info = useSession(store, (s) => s.info);
  const cwd = useSession(store, (s) => s.state?.cwd || s.info?.cwd || "");
  const accepts = useSession(store, (s) => acceptsOf(s.meta));
  const attachments = useAttachments(accepts);
  const [reply, setReply] = React.useState<ReplyTarget | null>(null);
  const send = React.useCallback(
    (c: Command) => {
      if (c.type === "approve") markAnswered([c.id]);
      if (store.send(c)) return;
      if (c.type === "approve") unmark([c.id]);
      toast.error("Not connected", "The subagent's machine is reconnecting; try again in a moment.");
    },
    [store],
  );
  const ownPage = React.useCallback((task?: string) => navigate(`/agents/${id}?own=1${task ? `&task=${encodeURIComponent(task)}` : ""}`), [navigate, id]);
  const end = () => void harnessApi.end(id, true).catch((e: unknown) => toast.error("Couldn't end it", errorMessage(e)));
  return (
    <>
      <Timeline
        store={store}
        agent={id}
        cwd={cwd}
        onOpenTask={ownPage}
        onCommand={send}
        onReply={setReply}
        agentLabel="Subagent"
        agentName={info?.name}
        onDecide={send}
        target={info?.target}
        kind={info?.kind}
        find={find}
      />
      <Dock
        store={store}
        id={id}
        attachments={attachments}
        toMain={false}
        task={null}
        turn={turn}
        onView={() => ownPage()}
        onTerminal={onTerminal}
        onOpenTask={ownPage}
        onRemove={end}
        send={send}
        reply={reply}
        onClearReply={() => setReply(null)}
      />
    </>
  );
}

/** Under the conversation: what runs besides it, how it ended (when it did), and the composer. */
const NO_TASKS: Task[] = [];
const NO_ASKS: Approval[] = [];

/** A message sent to one picked subagent: its row, its id, the session it went to, and how (over the agent's socket, or over HTTP and how that call came out). */
interface SentTo {
  key: string;
  id: string;
  session: string;
  how: "socket" | "sending" | "sent" | "failed";
}

/**
 * An agent's subagents as a tree for the line by its composer (subagent-tree.ts: its own harness's
 * from its live state, kiln's and theirs from the list, less what you answered here), those picked
 * there, and sending the next message to each picked, its own way: one of its own harness's with
 * its task on this socket (or over HTTP when that's down); any other to its session over HTTP. A
 * subagent at work is steered (it reads it at its next step), one that isn't gets it queued. Each
 * one's mark: delivered once its delivery says it started or it's in its thread, waiting while it's
 * queued (or its session paused), not sent when that failed. Nothing goes to the agent itself.
 */
function useSubagentPicks(store: SessionStore, id: string, info: AgentInfo | undefined, task: string | null) {
  const everyone = useRes(harnessSessionsRes).data;
  const done = useAnswered();
  const tasks = useSession(store, (s) => s.state?.tasks ?? NO_TASKS);
  const asks = useSession(store, (s) => s.state?.approvals ?? NO_ASKS);
  const delivery = useSession(store, (s) => s.state?.delivery);
  const events = useSession(store, (s) => s.events);
  const list = React.useMemo(() => everyone.map((a) => asAnswered(a, done)), [everyone, done]);
  const rows = React.useMemo(() => {
    const base = list.find((a) => a.id === id) ?? info;
    return base ? subagentRows(liveRoot(base, tasks, asks, done), list) : [];
  }, [list, id, info, tasks, asks, done]);
  const [picks, setPicks] = React.useState<ReadonlySet<string>>(() => new Set());
  const [sent, setSent] = React.useState<readonly SentTo[]>([]);
  const pick = React.useCallback(
    (keys: string[], on: boolean) =>
      setPicks((p) => {
        const next = new Set(p);
        for (const k of keys) {
          if (on) next.add(k);
          else next.delete(k);
        }
        return next;
      }),
    [],
  );
  // Over a subagent's thread the composer writes to that one, whatever's picked.
  const to = React.useMemo(() => (task === null ? rows.filter((r) => picks.has(r.key) && takesMessage(r)) : []), [rows, picks, task]);
  const sendTo = (m: { text: string; attachments?: Attachment[] }) => {
    const base = clientId();
    const out = to.map((r, i): SentTo => {
      const msg = `${base}-${i}`;
      const t = r.kind === "harness" && r.session === id ? tasks.find((x) => x.id === r.task) : undefined;
      const mode: SendMode = r.stand === "working" && (!t || (t.takes ?? []).includes("steer")) ? "steer" : "queue";
      const c: Command = { type: "send", id: msg, text: m.text, mode, attachments: m.attachments, ...(r.kind === "harness" && r.task ? { task: r.task } : {}) };
      if (r.session === id && store.send(c)) return { key: r.key, id: msg, session: r.session, how: "socket" };
      const went = (how: SentTo["how"]) => setSent((s) => s.map((x) => (x.id === msg ? { ...x, how } : x)));
      harnessApi.command(r.session, c).then(
        () => went("sent"),
        () => went("failed"),
      );
      return { key: r.key, id: msg, session: r.session, how: "sending" };
    });
    setSent(out);
  };
  const toMarks = React.useMemo(() => {
    const marks = new Map<string, ToMark>();
    for (const x of sent) {
      let mark: ToMark = "wait";
      if (x.how === "failed") mark = "fail";
      else if (x.how === "socket") {
        const d = delivery?.[x.id];
        if (d === "cancelled") mark = "fail";
        else if (d === "started" || d === "completed") mark = "ok";
        else if (!tasks.some((t) => t.queue?.some((q) => q.id === x.id)) && events.some((e) => e.kind === "user" && e.client_id === x.id)) mark = "ok";
      } else if (x.how === "sent") mark = rows.find((r) => r.key === x.key)?.stand === "paused" ? "wait" : "ok";
      marks.set(x.key, mark);
    }
    return marks;
  }, [sent, delivery, tasks, events, rows]);
  const clearTo = () => {
    setPicks(new Set());
    setSent([]);
  };
  return { rows, picks, pick, to, toMarks, sendTo, clearTo };
}

function Dock({
  store,
  id,
  attachments,
  toMain,
  task,
  kiln,
  turn,
  onView,
  onTerminal,
  onOpenTask,
  onRemove,
  send,
  reply,
  onClearReply,
}: {
  store: SessionStore;
  id: string;
  attachments: Attachments;
  toMain: boolean;
  /** The subagent whose thread is shown: the composer writes to it, as it takes messages. */
  task: string | null;
  /** The subagents kiln started for its goals or plan steps, beside its harness's own. */
  kiln?: readonly AgentInfo[];
  /** The agent's pause button's state, on the composer's border. */
  turn: BorderState;
  onView: (view: string) => void;
  onTerminal: (o: { what: "shell" | "screen"; command?: string }) => void;
  onOpenTask: (toolId: string) => void;
  onRemove: () => void;
  send: (c: Command) => void;
  /** What the next message answers (a message's ⋯ → Reply to). */
  reply?: ReplyTarget | null;
  onClearReply?: () => void;
}) {
  const navigate = useNavigate();
  const status = useSession(store, (s) => s.state?.status ?? s.info?.status ?? "starting");
  const exit = useSession(store, (s) => s.state?.exit);
  const empty = useSession(store, (s) => s.events.length === 0);
  const resume = useSession(store, (s) => (s.meta?.features ?? []).includes("resume"));
  const info = useSession(store, (s) => s.info);
  const { rows, picks, pick, to, toMarks, sendTo, clearTo } = useSubagentPicks(store, id, info ?? undefined, task);
  const harnessSession = useSession(store, (s) => s.state?.harness_session ?? s.info?.harness_session);
  const [moving, setMoving] = React.useState(false);
  return (
    <div className="hx-dock" onPointerDown={keepTyping} onMouseDown={keepTyping}>
      <ActivityLine store={store} onOpenTask={onOpenTask} kiln={kiln} inThread={task} rows={rows} picks={picks} onPick={pick} />
      {status === "exited" && exit?.error && empty ? (
        <div className="hx-startfail" role="alert">
          <b>This agent couldn't start.</b> {exit.error}
          <div className="mt-2 flex flex-wrap gap-2">
            {info && movable({ target: info.target, status, harness_session: harnessSession }) && (
              <Button size="sm" variant="secondary" onClick={() => setMoving(true)}>
                Move to…
              </Button>
            )}
            <Button size="sm" variant="secondary" onClick={() => navigate("/agents?new=1")}>
              Start a new one
            </Button>
            <Button size="sm" variant="ghost" onClick={onRemove}>
              Remove this one
            </Button>
          </div>
          {moving && info && <MoveDialog agent={{ ...info, status }} onClose={() => setMoving(false)} />}
        </div>
      ) : status === "exited" ? (
        <div className="hx-exited">
          <span className="hx-dim">
            {exit?.again_at != null
              ? `Its process ended${exit.code != null ? ` (exit ${exit.code})` : ""}. It starts again by itself, and its conversation goes on.`
              : `${exit?.error ?? `The agent stopped${exit?.code != null ? ` (exit ${exit.code})` : ""}.`} Sending a message picks up where it left off.`}
          </span>
          {resume && (
            <Button size="sm" variant="secondary" onClick={() => send({ type: "restart" })}>
              <RotateCcw /> Restart
            </Button>
          )}
        </div>
      ) : null}
      <PausedLine store={store} send={send} onLogin={() => onView("login")} />
      <AskedLine store={store} />
      <Composer
        store={store}
        agent={id}
        attachments={attachments}
        toMain={toMain}
        task={task}
        turn={turn}
        onView={onView}
        onTerminal={onTerminal}
        replyTo={reply}
        onClearReply={onClearReply}
        to={to}
        toMarks={toMarks}
        onSendTo={sendTo}
        onClearTo={clearTo}
      />
    </div>
  );
}

/** While you type, pressing one of the dock's buttons leaves the message box focused. On a phone
 *  the keyboard stays up and nothing moves under your finger: had the box lost focus (iOS doesn't
 *  focus a button it taps), the agent's head would come back at once, moving the buttons before the
 *  tap landed on them. */
function keepTyping(e: React.PointerEvent | React.MouseEvent) {
  const t = e.target as Element;
  if (document.activeElement?.classList.contains("hx-compose-input") && t.closest("button") && !t.closest("input, textarea, select")) e.preventDefault();
}

/** A phone's tabs, with what each holds. */
function PhoneTabs({ store, tab, onTab }: { store: SessionStore; tab: PhoneTab; onTab: (t: PhoneTab) => void }) {
  const asked = useSession(store, (s) => s.state?.approvals);
  const done = useAnswered();
  const approvals = asked ? stillAsked(asked, done).length : 0;
  const changes = useSession(store, (s): FileChange[] | null => s.changes);
  // Counted from the conversation's edits only when the server sends no changeset.
  const events = useSession(store, (s) => (s.changes ? NO_EVENTS : s.events));
  const edits = useSameList(React.useMemo(() => editsOf(events), [events]));
  const fileCount = React.useMemo(() => (changes ? changes.length : changesetOf(edits).length), [changes, edits]);
  const todos = useSession(store, (s) => s.state?.todos ?? NO_TODOS);
  const todosDone = todos.filter((t) => t.status === "completed").length;
  // A plan waiting on your review: its tab says so (review.tsx).
  const reviewing = useReviewWaits(store);
  const tabs: { id: PhoneTab; label: string; n?: string; warn?: boolean; waits?: boolean }[] = [
    { id: "work", label: "Work", n: approvals ? String(approvals) : undefined, warn: approvals > 0 },
    { id: "changes", label: "Changes", n: fileCount ? String(fileCount) : undefined },
    { id: "plan", label: "Plan", n: todos.length ? `${todosDone}/${todos.length}` : undefined, waits: reviewing !== null },
    { id: "tools", label: "Tools" },
  ];
  return (
    <div className="pt" role="tablist" aria-label="Agent">
      {tabs.map((t) => (
        <button key={t.id} type="button" role="tab" aria-selected={tab === t.id} onClick={() => onTab(t.id)}>
          {t.label}
          {t.n && (
            <span className="pt-n" data-tone={t.warn ? "warn" : undefined}>
              {t.n}
            </span>
          )}
          {t.waits && <i className="pr-tab-dot" role="img" aria-label="a plan waits on your review" />}
        </button>
      ))}
    </div>
  );
}

function Settings({ store, open, onOpenChange, send }: { store: SessionStore; open: boolean; onOpenChange: (o: boolean) => void; send: (c: Command) => void }) {
  const meta = useSession(store, (s) => s.meta);
  const state = useSession(store, (s) => s.state);
  const info = useSession(store, (s) => s.info);
  return <SettingsSheet open={open} onOpenChange={onOpenChange} agent={info} meta={meta} state={state} send={send} />;
}

/**
 * The page while its conversation's first snapshot comes: its frame (the head's lines, the phone's
 * tabs or the side, the composer's place) as the page will have it, and the loader where the
 * conversation goes, so nothing moves when it's in.
 */
function AgentLoading({ id }: { id: string }) {
  const wide = useMediaQuery("(min-width: 1100px)");
  const [trayCollapsed, setTrayCollapsed] = useLocalStorage(INSPECTOR_COLLAPSED, false);
  const draft = useDrafts((s) => s.drafts[id] ?? "");
  const sizeDraft = React.useCallback(
    (el: HTMLTextAreaElement | null) => {
      void draft;
      if (!el) return;
      el.style.height = "auto";
      el.style.height = `${el.scrollHeight}px`;
    },
    [draft],
  );
  const work = (
    <div className="hx-main">
      <div className="hx-timeline ag-loading" role="status">
        <LoadingMark />
        <PrismLabel live>Loading conversation</PrismLabel>
      </div>
      <div className="hx-dock">
        <div className="hx-composer cp">
          <div className="hx-compose-box">
            <div className="cp-field">
              <div className="hx-compose-input cp-mirror" aria-hidden="true">
                {draft}
                {"​"}
              </div>
              <textarea ref={sizeDraft} className="hx-compose-input cp-input" rows={1} value={draft} placeholder="Loading conversation…" disabled aria-label="Message to the agent" />
            </div>
            <div className="cp-bar">
              <span className="cp-gap" />
              <button type="button" className="cp-send" data-mode="send" disabled aria-label="Send">
                <span className="cp-label">Send</span>
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
  return (
    <div className="hx-agent" aria-busy="true">
      <title>Agent · kiln</title>
      <header className="ag-head">
        <p className="ag-eyebrow">
          <Link to="/agents" className="ag-back">
            <ArrowLeft size={12} aria-hidden="true" /> Agents
          </Link>
        </p>
        <div className="ag-top">
          <div className="ag-id">
            <h1 className="ag-title hx-ghost">Agent</h1>
          </div>
          <span className="ag-actions hx-ghost" aria-hidden="true">
            <span className="fig-round" />
          </span>
        </div>
        <div className="ag-clock-row" aria-hidden="true" />
        <div className="ag-run rs-tuner" aria-hidden="true">
          <div className="rs-top">
            <p className="rs-line" />
          </div>
        </div>
      </header>
      {wide ? (
        <DesktopLayout
          collapsed={trayCollapsed}
          onCollapsed={setTrayCollapsed}
          side={(actions) => (
            <div className="hx-inspector hx-side">
              <div className="hx-inspector-head">
                <div className="hx-tabs" aria-hidden="true">
                  {["Changes", "Plan", "Tools"].map((t) => (
                    <span key={t} className="hx-tab">
                      {t}
                    </span>
                  ))}
                </div>
                {actions}
              </div>
            </div>
          )}
        >
          {work}
        </DesktopLayout>
      ) : (
        <div className="hx-agent-phone">
          <div className="pt" aria-hidden="true">
            {["Work", "Changes", "Plan", "Tools"].map((t, i) => (
              <button key={t} type="button" tabIndex={-1} aria-selected={i === 0} disabled>
                {t}
              </button>
            ))}
          </div>
          {work}
        </div>
      )}
    </div>
  );
}
