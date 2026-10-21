/**
 * An agent's work as turns: what you asked, the steps it took (grouped: "Read 6 files",
 * "Edited 3 files +42 −7", "Ran cargo test — 2 failed"), its notes along the way, and its answer.
 * Every step opens into a view made for its kind: real diffs, command output with exit codes,
 * the files it read, the subagent it started.
 */
import * as React from "react";
import { ArrowDown, ArrowUpRight, Check, ChevronRight, CircleAlert, Copy, CornerDownRight, Info, TriangleAlert } from "lucide-react";
import { useSession, type Approval, type Command, type Delivery, type HEvent, type Meta, type SessionStore, type Task } from "@/api/harness";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { AccountCard, saysSignedOut } from "./account-card";
import { changesetOf, diffStats, duration, relPath, shareTurns, stepStatus, stepSummary, taskDot, taskDuration, taskLive, taskOfCall, turnsFrom, type Item, type Step, type ToolEvent, type Turn, type TurnsMemo } from "./derive";
import { ApprovalCard, type Decide } from "./approval";
import { DiffStat, FileDiffView } from "./diff";
import { TurnMenu } from "./controls";
import { bringIntoView, elementOf, FindNowContext, mark, useRevealed, type FindNow } from "./find";
import { Output, Prose, ToolIcon } from "./parts";
import { FilesContext, type ConversationFiles } from "./files";
import { PageView } from "./page";
import { pageOf } from "./page-of";
import { LiveView } from "./live";
import { liveOf } from "./live-of";
import { SavePlan, todosAsPlan } from "@/views/plans/save";
import { copyText } from "@/views/terminal/clipboard";
import { Beam, Optic, PrismLabel, ThinkingBody, ThinkingHead, ThinkingLabel, type BeamStatus } from "./signals";
import { latestThought, thinkingStarted, thoughtIsLive, type ThoughtItem } from "./thinking";
import { usePlaces, type Place } from "./state";
import { stillAsked, useAnswered } from "./answered";
import { useHeld, useRecent } from "./dom";

/** Tool calls the agent is asking you about right now (the rest of the pending ones wait their turn). */
const Asking = React.createContext<ReadonlySet<string>>(new Set());

/** The notice saying the harness isn't signed in (the latest, unless it has worked since), and where to sign it in. */
const SignIn = React.createContext<{ notice: string; target: string; kind: string } | null>(null);

const NO_TASKS: Task[] = [];

/** The agent's tasks: only the steps that started a subagent read them (so the rest don't re-render as they change). */
const Tasks = React.createContext<Task[]>(NO_TASKS);

function Contexts({
  asking,
  signIn,
  findNow,
  tasks,
  files,
  children,
}: {
  asking: ReadonlySet<string>;
  signIn: React.ContextType<typeof SignIn>;
  findNow: FindNow | null;
  tasks: Task[];
  /** The files its messages mention (files.tsx). */
  files: ConversationFiles;
  children: React.ReactNode;
}) {
  return (
    <Asking.Provider value={asking}>
      <SignIn.Provider value={signIn}>
        <Tasks.Provider value={tasks}>
          <FilesContext.Provider value={files}>
            <FindNowContext.Provider value={findNow}>{children}</FindNowContext.Provider>
          </FilesContext.Provider>
        </Tasks.Provider>
      </SignIn.Provider>
    </Asking.Provider>
  );
}

const STEP_ICON: Record<Step["group"], Parameters<typeof ToolIcon>[0]["kind"]> = {
  explore: "read",
  edit: "edit",
  shell: "shell",
  web: "web",
  agent: "agent",
  plan: "todo",
  ask: "question",
  tool: "other",
};

const beamFor = (s: ToolEvent["status"]): BeamStatus => (s === "running" || s === "done" || s === "error" || s === "denied" || s === "pending" || s === "cancelled" ? s : "done");
const dotFor = (s: ToolEvent["status"]) =>
  s === "done" ? "succeeded" : s === "error" ? "failed" : s === "running" ? "running" : s === "pending" ? "queued" : s === "denied" ? "timed_out" : "cancelled";

function Elapsed({ from, to }: { from: number; to?: number | null }) {
  return to ? <span className="hx-meta-num">{duration(to - from)}</span> : <Ticking from={from} />;
}

/** Counts by itself (the shared clock), so only this text re-renders each second. */
function Ticking({ from }: { from: number }) {
  const now = useNow();
  return <span className="hx-meta-num">{duration(now - from)}</span>;
}

/** A live child keeps counting even when the harness has reported an earlier duration. */
export function TaskElapsed({ task }: { task: Task }) {
  if (task.ended === undefined && taskLive(task)) return <Elapsed from={task.started} />;
  const ms = taskDuration(task);
  return ms !== undefined ? <span className="hx-meta-num">{duration(ms)}</span> : null;
}

/* ------------------------------------------------------------------ step bodies */

function ExploreBody({ calls, cwd }: { calls: ToolEvent[]; cwd: string }) {
  return (
    <ul className="hx-list">
      {calls.map((c) => (
        <li key={c.id} className="hx-list-row" data-ev={c.id}>
          <ToolIcon kind={c.tool} size={12} className="text-[var(--color-subtle)]" />
          <span className="hx-mono hx-trunc">{c.tool === "read" ? relPath(c.title, cwd) : c.title}</span>
          {c.detail && <span className="hx-dim hx-trunc">{c.tool === "read" ? c.detail : relPath(c.detail, cwd)}</span>}
          {c.status === "error" && <span className="hx-bad">failed</span>}
        </li>
      ))}
    </ul>
  );
}

function EditBody({ calls, cwd }: { calls: ToolEvent[]; cwd: string }) {
  const many = calls.reduce((n, c) => n + (c.diffs?.length ?? 0), 0) > 3;
  const now = React.useContext(FindNowContext);
  return (
    <div className="hx-stack">
      {calls.map((c) =>
        (c.diffs?.length ?? 0) > 0 ? (
          (c.diffs ?? []).map((d, i) => (
            <div key={`${c.id}-${i}`} className={cn(c.status === "denied" && "opacity-60")} data-ev={c.id}>
              {c.status === "denied" && <div className="hx-dim mb-1 text-[11px]">Refused — not applied</div>}
              <FileDiffView diff={d} label={relPath(d.path, cwd)} defaultOpen={!many} reveal={now?.id === c.id ? now.n : 0} />
            </div>
          ))
        ) : (
          <div key={c.id} className="hx-list-row" data-ev={c.id}>
            <ToolIcon kind="edit" size={12} />
            <span className="hx-mono hx-trunc">{relPath(c.title, cwd)}</span>
            {c.status === "error" && c.output && <span className="hx-bad hx-trunc">{c.output}</span>}
          </div>
        ),
      )}
    </div>
  );
}

/** A command that ran (or runs) on another machine (kiln reach). */
const elsewhere = (c: ToolEvent) => typeof c.runner === "string";

/** Where it ran, as a chip: the runner, what it must be until one is picked, or "another runner". */
function Where({ runner, title }: { runner: string; title?: string }) {
  return (
    <span className="hx-where" title={title ?? (runner ? `Ran on ${runner}, not this agent's machine` : "Runs on another runner")}>
      <ArrowUpRight size={11} aria-hidden="true" />
      <span className="hx-trunc">{runner || "another runner"}</span>
    </span>
  );
}

/**
 * Where a step's commands ran, for its head: the machine when all ran there ("adacachy"), how many
 * when only some did ("1 on adacachy"), and how many machines when several ("2 on 3 machines").
 */
function whereOfStep(calls: ToolEvent[], remote: string[]): string {
  const n = calls.filter(elsewhere).length;
  const machines = remote.length === 1 ? remote[0] || "another runner" : `${remote.length} machines`;
  return n === calls.length && remote.length === 1 ? machines : `${n} on ${machines}`;
}

function CommandRow({ c, open: initial }: { c: ToolEvent; open?: boolean }) {
  const [open, setOpen] = React.useState(Boolean(initial));
  useRevealed(
    (id) => id === c.id,
    () => setOpen(true),
  );
  const failed = c.status === "error" || (c.exit_code !== undefined && c.exit_code !== 0);
  // Nothing to show while it runs without output yet: the dot and the clock say it's running.
  const out = c.output ? c.output : c.status === "running" ? null : "(no output)";
  return (
    <div className="hx-cmd" data-failed={failed || undefined} data-remote={elsewhere(c) || undefined} data-ev={c.id}>
      <button type="button" className="hx-cmd-head" aria-expanded={open} onClick={() => setOpen(!open)}>
        <i className="dot" data-status={dotFor(failed ? "error" : c.status)} aria-hidden="true" />
        {typeof c.runner === "string" && <Where runner={c.runner} />}
        <code className="hx-cmd-line">{c.title}</code>
        <span className="hx-cmd-meta">
          {c.exit_code !== undefined && <span className={failed ? "hx-bad" : "hx-good"}>exit {c.exit_code}</span>}
          {c.status === "running" ? <Elapsed from={c.started} /> : c.ended ? <span className="hx-meta-num">{duration(c.ended - c.started)}</span> : null}
        </span>
      </button>
      {c.detail && !(elsewhere(c) && c.detail === `on ${c.runner}`) && <div className="hx-cmd-why">{c.detail}</div>}
      {open && out !== null && <Output text={out} truncated={c.truncated} error={failed} />}
    </div>
  );
}

function ShellBody({ calls }: { calls: ToolEvent[] }) {
  return (
    <div className="hx-stack">
      {calls.map((c) => (
        <CommandRow key={c.id} c={c} open={calls.length === 1 || c.status === "error" || (c.exit_code ?? 0) !== 0} />
      ))}
    </div>
  );
}

function PlanBody({ calls }: { calls: ToolEvent[] }) {
  const last = calls.at(-1);
  if (!last) return null;
  if (last.tool === "plan") {
    const plan = (last.input as { plan?: string } | null)?.plan ?? last.output ?? last.title;
    return (
      <>
        <Prose text={plan} className="hx-plan-text" ev={last.id} />
        <SavePlan body={plan} />
      </>
    );
  }
  const todos = last.todos ?? [];
  return (
    <div data-ev={last.id}>
      <TodoList todos={todos} />
      {todos.length > 0 && <SavePlan body={todosAsPlan(todos)} title={todos[0]?.text.slice(0, 80)} />}
    </div>
  );
}

export function TodoList({ todos }: { todos: { text: string; status: string }[] }) {
  if (todos.length === 0) return <div className="hx-dim text-[12px]">No plan yet.</div>;
  return (
    <ul className="hx-todos">
      {todos.map((t, i) => (
        <li key={i} data-status={t.status}>
          <span className="hx-check" aria-hidden="true" />
          <span>{t.text}</span>
        </li>
      ))}
    </ul>
  );
}

function AskBody({ calls }: { calls: ToolEvent[] }) {
  return (
    <div className="hx-stack">
      {calls.map((c) => {
        const qs = ((c.input as { questions?: { question: string }[] } | null)?.questions ?? []).map((q) => q.question);
        return (
          <div key={c.id} className="hx-ask" data-ev={c.id}>
            {(qs.length ? qs : [c.title]).map((q, i) => (
              <div key={i} className="hx-ask-q">
                {q}
              </div>
            ))}
            {c.output && (
              <div className="hx-ask-a">
                <CornerDownRight size={12} /> {c.output}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

function ToolBody({ calls }: { calls: ToolEvent[] }) {
  return (
    <div className="hx-stack">
      {calls.map((c) => {
        const input = c.input && typeof c.input === "object" && Object.keys(c.input as object).length ? JSON.stringify(c.input, null, 2) : null;
        return (
          <div key={c.id} className="hx-generic" data-ev={c.id}>
            <div className="hx-list-row">
              <ToolIcon kind={c.tool} size={12} />
              {c.server ? (
                <span className="hx-mono">
                  <span className="hx-dim">{c.server} · </span>
                  {c.tool_short ?? c.name}
                </span>
              ) : c.skill ? (
                <span className="hx-mono">/{c.skill}</span>
              ) : (
                <span className="hx-mono">{c.name}</span>
              )}
              <span className="hx-dim hx-trunc">{c.title}</span>
            </div>
            {input && (c.server || c.tool === "other" || c.tool === "mcp") && <pre className="hx-pre hx-tool-input">{input}</pre>}
            {c.output && <Output text={c.output} truncated={c.truncated} error={c.status === "error"} />}
          </div>
        );
      })}
    </div>
  );
}

function AgentBody({ calls, tasks, onOpenTask }: { calls: ToolEvent[]; tasks: Task[]; onOpenTask?: (t: string) => void }) {
  return (
    <div className="hx-stack">
      {calls.map((c) => {
        const task = taskOfCall(c, tasks);
        const codexActivity = c.name === "collabAgentToolCall" || c.name === "subAgentActivity";
        const thread = task?.tool_id ?? (c.task || !codexActivity ? c.tool_id : undefined);
        const live = task ? taskLive(task) : false;
        return (
          <div key={c.id} className="hx-sub" data-ev={c.id}>
            <div className="hx-sub-head">
              <i className="dot" data-status={task ? taskDot(task) : dotFor(c.status)} aria-hidden="true" />
              <span className="hx-sub-title min-w-0 break-words">{task?.description ?? c.title}</span>
              {task?.kind && <span className="hx-tag shrink-0">{task.kind}</span>}
            </div>
            <div className="hx-sub-meta">
              {task && <span>{task.status}</span>}
              {task?.tool_uses ? <span>{task.tool_uses} steps</span> : null}
              {task && <TaskElapsed task={task} />}
              {task?.last_tool && live && <span className="hx-trunc">{task.last_tool}</span>}
              {onOpenTask && thread && (
                <button type="button" className="hx-link" onClick={() => onOpenTask(thread)}>
                  Open its work →
                </button>
              )}
            </div>
            {(task?.summary ?? c.output) && <Prose text={task?.summary ?? c.output ?? ""} className="hx-sub-result" />}
          </div>
        );
      })}
    </div>
  );
}

/* ------------------------------------------------------------------ step */

/**
 * A step opens by itself while it runs once it has run this long: calls that come and go faster (an
 * agent reading, searching and running at full speed) don't open and close it on each one.
 */
const OPEN_AFTER_MS = 500;
/** A call that had been running this long when the page saw it first (by this device's clock, with room for another's) opens at once. */
const RUNNING_BEFORE_MS = 2000;

const StepRow = React.memo(function StepRow({ step, cwd, tasks, onOpenTask }: { step: Step; cwd: string; tasks: Task[]; onOpenTask?: (t: string) => void }) {
  const status = stepStatus(step.calls);
  const failures = step.calls.filter((c) => c.status === "error" || (c.tool === "shell" && c.exit_code !== undefined && c.exit_code !== 0)).length;
  const asking = React.useContext(Asking);
  const asked = step.calls.some((c) => asking.has(c.tool_id));
  // Open by default: what needs attention, and what is happening right now (once it has gone on a moment).
  const [open, setOpen] = React.useState<boolean | null>(null);
  // A match found in it opens it.
  useRevealed(
    (id) => step.calls.some((c) => c.id === id),
    () => setOpen(true),
  );
  const runningFrom = step.calls.reduce((t, c) => (c.status === "running" ? Math.max(t, c.started) : t), -Infinity);
  const runs = useHeld(status === "running", OPEN_AFTER_MS, Date.now() - runningFrom > RUNNING_BEFORE_MS);
  const isOpen = open ?? (runs || failures > 0 || step.group === "agent" || step.group === "ask" || (step.group === "plan" && step.calls.some((c) => c.tool === "plan")));
  const stats = step.group === "edit" ? diffStats(step.calls.filter((c) => c.status !== "denied").flatMap((c) => c.diffs ?? [])) : null;
  const first = step.calls[0];
  const last = step.calls.at(-1);
  // Commands run on other machines: where, in the step's head, so it shows folded too.
  const remote = [...new Set(step.calls.filter(elsewhere).map((c) => c.runner ?? ""))];
  const subtitle =
    step.group === "shell" && step.calls.length === 1
      ? first?.title
      : step.group === "edit"
        ? [...new Set(step.calls.flatMap((c) => (c.diffs ?? []).map((d) => relPath(d.path, cwd))))].slice(0, 3).join(", ")
        : step.group === "explore"
          ? step.calls
              .slice(0, 3)
              .map((c) => (c.tool === "read" ? relPath(c.title, cwd).split("/").pop() : c.title))
              .join(", ")
          : step.group === "agent"
            ? (taskOfCall(first, tasks)?.description ?? first?.title)
            : step.calls.length === 1
              ? first?.title
              : undefined;

  return (
    <div className="hx-step" data-group={step.group} data-status={status} data-remote={remote.length > 0 || undefined} data-ev={step.calls.map((c) => c.id).join(" ")}>
      <button type="button" className="hx-step-head" aria-expanded={isOpen} onClick={() => setOpen(!isOpen)}>
        <span className="hx-step-icon">
          <ToolIcon kind={STEP_ICON[step.group]} size={13} />
        </span>
        <span className="hx-step-title">{stepSummary(step.group, step.calls)}</span>
        {subtitle && <span className="hx-step-sub">{subtitle}</span>}
        {remote.length > 0 && <Where runner={whereOfStep(step.calls, remote)} title={remote.filter(Boolean).join(", ") || undefined} />}
        <span className="hx-step-end">
          {stats && (stats.add > 0 || stats.del > 0) && <DiffStat add={stats.add} del={stats.del} />}
          {status === "pending" && (asked ? <span className="hx-warn text-[11px]">waiting for you</span> : <span className="hx-dim text-[11px]">queued</span>)}
          {failures > 0 && <span className="hx-bad">{failures} failed</span>}
          {status === "running" && first ? <Elapsed from={first.started} /> : first && last?.ended ? <span className="hx-meta-num">{duration(last.ended - first.started)}</span> : null}
          <Beam status={beamFor(failures > 0 ? "error" : status)} />
          <ChevronRight size={13} className={cn("hx-chev", isOpen && "rotate-90")} />
        </span>
      </button>
      {isOpen && (
        <div className="hx-step-body">
          {step.group === "explore" && <ExploreBody calls={step.calls} cwd={cwd} />}
          {step.group === "edit" && <EditBody calls={step.calls} cwd={cwd} />}
          {step.group === "shell" && <ShellBody calls={step.calls} />}
          {step.group === "web" && <ExploreBody calls={step.calls} cwd={cwd} />}
          {step.group === "agent" && <AgentBody calls={step.calls} tasks={tasks} onOpenTask={onOpenTask} />}
          {step.group === "plan" && <PlanBody calls={step.calls} />}
          {step.group === "ask" && <AskBody calls={step.calls} />}
          {step.group === "tool" && <ToolBody calls={step.calls} />}
        </div>
      )}
    </div>
  );
});

/* ------------------------------------------------------------------ items & turns */

function Thought({ item, live, since }: { item: ThoughtItem; live: boolean; since: number }) {
  const { event: e, more = [] } = item;
  const [open, setOpen] = React.useState(false);
  useRevealed(
    (id) => id === e.id || more.some((m) => m.id === id),
    () => setOpen(true),
  );
  if (e.redacted) return null;
  const all = [e, ...more].filter((x) => !x.redacted);
  return (
    <div className="hx-thought" data-ev={all.map((x) => x.id).join(" ")}>
      <ThinkingHead live={live} since={since} open={open} onToggle={() => setOpen(!open)} />
      {all.length > 1 && !open && <span className="hx-thought-n">{all.length} thoughts</span>}
      {open && (
        <ThinkingBody live={live}>
          {all.map((x) => (
            <Prose key={x.id} text={x.text} className="hx-thought-text" ev={x.id} streaming={x.streaming} />
          ))}
        </ThinkingBody>
      )}
    </div>
  );
}

function Notice({ e, repeats }: { e: Extract<HEvent, { kind: "notice" }>; repeats?: number }) {
  const signIn = React.useContext(SignIn);
  const I = e.level === "error" ? CircleAlert : e.level === "warn" ? TriangleAlert : Info;
  return (
    <>
      <div className="hx-notice" data-level={e.level} data-ev={e.id}>
        <I size={12} /> <span>{e.text}</span>
        {repeats && repeats > 1 && (
          <span className="hx-notice-n" title={`Said ${repeats} times in a row`}>
            ×{repeats}
          </span>
        )}
      </div>
      {signIn?.notice === e.id && (
        <div className="ac-under">
          <AccountCard compact intent="login" target={signIn.target} kind={signIn.kind} />
        </div>
      )}
    </>
  );
}

const DELIVERY: Record<Delivery, string> = {
  sent: "sent",
  queued: "queued by the agent",
  started: "being worked on",
  completed: "done",
  cancelled: "dropped",
};

/** Copy what was said (your message, the agent's words), as it was written: a quiet icon that says when it took. */
function CopyMsg({ text, label, className }: { text: string; label: string; className?: string }) {
  const [done, setDone] = React.useState(false);
  return (
    <button
      type="button"
      className={cn("hx-icon hx-msg-copy", className)}
      aria-label={done ? "Copied" : label}
      title={done ? "Copied" : label}
      onClick={() =>
        void copyText(text).then((ok) => {
          if (!ok) return;
          setDone(true);
          window.setTimeout(() => setDone(false), 1400);
        })
      }
    >
      {done ? <Check size={13} /> : <Copy size={13} />}
    </button>
  );
}

/** When something was said, "14:05": one formatter for every message (making one for each, as each renders, is slow). */
const SAID_AT = new Intl.DateTimeFormat([], { hour: "2-digit", minute: "2-digit" });

/** What was asked: it renders only when it changes, not as the turn under it streams. */
const Prompt = React.memo(function Prompt({ e, delivery, meta, onCommand }: { e: Extract<HEvent, { kind: "user" }>; delivery?: Delivery; meta?: Meta | null; onCommand?: (c: Command) => void }) {
  const system = e.origin && e.origin !== "human";
  return (
    <div className="hx-prompt" data-origin={e.origin ?? "human"} data-ev={e.id}>
      <div className="hx-prompt-meta">
        <span className="hx-eyebrow">{system ? e.origin : "You"}</span>
        <span className="hx-dim">{SAID_AT.format(e.at)}</span>
        {delivery && <span className="hx-dim">· {DELIVERY[delivery]}</span>}
        {e.attachments?.length ? (
          <span className="hx-dim hx-trunc" title={e.attachments.join(", ")}>
            · {e.attachments.join(", ")}
          </span>
        ) : e.images ? (
          <span className="hx-dim">
            · {e.images} image{e.images === 1 ? "" : "s"}
          </span>
        ) : null}
        {e.text && <CopyMsg text={e.text} label={system ? "Copy the message" : "Copy your message"} className="hx-prompt-copy" />}
        {!system && onCommand && <TurnMenu meta={meta ?? null} eventId={e.id} send={onCommand} />}
      </div>
      <div className="hx-prompt-text">{e.text}</div>
    </div>
  );
});

/** A step that started a subagent: it shows the subagent's task as it goes. */
function AgentStep({ step, cwd, onOpenTask }: { step: Step; cwd: string; onOpenTask?: (t: string) => void }) {
  const tasks = React.useContext(Tasks);
  return <StepRow step={step} cwd={cwd} tasks={tasks} onOpenTask={onOpenTask} />;
}

/** One thing in a turn: it renders again only when it changes (or starts or stops being the live one), not as the rest of the turn streams. */
const ItemView = React.memo(function ItemView({ item, cwd, onOpenTask, active, since }: { item: Item; cwd: string; onOpenTask?: (t: string) => void; active: boolean; since: number }) {
  switch (item.type) {
    case "step":
      return item.group === "agent" ? <AgentStep step={item} cwd={cwd} onOpenTask={onOpenTask} /> : <StepRow step={item} cwd={cwd} tasks={NO_TASKS} onOpenTask={onOpenTask} />;
    case "note":
      // A tap (or a pointer resting on it) shows its copy icon.
      return (
        <div className="hx-said" data-ev={item.event.id} tabIndex={-1}>
          <Prose text={item.event.text} className={cn("hx-note", item.event.streaming && "hx-streaming")} seq={item.event.seq} streaming={item.event.streaming} />
          {!item.event.streaming && item.event.text && <CopyMsg text={item.event.text} label="Copy what it said" className="hx-said-copy" />}
        </div>
      );
    case "thought":
      return <Thought item={item} live={active && Boolean(latestThought(item).streaming)} since={since} />;
    case "notice":
      return <Notice e={item.event} repeats={item.repeats} />;
    case "page":
      // The page, or where it'll be while it's being kept; a call that failed says why, as a step.
      return pageOf(item.event) || item.event.status === "running" || item.event.status === "pending" ? (
        <PageView call={item.event} />
      ) : (
        <StepRow step={{ type: "step", key: item.key, group: "tool", calls: [item.event] }} cwd={cwd} tasks={NO_TASKS} onOpenTask={onOpenTask} />
      );
    case "live":
      return liveOf(item.event) || item.event.status === "running" || item.event.status === "pending" ? (
        <LiveView call={item.event} />
      ) : (
        <StepRow step={{ type: "step", key: item.key, group: "tool", calls: [item.event] }} cwd={cwd} tasks={NO_TASKS} onOpenTask={onOpenTask} />
      );
  }
});

/** Whether the end of a live turn shows it at work: a reply or a thought streaming, a call running. */
function moving(items: Item[], turnStarted?: number): boolean {
  const last = items.findLast((item) => item.type !== "notice");
  if (!last) return false;
  if (last.type === "note") return Boolean(last.event.streaming);
  if (last.type === "thought") return thoughtIsLive(last, true, turnStarted);
  if (last.type === "step") return last.calls.some((c) => c.status === "running" || c.status === "pending");
  if (last.type === "page" || last.type === "live") return last.event.status === "running" || last.event.status === "pending";
  return false;
}

/** How long the end of a turn holds still before it shows the turn at work: not in the moment between one quick call and the next. */
const TAIL_AFTER_MS = 600;

/** The end of a turn under way while nothing at its end moves: what it's doing (a model thinking
 *  after a call shows nothing for minutes) and the elapsed time across the whole turn, so it
 *  never looks stopped while it's at work. */
function LiveTail({ doing, since }: { doing?: string; since?: number }) {
  return (
    <div className="hx-live-tail" role="status">
      <span className="sg-thinking-line">
        <Optic state="thinking" />
        <ThinkingLabel doing={doing} since={since} />
      </span>
    </div>
  );
}

const TurnView = React.memo(function TurnView({
  turn,
  cwd,
  delivery,
  live,
  doing,
  turnStarted,
  onOpenTask,
  meta,
  onCommand,
  agentLabel,
  reading,
}: {
  /** Who answers (the harness's name), over the agent's side of the turn. */
  agentLabel?: string;
  turn: Turn;
  cwd: string;
  delivery?: Delivery;
  live: boolean;
  /** What the agent says it's doing, while the turn is live. */
  doing?: string;
  turnStarted?: number;
  onOpenTask?: (t: string) => void;
  meta?: Meta | null;
  onCommand?: (c: Command) => void;
  /** The part being read stays in the window while the turn grows. */
  reading: string | null;
}) {
  const edits = React.useMemo(() => (turn.outcome ? changesetOf(turn.items.flatMap((i) => (i.type === "step" ? i.calls : []))) : []), [turn]);
  const add = edits.reduce((n, f) => n + f.add, 0);
  const del = edits.reduce((n, f) => n + f.del, 0);
  // A turn of hundreds of steps shows its latest; the rest are a tap away (and there for a match
  // you find in them).
  const findNow = React.useContext(FindNowContext);
  const [allSteps, setAllSteps] = React.useState(false);
  const at = reading ? turn.items.findIndex((it) => itemHolds(it, reading)) : -1;
  const hidden = allSteps ? 0 : Math.max(0, Math.min(turn.items.length - STEPS_WINDOW, at < 0 ? Infinity : at));
  const end = allSteps ? turn.items.length : hidden + STEPS_WINDOW;
  const sought = findNow !== null && turn.items.some((it, i) => (i < hidden || i >= end) && itemHolds(it, findNow.id));
  const items = sought ? turn.items : turn.items.slice(hidden, end);
  const currentItem = turn.items.findLast((item) => item.type !== "notice");
  const since = thinkingStarted(turn, turnStarted);
  // At work with nothing moving at its end: said once the end has held still a moment (from the
  // start, before anything has moved, at once).
  const moves = live && moving(turn.items, turnStarted);
  const movedLately = useRecent(moves, TAIL_AFTER_MS);
  if (turn.divider) {
    return (
      <section className="hx-turn hx-turn-switch">
        <Notice e={turn.divider} />
      </section>
    );
  }
  // Who answered this turn: the harness it ran in (an earlier one, after a switch), else the agent's.
  const label = turn.by ?? agentLabel;
  return (
    <section className="hx-turn">
      {turn.prompt && <Prompt e={turn.prompt} delivery={delivery} meta={meta} onCommand={onCommand} />}
      <div className="hx-work">
        {label && (turn.items.length > 0 || turn.outcome || live) && <div className="hx-agent-label">{label}</div>}
        {hidden > 0 && !sought && (
          <button type="button" className="hx-earlier-steps" onClick={() => setAllSteps(true)}>
            {hidden} earlier step{hidden === 1 ? "" : "s"}
          </button>
        )}
        {items.map((it) => (
          <ItemView key={it.key} since={since} item={it} cwd={cwd} onOpenTask={onOpenTask} active={live && it === currentItem && (it.type !== "thought" || thoughtIsLive(it, true, turnStarted))} />
        ))}
        {live && !turn.outcome && !movedLately && <LiveTail doing={doing} since={since} />}
      </div>
      {turn.outcome && (
        <div className="hx-outcome" data-ev={turn.outcome.id}>
          <Prose text={turn.outcome.text} seq={turn.outcome.seq} />
          <div className="hx-outcome-foot">
            {edits.length > 0 && (
              <span>
                {edits.length} file{edits.length === 1 ? "" : "s"} changed <DiffStat add={add} del={del} />
              </span>
            )}
            {turn.ended && <span className="hx-meta-num">{duration(turn.ended - turn.started)}</span>}
            {turn.outcome.text && <CopyMsg text={turn.outcome.text} label="Copy the answer" className="hx-outcome-copy" />}
          </div>
        </div>
      )}
    </section>
  );
});

/* ------------------------------------------------------------------ where you are in it */

/** The parts a place in the conversation is kept by: what you asked, each thing it did or said, its answer. */
const PARTS = ".hx-turn > [data-ev], .hx-work > [data-ev]";
/** This close to the end counts as at the end: it follows what comes. */
const END_PX = 40;
/** A part you tapped (or keyed) that changes size this soon after (a step opening) stays where it was. */
const TOUCH_MS = 1000;

const partId = (el: Element) => (el.getAttribute("data-ev") ?? "").split(" ")[0] ?? "";

function partOf(root: HTMLElement, id: string): HTMLElement | null {
  const ev = `[data-ev~="${CSS.escape(id)}"]`;
  return root.querySelector<HTMLElement>(`.hx-turn > ${ev}, .hx-work > ${ev}`);
}

/** The first part in view, and how far below the view's top it starts (negative: above it). */
function firstInView(root: HTMLElement): { id: string; offset: number } | null {
  const top = root.getBoundingClientRect().top;
  const all = root.querySelectorAll<HTMLElement>(PARTS);
  let lo = 0;
  let hi = all.length - 1;
  let found: HTMLElement | null = null;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const el = all[mid];
    if (!el) break;
    if (el.getBoundingClientRect().bottom > top + 1) {
      found = el;
      hi = mid - 1;
    } else lo = mid + 1;
  }
  const id = found ? partId(found) : "";
  return found && id ? { id, offset: found.getBoundingClientRect().top - top } : null;
}

/** Scrolls so that part `at.id` starts `at.offset` below the view's top; false when it isn't there. */
function goTo(root: HTMLElement, at: { id: string; offset: number }): boolean {
  const el = partOf(root, at.id);
  if (!el) return false;
  const by = el.getBoundingClientRect().top - root.getBoundingClientRect().top - at.offset;
  if (Math.abs(by) >= 1) root.scrollTop += by;
  return true;
}

interface Keep {
  root: HTMLElement | null;
  /** At the end: it follows what comes. */
  end: boolean;
  /** The page holds the conversation's end (not a window further back): only then is its bottom the end. */
  tail: boolean;
  /** Otherwise: what's being read, and where. */
  at: { id: string; offset: number } | null;
  /** The part last tapped or keyed in: its height and place then, and when. */
  touched: { el: HTMLElement; h: number; offset: number; t: number } | null;
  saving: number;
}

/**
 * Keeps the conversation where you are, whatever comes in, loads or reflows: at its end it follows
 * what comes; scrolled up, what you're reading stays exactly where it is; a step you open opens
 * under your finger. Your place is kept under `key` for the next visit (and a reload).
 */
function useKeeper(key: string) {
  const k = React.useRef<Keep>({ root: null, end: true, tail: true, at: null, touched: null, saving: 0 });
  /** Where the view is now. */
  const read = React.useCallback(() => {
    const s = k.current;
    if (!s.root) return;
    s.end = s.tail && s.root.scrollHeight - s.root.scrollTop - s.root.clientHeight < END_PX;
    s.at = s.end ? null : firstInView(s.root);
  }, []);
  const restore = React.useCallback(() => {
    const s = k.current;
    if (s.root && s.at) goTo(s.root, s.at);
  }, []);
  const save = React.useCallback(() => {
    const s = k.current;
    window.clearTimeout(s.saving);
    s.saving = 0;
    const place: Place = s.end || !s.at ? { end: true } : s.at;
    usePlaces.getState().setPlace(key, place);
  }, [key]);
  const ref = React.useCallback(
    (root: HTMLDivElement | null) => {
      if (!root) return;
      const s = k.current;
      s.root = root;
      // Where you were; the end the first time.
      const was = usePlaces.getState().places[key];
      if (was && "id" in was && goTo(root, was)) {
        s.end = false;
        s.at = { id: was.id, offset: was.offset };
      } else {
        root.scrollTop = root.scrollHeight;
        s.end = true;
        s.at = null;
      }
      // Something changed size (it came in, loaded, opened, the view resized): before it's painted,
      // the end stays at the end, or what you were reading stays where it was.
      const hold = () => {
        // A keyboard or a growing answer can resize the pane while a question is being answered.
        // Keep that field visible instead of pinning an unrelated row (or the transcript's end).
        const focused = document.activeElement;
        if (focused instanceof HTMLElement && root.contains(focused) && focused.matches("input, textarea, [contenteditable='true']")) {
          const field = focused.getBoundingClientRect();
          const pane = root.getBoundingClientRect();
          if (field.top < pane.top + 12) root.scrollTop += field.top - pane.top - 12;
          else if (field.bottom > pane.bottom - 12) root.scrollTop += field.bottom - pane.bottom + 12;
          s.touched = null;
          read();
          return;
        }
        const t = s.touched;
        if (t && performance.now() - t.t < TOUCH_MS && t.el.isConnected && t.el.offsetHeight !== t.h) {
          // What you just opened (or closed) opens in place; where that leaves you is where you are.
          goTo(root, { id: partId(t.el), offset: t.offset });
          s.touched = null;
          read();
        } else if (s.end) root.scrollTop = root.scrollHeight;
        else if (s.at) goTo(root, s.at);
      };
      const watch = new ResizeObserver(hold);
      watch.observe(root);
      if (root.firstElementChild) watch.observe(root.firstElementChild);
      window.addEventListener("pagehide", save);
      return () => {
        watch.disconnect();
        window.removeEventListener("pagehide", save);
        save();
        s.root = null;
      };
    },
    [key, read, save],
  );
  const onScroll = () => {
    read();
    const s = k.current;
    s.touched = null;
    window.clearTimeout(s.saving);
    s.saving = window.setTimeout(save, 250);
    return s.at?.id ?? null;
  };
  /** At the end, following what comes. */
  const atEnd = React.useCallback(() => k.current.end, []);
  /** At the bottom of what's in the page, wherever that is in the conversation. */
  const atBottom = React.useCallback(() => {
    const r = k.current.root;
    return Boolean(r && r.scrollHeight - r.scrollTop - r.clientHeight < END_PX);
  }, []);
  /** Whether the page holds the conversation's end. */
  const setTail = React.useCallback((tail: boolean) => {
    k.current.tail = tail;
    if (!tail) k.current.end = false;
  }, []);
  /** To the end, following what comes. */
  const toEnd = React.useCallback(() => {
    const s = k.current;
    s.tail = true;
    s.end = true;
    s.at = null;
    if (s.root) s.root.scrollTop = s.root.scrollHeight;
  }, []);
  /** A tap or a key on a part: if it changes size because of it, it does so in place. */
  const onTouch = (e: React.SyntheticEvent) => {
    const root = k.current.root;
    const el = e.target instanceof Element ? e.target.closest<HTMLElement>(PARTS) : null;
    k.current.touched = el && root ? { el, h: el.offsetHeight, offset: el.getBoundingClientRect().top - root.getBoundingClientRect().top, t: performance.now() } : null;
  };
  return { ref, onScroll, onTouch, read, restore, atEnd, atBottom, setTail, toEnd };
}

/**
 * How much of a long conversation is in the page: its latest turns, this many at a time. What's
 * further back is added as you scroll up to it (or go to it: your place, a match you find), so a
 * conversation of thousands of events reads, streams and takes typing as one of a few dozen.
 */
const WINDOW = 30;
/** A turn with more steps than this (an agent working on its own for hours) shows its latest ones; the rest a tap away. */
const STEPS_WINDOW = 60;
/** The most turns in the page at once: a window grown past it lets go of its far end. */
const MOST = 3 * WINDOW;

/** Whether event `id` shows in item `it`. */
const itemHolds = (it: Item, id: string) => (it.type === "step" ? it.calls.some((c) => c.id === id) : it.event.id === id || (it.type === "thought" && (it.more ?? []).some((m) => m.id === id)));

/** The turn event `id` shows in (what was asked, a step, a word along the way, the answer): its index, or -1. */
function turnOf(turns: Turn[], id: string | undefined): number {
  if (!id) return -1;
  for (let i = turns.length - 1; i >= 0; i--) {
    const t = turns[i];
    if (t && (t.prompt?.id === id || t.outcome?.id === id || t.divider?.id === id || t.items.some((it) => itemHolds(it, id)))) return i;
  }
  return -1;
}

/** Where a conversation's place is kept: the agent's, or one of its subagents' threads. */
export const placeKey = (agent: string, parent?: string) => (parent ? `${agent}/${parent}` : agent);

const NO_APPROVALS: Approval[] = [];

/**
 * The timeline of the main thread, or of one subagent (`parent`: its spawning tool call), live from
 * the agent's session. Keeps your place: at the end it follows the newest work, scrolled up it
 * leaves you be, and coming back puts you where you were.
 */
export const Timeline = React.memo(function Timeline({
  store,
  agent,
  cwd,
  parent,
  onOpenTask,
  onCommand,
  footer,
  agentLabel,
  agentName,
  onDecide,
  target,
  kind,
  accountOpen,
  find,
}: {
  store: SessionStore;
  /** The agent's id: where your place in it is kept. */
  agent: string;
  /** Find in the conversation: the words, and the match being shown. */
  find?: { q: string; now: FindNow | null };
  agentLabel?: string;
  /** The agent's own name, for what it asks. */
  agentName?: string;
  /** Answers what waits on you (shown at the end of the conversation, where it was asked). */
  onDecide?: Decide;
  /** Where the agent runs and its harness: a notice that it isn't signed in gets its sign-in under it. */
  target?: string;
  kind?: string;
  /** The account is open already: the notice doesn't offer it twice. */
  accountOpen?: boolean;
  cwd: string;
  parent?: string;
  onOpenTask?: (t: string) => void;
  /** Turn actions (rewind, branch) go out through this. */
  onCommand?: (c: Command) => void;
  footer?: React.ReactNode;
}) {
  const events = useSession(store, (s) => s.events);
  const state = useSession(store, (s) => s.state);
  const meta = useSession(store, (s) => s.meta);
  const done = useAnswered();
  // The files its messages mention, as the server kept them: shown where they're mentioned.
  const keptFiles = useSession(store, (s) => s.files);
  const conversationFiles = React.useMemo(() => ({ session: agent, cwd, files: keptFiles }), [agent, cwd, keptFiles]);
  const task = parent ? state?.tasks.find((t) => t.tool_id === parent) : undefined;
  // A Codex child's own work is keyed by its thread: opened from any of its calls, or with its row
  // gone, it still shows (its row's id, else the thread the opened call names).
  const thread = React.useMemo(() => {
    if (!parent) return undefined;
    if (task) return task.id;
    const call = events.find((e) => e.id === parent);
    return call?.kind === "tool" ? call.task : undefined;
  }, [parent, task, events]);
  const going = parent ? Boolean(task && taskLive(task)) : state?.status === "running" || state?.status === "waiting" || state?.status === "starting";
  // Turns that didn't change stay the same objects, so only what changed renders again; and only
  // the turns from the first change on are worked out again (derive.ts), not the whole history.
  const [kept] = React.useState(() => new Map<string, Turn>());
  const [memo] = React.useState<{ last: TurnsMemo | null }>(() => ({ last: null }));
  const turns = React.useMemo(() => {
    const next = turnsFrom(memo.last, events, parent, going, thread);
    const shared = shareTurns(kept, next.turns);
    memo.last = { ...next, turns: shared };
    return shared;
  }, [kept, memo, events, parent, going, thread]);
  const keeper = useKeeper(placeKey(agent, parent));
  const { read, restore, atEnd, setTail, toEnd } = keeper;
  // Scrolling records the place for the next content update without rendering the conversation.
  const reading = React.useRef<string | null>(null);
  // The turns in the page (`WINDOW` at a time, `MOST` at most): from the one `anchor` names (null:
  // the latest), through the one `till` names (null: the end). Read back, the window stays put as
  // more comes; gone far back (your place from last time, a match you find), it's a window there,
  // and what's after it joins as you scroll down to it.
  const [anchor, setAnchor] = React.useState<string | null>(null);
  const [till, setTill] = React.useState<string | null>(null);
  const anchored = anchor ? turns.findIndex((t) => t.key === anchor) : -1;
  const tilled = till ? turns.findIndex((t) => t.key === till) : -1;
  const from = anchored >= 0 ? anchored : Math.max(0, turns.length - WINDOW);
  const to = Math.min(from + MOST, tilled >= 0 ? Math.max(from + 1, tilled + 1) : turns.length);
  /** A window around turn `i`, when it isn't in the page. */
  const around = (i: number) => {
    if (i < 0 || (i >= from && i < to)) return;
    setAnchor(turns[Math.max(0, i - 3)]?.key ?? null);
    setTill(i + WINDOW < turns.length ? (turns[i + WINDOW - 1]?.key ?? null) : null);
  };
  // Your place from last time, once its turns are in: the page opens on it.
  const [placed, setPlaced] = React.useState<string | null>(() => {
    const p = usePlaces.getState().places[placeKey(agent, parent)];
    return p && "id" in p ? p.id : null;
  });
  if (placed && turns.length > 0) {
    setPlaced(null);
    around(turnOf(turns, placed));
  }
  // A match you find, each time one is shown.
  const shownMatch = find?.now ? `${find.now.id}:${find.now.n}` : null;
  const [revealed, setRevealed] = React.useState<string | null>(null);
  if (shownMatch && shownMatch !== revealed) {
    setRevealed(shownMatch);
    around(turnOf(turns, find?.now?.id));
  }
  // Only a page that holds the end follows what comes at its bottom.
  const tail = to >= turns.length;
  setTail(tail);
  const win = React.useRef({ turns, from, to });
  win.current = { turns, from, to };
  // Near the top of what's in the page: what came before joins (the keeper holds your place), and
  // a window grown past `MOST` lets go of its far end.
  const earlier = React.useCallback((el: HTMLDivElement | null) => {
    if (!el) return;
    const io = new IntersectionObserver(
      ([e]) => {
        if (!e?.isIntersecting) return;
        const w = win.current;
        const start = Math.max(0, w.from - WINDOW);
        setAnchor(w.turns[start]?.key ?? null);
        if (w.to - start > MOST) setTill(w.turns[start + MOST - 1]?.key ?? null);
      },
      { root: el.closest<HTMLElement>(".hx-timeline"), rootMargin: "1500px 0px 0px 0px" },
    );
    io.observe(el);
    return () => io.disconnect();
  }, []);
  // Near the bottom of a window further back: what came after joins, and its near end lets go.
  const later = React.useCallback((el: HTMLDivElement | null) => {
    if (!el) return;
    const io = new IntersectionObserver(
      ([e]) => {
        if (!e?.isIntersecting) return;
        const w = win.current;
        const end = Math.min(w.turns.length, w.to + WINDOW);
        setTill(end >= w.turns.length ? null : (w.turns[end - 1]?.key ?? null));
        if (end - w.from > MOST) setAnchor(w.turns[end - MOST]?.key ?? null);
      },
      { root: el.closest<HTMLElement>(".hx-timeline"), rootMargin: "0px 0px 1500px 0px" },
    );
    io.observe(el);
    return () => io.disconnect();
  }, []);
  const onScroll = () => {
    reading.current = keeper.onScroll();
    // Back at the end: the page holds the latest turns again, and follows them. Read back, it stays put.
    if (atEnd()) {
      if (anchor !== null) setAnchor(null);
    } else if (anchor === null) setAnchor(win.current.turns[win.current.from]?.key ?? null);
  };
  /** To the latest, from a window further back. */
  const latest = () => {
    setAnchor(null);
    setTill(null);
    reading.current = null;
    toEnd();
  };
  // Find: every match marked in place (again as the conversation grows), and the one shown brought
  // into view when it's chosen (not again after: you may scroll away).
  const findQ = find?.q ?? "";
  const findNow = find?.now ?? null;
  const shown = React.useRef(0);
  const grows = findQ ? events : null;
  const holds = reading.current ? turns : null;
  const marks = React.useCallback(
    (flow: HTMLDivElement | null) => {
      const root = flow?.parentElement;
      if (!flow || !root) return;
      // Marked again as the conversation grows, while finding.
      void grows;
      // A bounded window can move without changing height (so no resize is reported).
      void holds;
      void from;
      void to;
      restore();
      const at = findNow ? elementOf(flow, findNow.id) : null;
      const first = mark(flow, findQ, at);
      if (findNow && findNow.n !== shown.current) {
        shown.current = findNow.n;
        const target = first ?? at;
        if (target) {
          bringIntoView(target, root);
          read();
        }
      }
      return () => void mark(flow, "", null);
    },
    [findQ, findNow, grows, holds, from, to, read, restore],
  );
  const running = parent ? task?.status === "running" : state?.status === "running";
  const tasks = state?.tasks ?? NO_TASKS;
  // What you answered here goes from sight at once (answered.ts).
  const approvals = stillAsked(state?.approvals ?? NO_APPROVALS, done);
  const askingKey = approvals.map((a) => a.tool_id ?? "").join(" ");
  const asking = React.useMemo(() => new Set(askingKey.split(" ").filter(Boolean)), [askingKey]);
  // What waits on you sits where it was asked: the latest thing the agent did, the conversation's
  // end. Scrolled away from it, a pill floating over the bottom edge says so (nothing moves).
  const pending = onDecide ? approvals : NO_APPROVALS;
  const hasAsks = pending.length > 0;
  const [endSeen, setEndSeen] = React.useState(true);
  const endRef = React.useCallback((el: HTMLDivElement | null) => {
    if (!el) return;
    const io = new IntersectionObserver(([e]) => setEndSeen(Boolean(e?.isIntersecting)), { root: el.closest<HTMLElement>(".hx-timeline"), rootMargin: `0px 0px ${END_PX}px 0px` });
    io.observe(el);
    return () => io.disconnect();
  }, []);
  const asksEl = React.useRef<HTMLDivElement | null>(null);
  const [asksSeen, setAsksSeen] = React.useState(true);
  const asksRef = React.useCallback((el: HTMLDivElement | null) => {
    asksEl.current = el;
    if (!el) return;
    const io = new IntersectionObserver(([e]) => setAsksSeen(Boolean(e?.isIntersecting)), { root: el.closest<HTMLElement>(".hx-timeline"), threshold: 0.05 });
    io.observe(el);
    return () => io.disconnect();
  }, []);
  const signedOut = React.useMemo(() => {
    if (parent || !kind || accountOpen) return null;
    for (let i = events.length - 1; i >= 0; i--) {
      const e = events[i];
      if (!e || e.parent) continue;
      // It has worked since: nothing to sign in.
      if (e.kind === "assistant" || e.kind === "tool" || e.kind === "thinking") return null;
      if (e.kind === "notice" && e.level !== "info" && saysSignedOut(kind, e.text)) return e.id;
    }
    return null;
  }, [events, parent, kind, accountOpen]);
  const signIn = React.useMemo(() => (signedOut && target && kind ? { notice: signedOut, target, kind } : null), [signedOut, target, kind]);
  return (
    <Contexts asking={asking} signIn={signIn} findNow={findNow} tasks={tasks} files={conversationFiles}>
      {/* Named for the router's scroll memory, so another page's never lands here or this one's there. */}
      <div
        ref={keeper.ref}
        className="hx-timeline"
        data-scroll-restoration-id={`timeline:${placeKey(agent, parent)}`}
        onScroll={onScroll}
        onPointerDownCapture={keeper.onTouch}
        onKeyDownCapture={keeper.onTouch}
      >
        <div ref={marks} className="hx-flow">
          {/* Keyed by where the page starts: each time more joins, a new watch says at once whether more should. */}
          {from > 0 && <div key={`from-${from}`} ref={earlier} className="hx-earlier" aria-hidden="true" />}
          {from === 0 && state && state.dropped > 0 && <div className="hx-dim px-1 pb-3 text-[11px]">{state.dropped} earlier events not shown</div>}
          {turns.length === 0 &&
            (parent ? (
              <div className="hx-sub-empty">
                <p className="hx-sub-what">{task?.description || "Subagent"}</p>
                {task?.prompt && <Prose text={task.prompt} className="hx-sub-prompt" />}
                {task && (
                  <div className="hx-sub-meta">
                    <i className="dot" data-status={taskDot(task)} aria-hidden="true" />
                    <PrismLabel live={going}>{task.status}</PrismLabel>
                    <TaskElapsed task={task} />
                    {going && task.last_tool && <span className="hx-trunc">{task.last_tool}</span>}
                  </div>
                )}
                {task?.summary ? <Prose text={task.summary} className="hx-sub-result" /> : <p className="hx-dim text-[12px]">No work has been reported in this thread yet.</p>}
              </div>
            ) : (
              <div className="hx-empty">Nothing yet. Tell the agent what to do.</div>
            ))}
          {turns.slice(from, to).map((t, j) => (
            <TurnView
              key={t.key}
              turn={t}
              cwd={cwd}
              delivery={t.prompt?.client_id ? state?.delivery[t.prompt.client_id] : undefined}
              live={running && from + j === turns.length - 1}
              doing={running && from + j === turns.length - 1 ? state?.doing : undefined}
              turnStarted={running && from + j === turns.length - 1 ? parent ? task?.started : state?.turn_started ?? undefined : undefined}
              onOpenTask={onOpenTask}
              meta={meta}
              onCommand={onCommand}
              agentLabel={agentLabel}
              reading={reading.current}
            />
          ))}
          {/* A window further back: what came after joins as you scroll down to it. */}
          {!tail && <div key={`to-${to}`} ref={later} className="hx-earlier" aria-hidden="true" />}
          {tail && hasAsks && onDecide && (
            <div ref={asksRef} className="hx-asks">
              {pending.map((a) => (
                <ApprovalCard key={a.id} a={a} cwd={cwd} onDecide={onDecide} agentName={agentName} kind={agentLabel} agentId={agent} />
              ))}
            </div>
          )}
          {tail && footer}
          <div ref={endRef} aria-hidden="true" />
          <div className="hx-waiting-anchor">
            {!tail || (!hasAsks && !endSeen) ? (
              <button type="button" className="hx-waiting" onClick={latest}>
                {hasAsks && <i aria-hidden="true" />}
                {hasAsks ? (pending.length === 1 ? "Waiting on you" : `${pending.length} waiting on you`) : "Latest"}
                <ArrowDown size={12} aria-hidden="true" />
              </button>
            ) : (
              hasAsks &&
              !asksSeen && (
                <button type="button" className="hx-waiting" onClick={() => asksEl.current?.scrollIntoView({ block: "start", behavior: "smooth" })}>
                  <i aria-hidden="true" />
                  {pending.length === 1 ? "Waiting on you" : `${pending.length} waiting on you`}
                  <ArrowDown size={12} aria-hidden="true" />
                </button>
              )
            )}
          </div>
        </div>
      </div>
    </Contexts>
  );
});
