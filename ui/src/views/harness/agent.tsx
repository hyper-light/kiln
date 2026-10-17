/**
import "./harness.css";
 * One agent, in full: its work as a timeline, what it's waiting on you for, the composer, and
 * beside it the changeset, plan, commands and subagents. On a phone the side panels become tabs.
 */
import * as React from "react";
import { Link, useNavigate, useParams } from "react-router";
import { ArrowLeft, ChevronRight, Power, RotateCcw, SquareTerminal, X } from "lucide-react";
import { harnessApi, useAgentSession, type Command } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { ConfirmDialog } from "@/components/ui/dialog";
import { toast } from "@/components/ui/toast";
import { errorMessage, useDocumentTitle, useMediaQuery } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { ApprovalCard } from "./approval";
import { Composer } from "./composer";
import { money, tildify } from "./derive";
import { Inspector, type InspectorTab } from "./inspector";
import { HarnessMark, StatusPill } from "./parts";
import { Timeline } from "./timeline";

type PhoneTab = "work" | InspectorTab;

function Picker({
  label,
  value,
  options,
  onPick,
}: {
  label: string;
  value: string;
  options: { id: string; label: string; dangerous?: boolean; description?: string }[];
  onPick: (id: string, dangerous: boolean) => void;
}) {
  if (options.length === 0) return null;
  return (
    <label className="hx-picker">
      <span className="hx-eyebrow">{label}</span>
      <select
        value={value}
        onChange={(e) => {
          const o = options.find((x) => x.id === e.target.value);
          if (o) onPick(o.id, Boolean(o.dangerous));
        }}
      >
        {!options.some((o) => o.id === value) && value && <option value={value}>{value}</option>}
        {options.map((o) => (
          <option key={o.id} value={o.id} title={o.description}>
            {o.label}
            {o.dangerous ? " (no checks)" : ""}
          </option>
        ))}
      </select>
    </label>
  );
}

export default function AgentPage() {
  const { id = "" } = useParams();
  const navigate = useNavigate();
  const [snap, store] = useAgentSession(id);
  const { info, meta, state, events, conn, changes } = snap;
  const wide = useMediaQuery("(min-width: 1100px)");
  const [tab, setTab] = React.useState<InspectorTab>("changes");
  const [phoneTab, setPhoneTab] = React.useState<PhoneTab>("work");
  const [task, setTask] = React.useState<string | null>(null);
  const [confirmStop, setConfirmStop] = React.useState(false);
  const [risky, setRisky] = React.useState<string | null>(null);
  useDocumentTitle(info ? `${info.name} · Agents` : "Agent");

  const cwd = state?.cwd || info?.cwd || "";
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
      <Timeline events={events} state={state} cwd={cwd} parent={task ?? undefined} onOpenTask={openTask} />
      <div className="hx-dock">
        {approvals.length > 0 && (
          <div className="hx-dock-approvals">
            {approvals.length > 1 && <div className="hx-eyebrow px-1">{approvals.length} waiting on you</div>}
            {approvals.slice(0, 1).map((a) => (
              <ApprovalCard key={a.id} a={a} cwd={cwd} onDecide={send} />
            ))}
          </div>
        )}
        {status === "exited" ? (
          <div className="hx-exited">
            <span className="hx-dim">{state?.exit?.error ?? `The agent stopped${state?.exit?.code != null ? ` (exit ${state.exit.code})` : ""}.`} Sending a message picks up where it left off.</span>
            {features.includes("resume") && (
              <Button size="sm" variant="secondary" onClick={() => send({ type: "restart" })}>
                <RotateCcw /> Restart
              </Button>
            )}
          </div>
        ) : null}
        <Composer store={store} state={state} meta={meta} disabled={conn !== "open"} />
      </div>
    </div>
  );

  return (
    <div className="hx-agent">
      <header className="hx-agent-head">
        <Link to="/agents" className="hx-back" aria-label="All agents">
          <ArrowLeft size={15} />
        </Link>
        {info && <HarnessMark kind={info.kind} name={info.kind_name} size={26} />}
        <div className="hx-agent-id">
          <div className="hx-agent-name">
            <span className="hx-trunc">{info?.name ?? "Agent"}</span>
            <StatusPill status={status} activity={state?.activity ?? info?.activity} />
          </div>
          <div className="hx-agent-where">
            <span>{info?.target === "local" ? "server" : (info?.target ?? "").replace(/^agent:/, "")}</span>
            <span className="hx-trunc">{tildify(cwd)}</span>
            {state?.usage && <span>{money(state.usage.cost_usd)}</span>}
            {conn === "reconnecting" && <span className="hx-warn">reconnecting…</span>}
          </div>
        </div>
        <div className="hx-agent-actions">
          {features.includes("set_mode") && (
            <Picker
              label="Permissions"
              value={state?.mode ?? ""}
              options={meta?.modes ?? []}
              onPick={(m, dangerous) => (dangerous ? setRisky(m) : send({ type: "set_mode", mode: m }))}
            />
          )}
          {features.includes("set_model") && <Picker label="Model" value={state?.model ?? ""} options={meta?.models ?? []} onPick={(m) => send({ type: "set_model", model: m })} />}
          {info?.shell && (
            <Button size="sm" variant="ghost" onClick={() => navigate(`/terminal?session=${encodeURIComponent(info.shell ?? "")}`)} aria-label="Open its shell">
              <SquareTerminal />
              <span className="hidden lg:inline">Shell</span>
            </Button>
          )}
          <Button size="sm" variant="ghost" onClick={() => setConfirmStop(true)} aria-label="Stop the agent">
            <Power />
            <span className="hidden lg:inline">End</span>
          </Button>
        </div>
      </header>

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
          <Inspector tab={tab} onTab={setTab} events={events} changes={changes} state={state} meta={meta} store={store} cwd={cwd} onOpenTask={openTask} className="hx-side" />
        </div>
      ) : (
        <div className="hx-agent-phone">
          <div className="hx-tabs hx-phone-tabs" role="tablist">
            {(["work", "changes", "plan", "runs", "agents"] as PhoneTab[]).map((t) => (
              <button key={t} type="button" role="tab" aria-selected={phoneTab === t} className="hx-tab" onClick={() => setPhoneTab(t)}>
                {t === "work" ? "Work" : t === "changes" ? "Changes" : t === "plan" ? "Plan" : t === "runs" ? "Runs" : "Subagents"}
                {t === "work" && approvals.length > 0 && <span className="hx-tab-count hx-tab-alert">{approvals.length}</span>}
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
              className={cn("hx-side hx-side-phone")}
            />
          )}
        </div>
      )}

      <ConfirmDialog
        open={confirmStop}
        onOpenChange={setConfirmStop}
        title="End this agent?"
        description="Its process stops on the machine. Its changes stay on disk; the conversation can be resumed later from its harness."
        confirmLabel="End agent"
        danger
        onConfirm={stop}
      />
      <ConfirmDialog
        open={risky !== null}
        onOpenChange={(o) => !o && setRisky(null)}
        title="Turn off permission checks?"
        description="In this mode the agent runs commands and changes files without asking you. Only use it in a sandbox you can throw away."
        confirmLabel="Turn off checks"
        danger
        onConfirm={() => {
          if (risky) send({ type: "set_mode", mode: risky });
          setRisky(null);
        }}
      />
    </div>
  );
}
