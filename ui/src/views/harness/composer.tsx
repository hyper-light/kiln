/**
 * kiln's composer: typing happens here, on your device (no round trip per key), and a message
 * reaches the agent the way you choose while it works — queued for when it finishes, steered into
 * the running turn, or sent now (stopping the turn). Queued messages stay editable and reorderable
 * on every device until they go out.
 *
 * What you type lives in the drafts store (state.ts) and only this reads it, so typing renders the
 * composer and nothing else; how a message goes is kept until you change it.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, Check, ChevronDown, Clock, CornerDownLeft, Navigation, Paperclip, Pencil, Square, X, Zap } from "lucide-react";
import { clientId, harnessApi, FORCE_STOP_AFTER_MS, useSession, useStopAt, type Attachment, type QueueItem, type SendMode, type SessionStore, type SlashCommand } from "@/api/harness";
import { sendingProps } from "@/lib/action";
import { labelOf, useLabels } from "@/lib/data";
import { useMediaQuery, useNow } from "@/lib/hooks";
import { Tip } from "@/components/ui/tooltip";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { attachedPreview, useAttachments, type Attachments, type Taken } from "./controls";
import type { ReplyTarget } from "./timeline";
import { CommandList, findCommand, matchCommands, parseSlash, ReplyPanel, runCommand, slashAt, type Reply } from "./palette";
import { commandNames } from "./marks";
import { ComposeField } from "./compose-field";
import { SendWith } from "./run-settings";
import { MicButton } from "./voice";
import { isInternal, modelLabel } from "./derive";
import { holdWith, uploadingHere, useDraft, useHeld, useSendMode, type Held } from "./state";
import { stopperOf, subagentPlaceholder, takesMessages } from "./tasks";
import { notShown } from "./queue-shown";
import { KindGlyph } from "./kind-glyph";
import type { SubagentRow } from "./subagent-tree";
import { sessionAvailability, sessionStatus } from "@/api/session-availability";
import { mailScope, onMailScopeChange } from "@/api/client";

export const MODES: { id: SendMode; label: string; hint: string; feature?: string }[] = [
  { id: "queue", label: "Queue", hint: "Sent when it finishes what it's doing" },
  { id: "steer", label: "Steer", hint: "Into the running turn — it reads it at its next step", feature: "steer" },
  { id: "now", label: "Now", hint: "Stop the current turn and send this at once", feature: "send_now" },
];

const fine = () => typeof window !== "undefined" && window.matchMedia?.("(pointer: fine)").matches;

/** What can be done with a waiting message, where it waits (kiln's queue, or this browser). */
interface RowActs {
  onEdit: (text: string) => void;
  onRemove: () => void;
  /** Its files change while it waits: the whole new set. */
  onAttach?: (attachments: Attachment[]) => void;
  /** It no longer waits for the `i`th of its files still to come (one whose upload went with an earlier page). */
  onWithout?: (i: number) => void;
  onRetryFiles?: () => void;
  onMove?: (index: number) => void;
  onSteer?: () => void;
  onNow?: () => void;
}

/** Where a queued image's preview comes from: this session's upload, else its inline bytes. */
const queueAttachSrc = (a: Attachment): string | undefined => {
  if (!a.mime.startsWith("image/")) return undefined;
  if (a.file_id) return attachedPreview(a.file_id);
  return a.data_base64 ? `data:${a.mime};base64,${a.data_base64}` : undefined;
};

/** What waits: its words, its files, and (one kept here) the names of those still on their way up. */
type Waiting = { id: string; text: string; attachments?: Attachment[]; uploading?: string[] };

/** One waiting message: its words (tap to read them whole), its files (add or drop until it goes), and what can be done with it. */
function QueueRow({ q, index, count, onEdit, onAttach, onWithout, onRetryFiles, onRemove, onMove, onSteer, onNow }: { q: Waiting; index: number; count: number } & RowActs) {
  const text = q.text;
  const attach = useAttachments("any");
  const picker = React.useRef<HTMLInputElement>(null);
  const [editing, setEditing] = React.useState(false);
  const [whole, setWhole] = React.useState(false);
  const [draft, setDraft] = React.useState(text);
  const [viewImg, setViewImg] = React.useState<{ name: string; src: string } | null>(null);
  // Changed elsewhere (another device): what's edited starts from that.
  const [was, setWas] = React.useState(text);
  if (was !== text) {
    setWas(text);
    setDraft(text);
  }
  return (
    <li className="qd-row">
      <span className="qd-n">{index + 1}</span>
      {editing ? (
        <textarea
          className="hx-input qd-edit"
          autoFocus
          rows={Math.min(6, draft.split("\n").length + 1)}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={() => {
            setEditing(false);
            if (draft.trim() && draft !== text) onEdit(draft);
          }}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              setDraft(text);
              setEditing(false);
            }
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) (e.target as HTMLTextAreaElement).blur();
          }}
        />
      ) : (
        <button type="button" className={cn("qd-text", whole && "qd-whole")} onClick={() => setWhole(!whole)} aria-expanded={whole}>
          {text}
        </button>
      )}
      {(q.attachments?.length || q.uploading?.length || onAttach) && (
        <span className="hx-attach qd-attach">
          {(q.attachments ?? []).map((a, i) => {
            const src = queueAttachSrc(a);
            return (
              <span key={i} className={cn("hx-attach-chip", src && "hx-attach-view")} onClick={() => src && setViewImg({ name: a.name, src })} role={src ? "button" : undefined}>
                {src ? <img src={src} alt="" /> : null}
                <span className="hx-trunc">{a.name}</span>
                {onAttach && (
                  <button
                    type="button"
                    aria-label={`Remove ${a.name}`}
                    onClick={(e) => {
                      e.stopPropagation();
                      onAttach((q.attachments ?? []).filter((_, j) => j !== i));
                    }}
                  >
                    <X size={11} />
                  </button>
                )}
              </span>
            );
          })}
          {/* Sent before they were up (W21-R15), it goes once they are; one whose upload went with an earlier page waits to be let go. */}
          {q.uploading?.map((name, i) =>
            uploadingHere.has(q.id) ? (
              <span key={`up-${i}`} className="hx-attach-chip hx-dim">
                <span className="hx-trunc">Uploading {name}…</span>
              </span>
            ) : (
              <span key={`up-${i}`} className="hx-attach-chip hx-bad">
                <span className="hx-trunc">{name} didn't finish uploading</span>
                {onWithout && (
                  <button type="button" aria-label={`Send without ${name}`} onClick={() => onWithout(i)}>
                    <X size={11} />
                  </button>
                )}
              </span>
            ),
          )}
          {onAttach && (
            <>
              <input
                ref={picker}
                type="file"
                multiple
                hidden
                onChange={(ev) => {
                  const list = ev.target.files;
                  ev.target.value = "";
                  if (!list?.length) return;
                  void attach.add(list).then((added) => {
                    attach.clear();
                    if (added.length) onAttach([...(q.attachments ?? []), ...added]);
                  });
                }}
              />
              {/* While files go up it says so (W21-R15), and takes no more till they're in: a batch joins the message as it was when picked. */}
              <button
                type="button"
                className="hx-attach-chip hx-attach-add"
                aria-label="Attach files"
                {...sendingProps(attach.pending > 0)}
                onClick={() => attach.pending === 0 && picker.current?.click()}
              >
                <Paperclip size={11} />
              </button>
            </>
          )}
        </span>
      )}
      <span className="qd-acts">
        {onRetryFiles && (
          <button type="button" className="hx-attach-chip" onClick={onRetryFiles}>
            Retry files
          </button>
        )}
        <Tip content="Edit">
          <button type="button" className="hx-icon" aria-label="Edit" onClick={() => setEditing(true)}>
            <Pencil size={13} />
          </button>
        </Tip>
        {onSteer && (
          <Tip content="Steer it into the running turn now, without interrupting">
            <button type="button" className="hx-icon" aria-label="Steer now" onClick={onSteer}>
              <Navigation size={13} />
            </button>
          </Tip>
        )}
        {onNow && (
          <Tip content="Stop the current turn and send this now">
            <button type="button" className="hx-icon" aria-label="Send now" onClick={onNow}>
              <Zap size={13} />
            </button>
          </Tip>
        )}
        {onMove && (
          <>
            <Tip content="Move up">
              <button type="button" className="hx-icon" aria-label="Move up" disabled={index === 0} onClick={() => onMove(index - 1)}>
                <ArrowUp size={13} />
              </button>
            </Tip>
            <Tip content="Move down">
              <button type="button" className="hx-icon" aria-label="Move down" disabled={index === count - 1} onClick={() => onMove(index + 1)}>
                <ArrowDown size={13} />
              </button>
            </Tip>
          </>
        )}
        <Tip content="Remove">
          <button type="button" className="hx-icon qd-remove" aria-label="Remove" onClick={onRemove}>
            <X size={13} />
          </button>
        </Tip>
      </span>
      {viewImg && (
        <div className="hx-cover" role="dialog" aria-modal="true" aria-label={viewImg.name} onClick={() => setViewImg(null)}>
          <div className="hx-cover-card hx-viewer" onClick={(e) => e.stopPropagation()}>
            <div className="hx-viewer-head">
              <button type="button" className="hx-icon" aria-label="Close" onClick={() => setViewImg(null)}>
                <X size={14} />
              </button>
              <span className="hx-viewer-name hx-trunc">{viewImg.name}</span>
            </div>
            <div className="hx-viewer-body">
              <img src={viewImg.src} alt={viewImg.name} />
            </div>
            <div className="hx-viewer-foot">
              <Button
                size="sm"
                variant="ghost"
                onClick={() => {
                  const i = (q.attachments ?? []).findIndex((a) => a.name === viewImg.name);
                  if (i >= 0) onAttach?.((q.attachments ?? []).filter((_, j) => j !== i));
                  setViewImg(null);
                }}
              >
                Remove
              </Button>
              <Button size="sm" variant="secondary" onClick={() => setViewImg(null)}>
                Keep attached
              </Button>
            </div>
          </div>
        </div>
      )}
    </li>
  );
}

/**
 * What waits: one line until opened; it folds again a few seconds after you leave it. `label`:
 * where it waits ("Queued", "Kept here"); `hint`: when it goes; `acts`: what each one can do.
 */
function Queued({ queue, label, hint, acts }: { queue: Waiting[]; label: string; hint: string; acts: (id: string) => RowActs }) {
  const [open, setOpen] = React.useState(false);
  const timer = React.useRef(0);
  const root = React.useRef<HTMLDivElement>(null);
  const hovered = React.useRef(false);
  // It folds a few seconds after the last touch, unless one is being edited or a mouse rests on it.
  const fold = React.useCallback(() => {
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      const editing = root.current?.contains(document.activeElement) && document.activeElement?.tagName === "TEXTAREA";
      if (editing || hovered.current) fold();
      else setOpen(false);
    }, 4000);
  }, []);
  // While it's open (and again whenever the queue changes), it's counting down to folding.
  const count = queue.length;
  const folding = React.useCallback(
    (list: HTMLOListElement | null) => {
      if (!list) return;
      void count;
      fold();
      return () => window.clearTimeout(timer.current);
    },
    [fold, count],
  );
  const first = queue[0]?.text ?? "";
  return (
    <div
      ref={root}
      className="qd"
      data-open={open || undefined}
      onPointerEnter={(e) => {
        if (e.pointerType === "mouse") hovered.current = true;
      }}
      onPointerLeave={() => {
        hovered.current = false;
      }}
      onPointerDown={() => open && fold()}
      onKeyDown={() => open && fold()}
    >
      <button type="button" className="qd-line" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span className="qd-label">
          {label} · {queue.length}
        </span>
        {!open && <span className="qd-first">{first}</span>}
        {open && <span className="qd-hint">{hint}</span>}
        <ChevronDown size={12} className="qd-chev" aria-hidden="true" />
      </button>
      {open && (
        <ol ref={folding} className="qd-list">
          {queue.map((q, i) => (
            <QueueRow key={q.id} q={q} index={i} count={queue.length} {...acts(q.id)} />
          ))}
        </ol>
      )}
    </div>
  );
}

/** How a message goes while it works: kept until changed (the composer's typing doesn't render it). */
export const SendModes = React.memo(function SendModes({ modes, mode, onMode }: { modes: typeof MODES; mode: SendMode; onMode: (m: SendMode) => void }) {
  return (
    <>
      <div className="cp-modes" role="radiogroup" aria-label="How to send while it works">
        {modes.map((m) => (
          <Tip key={m.id} content={m.hint}>
            <button type="button" role="radio" aria-checked={mode === m.id} data-mode={m.id} onClick={() => onMode(m.id)}>
              {m.label}
            </button>
          </Tip>
        ))}
      </div>
      <label className="cp-mode-pick" data-mode={mode}>
        <span className="cp-mode-value">{modes.find((m) => m.id === mode)?.label ?? "Queue"}</span>
        <ChevronDown size={11} aria-hidden="true" />
        <select aria-label="How to send while it works" value={mode} onChange={(e) => onMode(e.target.value as SendMode)}>
          {modes.map((m) => (
            <option key={m.id} value={m.id}>
              {m.label}
            </option>
          ))}
        </select>
      </label>
    </>
  );
});

/** Stop the turn; once asked it says so, and if the turn goes on regardless, offers to force it. */
const StopTurn = React.memo(function StopTurn({ store }: { store: SessionStore }) {
  const stopAt = useStopAt(store);
  const now = useNow();
  if (stopAt != null && now - stopAt > FORCE_STOP_AFTER_MS) {
    return (
      <Tip content="It didn't stop when asked: force it (it starts again, the conversation going on)">
        <button type="button" className="cp-stop" data-force onClick={() => store.stop(true)} aria-label="Force stop">
          <Square size={10} fill="currentColor" />
          <span className="cp-label">Force stop</span>
        </button>
      </Tip>
    );
  }
  return (
    <Tip content="Stop the current turn (Esc)">
      <button type="button" className="cp-stop" disabled={stopAt != null} onClick={() => store.stop()} aria-label={stopAt != null ? "Stopping…" : "Stop the current turn"}>
        <Square size={10} fill="currentColor" />
        <span className="cp-label">{stopAt != null ? "Stopping…" : "Stop"}</span>
      </button>
    </Tip>
  );
});

/** Stop the subagent shown (its turn; one whose harness can't pause it ends): as Stop does the agent's. */
const StopSubagent = React.memo(function StopSubagent({ store, task }: { store: SessionStore; task: string }) {
  const [at, setAt] = React.useState<number | null>(null);
  const now = useNow();
  const asked = at != null && now - at < 15_000;
  return (
    <Tip content="Stop the subagent (Esc)">
      <button
        type="button"
        className="cp-stop"
        disabled={asked}
        onClick={() => {
          if (store.send({ type: "stop_task", task })) setAt(Date.now());
        }}
        aria-label={asked ? "Stopping…" : "Stop the subagent"}
      >
        <Square size={10} fill="currentColor" />
        <span className="cp-label">{asked ? "Stopping…" : "Stop"}</span>
      </button>
    </Tip>
  );
});

/**
 * What was kept here for `agent` goes, in order, if it can take it now (connected, its machine
 * there). Those that went are dropped at once, after: a send that fails stops the rest.
 */
const heldSending = new WeakSet<SessionStore>();
async function sendHeld(store: SessionStore, agent: string) {
  if (heldSending.has(store)) return;
  heldSending.add(store);
  try {
    const s = store.get();
    if (sessionAvailability(s) !== "available") return;
    for (const m of useHeld.getState().held[agent] ?? []) {
      if (m.owner !== mailScope()) continue;
      // Earlier originals retain their place through durable server waiting. Source admission
      // removes their browser copy, then the next queued original can go under its own id.
      if (m.custody) break;
      // One whose files are still on their way waits for them, and those after it behind it.
      if (m.uploading?.length) break;
      // As it was to go: a "now" kept through a reconnection still stops what runs. One for a
      // subagent keeps its way too (the agent sends it the nearest way that one takes then).
      const status = sessionStatus(s);
      const running = status === "running" || status === "waiting";
      const mode = m.task || running ? (m.mode ?? "queue") : "queue";
      if (
        !(await store.sendMail(
          { type: "send", id: m.id, text: m.text, mode, attachments: m.attachments, overrides: m.overrides, reply_to: m.reply_to, ...(m.task ? { task: m.task } : {}) },
          m.originals,
        ))
      )
        break;
      break;
    }
  } finally {
    heldSending.delete(store);
  }
}

const NO_QUEUE: QueueItem[] = [];
const NO_HELD: Held[] = [];
const NO_FEATURES: string[] = [];
const NO_COMMANDS: SlashCommand[] = [];
const NO_MODES: SendMode[] = [];

/** Where a message to one of the subagents picked stands: delivered, waiting to be taken, or not sent. */
export type ToMark = "ok" | "wait" | "fail";

const NO_TO: readonly SubagentRow[] = [];

/**
 * Who what's written goes to: the subagents picked, each its last message's mark before its name
 * (so a name cut short never hides it), as many as the width shows (+N for the rest), and Clear.
 */
function ToLine({ to, marks, onClear }: { to: readonly SubagentRow[]; marks?: ReadonlyMap<string, ToMark>; onClear?: () => void }) {
  // Two names on a phone, four wider; the rest counted.
  const many = useMediaQuery("(min-width: 720px)") ? 4 : 2;
  const more = to.length - many;
  return (
    <div className="hx-to" role="group" aria-label="To">
      <KindGlyph kind="agent" className="hx-to-mark" />
      <ul className="hx-to-names">
        {to.slice(0, many).map((r) => {
          const m = marks?.get(r.key);
          return (
            <li key={r.key} className="hx-to-name" data-mark={m}>
              {m === "ok" && <Check size={11} aria-label="Delivered" />}
              {m === "wait" && <Clock size={11} aria-label="Waiting" />}
              {m === "fail" && <X size={11} aria-label="Not sent" />}
              <span>{r.name}</span>
            </li>
          );
        })}
      </ul>
      {more > 0 && <span className="hx-to-more">+{more}</span>}
      <button type="button" className="hx-to-clear" aria-label="Clear who it's to" onClick={onClear}>
        <X size={12} />
      </button>
    </div>
  );
}

export function Composer({
  store,
  agent,
  attachments,
  toMain,
  task,
  onView,
  onTerminal,
  replyTo,
  onClearReply,
  to: picked = NO_TO,
  toMarks,
  onSendTo,
  onClearTo,
}: {
  store: SessionStore;
  /** The agent's id: what's written to it is kept under it until it's sent. */
  agent: string;
  attachments: Attachments;
  /** Shown over a subagent's thread: what's typed here goes to the main agent (unless that subagent takes messages). */
  toMain?: boolean;
  /** The subagent whose thread is shown (its task's id, or its call's): a message goes to it, as it takes one now (`Task.takes`). */
  task?: string | null;
  /** A command that opens a kiln panel (model, mcp, changes, tasks…). */
  onView?: (view: string) => void;
  /** Open a terminal beside the agent: its shell (`!command`), or its harness's own screen (a command only that screen does). */
  onTerminal?: (open: { what: "shell" | "screen"; command?: string }) => void;
  /** What the next message answers (a messenger-style reply, from a message's ⋯ menu). */
  replyTo?: ReplyTarget | null;
  onClearReply?: () => void;
  /** Its subagents picked by the line above (background.tsx): what's written goes to each, its own way (`onSendTo`), not to the agent. */
  to?: readonly SubagentRow[];
  /** Where the last message to them stands, each by its row's key: delivered, waiting, or not sent. */
  toMarks?: ReadonlyMap<string, ToMark>;
  onSendTo?: (m: { text: string; taken: Taken }) => Promise<boolean>;
  onClearTo?: () => void;
}) {
  const [text, setText] = useDraft(agent);
  const chosen = useSendMode((s) => s.mode);
  const setMode = useSendMode((s) => s.setMode);
  // The connection's down: what needs the server (the mic, commands' replies) waits for it.
  const disabled = useSession(store, (s) => s.conn !== "open");
  const status = useSession(store, sessionStatus);
  // The agent can't take a message now (its machine away, or the connection down): what you
  // send is kept here, and goes once it can. Typing never stops.
  const reachable = useSession(store, (s) => sessionAvailability(s) === "available");
  // The machine it runs on, as people know it (none for one on the kiln server itself).
  const on = useSession(store, (s) => (s.info?.target?.startsWith("agent:") ? s.info.target.slice(6) : ""));
  const labels = useLabels();
  const machine = on && labelOf(on, labels);
  const paused = useSession(store, (s) => (s.state?.pause ? (s.state.pause.auto ? "auto" : "manual") : null));
  const allHeld = useHeld((s) => s.held[agent] ?? NO_HELD);
  const owner = React.useSyncExternalStore(onMailScopeChange, mailScope, () => null);
  const pendingOriginals = React.useMemo(() => allHeld.filter((message) => owner !== null && message.owner === owner), [allHeld, owner]);
  const held = React.useMemo(() => pendingOriginals.filter((message) => !message.custody), [pendingOriginals]);
  const custodyError = useHeld((s) => s.error);
  const queue = useSession(store, (s) => s.state?.queue ?? NO_QUEUE);
  const approvals = useSession(store, (s) => s.state?.approvals.length ?? 0);
  const model = useSession(store, (s) => s.state?.model ?? "");
  const effort = useSession(store, (s) => s.state?.effort ?? "");
  const meta = useSession(store, (s) => s.meta);
  // Over a subagent's thread: one that takes messages now gets what's written here, its way.
  const sub = useSession(store, (s) => (task ? s.state?.tasks.find((t) => t.id === task || t.tool_id === task) : undefined));
  const subTakes = sub?.takes ?? NO_MODES;
  const toSub = takesMessages(sub) ? sub : undefined;
  const delivery = useSession(store, (s) => s.state?.delivery);
  // What agents send each other and kiln's news to the agent wait in its queue unseen (derive.ts `isInternal`).
  const shownQueue = React.useMemo(() => notShown(toSub ? (toSub.queue ?? NO_QUEUE) : queue, delivery).filter((q) => !isInternal(q.id)), [toSub, queue, delivery]);
  const [suggest, setSuggest] = React.useState<{ kind: "/" | "@"; items: { label: string; hint?: string }[]; index: number } | null>(null);
  const ta = React.useRef<HTMLTextAreaElement | null>(null);
  // Where the caret is, as the field last said; null: at the end (text put in, not typed).
  const [caret, setCaret] = React.useState<number | null>(null);
  const features = meta?.features ?? NO_FEATURES;
  const running = toSub ? toSub.status === "running" || toSub.status === "pending" || toSub.status === "paused" : status === "running" || status === "waiting";
  const modes = React.useMemo(() => MODES.filter((m) => (toSub ? subTakes.includes(m.id) : !m.feature || features.includes(m.feature))), [features, toSub, subTakes]);
  // How you chose to send stays chosen; an agent that can't send that way queues.
  const mode: SendMode = modes.some((m) => m.id === chosen) ? chosen : "queue";
  const canAttach = attachments.enabled;
  const canOverride = !toSub && features.includes("overrides");
  const subStops = Boolean(toSub && running && stopperOf(toSub, features));
  const { files, add, chips, pending } = attachments;
  const picker = React.useRef<HTMLInputElement>(null);
  const [override, setOverride] = React.useState<{ model?: string; effort?: string }>({});
  /** "Send with": a model or effort for the next message only (long-press or right-click Send). */
  const [withOpen, setWithOpen] = React.useState(false);
  const pressTimer = React.useRef(0);
  const longPressed = React.useRef(false);
  const shellLine = Boolean(onTerminal) && text.trimStart().startsWith("!");
  // To subagents picked, each takes it its own way (steered at work, else queued): it's sent, whatever the agent is doing.
  const sendLabel = shellLine
    ? "Run"
    : !toSub && picked.length > 0
      ? "Send"
      : !reachable || paused
        ? "Queue"
        : running
          ? mode === "queue"
            ? "Queue"
            : mode === "steer"
              ? "Steer"
              : "Send now"
          : "Send";
  const away = machine ? `${machine} is back` : "it's back";

  // What was kept here goes, in order, once the agent can take it: whenever the session or what's
  // kept changes, while the composer shows.
  const sendsHeld = React.useCallback(
    (el: HTMLDivElement | null) => {
      if (!el) return;
      const go = () => {
        void sendHeld(store, agent);
      };
      go();
      const offSession = store.subscribe(go);
      const offHeld = useHeld.subscribe(go);
      return () => {
        offSession();
        offHeld();
      };
    },
    [store, agent],
  );

  // Commands, while a name is typed after "/" (at the start of the message or anywhere after a
  // space): every one the agent's harness offers, by name, alias or what it does.
  const commands = meta?.commands ?? NO_COMMANDS;
  const names = React.useMemo(() => commandNames(commands), [commands]);
  const slash = parseSlash(text);
  const token = slashAt(text, Math.min(caret ?? text.length, text.length));
  const [palIndex, setPalIndex] = React.useState(0);
  const [palClosed, setPalClosed] = React.useState(false);
  const [reply, setReply] = React.useState<Reply | "loading" | null>(null);
  const [note, setNote] = React.useState<string | null>(null);
  // Another command name: the list starts at its top again, open, and a new name replaces an old
  // note (clearing the field after one keeps it in view).
  const tokenKey = token ? `${token.start}/${token.name}` : undefined;
  const [named, setNamed] = React.useState(tokenKey);
  if (named !== tokenKey) {
    setNamed(tokenKey);
    setPalIndex(0);
    setPalClosed(false);
    if (token?.name) setNote(null);
  }
  const query = token && !palClosed ? token.name : null;
  const palItems = React.useMemo(() => (query === null ? [] : matchCommands(commands, query)), [query, commands]);
  const typed = slash && !slash.typingName ? findCommand(commands, slash.name) : undefined;

  // @-file suggestions for the word being typed at the end, asked for a moment after typing stops.
  const asking = React.useRef({ n: 0, timer: 0 });
  const suggestFor = React.useCallback(
    (t: string) => {
      const a = asking.current;
      a.n += 1;
      window.clearTimeout(a.timer);
      const at = t.match(/(?:^|\s)@(\S*)$/);
      if (!at || !features.includes("file_suggestions")) {
        setSuggest(null);
        return;
      }
      const n = a.n;
      a.timer = window.setTimeout(() => {
        void store
          .request((client) => ({ type: "suggest", client, query: at[1] ?? "" }))
          .then((v) => {
            if (asking.current.n !== n) return;
            const list = Array.isArray(v) ? (v as unknown[]).filter((x): x is string => typeof x === "string").slice(0, 8) : [];
            setSuggest(list.length ? { kind: "@", items: list.map((p) => ({ label: `@${p}` })), index: 0 } : null);
          });
      }, 120);
    },
    [features, store],
  );
  /**
   * The text changed here (typed, completed, cleared): its suggestions follow it. `caretAt`: where
   * the caret is in it (by default its end).
   */
  const write = React.useCallback(
    (t: string, caretAt: number | null = null) => {
      setText(t);
      setCaret(caretAt);
      suggestFor(t);
    },
    [setText, suggestFor],
  );

  const complete = (label: string) => {
    write(text.replace(/@\S*$/, `${label} `));
    ta.current?.focus();
  };

  /** Run a catalog command with the words typed after it. True when it was handled here. */
  const runEntry = (entry: NonNullable<ReturnType<typeof findCommand>>, args: string): string | true => {
    const out = runCommand(entry, args, store);
    if (out.kind === "send") return out.text;
    write("");
    if (out.kind === "view") {
      if (out.view === "files") {
        write("@");
        ta.current?.focus();
      } else onView?.(out.view);
    } else if (out.kind === "unsupported") setNote(`/${entry.name}: ${out.reason}`);
    else if (out.kind === "terminal") {
      if (onTerminal) onTerminal({ what: "screen", command: out.command });
      else setNote(`/${entry.name} is done in the harness's own screen, in a terminal`);
    } else if (out.kind === "reply") {
      setReply("loading");
      out.promise.then(setReply, (e: unknown) => setReply({ title: `/${entry.name}`, text: "", error: e instanceof Error ? e.message : String(e) }));
    }
    return true;
  };

  /**
   * Pick a command from the list. When it's the whole message, one that takes no words runs;
   * otherwise its name goes in where it was typed, the caret after it.
   */
  const pick = (entry: NonNullable<ReturnType<typeof findCommand>>) => {
    if (entry.route?.kind === "unsupported") {
      setNote(`/${entry.name}: ${entry.route.reason}`);
      return;
    }
    const { start, end } = token ?? { start: 0, end: text.length };
    const rest = text.slice(end);
    if (start === 0 && !rest.trim() && (entry.args === "none" || (!entry.args && entry.route?.kind === "view"))) {
      const r = runEntry(entry, "");
      if (r !== true) sendText(r);
      return;
    }
    const pos = start + entry.name.length + 2;
    write(`${text.slice(0, start)}/${entry.name}${/^\s/.test(rest) ? "" : " "}${rest}`, pos);
    setPalClosed(true);
    const el = ta.current;
    el?.focus();
    // Once the field has the new text (which puts the caret at its end).
    requestAnimationFrame(() => el?.setSelectionRange(pos, pos));
  };

  const submit = () => {
    const raw = text.trim();
    // `!command` runs in the shell beside the agent, in its folder (not sent to it).
    if (raw.startsWith("!") && onTerminal) {
      onTerminal({ what: "shell", command: raw.slice(1).trim() || undefined });
      write("");
      return;
    }
    const s = parseSlash(raw);
    const entry = s ? findCommand(commands, s.name) : undefined;
    if (entry && s) {
      const r = runEntry(entry, s.args);
      if (r !== true) sendText(r);
      return;
    }
    sendText(raw);
  };

  const sending = React.useRef(false);
  const sendText = async (body: string) => {
    if (sending.current) return;
    if (!body && files.length === 0 && pending === 0) return;
    sending.current = true;
    setNote("Saving message…");
    try {
      // Its files go with it, those still uploading too: Send doesn't wait for them (W21-R15).
      const taken = attachments.take();
      // To the subagents picked: one message each, sent by the page (agent.tsx), none to the agent;
      // files still on their way go with it once they're up.
      if (!toSub && picked.length > 0 && onSendTo) {
        if (!(await onSendTo({ text: body, taken }))) {
          taken.back();
          setNote("The message stayed in this box because its originals could not all be saved.");
          return;
        }
        write("");
        setNote(null);
        setOverride({});
        setWithOpen(false);
        onClearReply?.();
        return;
      }
      const m = { id: clientId(), text: body, attachments: taken.files.length ? taken.files : undefined, overrides: override.model || override.effort ? override : undefined, reply_to: replyTo?.id };
      // It can't take it now (or what was kept here hasn't all gone yet): kept here, sent once it
      // can, in order — never lost, never blocked. "Now" and "steer" don't wait behind what's kept:
      // they're for the turn under way. One whose files are still on their way is kept till they're up.
      const go = running ? mode : "queue";
      const behind = pendingOriginals.length > 0 && go === "queue";
      const to = toSub ? { task: toSub.id } : {};
      let accepted: boolean;
      if (taken.uploading.length) accepted = await holdWith(agent, { ...m, mode: go, at: Date.now(), ...to }, taken);
      // Kept here until it can go; a browser that can't keep it sends it now instead (the server holds it).
      else if (behind || !reachable)
        accepted =
          (await useHeld.getState().hold(agent, { ...m, mode: go, at: Date.now(), ...to, originals: taken.originals })) ||
          (await store.sendMail({ type: "send", ...m, mode: go, ...to }, taken.originals));
      else accepted = await store.sendMail({ type: "send", ...m, mode: go, ...to }, taken.originals);
      if (!accepted) {
        if (!taken.uploading.length) taken.back();
        setNote(useHeld.getState().error ?? store.get().error ?? "The message stayed in this box. Keep its text and files and try when the connection returns.");
        return;
      }
      taken.commit?.();
      write("");
      setNote(null);
      setOverride({});
      setWithOpen(false);
      onClearReply?.();
    } finally {
      sending.current = false;
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (palItems.length > 0) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const n = palItems.length;
        setPalIndex((i) => (i + (e.key === "ArrowDown" ? 1 : n - 1)) % n);
        return;
      }
      if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
        const it = palItems[palIndex];
        if (it) {
          e.preventDefault();
          pick(it);
          return;
        }
      }
      if (e.key === "Escape") {
        e.preventDefault();
        setPalClosed(true);
        return;
      }
    }
    if (suggest) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const n = suggest.items.length;
        setSuggest({ ...suggest, index: (suggest.index + (e.key === "ArrowDown" ? 1 : n - 1)) % n });
        return;
      }
      if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
        const it = suggest.items[suggest.index];
        if (it) {
          e.preventDefault();
          complete(it.label);
          return;
        }
      }
      if (e.key === "Escape") {
        setSuggest(null);
        return;
      }
    }
    if (e.key === "Enter" && (e.metaKey || e.ctrlKey || (!e.shiftKey && fine()))) {
      e.preventDefault();
      submit();
      return;
    }
    if (e.key === "Escape" && running && !text && (toSub ? subStops : features.includes("interrupt"))) {
      e.preventDefault();
      if (toSub) store.send({ type: "stop_task", task: toSub.id });
      else store.stop();
    }
  };

  return (
    <div ref={sendsHeld} className="hx-composer cp">
      {custodyError && (
        <div className="pl-note" role="alert">
          {custodyError}
        </div>
      )}
      {owner && allHeld.some((message) => !message.owner) && (
        <div className="pl-note" role="status">
          Saved messages need their original account verified before sending. Their originals remain stored.
        </div>
      )}
      {held.length > 0 && (
        <Queued
          queue={held}
          label="Kept here"
          hint={!reachable ? `sent in order once ${away}` : held.some((m) => m.uploading?.length) ? "sent in order once their files are up" : "sending…"}
          acts={(id) => ({
            onEdit: (t) => useHeld.getState().edit(agent, id, t),
            onWithout: (i) => useHeld.getState().without(agent, id, i),
            onRemove: () => useHeld.getState().drop(agent, id),
            onRetryFiles:
              held.find((message) => message.id === id)?.originals?.length && !uploadingHere.has(id)
                ? () => {
                    const message = held.find((item) => item.id === id);
                    if (!message) return;
                    uploadingHere.add(id);
                    void Promise.all((message.originals ?? []).map(async (file) => ({ name: file.name, mime: file.mime, file_id: await harnessApi.upload(file.blob, file.name, file.mime) })))
                      .then(
                        (files) => useHeld.getState().uploaded(agent, id, files, message.owner, message.originals),
                        () => {
                          setNote("The files could not upload yet; their originals remain saved here.");
                        },
                      )
                      .finally(() => {
                        uploadingHere.delete(id);
                      });
                  }
                : undefined,
          })}
        />
      )}
      {shownQueue.length > 0 && (
        <Queued
          queue={shownQueue}
          label="Queued"
          hint={
            paused === "auto"
              ? "sent in order once the limit resets"
              : paused
                ? "sent in order once you resume it"
                : toSub
                  ? "sent in order when the subagent finishes"
                  : "sent in order when it finishes"
          }
          acts={(id) => ({
            onEdit: (text) => store.send({ type: "queue_edit", id, text }),
            onAttach: (atts) => {
              const q = queue.find((x) => x.id === id);
              if (q) store.send({ type: "queue_edit", id, text: q.text, attachments: atts });
            },
            onRemove: () => store.send({ type: "queue_remove", id }),
            onMove: (index) => store.send({ type: "queue_move", id, index }),
            onSteer: (toSub ? subTakes.includes("steer") : features.includes("queue_steer")) && !paused ? () => store.send({ type: "queue_steer", id }) : undefined,
            onNow: (toSub ? subTakes.includes("now") : features.includes("send_now")) && !paused ? () => store.send({ type: "queue_send_now", id }) : undefined,
          })}
        />
      )}

      {replyTo && (
        <div className="cp-reply" role="note">
          <CornerDownLeft size={12} aria-hidden="true" />
          <span className="cp-reply-who">{replyTo.who}</span>
          <span className="cp-reply-text">{replyTo.text}</span>
          <button type="button" className="hx-icon" aria-label="Stop replying" onClick={onClearReply}>
            <X size={12} />
          </button>
        </div>
      )}
      {reply && <ReplyPanel reply={reply} onClose={() => setReply(null)} />}
      {palItems.length > 0 && <CommandList items={palItems} index={palIndex} onPick={pick} onHover={setPalIndex} />}
      {note && (
        <div className="pl-note" role="status">
          <span>{note}</span>
          <button type="button" className="hx-icon" aria-label="Dismiss" onClick={() => setNote(null)}>
            <X size={12} />
          </button>
        </div>
      )}
      {typed && palItems.length === 0 && (typed.hint || typed.description) && (
        <div className="pl-typed">
          <span className="pl-name">
            /{typed.name}
            {typed.hint && <span className="pl-hint"> {typed.hint}</span>}
          </span>
          {typed.description && <span className="pl-desc">{typed.description}</span>}
        </div>
      )}
      {suggest && (
        <ul className="hx-suggest" role="listbox">
          {suggest.items.map((it, i) => (
            <li key={it.label}>
              <button
                type="button"
                role="option"
                aria-selected={i === suggest.index}
                onPointerDown={(e) => {
                  e.preventDefault();
                  complete(it.label);
                }}
              >
                <span className="hx-mono">{it.label}</span>
                {it.hint && <span className="hx-dim hx-trunc">{it.hint}</span>}
              </button>
            </li>
          ))}
        </ul>
      )}

      {/* Files dropped or pasted here are the agent page's to attach (it takes them anywhere on
          the page); taking them here as well attached each one twice. */}
      <div className="hx-compose-box">
        {!toSub && picked.length > 0 && <ToLine to={picked} marks={toMarks} onClear={onClearTo} />}
        {chips}
        {canOverride && (withOpen || override.model || override.effort) && (
          <SendWith className="cp-with" models={meta?.models ?? []} model={model} modelLabel={modelLabel} efforts={meta?.efforts ?? []} effort={effort} value={override} onChange={setOverride} />
        )}
        <ComposeField
          text={text}
          names={names}
          field={ta}
          placeholder={
            !reachable
              ? `${disabled ? "Not connected" : `${machine || "Its machine"} is offline`}: what you send waits here and goes once ${away}`
              : paused
                ? `Queue a message: it goes once ${paused === "auto" ? "the limit resets" : "you resume it"}`
                : !toSub && picked.length > 0
                  ? "Message them"
                  : toSub
                    ? subagentPlaceholder(running ? mode : null, toSub.via === "parent")
                    : toMain
                      ? "Message the main agent"
                      : running
                        ? mode === "steer"
                          ? "Steer it — it reads this at its next step"
                          : mode === "now"
                            ? approvals > 0
                              ? `Cancel ${approvals === 1 ? "the request" : `the ${approvals} requests`}, stop and say…`
                              : "Stop and say…"
                            : "Queue a message for when it finishes"
                        : "Tell the agent what to do"
          }
          onChange={(e) => write(e.target.value, e.target.selectionStart)}
          onSelect={(e) => setCaret(e.currentTarget.selectionStart)}
          onKeyDown={onKeyDown}
          aria-label={toSub ? "Message to the subagent" : "Message to the agent"}
          enterKeyHint="send"
          autoCapitalize="sentences"
        />
        <div className="cp-bar">
          {canAttach && (
            <>
              <input
                ref={picker}
                type="file"
                multiple
                hidden
                accept={attachments.accepts === "images" ? "image/*" : undefined}
                onChange={(e) => {
                  if (e.target.files) void add(e.target.files);
                  e.target.value = "";
                }}
              />
              <Tip content="Attach files or images">
                <button type="button" className="cp-icon" aria-label="Attach files" onClick={() => picker.current?.click()}>
                  <Paperclip size={15} />
                  {files.length > 0 && <span className="cp-badge">{files.length}</span>}
                </button>
              </Tip>
            </>
          )}
          <MicButton field={ta} setText={write} onNote={setNote} disabled={disabled} />
          {running && modes.length > 1 && <SendModes modes={modes} mode={mode} onMode={setMode} />}
          <span className="cp-gap" />
          {toSub ? subStops && <StopSubagent store={store} task={toSub.id} /> : running && features.includes("interrupt") && <StopTurn store={store} />}
          <button
            type="button"
            className="cp-send"
            data-mode={running ? mode : "send"}
            onClick={() => {
              if (longPressed.current) {
                longPressed.current = false;
                return;
              }
              submit();
            }}
            onContextMenu={(e) => {
              if (!canOverride) return;
              e.preventDefault();
              setWithOpen((o) => !o);
            }}
            onPointerDown={() => {
              if (!canOverride) return;
              window.clearTimeout(pressTimer.current);
              pressTimer.current = window.setTimeout(() => {
                longPressed.current = true;
                setWithOpen(true);
              }, 450);
            }}
            onPointerUp={() => window.clearTimeout(pressTimer.current)}
            onPointerLeave={() => window.clearTimeout(pressTimer.current)}
            disabled={!text.trim() && files.length === 0 && pending === 0}
            aria-label={sendLabel}
          >
            {sendLabel === "Queue" ? <CornerDownLeft size={15} /> : <ArrowUp size={15} />}
            <span className="cp-label">{sendLabel}</span>
          </button>
        </div>
      </div>
    </div>
  );
}
