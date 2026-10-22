/**
 * What an agent leaves running: shells it started in the background, monitors that wake it when
 * something happens, MCP calls it moved aside. Each says what it runs and how it's doing; its
 * output reads live; it stops with one tap, or all of them at once.
 */
import * as React from "react";
import { ChevronRight, Square } from "lucide-react";
import { harnessApi, useSession, type Command, type SessionStore, type State, type Task } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Confirm } from "@/components/ui/misc";
import { toast } from "@/components/ui/toast";
import { errorMessage, useNow } from "@/lib/hooks";
import { cn } from "@/lib/utils";
import { duration, tildify } from "./derive";
import { glyphKind, KindGlyph, type GlyphKind } from "./kind-glyph";
import { Shimmer } from "./signals";
import { classOf, isSubagent, LEFT, stopperOf, subagentRunning, subagentTasksAtWork } from "./tasks";

const BACKGROUND = new Set(["shell", "monitor", "mcp"]);
export const isBackground = (t: Task) => BACKGROUND.has(classOf(t));
const isLive = (t: Task) => t.status === "running" || t.status === "pending" || t.status === "paused";
const WORD: Record<string, string> = { shell: "Shell", monitor: "Monitor", workflow: "Workflow", mcp: "MCP call" };

export function backgroundOf(state: State | null): Task[] {
  return (state?.tasks ?? []).filter(isBackground);
}

interface Tail {
  text: string;
  note: string | null;
  offset?: number;
  /** Read at least once. */
  read?: boolean;
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
              : {
                  text: r.reset ? (r.data ?? "").slice(-200_000) : r.data ? (was.text + r.data).slice(-200_000) : was.text,
                  offset: r.offset ?? was.offset,
                  read: true,
                  // No file while it runs: its harness keeps none for it (a Claude Code monitor's events).
                  note: r.gone ? (r.live ? "Its harness keeps no output for it that kiln can read." : "Its output file is gone: the task has ended.") : null,
                },
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

/** The task's output, tailed while it runs. What kiln took over when a harness stopped has none kept. */
export function Output({ agent, task }: { agent: string; task: Task }) {
  if (task.id.startsWith(LEFT))
    return (
      <div className="bg-out">
        <p className="bg-note">kiln took this over when its agent&rsquo;s harness stopped, so its output isn&rsquo;t kept.</p>
      </div>
    );
  return <Tailed agent={agent} task={task} />;
}

function Tailed({ agent, task }: { agent: string; task: Task }) {
  const { text, note, read } = useTail(agent, task.id, isLive(task));
  // The newest output stays in view.
  const end = React.useCallback(
    (el: HTMLPreElement | null) => {
      void text;
      if (el) el.scrollTop = el.scrollHeight;
    },
    [text],
  );
  // Nothing to show but why: that one line alone.
  return (
    <div className="bg-out">
      {(text || !note) && <pre ref={end}>{text || (read ? "No output yet." : "Reading its output…")}</pre>}
      {note && <p className="bg-note">{note.charAt(0).toUpperCase() + note.slice(1)}</p>}
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
        {live && (t.id.startsWith(LEFT) || t.stoppable !== false) && (
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
 * monitor shows its output right there, following it while it runs. Each stops from its row.
 */
const NO_TASKS: Task[] = [];
const NO_FEATURES: string[] = [];

/** How long it has run, counting by itself (only this text renders each second). */
function Since({ from }: { from: number }) {
  const now = useNow();
  return <>{duration(now - from)}</>;
}

/** What stopping it ends, said before it's done; a monitor only watches, so it stops at once. */
const STOP_ASK: Record<string, string> = {
  agent: "Stop this subagent? What it hasn't finished is lost; the agent keeps going.",
  shell: "Stop this shell? What it runs is killed; the agent keeps going.",
  workflow: "Stop this workflow? What it hasn't finished is lost; the agent keeps going.",
  mcp: "Stop this call? Its answer is lost; the agent keeps going.",
};

/** A stop asked: said for a while (one that takes ends the row), then Stop is offered again. */
function Stopping({ at, children }: { at: number; children: React.ReactNode }) {
  const now = useNow();
  return now - at < 15_000 ? (
    <span className="al-stop" data-busy="" role="status">
      Stopping…
    </span>
  ) : (
    children
  );
}

function ActivityItem({ t, store, mixed, shown, onShow, onOpenTask }: { t: Task; store: SessionStore; mixed: boolean; shown: boolean; onShow: () => void; onOpenTask: (toolId: string) => void }) {
  const features = useSession(store, (s) => s.meta?.features ?? NO_FEATURES);
  const harness = useSession(store, (s) => s.info?.kind_name ?? "");
  const [asking, setAsking] = React.useState(false);
  const [why, setWhy] = React.useState(false);
  const [stopAt, setStopAt] = React.useState<number | null>(null);
  const sub = isSubagent(t);
  const cls = classOf(t);
  const noun = sub ? "subagent" : cls === "mcp" ? "call" : cls;
  // A subagent opens its thread; anything else (or a subagent without one) its output, under it.
  const thread = sub ? t.tool_id : undefined;
  const last = sub ? t.last_tool : cls === "monitor" ? t.last_event : undefined;
  const stop = () => {
    setAsking(false);
    setStopAt(Date.now());
    // Said over the socket, else over HTTP: a stop mustn't wait for a reconnect.
    const c: Command = { type: "stop_task", task: t.id };
    if (!store.send(c))
      harnessApi.command(store.id, c).catch((e: unknown) => {
        setStopAt(null);
        toast.error("Couldn't stop it", errorMessage(e));
      });
  };
  const stopButton = stopperOf(t, features) ? (
    <button
      type="button"
      className="al-stop"
      aria-expanded={STOP_ASK[cls] ? asking : undefined}
      aria-label={`Stop ${t.description || t.command || noun}`}
      onClick={() => (STOP_ASK[cls] ? setAsking(!asking) : stop())}
    >
      Stop
    </button>
  ) : (
    // Its harness can't stop it: say so, rather than offer nothing.
    <button type="button" className="al-stop" data-off="" aria-expanded={why} onClick={() => setWhy(!why)}>
      Can&rsquo;t stop
    </button>
  );
  return (
    <li className="al-item">
      <div className="al-head">
        <button type="button" className="al-row" aria-expanded={thread ? undefined : shown} onClick={() => (thread ? onOpenTask(thread) : onShow())}>
          {sub ? (
            <Shimmer on={t.status === "running"} className="al-glyph">
              <KindGlyph kind="agent" pending={t.status === "pending"} />
            </Shimmer>
          ) : (
            <KindGlyph kind={glyphKind(cls)} live={t.status === "running"} pending={t.status === "pending"} />
          )}
          <span className="al-what">
            {mixed && <span className="al-kind">{sub ? "Subagent" : (WORD[cls] ?? cls)}</span>}
            <span className="al-desc">{t.description || t.command || t.id}</span>
          </span>
          <span className="al-time">
            {last && <span className="al-last">{last}</span>}
            <Since from={t.started} />
          </span>
          {!thread && <ChevronRight size={12} className={cn("al-open", shown && "rotate-90")} aria-hidden="true" />}
        </button>
        {stopAt !== null ? <Stopping at={stopAt}>{stopButton}</Stopping> : stopButton}
      </div>
      {why && (
        <p className="al-note">
          {harness || "Its harness"} can&rsquo;t stop this {noun} from kiln.
        </p>
      )}
      {asking && (
        <Confirm
          className="al-confirm"
          label={`Stop this ${noun}`}
          tone="danger"
          actions={
            <>
              <Button size="xs" variant="danger" onClick={stop}>
                Stop it
              </Button>
              <Button size="xs" variant="ghost" onClick={() => setAsking(false)}>
                Keep it running
              </Button>
            </>
          }
        >
          <p>{STOP_ASK[cls]}</p>
        </Confirm>
      )}
      {shown && <Output agent={store.id} task={t} />}
    </li>
  );
}

export function ActivityLine({ store, onOpenTask }: { store: SessionStore; onOpenTask: (toolId: string) => void }) {
  const [open, setOpen] = React.useState(false);
  const [shown, setShown] = React.useState<string | null>(null);
  const tasks = useSession(store, (s) => s.state?.tasks ?? NO_TASKS);
  const subs = subagentTasksAtWork(tasks);
  const bg = tasks.filter((t) => !isSubagent(t) && isLive(t));
  if (subs.length + bg.length === 0) return null;
  const count = (n: number, one: string, many = `${one}s`) => (n ? `${n} ${n === 1 ? one : many}` : "");
  const of = (cls: string) => bg.filter((t) => classOf(t) === cls).length;
  const shells = of("shell");
  const monitors = of("monitor");
  const workflows = of("workflow");
  const other = bg.length - shells - monitors - workflows;
  // The header already says what they are; a row names its kind only in a mixed list.
  const mixed = subs.length > 0 && bg.length > 0;
  // One glyph per kind, so the line says what is running at a glance; a subagent running, the
  // prism sweeps through its words (the thinking line's shimmer).
  const segments: { kind: GlyphKind; text: string; live: boolean }[] = [
    { kind: "agent" as const, text: count(subs.length, "subagent"), live: subagentRunning(subs) },
    { kind: "shell" as const, text: count(shells, "shell"), live: true },
    { kind: "monitor" as const, text: count(monitors, "monitor"), live: true },
    { kind: "workflow" as const, text: count(workflows, "workflow"), live: true },
    { kind: "mcp" as const, text: count(other, "background call"), live: true },
  ].filter((s) => s.text);
  return (
    <div className="al" data-open={open || undefined}>
      <button type="button" className="al-line" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span className="al-summary">
          {segments.map((s, i) => (
            <span key={s.kind} className="al-seg">
              {i > 0 && <span className="al-sep">·</span>}
              {s.kind === "agent" ? (
                <Shimmer on={s.live} className="al-mark">
                  <KindGlyph kind="agent" />
                  {s.text}
                </Shimmer>
              ) : (
                <>
                  <KindGlyph kind={s.kind} live={s.live} />
                  {s.text}
                </>
              )}
            </span>
          ))}
        </span>
        <ChevronRight size={12} className={cn("al-chev", open && "rotate-90")} aria-hidden="true" />
      </button>
      {open && (
        <ul className="al-list">
          {[...subs, ...bg].map((t) => (
            <ActivityItem key={t.id} t={t} store={store} mixed={mixed} shown={shown === t.id} onShow={() => setShown(shown === t.id ? null : t.id)} onOpenTask={onOpenTask} />
          ))}
        </ul>
      )}
    </div>
  );
}
