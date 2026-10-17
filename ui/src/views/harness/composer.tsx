/**
 * kiln's composer: typing happens here, on your device (no round trip per key), and a message
 * reaches the agent the way you choose while it works — queued for when it finishes, steered into
 * the running turn, or sent now (stopping the turn). Queued messages stay editable and reorderable
 * on every device until they go out.
 */
import * as React from "react";
import { ArrowUp, ChevronDown, CornerDownLeft, Paperclip, Square, X } from "lucide-react";
import { clientId, type Meta, type SendMode, type SessionStore, type State } from "@/api/harness";
import { Tip } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import type { Attachments } from "./controls";
import { CommandList, findCommand, matchCommands, parseSlash, ReplyPanel, runCommand, type Reply } from "./palette";
import { SendWith } from "./run-settings";
import { modelLabel } from "./derive";

const MODES: { id: SendMode; label: string; hint: string; feature?: string }[] = [
  { id: "queue", label: "Queue", hint: "Sent when it finishes what it's doing" },
  { id: "steer", label: "Steer", hint: "Into the running turn — it reads it at its next step", feature: "steer" },
  { id: "now", label: "Now", hint: "Stop the current turn and send this at once", feature: "send_now" },
];

const fine = () => typeof window !== "undefined" && window.matchMedia?.("(pointer: fine)").matches;

/** One queued message: its words (tap to read them whole), and what can be done with it. */
function QueueRow({ id, text, index, count, store, canNow, canSteer }: { id: string; text: string; index: number; count: number; store: SessionStore; canNow: boolean; canSteer: boolean }) {
  const [editing, setEditing] = React.useState(false);
  const [whole, setWhole] = React.useState(false);
  const [draft, setDraft] = React.useState(text);
  React.useEffect(() => setDraft(text), [text]);
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
            if (draft.trim() && draft !== text) store.send({ type: "queue_edit", id, text: draft });
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
      <span className="qd-acts">
        <button type="button" onClick={() => setEditing(true)}>
          edit
        </button>
        {canSteer && (
          <button type="button" onClick={() => store.send({ type: "queue_steer", id })} title="Into the running turn now, without interrupting it">
            steer
          </button>
        )}
        {canNow && (
          <button type="button" onClick={() => store.send({ type: "queue_send_now", id })} title="Stop the current turn and send this now">
            now
          </button>
        )}
        <button type="button" aria-label="Move up" disabled={index === 0} onClick={() => store.send({ type: "queue_move", id, index: index - 1 })}>
          ↑
        </button>
        <button type="button" aria-label="Move down" disabled={index === count - 1} onClick={() => store.send({ type: "queue_move", id, index: index + 1 })}>
          ↓
        </button>
        <button type="button" className="qd-remove" onClick={() => store.send({ type: "queue_remove", id })}>
          remove
        </button>
      </span>
    </li>
  );
}

/** What's queued: one line until opened; it folds again a few seconds after you leave it. */
function Queued({ queue, store, canNow, canSteer }: { queue: { id: string; text: string }[]; store: SessionStore; canNow: boolean; canSteer: boolean }) {
  const [open, setOpen] = React.useState(false);
  const timer = React.useRef(0);
  const root = React.useRef<HTMLDivElement>(null);
  const hold = () => window.clearTimeout(timer.current);
  const fold = () => {
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      if (!root.current?.contains(document.activeElement)) setOpen(false);
    }, 4000);
  };
  React.useEffect(() => () => window.clearTimeout(timer.current), []);
  React.useEffect(() => {
    if (open) fold();
  }, [open, queue.length]);
  const first = queue[0]?.text ?? "";
  return (
    <div
      ref={root}
      className="qd"
      data-open={open || undefined}
      onPointerEnter={hold}
      onPointerLeave={() => open && fold()}
      onFocus={hold}
      onBlur={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) fold();
      }}
    >
      <button type="button" className="qd-line" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span className="qd-label">Queued · {queue.length}</span>
        {!open && <span className="qd-first">{first}</span>}
        {open && <span className="qd-hint">sent in order when it finishes</span>}
        <ChevronDown size={12} className="qd-chev" aria-hidden="true" />
      </button>
      {open && (
        <ol className="qd-list">
          {queue.map((q, i) => (
            <QueueRow key={q.id} id={q.id} text={q.text} index={i} count={queue.length} store={store} canNow={canNow} canSteer={canSteer} />
          ))}
        </ol>
      )}
    </div>
  );
}

export function Composer({
  store,
  state,
  meta,
  disabled,
  attachments,
  toMain,
  onView,
}: {
  store: SessionStore;
  state: State | null;
  meta: Meta | null;
  disabled?: boolean;
  attachments: Attachments;
  /** Shown over a subagent's thread: what's typed here goes to the main agent. */
  toMain?: boolean;
  /** A command that opens a kiln panel (model, mcp, changes, tasks…). */
  onView?: (view: string) => void;
}) {
  const [text, setText] = React.useState("");
  const [mode, setMode] = React.useState<SendMode>("queue");
  const [suggest, setSuggest] = React.useState<{ kind: "/" | "@"; items: { label: string; hint?: string }[]; index: number } | null>(null);
  const ta = React.useRef<HTMLTextAreaElement>(null);
  const features = React.useMemo(() => meta?.features ?? [], [meta]);
  const running = state?.status === "running" || state?.status === "waiting";
  const modes = MODES.filter((m) => !m.feature || features.includes(m.feature));
  const queue = state?.queue ?? [];
  const canAttach = attachments.enabled;
  const canOverride = features.includes("overrides");
  const { files, add, clear, chips, pending } = attachments;
  const picker = React.useRef<HTMLInputElement>(null);
  const [override, setOverride] = React.useState<{ model?: string; effort?: string }>({});
  /** "Send with": a model or effort for the next message only (long-press or right-click Send). */
  const [withOpen, setWithOpen] = React.useState(false);
  const pressTimer = React.useRef(0);
  const longPressed = React.useRef(false);
  const approvals = state?.approvals.length ?? 0;
  const sendLabel = running ? (mode === "queue" ? "Queue" : mode === "steer" ? "Steer" : "Send now") : "Send";

  // Grow with the text, up to a third of the screen.
  React.useLayoutEffect(() => {
    const el = ta.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, Math.round(window.innerHeight / 3))}px`;
  }, [text]);

  // Commands, while a name is typed after "/": every one the agent's harness offers, by name,
  // alias or what it does.
  const commands = React.useMemo(() => meta?.commands ?? [], [meta]);
  const slash = parseSlash(text);
  const [palIndex, setPalIndex] = React.useState(0);
  const [palClosed, setPalClosed] = React.useState(false);
  const [reply, setReply] = React.useState<Reply | "loading" | null>(null);
  const [note, setNote] = React.useState<string | null>(null);
  const palItems = React.useMemo(() => (slash?.typingName && !palClosed ? matchCommands(commands, slash.name) : []), [slash?.typingName, slash?.name, palClosed, commands]);
  React.useEffect(() => {
    setPalIndex(0);
    setPalClosed(false);
    // A new command name replaces an old note; clearing the field after one keeps it in view.
    if (slash?.name) setNote(null);
  }, [slash?.name]);
  const typed = slash && !slash.typingName ? findCommand(commands, slash.name) : undefined;

  // @-file suggestions for the word being typed at the end.
  React.useEffect(() => {
    const at = text.match(/(?:^|\s)@(\S*)$/);
    if (at && features.includes("file_suggestions")) {
      let live = true;
      const t = window.setTimeout(() => {
        void store
          .request((client) => ({ type: "suggest", client, query: at[1] ?? "" }))
          .then((v) => {
            if (!live) return;
            const list = Array.isArray(v) ? (v as unknown[]).filter((x): x is string => typeof x === "string").slice(0, 8) : [];
            setSuggest(list.length ? { kind: "@", items: list.map((p) => ({ label: `@${p}` })), index: 0 } : null);
          });
      }, 120);
      return () => {
        live = false;
        window.clearTimeout(t);
      };
    }
    setSuggest(null);
  }, [text, meta, features, store]);

  const complete = (label: string) => {
    setText((t) => t.replace(/@\S*$/, `${label} `));
    setSuggest(null);
    ta.current?.focus();
  };

  /** Run a catalog command with the words typed after it. True when it was handled here. */
  const runEntry = (entry: NonNullable<ReturnType<typeof findCommand>>, args: string): string | true => {
    const out = runCommand(entry, args, store);
    if (out.kind === "send") return out.text;
    setText("");
    if (out.kind === "view") {
      if (out.view === "files") {
        setText("@");
        ta.current?.focus();
      } else onView?.(out.view);
    } else if (out.kind === "unsupported") setNote(`/${entry.name}: ${out.reason}`);
    else if (out.kind === "reply") {
      setReply("loading");
      out.promise.then(setReply, (e: unknown) => setReply({ title: `/${entry.name}`, text: "", error: e instanceof Error ? e.message : String(e) }));
    }
    return true;
  };

  /** Pick a command from the list: one that takes no words runs; otherwise its name goes in the field. */
  const pick = (entry: NonNullable<ReturnType<typeof findCommand>>) => {
    if (entry.route?.kind === "unsupported") {
      setNote(`/${entry.name}: ${entry.route.reason}`);
      return;
    }
    if (entry.args === "none" || (!entry.args && entry.route?.kind === "view")) {
      const r = runEntry(entry, "");
      if (r !== true) sendText(r);
      return;
    }
    setText(`/${entry.name} `);
    setPalClosed(true);
    ta.current?.focus();
  };

  const submit = () => {
    const raw = text.trim();
    const s = parseSlash(raw);
    const entry = s ? findCommand(commands, s.name) : undefined;
    if (entry && s) {
      const r = runEntry(entry, s.args);
      if (r !== true) sendText(r);
      return;
    }
    sendText(raw);
  };

  const sendText = (body: string) => {
    if ((!body && files.length === 0) || disabled || pending > 0) return;
    const ok = store.send({
      type: "send",
      id: clientId(),
      text: body,
      mode: running ? mode : "queue",
      attachments: files.length ? attachments.payload() : undefined,
      overrides: override.model || override.effort ? override : undefined,
    });
    if (ok) {
      setText("");
      setSuggest(null);
      clear();
      setOverride({});
      setWithOpen(false);
      // "Now" and "Steer" are for one message: the next one queues again unless chosen.
      setMode("queue");
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
    if (e.key === "Escape" && running && features.includes("interrupt") && !text) {
      e.preventDefault();
      store.send({ type: "interrupt" });
    }
  };

  return (
    <div className="hx-composer cp">
      {queue.length > 0 && <Queued queue={queue} store={store} canNow={features.includes("send_now")} canSteer={features.includes("queue_steer")} />}

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

      <div
        className={cn("hx-compose-box", running && "hx-compose-live")}
        onDragOver={(e) => canAttach && e.preventDefault()}
        onDrop={(e) => {
          if (!canAttach || !e.dataTransfer.files.length) return;
          e.preventDefault();
          void add(e.dataTransfer.files);
        }}
      >
        {chips}
        {canOverride && (withOpen || override.model || override.effort) && (
          <SendWith
            className="cp-with"
            models={meta?.models ?? []}
            model={state?.model ?? ""}
            modelLabel={modelLabel}
            efforts={meta?.efforts ?? []}
            effort={state?.effort ?? ""}
            value={override}
            onChange={setOverride}
          />
        )}
        <textarea
          onPaste={(e) => {
            if (canAttach && e.clipboardData.files.length) {
              e.preventDefault();
              void add(e.clipboardData.files);
            }
          }}
          ref={ta}
          className="hx-compose-input"
          rows={1}
          value={text}
          disabled={disabled}
          placeholder={
            disabled
              ? "Not connected"
              : toMain
                ? "Message the main agent"
                : running
                  ? mode === "steer"
                    ? "Steer it — it reads this at its next step"
                    : mode === "now"
                      ? "Stop and say…"
                      : "Queue a message for when it finishes"
                  : "Tell the agent what to do"
          }
          onChange={(e) => setText(e.target.value)}
          onKeyDown={onKeyDown}
          aria-label="Message to the agent"
          enterKeyHint="send"
          autoCapitalize="sentences"
        />
        {running && mode === "now" && (
          <div className="cp-warn" role="note">
            Now stops the current turn
            {approvals > 0 ? ` and cancels ${approvals === 1 ? "the request waiting on you" : `the ${approvals} requests waiting on you`}` : ""}, then sends this.
          </div>
        )}
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
          {running && modes.length > 1 && (
            <>
              <div className="cp-modes" role="radiogroup" aria-label="How to send while it works">
                {modes.map((m) => (
                  <Tip key={m.id} content={m.hint}>
                    <button type="button" role="radio" aria-checked={mode === m.id} data-mode={m.id} onClick={() => setMode(m.id)}>
                      {m.label}
                    </button>
                  </Tip>
                ))}
              </div>
              <label className="cp-mode-pick" data-mode={mode}>
                <span className="cp-mode-value">{modes.find((m) => m.id === mode)?.label ?? "Queue"}</span>
                <ChevronDown size={11} aria-hidden="true" />
                <select aria-label="How to send while it works" value={mode} onChange={(e) => setMode(e.target.value as SendMode)}>
                  {modes.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.label}
                    </option>
                  ))}
                </select>
              </label>
            </>
          )}
          <span className="cp-gap" />
          {running && features.includes("interrupt") && (
            <Tip content="Stop the current turn (Esc)">
              <button type="button" className="cp-stop" onClick={() => store.send({ type: "interrupt" })} aria-label="Stop the current turn">
                <Square size={10} fill="currentColor" />
                <span className="cp-label">Stop</span>
              </button>
            </Tip>
          )}
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
            disabled={(!text.trim() && files.length === 0) || disabled || pending > 0}
            aria-label={sendLabel}
          >
            {running && mode === "queue" ? <CornerDownLeft size={15} /> : <ArrowUp size={15} />}
            <span className="cp-label">{sendLabel}</span>
          </button>
        </div>
      </div>
    </div>
  );
}
