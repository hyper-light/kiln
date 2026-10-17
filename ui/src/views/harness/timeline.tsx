/**
 * An agent's work as turns: what you asked, the steps it took (grouped: "Read 6 files",
 * "Edited 3 files +42 −7", "Ran cargo test — 2 failed"), its notes along the way, and its answer.
 * Every step opens into a view made for its kind: real diffs, command output with exit codes,
 * the files it read, the subagent it started.
 */
import * as React from "react";
import { Brain, ChevronRight, CircleAlert, CornerDownRight, Info, TriangleAlert } from "lucide-react";
import type { Delivery, HEvent, State, Task } from "@/api/harness";
import { cn } from "@/lib/utils";
import { changesetOf, diffStats, duration, relPath, stepStatus, stepSummary, turnsOf, type Item, type Step, type ToolEvent, type Turn } from "./derive";
import { DiffStat, FileDiffView } from "./diff";
import { Output, Prose, ToolIcon } from "./parts";

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

const dotFor = (s: ToolEvent["status"]) =>
  s === "done" ? "succeeded" : s === "error" ? "failed" : s === "running" ? "running" : s === "pending" ? "queued" : s === "denied" ? "timed_out" : "cancelled";

function Elapsed({ from, to }: { from: number; to?: number | null }) {
  const [now, setNow] = React.useState(() => Date.now());
  React.useEffect(() => {
    if (to) return;
    const t = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(t);
  }, [to]);
  return <span className="hx-meta-num">{duration((to ?? now) - from)}</span>;
}

/* ------------------------------------------------------------------ step bodies */

function ExploreBody({ calls, cwd }: { calls: ToolEvent[]; cwd: string }) {
  return (
    <ul className="hx-list">
      {calls.map((c) => (
        <li key={c.id} className="hx-list-row">
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
  return (
    <div className="hx-stack">
      {calls.map((c) =>
        (c.diffs?.length ?? 0) > 0 ? (
          (c.diffs ?? []).map((d, i) => (
            <div key={`${c.id}-${i}`} className={cn(c.status === "denied" && "opacity-60")}>
              {c.status === "denied" && <div className="hx-dim mb-1 text-[11px]">Refused — not applied</div>}
              <FileDiffView diff={d} label={relPath(d.path, cwd)} defaultOpen={!many} />
            </div>
          ))
        ) : (
          <div key={c.id} className="hx-list-row">
            <ToolIcon kind="edit" size={12} />
            <span className="hx-mono hx-trunc">{relPath(c.title, cwd)}</span>
            {c.status === "error" && c.output && <span className="hx-bad hx-trunc">{c.output}</span>}
          </div>
        ),
      )}
    </div>
  );
}

function CommandRow({ c, open: initial }: { c: ToolEvent; open?: boolean }) {
  const [open, setOpen] = React.useState(Boolean(initial));
  const failed = c.status === "error" || (c.exit_code !== undefined && c.exit_code !== 0);
  return (
    <div className="hx-cmd" data-failed={failed || undefined}>
      <button type="button" className="hx-cmd-head" aria-expanded={open} onClick={() => setOpen(!open)}>
        <i className="dot" data-status={dotFor(failed ? "error" : c.status)} aria-hidden="true" />
        <code className="hx-cmd-line">{c.title}</code>
        <span className="hx-cmd-meta">
          {c.exit_code !== undefined && <span className={failed ? "hx-bad" : "hx-good"}>exit {c.exit_code}</span>}
          {c.status === "running" ? <Elapsed from={c.started} /> : c.ended ? <span className="hx-meta-num">{duration(c.ended - c.started)}</span> : null}
        </span>
      </button>
      {c.detail && <div className="hx-cmd-why">{c.detail}</div>}
      {open && <Output text={c.output ?? (c.status === "running" ? "" : "(no output)")} truncated={c.truncated} error={failed} />}
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
    return <Prose text={plan} className="hx-plan-text" />;
  }
  return <TodoList todos={last.todos ?? []} />;
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
          <div key={c.id} className="hx-ask">
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
      {calls.map((c) => (
        <div key={c.id} className="hx-generic">
          <div className="hx-list-row">
            <ToolIcon kind={c.tool} size={12} />
            <span className="hx-mono">{c.name}</span>
            <span className="hx-dim hx-trunc">{c.title}</span>
          </div>
          {c.output && <Output text={c.output} truncated={c.truncated} error={c.status === "error"} />}
        </div>
      ))}
    </div>
  );
}

function AgentBody({ calls, tasks, onOpenTask }: { calls: ToolEvent[]; tasks: Task[]; onOpenTask?: (t: string) => void }) {
  return (
    <div className="hx-stack">
      {calls.map((c) => {
        const task = tasks.find((t) => t.tool_id === c.tool_id || t.id === c.task);
        return (
          <div key={c.id} className="hx-sub">
            <div className="hx-sub-head">
              <i className="dot" data-status={dotFor(task?.status === "running" ? "running" : c.status)} aria-hidden="true" />
              <span className="hx-sub-title">{task?.description ?? c.title}</span>
              {task?.kind && <span className="hx-tag">{task.kind}</span>}
            </div>
            <div className="hx-sub-meta">
              {task?.tool_uses ? <span>{task.tool_uses} steps</span> : null}
              {task?.duration_ms ? <span>{duration(task.duration_ms)}</span> : task && !task.ended ? <Elapsed from={task.started} /> : null}
              {task?.last_tool && !task.ended && <span className="hx-trunc">{task.last_tool}</span>}
              {onOpenTask && (
                <button type="button" className="hx-link" onClick={() => onOpenTask(c.tool_id)}>
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

const StepRow = React.memo(function StepRow({
  step,
  cwd,
  tasks,
  onOpenTask,
}: {
  step: Step;
  cwd: string;
  tasks: Task[];
  onOpenTask?: (t: string) => void;
}) {
  const status = stepStatus(step.calls);
  const failures = step.calls.filter((c) => c.status === "error" || (c.tool === "shell" && c.exit_code !== undefined && c.exit_code !== 0)).length;
  // Open by default: what needs attention, and what is happening right now.
  const [open, setOpen] = React.useState<boolean | null>(null);
  const isOpen = open ?? (status === "running" || failures > 0 || step.group === "agent" || step.group === "ask" || (step.group === "plan" && step.calls.some((c) => c.tool === "plan")));
  const stats = step.group === "edit" ? diffStats(step.calls.filter((c) => c.status !== "denied").flatMap((c) => c.diffs ?? [])) : null;
  const first = step.calls[0];
  const last = step.calls.at(-1);
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
            ? (tasks.find((t) => t.tool_id === first?.tool_id)?.description ?? first?.title)
            : step.calls.length === 1
              ? first?.title
              : undefined;

  return (
    <div className="hx-step" data-group={step.group} data-status={status}>
      <button type="button" className="hx-step-head" aria-expanded={isOpen} onClick={() => setOpen(!isOpen)}>
        <span className="hx-step-icon">
          <ToolIcon kind={STEP_ICON[step.group]} size={13} />
        </span>
        <span className="hx-step-title">{stepSummary(step.group, step.calls)}</span>
        {subtitle && <span className="hx-step-sub">{subtitle}</span>}
        <span className="hx-step-end">
          {stats && <DiffStat add={stats.add} del={stats.del} />}
          {failures > 0 && <span className="hx-bad">{failures} failed</span>}
          {status === "running" && first ? <Elapsed from={first.started} /> : first && last?.ended ? <span className="hx-meta-num">{duration(last.ended - first.started)}</span> : null}
          <i className="dot" data-status={dotFor(failures > 0 ? "error" : status)} aria-hidden="true" />
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

function Thought({ e }: { e: Extract<HEvent, { kind: "thinking" }> }) {
  const [open, setOpen] = React.useState(false);
  if (e.redacted) return null;
  return (
    <div className="hx-thought">
      <button type="button" className="hx-thought-head" aria-expanded={open} onClick={() => setOpen(!open)}>
        <Brain size={12} /> {e.streaming ? "Thinking…" : "Thought"}
        <ChevronRight size={12} className={cn("hx-chev", open && "rotate-90")} />
      </button>
      {open && <Prose text={e.text} className="hx-thought-text" />}
    </div>
  );
}

function Notice({ e }: { e: Extract<HEvent, { kind: "notice" }> }) {
  const I = e.level === "error" ? CircleAlert : e.level === "warn" ? TriangleAlert : Info;
  return (
    <div className="hx-notice" data-level={e.level}>
      <I size={12} /> <span>{e.text}</span>
    </div>
  );
}

const DELIVERY: Record<Delivery, string> = {
  sent: "sent",
  queued: "queued by the agent",
  started: "being worked on",
  completed: "done",
  cancelled: "dropped",
};

function Prompt({ e, delivery }: { e: Extract<HEvent, { kind: "user" }>; delivery?: Delivery }) {
  const system = e.origin && e.origin !== "human";
  return (
    <div className="hx-prompt" data-origin={e.origin ?? "human"}>
      <div className="hx-prompt-meta">
        <span className="hx-eyebrow">{system ? e.origin : "You"}</span>
        <span className="hx-dim">{new Date(e.at).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}</span>
        {delivery && <span className="hx-dim">· {DELIVERY[delivery]}</span>}
        {e.images ? <span className="hx-dim">· {e.images} image{e.images === 1 ? "" : "s"}</span> : null}
      </div>
      <div className={cn("hx-prompt-text", system && "hx-dim")}>{e.text}</div>
    </div>
  );
}

function ItemView({ item, cwd, tasks, onOpenTask }: { item: Item; cwd: string; tasks: Task[]; onOpenTask?: (t: string) => void }) {
  switch (item.type) {
    case "step":
      return <StepRow step={item} cwd={cwd} tasks={tasks} onOpenTask={onOpenTask} />;
    case "note":
      return <Prose text={item.event.text} className={cn("hx-note", item.event.streaming && "hx-streaming")} />;
    case "thought":
      return <Thought e={item.event} />;
    case "notice":
      return <Notice e={item.event} />;
  }
}

const TurnView = React.memo(function TurnView({
  turn,
  cwd,
  tasks,
  delivery,
  live,
  onOpenTask,
}: {
  turn: Turn;
  cwd: string;
  tasks: Task[];
  delivery?: Delivery;
  live: boolean;
  onOpenTask?: (t: string) => void;
}) {
  const edits = React.useMemo(() => changesetOf(turn.items.flatMap((i) => (i.type === "step" ? i.calls : []))), [turn]);
  const add = edits.reduce((n, f) => n + f.add, 0);
  const del = edits.reduce((n, f) => n + f.del, 0);
  return (
    <section className="hx-turn">
      {turn.prompt && <Prompt e={turn.prompt} delivery={delivery} />}
      <div className="hx-work">
        {turn.items.map((it) => (
          <ItemView key={it.key} item={it} cwd={cwd} tasks={tasks} onOpenTask={onOpenTask} />
        ))}
        {live && turn.items.length === 0 && !turn.outcome && <div className="hx-dim hx-pulse text-[12px]">Starting…</div>}
      </div>
      {turn.outcome && (
        <div className="hx-outcome">
          <Prose text={turn.outcome.text} />
          {(edits.length > 0 || turn.ended) && (
            <div className="hx-outcome-foot">
              {edits.length > 0 && (
                <span>
                  {edits.length} file{edits.length === 1 ? "" : "s"} changed <DiffStat add={add} del={del} />
                </span>
              )}
              {turn.ended && <span className="hx-meta-num">{duration(turn.ended - turn.started)}</span>}
            </div>
          )}
        </div>
      )}
    </section>
  );
});

/**
 * The timeline of the main thread, or of one subagent (`parent`: its spawning tool call). Keeps
 * itself scrolled to the newest work while you are at the bottom, and leaves you be when you
 * scrolled up to read.
 */
export function Timeline({
  events,
  state,
  cwd,
  parent,
  onOpenTask,
  footer,
}: {
  events: HEvent[];
  state: State | null;
  cwd: string;
  parent?: string;
  onOpenTask?: (t: string) => void;
  footer?: React.ReactNode;
}) {
  const turns = React.useMemo(() => turnsOf(events, parent), [events, parent]);
  const ref = React.useRef<HTMLDivElement>(null);
  const pinned = React.useRef(true);
  const onScroll = () => {
    const el = ref.current;
    if (el) pinned.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
  };
  React.useLayoutEffect(() => {
    const el = ref.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  });
  const running = state?.status === "running";
  const tasks = state?.tasks ?? [];
  return (
    <div ref={ref} className="hx-timeline" onScroll={onScroll}>
      {state && state.dropped > 0 && <div className="hx-dim px-1 pb-3 text-[11px]">{state.dropped} earlier events not shown</div>}
      {turns.length === 0 && <div className="hx-empty">Nothing yet. Tell the agent what to do.</div>}
      {turns.map((t, i) => (
        <TurnView
          key={t.key}
          turn={t}
          cwd={cwd}
          tasks={tasks}
          delivery={t.prompt?.client_id ? state?.delivery[t.prompt.client_id] : undefined}
          live={running && i === turns.length - 1}
          onOpenTask={onOpenTask}
        />
      ))}
      {footer}
    </div>
  );
}
