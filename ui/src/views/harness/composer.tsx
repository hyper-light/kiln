/**
 * kiln's composer: typing happens here, on your device (no round trip per key), and a message
 * reaches the agent the way you choose while it works — queued for when it finishes, steered into
 * the running turn, or sent now (stopping the turn). Queued messages stay editable and reorderable
 * on every device until they go out.
 */
import * as React from "react";
import { ArrowDown, ArrowUp, CornerDownLeft, Paperclip, Pencil, Send, Square, X, Zap } from "lucide-react";
import { clientId, type Meta, type SendMode, type SessionStore, type State } from "@/api/harness";
import { Button } from "@/components/ui/button";
import { Tip } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";
import type { Attachments } from "./controls";

const MODES: { id: SendMode; label: string; hint: string; feature?: string }[] = [
  { id: "queue", label: "Queue", hint: "Sent when it finishes what it's doing" },
  { id: "steer", label: "Steer", hint: "Into the running turn — it reads it at its next step", feature: "steer" },
  { id: "now", label: "Now", hint: "Stop the current turn and send this at once", feature: "send_now" },
];

const fine = () => typeof window !== "undefined" && window.matchMedia?.("(pointer: fine)").matches;

function QueueRow({ id, text, index, count, store, canNow }: { id: string; text: string; index: number; count: number; store: SessionStore; canNow: boolean }) {
  const [editing, setEditing] = React.useState(false);
  const [draft, setDraft] = React.useState(text);
  React.useEffect(() => setDraft(text), [text]);
  return (
    <li className="hx-queue-row">
      <span className="hx-queue-n">{index + 1}</span>
      {editing ? (
        <textarea
          className="hx-input hx-queue-edit"
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
        <button type="button" className="hx-queue-text" onClick={() => setEditing(true)} title="Edit">
          {text}
        </button>
      )}
      <span className="hx-queue-actions">
        <button type="button" className="hx-icon" aria-label="Edit" onClick={() => setEditing(true)}>
          <Pencil size={12} />
        </button>
        <button type="button" className="hx-icon" aria-label="Move up" disabled={index === 0} onClick={() => store.send({ type: "queue_move", id, index: index - 1 })}>
          <ArrowUp size={12} />
        </button>
        <button type="button" className="hx-icon" aria-label="Move down" disabled={index === count - 1} onClick={() => store.send({ type: "queue_move", id, index: index + 1 })}>
          <ArrowDown size={12} />
        </button>
        {canNow && (
          <Tip content="Stop the current turn and send this now">
            <button type="button" className="hx-icon" aria-label="Send now" onClick={() => store.send({ type: "queue_send_now", id })}>
              <Zap size={12} />
            </button>
          </Tip>
        )}
        <button type="button" className="hx-icon" aria-label="Remove" onClick={() => store.send({ type: "queue_remove", id })}>
          <X size={12} />
        </button>
      </span>
    </li>
  );
}

export function Composer({ store, state, meta, disabled, attachments }: { store: SessionStore; state: State | null; meta: Meta | null; disabled?: boolean; attachments: Attachments }) {
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
  const { files, add, clear, chips } = attachments;
  const picker = React.useRef<HTMLInputElement>(null);
  const [override, setOverride] = React.useState<{ model?: string; effort?: string }>({});

  // Grow with the text, up to a third of the screen.
  React.useLayoutEffect(() => {
    const el = ta.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, Math.round(window.innerHeight / 3))}px`;
  }, [text]);

  // Slash commands and @-file suggestions for the word being typed at the end.
  React.useEffect(() => {
    const slash = text.match(/^\/(\S*)$/);
    if (slash && meta?.commands.length) {
      const q = (slash[1] ?? "").toLowerCase();
      const items = meta.commands.filter((c) => c.name.toLowerCase().startsWith(q)).slice(0, 8);
      setSuggest(items.length ? { kind: "/", items: items.map((c) => ({ label: `/${c.name}`, hint: c.description })), index: 0 } : null);
      return;
    }
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
    setText((t) => (suggest?.kind === "/" ? `${label} ` : t.replace(/@\S*$/, `${label} `)));
    setSuggest(null);
    ta.current?.focus();
  };

  const submit = () => {
    const body = text.trim();
    if (!body || disabled) return;
    const ok = store.send({
      type: "send",
      id: clientId(),
      text: body,
      mode: running ? mode : "queue",
      attachments: files.length ? files.map(({ name, mime, data_base64 }) => ({ name, mime, data_base64 })) : undefined,
      overrides: override.model || override.effort ? override : undefined,
    });
    if (ok) {
      setText("");
      setSuggest(null);
      clear();
      setOverride({});
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
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
    <div className="hx-composer">
      {queue.length > 0 && (
        <div className="hx-queue">
          <div className="hx-queue-head">
            <span className="hx-eyebrow">Queued · {queue.length}</span>
            <span className="hx-dim">sent in order when it finishes</span>
          </div>
          <ol>
            {queue.map((q, i) => (
              <QueueRow key={q.id} id={q.id} text={q.text} index={i} count={queue.length} store={store} canNow={features.includes("send_now")} />
            ))}
          </ol>
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
        <div className="hx-compose-bar">
          {running && modes.length > 1 && (
            <div className="hx-modes" role="radiogroup" aria-label="How to send while it works">
              {modes.map((m) => (
                <Tip key={m.id} content={m.hint}>
                  <button type="button" role="radio" aria-checked={mode === m.id} className="hx-mode" data-mode={m.id} onClick={() => setMode(m.id)}>
                    {m.label}
                  </button>
                </Tip>
              ))}
            </div>
          )}
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
                <button type="button" className="hx-icon" aria-label="Attach files" onClick={() => picker.current?.click()}>
                  <Paperclip size={14} />
                </button>
              </Tip>
            </>
          )}
          {canOverride && (meta?.models.length || meta?.efforts?.length) ? (
            <span className="hx-override">
              {meta?.models.length ? (
                <select aria-label="Model for this message" value={override.model ?? ""} onChange={(e) => setOverride({ ...override, model: e.target.value || undefined })}>
                  <option value="">model</option>
                  {meta.models.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.label}
                    </option>
                  ))}
                </select>
              ) : null}
              {meta?.efforts?.length ? (
                <select aria-label="Effort for this message" value={override.effort ?? ""} onChange={(e) => setOverride({ ...override, effort: e.target.value || undefined })}>
                  <option value="">effort</option>
                  {meta.efforts.map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.label}
                    </option>
                  ))}
                </select>
              ) : null}
            </span>
          ) : null}
          <span className="flex-1" />
          {running && features.includes("interrupt") && (
            <Tip content="Stop the current turn (Esc)">
              <button type="button" className="hx-stop" onClick={() => store.send({ type: "interrupt" })} aria-label="Stop the current turn">
                <Square size={11} fill="currentColor" /> Stop
              </button>
            </Tip>
          )}
          <Button size="sm" onClick={submit} disabled={!text.trim() || disabled} aria-label="Send">
            {running && mode === "queue" ? <CornerDownLeft /> : <Send />}
            {running ? (mode === "queue" ? "Queue" : mode === "steer" ? "Steer" : "Send now") : "Send"}
          </Button>
        </div>
      </div>
    </div>
  );
}
