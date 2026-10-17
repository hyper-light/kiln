/**
 * One agent: the conversation (what you asked, what it did, what it answered), what's waiting on
 * you, and the composer — with, beside it, what it changed, its plan, and what it works with. On a
 * phone those become four tabs.
 */
import * as React from "react";
import "./harness.css";
import { Link, useNavigate, useParams } from "react-router";
import { ChevronRight, RotateCcw, X } from "lucide-react";
import { harnessApi, useAgentSession, type Command } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { toast } from "@/components/ui/toast";
import { errorMessage, useDocumentTitle, useMediaQuery } from "@/lib/hooks";
import { AccountCard } from "./account-card";
import { Composer } from "./composer";
import { useAgentActions } from "./actions";
import { SettingsSheet, useAttachments } from "./controls";
import { changesetOf } from "./derive";
import { FindBar, findIn, type FindNow } from "./find";
import { AgentHeader } from "./head";
import { Inspector, type InspectorTab } from "./inspector";
import { useMarkSeen } from "./seen";
import { ActivityLine } from "./background";
import { Timeline } from "./timeline";
import { BesideTerminals, type BesideRequest } from "./beside";
import { PlanSourceContext } from "@/views/plans/save";

type PhoneTab = "work" | InspectorTab;

export default function AgentPage() {
  const { id = "" } = useParams();
  const navigate = useNavigate();
  const [snap, store] = useAgentSession(id);
  const { info, meta, state, events, conn, changes } = snap;
  useMarkSeen(id);
  const wide = useMediaQuery("(min-width: 1100px)");
  const [tab, setTab] = React.useState<InspectorTab>("changes");
  const [phoneTab, setPhoneTab] = React.useState<PhoneTab>("work");
  const [task, setTask] = React.useState<string | null>(null);
  const [confirmStop, setConfirmStop] = React.useState(false);
  const [settings, setSettings] = React.useState(false);
  /** /login or /logout: the harness's account on its machine, opened in place over the composer. */
  const [account, setAccount] = React.useState<"login" | "logout" | null>(null);
  /** A terminal beside it (its shell, its harness's own screen), asked for from the composer. */
  const [beside, setBeside] = React.useState<BesideRequest | null>(null);
  /** What the terminal hands to the composer. */
  const [handed, setHanded] = React.useState<{ text: string; seq: number } | null>(null);
  const openBeside = React.useCallback((o: { what: "shell" | "screen"; command?: string }) => {
    setBeside({ ...o, seq: Date.now() });
    setPhoneTab("work");
  }, []);
  const hand = React.useCallback((text: string) => setHanded({ text, seq: Date.now() }), []);
  const taken = React.useCallback(() => setBeside(null), []);
  // /login and /logout open the account in the conversation, below what was said: brought into view once.
  const showAccount = React.useCallback((el: HTMLDivElement | null) => el?.scrollIntoView({ block: "nearest", behavior: "smooth" }), []);
  // Find in the conversation (Ctrl/⌘F, or the head's button): the words, and the match shown.
  const [findOpen, setFindOpen] = React.useState(false);
  const [findQ, setFindQ] = React.useState("");
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
  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey || e.key.toLowerCase() !== "f") return;
      // Asked again from the find bar: the browser's own find.
      if (document.activeElement === findInput.current) return;
      e.preventDefault();
      openFind();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [openFind]);
  const attachments = useAttachments((meta?.features ?? []).includes("attachments") ? "any" : (meta?.features ?? []).includes("images") ? "images" : "none");
  const [dragging, setDragging] = React.useState(false);
  const addFiles = attachments.add;
  // Screenshots pasted anywhere on the page attach to the next message (text pastes are left alone).
  React.useEffect(() => {
    const onPaste = (e: ClipboardEvent) => {
      const files = e.clipboardData?.files;
      if (!files?.length || !attachments.enabled) return;
      const t = e.target as HTMLElement | null;
      if (t?.closest("input:not(.hx-compose-input), textarea:not(.hx-compose-input)")) return;
      e.preventDefault();
      void addFiles(files);
    };
    document.addEventListener("paste", onPaste);
    return () => document.removeEventListener("paste", onPaste);
  }, [addFiles, attachments.enabled]);
  useDocumentTitle(info ? `${info.name} · Agents` : "Agent");

  const cwd = state?.cwd || info?.cwd || "";
  const actions = useAgentActions(
    {
      id,
      name: info?.name ?? "this agent",
      status: state?.status ?? info?.status ?? "starting",
      kind: info?.kind ?? "claude",
      target: info?.target ?? "local",
      cwd,
      harness_session: state?.harness_session ?? info?.harness_session,
      left_behind: state?.left_behind ?? info?.left_behind,
    },
    { onGone: () => navigate("/agents") },
  );
  const status = state?.status ?? info?.status ?? "starting";
  const features = meta?.features ?? [];
  const approvals = state?.approvals ?? [];
  const send = (c: Command) => {
    if (!store.send(c)) toast.error("Not connected", "The agent's machine is reconnecting; try again in a moment.");
  };
  const openTask = (toolId: string) => {
    setTask(toolId);
    setPhoneTab("work");
  };
  const taskInfo = task ? state?.tasks.find((t) => t.tool_id === task) : null;

  // The matches, in the thread shown; a new search starts at the newest, and the one shown stays
  // shown as the conversation grows.
  const q = React.useDeferredValue(findQ);
  const matches = React.useMemo(() => (findOpen ? findIn(events, task ?? undefined, q) : []), [findOpen, events, task, q]);
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
    document.querySelector<HTMLTextAreaElement>(".hx-compose-input")?.focus();
  };
  const showTools = () => {
    setTab("tools");
    setPhoneTab("tools");
  };
  const showPlan = () => {
    setTab("plan");
    setPhoneTab("plan");
  };
  /** A slash command that opens one of kiln's panels. */
  /** /model, /effort, /permissions: that setting of the head's run settings, opened in place. */
  const openSetting = (label: "Model" | "Effort" | "Permissions") => {
    const b = document.querySelector<HTMLButtonElement>(`.ag-head button.rs-v[aria-label^="${label}:"]`);
    if (!b) return;
    if (b.getAttribute("aria-expanded") !== "true") b.click();
    b.focus({ preventScroll: true });
    b.scrollIntoView({ block: "nearest" });
  };
  const openView = (view: string) => {
    if (view === "settings") setSettings(true);
    else if (view === "login" || view === "logout") setAccount(view);
    else if (view === "model" || view === "effort" || view === "permissions") openSetting(view === "model" ? "Model" : view === "effort" ? "Effort" : "Permissions");
    else if (view === "terminal") openBeside({ what: "shell" });
    else if (view === "screen") openBeside({ what: "screen" });
    else if (view === "tasks" || view === "goals") showPlan();
    else if (view === "changes") {
      setTab("changes");
      setPhoneTab("changes");
    } else if (view === "new_agent") navigate("/agents?new=1");
    else showTools();
  };

  const stop = async () => {
    try {
      await harnessApi.stop(id);
      navigate("/agents");
    } catch (e) {
      toast.error("Couldn't stop the agent", errorMessage(e));
    }
  };

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
          partial={state?.dropped ? events.length : undefined}
          inputRef={findInput}
        />
      )}
      {task && (
        <div className="hx-crumbs">
          <button type="button" className="hx-link" onClick={() => setTask(null)}>
            Main thread
          </button>
          <ChevronRight size={12} />
          <span className="hx-trunc">{taskInfo?.description ?? "Subagent"}</span>
          <button type="button" className="hx-icon ml-auto" aria-label="Back to the main thread" onClick={() => setTask(null)}>
            <X size={13} />
          </button>
        </div>
      )}
      <PlanSourceContext.Provider value={info ? { session: info.id, harness: info.kind, machine: info.target === "local" ? null : info.target.replace(/^agent:/, ""), cwd, agent: info.name } : null}>
        <Timeline
        events={events}
        state={state}
        meta={meta}
        cwd={cwd}
        parent={task ?? undefined}
        onOpenTask={openTask}
        onCommand={send}
        agentLabel={task ? "Subagent" : info?.kind_name}
        agentName={info?.name}
        onDecide={send}
        target={info?.target}
        kind={info?.kind}
        accountOpen={account !== null}
        find={findOpen ? { q, now: findAt } : undefined}
        footer={
          <>
            {account && info && (
              <div ref={showAccount} className="hx-signin">
                <AccountCard key={account} target={info.target} kind={info.kind} intent={account} onClose={() => setAccount(null)} />
              </div>
            )}
            {!task && <BesideTerminals agent={id} info={info ?? null} request={beside} onTaken={taken} onInsert={hand} />}
          </>
        }
      />
      </PlanSourceContext.Provider>
      <div className="hx-dock">
        <ActivityLine state={state} agent={id} onOpenTask={openTask} />
        {status === "exited" && state?.exit?.error && events.length === 0 ? (
          <div className="hx-startfail" role="alert">
            <b>This agent couldn't start.</b> {state.exit.error}
            <div className="mt-2 flex flex-wrap gap-2">
              <Button size="sm" variant="secondary" onClick={() => navigate("/agents?new=1")}>
                Start a new one
              </Button>
              <Button size="sm" variant="ghost" onClick={() => setConfirmStop(true)}>
                Remove this one
              </Button>
            </div>
          </div>
        ) : status === "exited" ? (
          <div className="hx-exited">
            <span className="hx-dim">{state?.exit?.error ?? `The agent stopped${state?.exit?.code != null ? ` (exit ${state.exit.code})` : ""}.`} Sending a message picks up where it left off.</span>
            {features.includes("resume") && (
              <Button size="sm" variant="secondary" onClick={() => send({ type: "restart" })}>
                <RotateCcw /> Restart
              </Button>
            )}
          </div>
        ) : null}
        <Composer store={store} state={state} meta={meta} disabled={conn !== "open"} attachments={attachments} toMain={task !== null} onView={openView} onTerminal={openBeside} insert={handed} />
      </div>
    </div>
  );

  const fileCount = changes ? changes.length : changesetOf(events).length;
  const todos = state?.todos ?? [];
  const todosDone = todos.filter((t) => t.status === "completed").length;
  const phoneTabs: { id: PhoneTab; label: string; n?: string; warn?: boolean }[] = [
    { id: "work", label: "Work", n: approvals.length ? String(approvals.length) : undefined, warn: approvals.length > 0 },
    { id: "changes", label: "Changes", n: fileCount ? String(fileCount) : undefined },
    { id: "plan", label: "Plan", n: todos.length ? `${todosDone}/${todos.length}` : undefined },
    { id: "tools", label: "Tools" },
  ];

  const hasFiles = (e: React.DragEvent) => [...e.dataTransfer.types].includes("Files");
  return (
    <div
      className="hx-agent"
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
      {dragging && (
        <div className="hx-dropzone" aria-hidden="true">
          <span>{attachments.enabled ? "Drop screenshots or files to attach" : "This agent can't take attachments"}</span>
        </div>
      )}
      <AgentHeader
        info={info}
        state={state}
        meta={meta}
        status={status}
        conn={conn}
        cwd={cwd}
        actions={actions}
        send={send}
        onUsage={showTools}
        onBackground={showPlan}
        onSettings={() => setSettings(true)}
        onFind={openFind}
      />

      {conn === "gone" ? (
        <div className="hx-empty mt-16">
          This agent has ended.{" "}
          <Link to="/agents" className="text-link">
            Back to all agents
          </Link>
        </div>
      ) : wide ? (
        <div className="hx-agent-body">
          {work}
          <Inspector tab={tab} onTab={setTab} events={events} changes={changes} state={state} meta={meta} store={store} cwd={cwd} onOpenTask={openTask} agentId={id} harness={info?.kind_name} send={send} className="hx-side" />
        </div>
      ) : (
        <div className="hx-agent-phone">
          <div className="pt" role="tablist" aria-label="Agent">
            {phoneTabs.map((t) => (
              <button key={t.id} type="button" role="tab" aria-selected={phoneTab === t.id} onClick={() => setPhoneTab(t.id)}>
                {t.label}
                {t.n && (
                  <span className="pt-n" data-tone={t.warn ? "warn" : undefined}>
                    {t.n}
                  </span>
                )}
              </button>
            ))}
          </div>
          {phoneTab === "work" ? (
            work
          ) : (
            <Inspector
              tab={phoneTab}
              onTab={(t) => setPhoneTab(t)}
              events={events}
              changes={changes}
              state={state}
              meta={meta}
              store={store}
              cwd={cwd}
              onOpenTask={openTask}
              agentId={id}
              harness={info?.kind_name}
              send={send}
              hideTabs
              className="hx-side hx-side-phone"
            />
          )}
        </div>
      )}

      <SettingsSheet open={settings} onOpenChange={setSettings} meta={meta} state={state} send={send} />
      {actions.dialogs}
      <ConfirmDialog
        open={confirmStop}
        onOpenChange={setConfirmStop}
        title="End this agent?"
        description="Its process stops on the machine. Its changes stay on disk; the conversation can be resumed later from its harness."
        confirmLabel="End agent"
        danger
        onConfirm={stop}
      />
    </div>
  );
}
