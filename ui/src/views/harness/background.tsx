/**
 * What an agent leaves running: shells it started in the background, monitors that wake it when
 * something happens, MCP calls it moved aside. Each says what it runs and how it's doing; its
 * output reads live; it stops with one tap, or all of them at once.
 */
import * as React from "react";
import { ChevronRight, Square } from "lucide-react";
import { harnessApi, type Command, type State, type Task } from "@/api/harness";
import { ConfirmDialog } from "@/components/ui/dialog";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { duration, tildify } from "./derive";
import { Optic } from "./signals";

const BACKGROUND = new Set(["shell", "monitor", "mcp"]);
export const isBackground = (t: Task) => (t.class ? BACKGROUND.has(t.class) : false);
const isLive = (t: Task) => t.status === "running" || t.status === "pending" || t.status === "paused";
const WORD: Record<string, string> = { shell: "Shell", monitor: "Monitor", mcp: "MCP call" };

export function backgroundOf(state: State | null): Task[] {
  return (state?.tasks ?? []).filter(isBackground);
}

/** The task's output, tailed while it runs. */
export function Output({ agent, task }: { agent: string; task: Task }) {
  const [text, setText] = React.useState("");
  const [note, setNote] = React.useState<string | null>(null);
  const offset = React.useRef<number | undefined>(undefined);
  const pre = React.useRef<HTMLPreElement>(null);
  const live = isLive(task);
  React.useEffect(() => {
    let stop = false;
    let timer = 0;
    const poll = async () => {
      try {
        const r = await harnessApi.taskOutput(agent, task.id, offset.current);
        if (stop) return;
        if (r.error) setNote(r.error);
        else {
          if (r.data) setText((t) => (t + r.data).slice(-200_000));
          if (r.offset !== undefined) offset.current = r.offset;
          setNote(r.gone ? "Its output file is gone: the task has ended." : null);
        }
      } catch (e) {
        if (!stop) setNote(e instanceof Error ? e.message : String(e));
      }
      if (!stop && live) timer = window.setTimeout(poll, 1500);
    };
    void poll();
    return () => {
      stop = true;
      window.clearTimeout(timer);
    };
  }, [agent, task.id, live]);
  React.useLayoutEffect(() => {
    const el = pre.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [text]);
  return (
    <div className="bg-out">
      <pre ref={pre}>{text || (note ? "" : "No output yet.")}</pre>
      {note && <p className="bg-note">{note}</p>}
    </div>
  );
}

function Row({ t, agent, send }: { t: Task; agent: string; send: (c: Command) => void }) {
  const [open, setOpen] = React.useState(false);
  const now = useNow();
  const live = isLive(t);
  const what = t.command || t.description || t.id;
  const facts = [
    live ? (t.status === "paused" ? "paused" : `running ${duration(now - t.started)}`) : t.exit_code !== undefined ? `ended · exit ${t.exit_code}` : `ended ${t.status !== "completed" ? `· ${t.status}` : ""}`.trim(),
    t.class === "monitor" && t.events ? `${t.events} event${t.events === 1 ? "" : "s"}` : "",
    t.class === "monitor" && t.wakes ? (t.wakes === "agent" ? "wakes the agent" : "wakes its subagent") : "",
    t.cwd ? tildify(t.cwd) : "",
  ].filter(Boolean);
  return (
    <li className="bg-row" data-live={live || undefined}>
      <div className="bg-main">
        <span className="bg-class">{WORD[t.class ?? ""] ?? t.class}</span>
        <span className="bg-what" title={what}>
          {what}
        </span>
        <span className="bg-facts">{facts.join(" · ")}</span>
        {t.class === "monitor" && t.last_event && <span className="bg-last">Latest: {t.last_event}</span>}
      </div>
      <div className="bg-actions">
        <button type="button" className="bg-btn" aria-expanded={open} onClick={() => setOpen(!open)}>
          <ChevronRight size={12} className={cn("bg-chev", open && "rotate-90")} aria-hidden="true" /> Output
        </button>
        {live && t.stoppable !== false && (
          <button type="button" className="bg-btn bg-stop" onClick={() => send({ type: "stop_task", task: t.id })}>
            <Square size={9} fill="currentColor" aria-hidden="true" /> Stop
          </button>
        )}
      </div>
      {open && <Output agent={agent} task={t} />}
    </li>
  );
}

/** The inspector's section: everything in the background, running first. */
export function Background({ state, agent, send }: { state: State | null; agent: string; send: (c: Command) => void }) {
  const [confirm, setConfirm] = React.useState(false);
  const tasks = backgroundOf(state);
  if (tasks.length === 0) return null;
  const live = tasks.filter(isLive);
  const ordered = [...live, ...tasks.filter((t) => !isLive(t)).reverse()];
  return (
    <section>
      <h3 className="in-sec-head">
        In the background
        <span>
          {live.length} running
          {live.length > 1 && (
            <button type="button" className="bg-all" onClick={() => setConfirm(true)}>
              Stop all
            </button>
          )}
        </span>
      </h3>
      <ul className="bg-list">
        {ordered.map((t) => (
          <Row key={t.id} t={t} agent={agent} send={send} />
        ))}
      </ul>
      <ConfirmDialog
        open={confirm}
        onOpenChange={setConfirm}
        title={`Stop ${live.length} background tasks?`}
        description="The shells and monitors this agent left running are stopped on its machine. The agent keeps going."
        confirmLabel="Stop all"
        danger
        onConfirm={() => send({ type: "stop_tasks" })}
      />
    </section>
  );
}

/** Above the composer: a quiet line while anything runs in the background. */
export function BackgroundStrip({ state, onView }: { state: State | null; onView: () => void }) {
  const live = backgroundOf(state).filter(isLive);
  if (live.length === 0) return null;
  const names = live.map((t) => t.command || t.description || WORD[t.class ?? ""] || "task");
  return (
    <button type="button" className="bg-strip" onClick={onView}>
      <i aria-hidden="true" />
      <span className="bg-strip-n">{live.length} in the background</span>
      <span className="bg-strip-what">{names.join(" · ")}</span>
      <ChevronRight size={13} aria-hidden="true" />
    </button>
  );
}

/**
 * By the composer: what's running besides the agent itself — its subagents, and the shells and
 * monitors it started — as one line that opens in place. A subagent opens its thread; a shell or
 * monitor shows its output right there.
 */
export function ActivityLine({ state, agent, onOpenTask }: { state: State | null; agent: string; onOpenTask: (toolId: string) => void }) {
  const [open, setOpen] = React.useState(false);
  const [shown, setShown] = React.useState<string | null>(null);
  const now = useNow();
  const tasks = state?.tasks ?? [];
  const subs = tasks.filter((t) => !isBackground(t) && isLive(t));
  const bg = tasks.filter((t) => isBackground(t) && isLive(t));
  if (subs.length + bg.length === 0) return null;
  const count = (n: number, one: string, many = `${one}s`) => (n ? `${n} ${n === 1 ? one : many}` : "");
  const shells = bg.filter((t) => t.class === "shell").length;
  const monitors = bg.filter((t) => t.class === "monitor").length;
  const other = bg.length - shells - monitors;
  const summary = [count(subs.length, "subagent"), count(shells, "shell"), count(monitors, "monitor"), count(other, "background call")].filter(Boolean).join(" · ");
  return (
    <div className="al" data-open={open || undefined}>
      <button type="button" className="al-line" aria-expanded={open} onClick={() => setOpen(!open)}>
        <Optic state="tool" />
        <span className="al-summary">{summary}</span>
        <ChevronRight size={12} className={cn("al-chev", open && "rotate-90")} aria-hidden="true" />
      </button>
      {open && (
        <ul className="al-list">
          {[...subs, ...bg].map((t) => {
            const isSub = !isBackground(t);
            const last = isSub ? t.last_tool : t.class === "monitor" ? t.last_event : undefined;
            return (
              <li key={t.id} className="al-item">
                <button
                  type="button"
                  className="al-row"
                  onClick={() => {
                    if (isSub && t.tool_id) onOpenTask(t.tool_id);
                    else setShown(shown === t.id ? null : t.id);
                  }}
                >
                  <Optic state={t.status === "pending" ? "thinking" : "tool"} />
                  <span className="al-what">
                    <span className="al-kind">{isSub ? "Subagent" : (WORD[t.class ?? ""] ?? t.class)}</span>
                    <span className="al-desc">{t.description || t.command || t.id}</span>
                  </span>
                  <span className="al-time">{duration(now - t.started)}</span>
                  {last && <span className="al-last">{last}</span>}
                </button>
                {shown === t.id && <Output agent={agent} task={t} />}
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
