/**
 * The review composer (docs/ci.md W26-R08, R15): the agent chat's text box and drafts store
 * (harness/compose-field.tsx, harness/state.ts), so what's typed is kept by where it's written
 * (draft.ts) and renders this and nothing else; a live update never touches it and a reload puts it
 * back. Markdown, with a preview; Insert a suggestion puts the lines it's on in as a suggestion to
 * edit; a new thread on a line has a kind, a question until another is chosen. Sending is a React
 * Action calling kiln's endpoint (the caller's): the draft goes once it's sent, and stays if it isn't.
 */
import * as React from "react";
import { ArrowUp, Diff, Eye, X } from "lucide-react";
import type { ThreadKind } from "@/api/review";
import { ComposeField } from "@/views/harness/compose-field";
import { useDraft } from "@/views/harness/state";
import { CommentText } from "./comment";
import { draftKey, insertSuggestion, useDraftKind, type DraftAt } from "./draft";
import { KindPicker } from "./kinds";
import type { Lines } from "./suggestion";
import "./review.css";

const said = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function ReviewComposer({
  at,
  lines = null,
  placeholder,
  sendLabel = "Comment",
  onSend,
  onCancel,
  autoFocus,
}: {
  /** Where it's written: its draft is kept by this. */
  at: DraftAt;
  /** The lines it's on, as the revision shown has them: what a suggestion starts from, and what the preview shows one against. */
  lines?: Lines | null;
  placeholder: string;
  sendLabel?: string;
  /** Send it, with its kind when it opens a thread on a line. */
  onSend: (body: string, kind?: ThreadKind) => Promise<void>;
  /** Close it (what's written stays, for when it opens again). */
  onCancel?: () => void;
  autoFocus?: boolean;
}) {
  const key = draftKey(at);
  const [text, setText] = useDraft(key);
  const [kind, setKind] = useDraftKind(key);
  const [preview, setPreview] = React.useState(false);
  const [sending, startSend] = React.useTransition();
  const [error, setError] = React.useState<string | null>(null);
  const field = React.useRef<HTMLTextAreaElement | null>(null);
  const newThread = at.thread == null && at.line != null;
  // A suggestion replaces lines of the file as it is now: the new side's.
  const picked = lines != null && lines.text.length > 0 && at.side !== "old" ? lines : null;
  // What goes: without the blank lines before it or the spaces after it.
  const body = text.replace(/^\s*\n/, "").trimEnd();

  const send = () => {
    if (!body || sending) return;
    setError(null);
    startSend(async () => {
      try {
        await onSend(body, newThread ? kind : undefined);
        setText("");
        setKind("question");
        setPreview(false);
      } catch (e) {
        setError(said(e));
      }
    });
  };

  const suggest = () => {
    if (!picked) return;
    const el = field.current;
    const next = insertSuggestion(text, !preview && el ? el.selectionStart : text.length, picked.text);
    setText(next.text);
    setPreview(false);
    // Once the field has it: the caret at the end of the lines suggested, to edit them.
    requestAnimationFrame(() => {
      field.current?.focus();
      field.current?.setSelectionRange(next.caret, next.caret);
    });
  };

  return (
    <div className="rvc cp" data-preview={preview || undefined}>
      <div className="hx-compose-box">
        {preview && <div className="rvc-preview">{body ? <CommentText body={body} old={picked} /> : <p className="rvc-none">Nothing to preview.</p>}</div>}
        <ComposeField
          text={text}
          field={field}
          placeholder={`${placeholder} · markdown`}
          aria-label={placeholder}
          autoFocus={autoFocus}
          enterKeyHint="enter"
          autoCapitalize="sentences"
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
              e.preventDefault();
              send();
            }
          }}
        />
        <div className="cp-bar">
          {newThread && <KindPicker value={kind} onChange={setKind} />}
          <button type="button" className="cp-icon" aria-label="Preview" aria-pressed={preview} title={preview ? "Back to writing" : "Preview"} onClick={() => setPreview(!preview)}>
            <Eye size={15} aria-hidden="true" />
          </button>
          {picked && (
            <button type="button" className="cp-icon" aria-label="Insert a suggestion" title="Insert a suggestion" onClick={suggest}>
              <Diff size={15} aria-hidden="true" />
            </button>
          )}
          <span className="cp-gap" />
          {onCancel && (
            <button type="button" className="cp-icon" aria-label="Close" title="Close (what's written stays)" onClick={onCancel}>
              <X size={15} aria-hidden="true" />
            </button>
          )}
          <button type="button" className="cp-send" disabled={!body || sending} aria-label={sendLabel} onClick={send}>
            <ArrowUp size={15} aria-hidden="true" />
            <span className="cp-label">{sending ? "Sending…" : sendLabel}</span>
          </button>
        </div>
      </div>
      {error && (
        <p className="rvc-error" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
