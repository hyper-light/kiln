/**
 * What an agent leaves running: shells it started in the background, monitors that wake it when
 * something happens, MCP calls it moved aside. Each says what it runs and how it's doing; its
 * output reads live; it stops with one tap, or all of them at once.
 */
import * as React from "react";
import { ChevronRight, Square } from "lucide-react";
import { harnessApi, useSession, type Command, type SessionStore, type State, type Task } from "@/api/harness";
import { useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { duration, tildify } from "./derive";
import { glyphKind, KindGlyph, type GlyphKind } from "./kind-glyph";

const BACKGROUND = new Set(["shell", "monitor", "mcp"]);
/** What a task is; machines from before `class` only say the harness's own task type. */
export const classOf = (t: Task): string =>
  t.class ??
  (t.kind?.startsWith("monitor") ? "monitor" : t.kind === "local_bash" ? "shell" : t.kind === "local_workflow" ? "workflow" : t.kind?.includes("mcp") ? "mcp" : "agent");
export const isBackground = (t: Task) => BACKGROUND.has(classOf(t));
const isLive = (t: Task) => t.status === "running" || t.status === "pending" || t.status === "paused";
const WORD: Record<string, string> = { shell: "Shell", monitor: "Monitor", mcp: "MCP call" };

export function backgroundOf(state: State | null): Task[] {
  return (state?.tasks ?? []).filter(isBackground);
}

interface Tail {
  text: string;
  note: string | null;
  offset?: number;
}

const NO_TAIL: Tail = { text: "", note: null };
/** Each task's output read so far (opened again, it's there at once and goes on from where it was). */
const tails = new Map<string, Tail>();
const readers = new Map<string, number>();

function pruneTails() {
  let idle = 0;
  for (const key of [...tails.keys()].reverse()) {
    if (!readers.has(key) && ++idle > 32) tails.delete(key);
  }
}

/** A task's output: read while shown, every moment while it runs (subscribed to as a store is). */
function useTail(agent: string, task: string, live: boolean): Tail {
  const key = `${agent}\n${task}`;
  const subscribe = React.useCallback(
    (changed: () => void) => {
      let stop = false;
      let timer = 0;
      readers.set(key, (readers.get(key) ?? 0) + 1);
      const poll = async () => {
        if (live && document.hidden) {
          timer = window.setTimeout(poll, 1500);
          return;
        }
        const was = tails.get(key) ?? NO_TAIL;
        try {
          const r = await harnessApi.taskOutput(agent, task, was.offset);
          // Read meanwhile by another view of it: that one's is the newer.
          if (stop || (tails.get(key) ?? NO_TAIL) !== was) return;
          tails.set(
            key,
            r.error
              ? { ...was, note: r.error }
              : { text: r.data ? (was.text + r.data).slice(-200_000) : was.text, offset: r.offset ?? was.offset, note: r.gone ? "Its output file is gone: the task has ended." : null },
          );
          changed();
        } catch (e) {
          if (stop) return;
          tails.set(key, { ...(tails.get(key) ?? NO_TAIL), note: e instanceof Error ? e.message : String(e) });
          changed();
        } finally {
          if (!stop && live) timer = window.setTimeout(poll, 1500);
        }
      };
      void poll();
      return () => {
        stop = true;
        window.clearTimeout(timer);
        const count = (readers.get(key) ?? 1) - 1;
        if (count) readers.set(key, count);
        else readers.delete(key);
        pruneTails();
      };
    },
    [key, agent, task, live],
  );
  return React.useSyncExternalStore(
    subscribe,
    () => tails.get(key) ?? NO_TAIL,
    () => NO_TAIL,
  );
}

/** The task's output, tailed while it runs. */
export function Output({ agent, task }: { agent: string; task: Task }) {
  const { text, note } = useTail(agent, task.id, isLive(task));
  // The newest output stays in view.
  const end = React.useCallback(
    (el: HTMLPreElement | null) => {
      void text;
      if (el) el.scrollTop = el.scrollHeight;
    },
    [text],
  );
  return (
    <div className="bg-out">
      <pre ref={end}>{text || (note ? "" : "No output yet.")}</pre>
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
    classOf(t) === "monitor" && t.events ? `${t.events} event${t.events === 1 ? "" : "s"}` : "",
    classOf(t) === "monitor" && t.wakes ? (t.wakes === "agent" ? "wakes the agent" : "wakes its subagent") : "",
    t.cwd ? tildify(t.cwd) : "",
  ].filter(Boolean);
  return (
    <li className="bg-row" data-live={live || undefined}>
      <div className="bg-main">
        <span className="bg-class">{WORD[classOf(t)] ?? classOf(t)}</span>
        <span className="bg-what" title={what}>
          {what}
        </span>
        <span className="bg-facts">{facts.join(" · ")}</span>
        {classOf(t) === "monitor" && t.last_event && <span className="bg-last">Latest: {t.last_event}</span>}
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
  const [ended, setEnded] = React.useState(false);
  const tasks = backgroundOf(state);
  if (tasks.length === 0) return null;
  const live = tasks.filter(isLive);
  const done = tasks.filter((t) => !isLive(t)).reverse();
  return (
    <section>
      <h3 className="in-sec-head">
        In the background
        <span>
          {live.length} running
          {live.length > 1 && !confirm && (
            <button type="button" className="bg-all" onClick={() => setConfirm(true)}>
              Stop all
            </button>
          )}
        </span>
      </h3>
      {confirm && (
        <div className="bg-confirm" role="group" aria-label="Stop everything in the background">
          <span>Stop the {live.length} shells and monitors it left running? The agent keeps going.</span>
          <button
            type="button"
            className="bg-all"
            onClick={() => {
              setConfirm(false);
              send({ type: "stop_tasks" });
            }}
          >
            Stop all
          </button>
          <button type="button" className="bg-all" onClick={() => setConfirm(false)}>
            Keep them
          </button>
        </div>
      )}
      <ul className="bg-list">
        {live.map((t) => (
          <Row key={t.id} t={t} agent={agent} send={send} />
        ))}
        {done.length > 0 && (
          <li className="bg-ended">
            <button type="button" className="bg-all" onClick={() => setEnded(!ended)}>
              {ended ? "Hide the finished ones" : `${done.length} finished`}
            </button>
          </li>
        )}
        {ended && done.map((t) => <Row key={t.id} t={t} agent={agent} send={send} />)}
      </ul>
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
const NO_TASKS: Task[] = [];

/** How long it has run, counting by itself (only this text renders each second). */
function Since({ from }: { from: number }) {
  const now = useNow();
  return <>{duration(now - from)}</>;
}

export function ActivityLine({ store, agent, onOpenTask }: { store: SessionStore; agent: string; onOpenTask: (toolId: string) => void }) {
  const [open, setOpen] = React.useState(false);
  const [shown, setShown] = React.useState<string | null>(null);
  const tasks = useSession(store, (s) => s.state?.tasks ?? NO_TASKS);
  const subs = tasks.filter((t) => !isBackground(t) && isLive(t));
  const bg = tasks.filter((t) => isBackground(t) && isLive(t));
  if (subs.length + bg.length === 0) return null;
  const count = (n: number, one: string, many = `${one}s`) => (n ? `${n} ${n === 1 ? one : many}` : "");
  const shells = bg.filter((t) => classOf(t) === "shell").length;
  const monitors = bg.filter((t) => classOf(t) === "monitor").length;
  const other = bg.length - shells - monitors;
  // One glyph per kind, so the line says what is running at a glance.
  const segments: { kind: GlyphKind; text: string; live: boolean }[] = [
    { kind: "agent" as const, text: count(subs.length, "subagent"), live: subs.some((t) => t.status === "running") },
    { kind: "shell" as const, text: count(shells, "shell"), live: true },
    { kind: "monitor" as const, text: count(monitors, "monitor"), live: true },
    { kind: "mcp" as const, text: count(other, "background call"), live: true },
  ].filter((s) => s.text);
  return (
    <div className="al" data-open={open || undefined}>
      <button type="button" className="al-line" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span className="al-summary">
          {segments.map((s, i) => (
            <span key={s.kind} className="al-seg">
              {i > 0 && <span className="al-sep">·</span>}
              <KindGlyph kind={s.kind} live={s.live} />
              {s.text}
            </span>
          ))}
        </span>
        <ChevronRight size={12} className={cn("al-chev", open && "rotate-90")} aria-hidden="true" />
      </button>
      {open && (
        <ul className="al-list">
          {[...subs, ...bg].map((t) => {
            const isSub = !isBackground(t);
            const last = isSub ? t.last_tool : classOf(t) === "monitor" ? t.last_event : undefined;
            // The header already says what they are; a row names its kind only in a mixed list.
            const mixed = subs.length > 0 && bg.length > 0;
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
                  <KindGlyph kind={isSub ? "agent" : glyphKind(classOf(t))} live={t.status === "running"} pending={t.status === "pending"} />
                  <span className="al-what">
                    {mixed && <span className="al-kind">{isSub ? "Subagent" : (WORD[classOf(t)] ?? classOf(t))}</span>}
                    <span className="al-desc">{t.description || t.command || t.id}</span>
                  </span>
                  <span className="al-time">
                    {last && <span className="al-last">{last}</span>}
                    <Since from={t.started} />
                  </span>
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
