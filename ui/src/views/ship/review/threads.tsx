/**
 * A thread in a Ship item's dropdown (docs/ci.md W26-R07, R08): flat like the agent chat's turns
 * (a lighter band, top and bottom borders, no inset cards). Each comment's line is who, the
 * signature seal, the kind's icon on the thread's first, when, and ⋯ (reply, copy, quote, edit,
 * delete, the kind's, what can't run shown with why); a long press on the body copies it. The
 * body is markdown (step 6's CommentText, its suggestions with Apply). Reply and Resolve are
 * aligned under the last comment; a reply is the step 6 composer.
 */
import * as React from "react";
import { BadgeCheck, Check, Copy, MoreHorizontal, Pencil, Quote, Reply, RotateCcw, Trash2, X } from "lucide-react";
import { whoSaid, type Thread, type ThreadComment } from "@/api/review";
import { relativeTime } from "@/lib/format";
import { CommentText } from "./comment";
import { ReviewComposer } from "./composer";
import { draftKey, replyAt } from "./draft";
import { KINDS, KIND_ICON, KIND_SAYS } from "./kinds";
import type { LinesOf, Ops } from "./ops";
import type { Side } from "./fold";
import type { Lines } from "./suggestion";
import { useDraft } from "@/views/harness/state";
import "./review.css";

const said = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Held this long, a press copies the comment (the agent chat's hold, f14). */
const HOLD_MS = 420;
const SLOP = 9;

/** A press and hold on the body copies it; moving first is scrolling. */
function usePressCopy(text: string) {
  const [copied, setCopied] = React.useState(false);
  const t = React.useRef(0);
  const at = React.useRef({ x: 0, y: 0 });
  const copy = React.useCallback(() => {
    void navigator.clipboard?.writeText(text).then(
      () => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1400);
      },
      () => {},
    );
  }, [text]);
  const end = () => window.clearTimeout(t.current);
  return {
    copied,
    copy,
    press: {
      onPointerDown: (e: React.PointerEvent) => {
        at.current = { x: e.clientX, y: e.clientY };
        window.clearTimeout(t.current);
        t.current = window.setTimeout(copy, HOLD_MS);
      },
      onPointerUp: end,
      onPointerCancel: end,
      onPointerMove: (e: React.PointerEvent) => {
        if (Math.hypot(e.clientX - at.current.x, e.clientY - at.current.y) > SLOP) end();
      },
    },
  };
}

/** What the ⋯ shows: a row of icon buttons inline under the comment's line (never a popup). */
function Menu({
  c,
  t,
  root,
  ops,
  onReply,
  onQuote,
  onEdit,
  onDelete,
}: {
  c: ThreadComment;
  t: Thread;
  root: boolean;
  ops: Ops;
  onReply: () => void;
  onQuote: () => void;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const mine = c.author === ops.me;
  const [err, setErr] = React.useState<string | null>(null);
  const icon = (label: string, title: string, onClick: () => void, children: React.ReactNode) => (
    <button type="button" className="rv-ib" aria-label={label} title={title} onClick={onClick}>
      {children}
    </button>
  );
  const setKind = (k: (typeof KINDS)[number]) => {
    setErr(null);
    ops.setKind(t, k).catch((e) => setErr(said(e)));
  };
  return (
    <div className="rv-menu" role="menu" aria-label="Comment actions">
      {icon("Reply", "Reply", onReply, <Reply size={14} aria-hidden="true" />)}
      {icon("Copy", "Copy", () => void navigator.clipboard?.writeText(c.body).catch(() => {}), <Copy size={14} aria-hidden="true" />)}
      {icon("Quote", "Quote", onQuote, <Quote size={14} aria-hidden="true" />)}
      {ops.kiln && (mine ? icon("Edit", "Edit", onEdit, <Pencil size={14} aria-hidden="true" />) : icon("Edit", "Only its author edits a comment", () => {}, <Pencil size={14} aria-hidden="true" />))}
      {ops.kiln &&
        (mine ? icon("Delete", "Delete", onDelete, <Trash2 size={14} aria-hidden="true" />) : icon("Delete", "Only its author deletes a comment", () => {}, <Trash2 size={14} aria-hidden="true" />))}
      {ops.kiln && root && t.kind && (
        <span className="rv-menu-kinds" role="radiogroup" aria-label="Change the kind">
          {KINDS.map((k) => {
            const Icon = KIND_ICON[k];
            const allowed = t.created_by === ops.me;
            return (
              <button
                key={k}
                type="button"
                role="radio"
                className="rv-kind"
                data-k={k}
                aria-checked={t.kind === k}
                aria-label={k}
                title={allowed ? KIND_SAYS[k] : "Only its author changes a thread's kind"}
                disabled={!allowed}
                onClick={() => setKind(k)}
              >
                <Icon size={14} aria-hidden="true" />
              </button>
            );
          })}
        </span>
      )}
      {err && (
        <span className="rv-menu-err" role="alert">
          {err}
        </span>
      )}
    </div>
  );
}

/** One comment: its line (who, seal, kind, when, ⋯) and its markdown, with the press to copy. */
function CommentCard({ c, t, root, ops, linesOf, where, onReplyDraft }: { c: ThreadComment; t: Thread; root: boolean; ops: Ops; linesOf: LinesOf; where?: string; onReplyDraft: () => void }) {
  const [menu, setMenu] = React.useState(false);
  const [editing, setEditing] = React.useState(false);
  const [confirm, setConfirm] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const [busy, start] = React.useTransition();
  const { copied, press } = usePressCopy(c.body);
  const key = draftKey(replyAt(ops.item, t));
  const [, setReplyText] = useDraft(key);
  const KindIcon = root && t.kind ? KIND_ICON[t.kind] : null;
  const run = (f: () => Promise<void>) =>
    start(async () => {
      setErr(null);
      try {
        await f();
      } catch (e) {
        setErr(said(e));
      }
    });
  const quote = () => {
    setReplyText((was) => (was.trim() ? `${was.replace(/\n+$/, "")}\n\n> ${c.body}\n\n` : `> ${c.body}\n\n`));
    setMenu(false);
    onReplyDraft();
  };
  return (
    <div className="rv-m" data-who={c.author === ops.me ? "you" : c.author.startsWith("agent:") ? "agent" : "person"}>
      <div className="rv-meta">
        <span className="rv-who">
          {whoSaid(c.author)}
          <span className="rv-seal" title="unsigned" aria-label="unsigned">
            <BadgeCheck size={12} aria-hidden="true" />
          </span>
          {KindIcon && (
            <span className="rv-k" data-k={t.kind} title={t.kind ?? ""} aria-label={t.kind ?? ""}>
              <KindIcon size={12} aria-hidden="true" />
            </span>
          )}
          {where && <span className="rv-where">{where}</span>}
        </span>
        <span className="rv-when">
          {copied ? (
            <span className="rv-copied" aria-label="Copied">
              <Check size={12} aria-hidden="true" />
            </span>
          ) : (
            relativeTime(c.created_at)
          )}
          {c.draft && <span className="rv-draft">draft</span>}
        </span>
        <button type="button" className="rv-ib rv-dots" aria-label="Comment actions" aria-expanded={menu} onClick={() => setMenu(!menu)}>
          <MoreHorizontal size={15} aria-hidden="true" />
        </button>
      </div>
      {menu && (
        <Menu
          c={c}
          t={t}
          root={root}
          ops={ops}
          onReply={() => {
            setMenu(false);
            onReplyDraft();
          }}
          onQuote={quote}
          onEdit={() => {
            setMenu(false);
            setEditing(true);
          }}
          onDelete={() => {
            setMenu(false);
            setConfirm(true);
          }}
        />
      )}
      {c.deleted ? (
        <p className="rv-deleted">deleted</p>
      ) : editing ? (
        <EditBody body={c.body} busy={busy} onSave={(body) => run(() => ops.edit(c.id, body).then(() => setEditing(false)))} onCancel={() => setEditing(false)} />
      ) : (
        <div className="rv-body" {...press} onContextMenu={(e) => e.preventDefault()}>
          <CommentText body={c.body} suggestions={c.suggestions} old={oldOf(t, linesOf)} onApply={ops.kiln ? (index) => ops.apply(c.id, index) : undefined} />
        </div>
      )}
      {confirm && (
        <div className="rv-note" data-k="blocking">
          <Trash2 size={13} aria-hidden="true" />
          <span>Delete this comment?</span>
          <button type="button" className="rv-btn" disabled={busy} onClick={() => run(() => ops.remove(c.id))}>
            Delete
          </button>
          <button type="button" className="rv-ib" aria-label="Keep" onClick={() => setConfirm(false)}>
            <X size={14} aria-hidden="true" />
          </button>
        </div>
      )}
      {err && (
        <p className="rvc-error" role="alert">
          {err}
        </p>
      )}
    </div>
  );
}

/** The lines a thread's suggestions go against: its own span, as the view has them. */
function oldOf(t: Thread, linesOf: LinesOf): Lines | null {
  if (t.file == null || t.start_line == null) return null;
  return linesOf(t.file, t.side as Side, t.start_line, t.end_line ?? t.start_line);
}

function EditBody({ body, busy, onSave, onCancel }: { body: string; busy: boolean; onSave: (body: string) => void; onCancel: () => void }) {
  const [text, setText] = React.useState(body);
  return (
    <div className="rv-edit">
      <textarea className="rv-ta" rows={Math.min(12, text.split("\n").length + 1)} value={text} onChange={(e) => setText(e.target.value)} aria-label="Edit the comment" />
      <div className="rv-btns">
        <button type="button" className="rv-btn rv-btn-go" disabled={busy || !text.trim()} onClick={() => onSave(text.trim())}>
          Save
        </button>
        <button type="button" className="rv-ib" aria-label="Cancel" onClick={onCancel}>
          <X size={14} aria-hidden="true" />
        </button>
      </div>
    </div>
  );
}

/**
 * The thread: its comments, then Reply and Resolve aligned under the last, or the composer while
 * its reply is written. A resolved thread folds to one line ("3 · resolved by kim") with Reopen.
 */
export function ThreadView({
  t,
  ops,
  linesOf,
  draft,
  setDraft,
  where,
}: {
  t: Thread;
  ops: Ops;
  linesOf: LinesOf;
  /** The draft open on this thread (its reply's), when it is. */
  draft: boolean;
  setDraft: (d: { kind: "reply"; thread: string } | null) => void;
  /** What names where it sits, in the Conversation (`matrix.rs:42`). */
  where?: string;
}) {
  const resolved = t.state === "resolved";
  const blocking = t.kind === "blocking" && t.created_by !== ops.me;
  const [err, setErr] = React.useState<string | null>(null);
  const [busy, start] = React.useTransition();
  const at = replyAt(ops.item, t);
  const send = (body: string) => ops.reply(t, body).then(() => setDraft(null));
  if (resolved)
    return (
      <div className="rv-thread" data-done="1" data-k={t.kind}>
        <span className="rv-done">
          <Check size={13} aria-hidden="true" />
          <span>
            {t.comments.length} · resolved{t.resolved_by ? ` by ${whoSaid(t.resolved_by)}` : ""}
          </span>
        </span>
        {ops.kiln && (
          <button type="button" className="rv-ib" aria-label="Reopen" title="Reopen" disabled={busy} onClick={() => start(async () => ops.resolve(t, false).catch((e) => setErr(said(e))))}>
            <RotateCcw size={13} aria-hidden="true" />
          </button>
        )}
      </div>
    );
  return (
    <div className="rv-thread" data-k={t.kind}>
      {t.comments.map((c, i) => (
        <CommentCard key={c.id} c={c} t={t} root={i === 0} ops={ops} linesOf={linesOf} where={where} onReplyDraft={() => setDraft({ kind: "reply", thread: t.id })} />
      ))}
      {err && (
        <p className="rvc-error" role="alert">
          {err}
        </p>
      )}
      {draft ? (
        <ReviewComposer at={at} lines={oldOf(t, linesOf)} placeholder={`Reply to ${whoSaid(t.comments[0]?.author ?? "")}`} sendLabel="Reply" onSend={send} onCancel={() => setDraft(null)} autoFocus />
      ) : (
        <div className="rv-btns">
          <button type="button" className="rv-btn" onClick={() => setDraft({ kind: "reply", thread: t.id })}>
            <Reply size={13} aria-hidden="true" />
            <span>Reply</span>
          </button>
          {ops.kiln && (
            <button
              type="button"
              className="rv-btn"
              disabled={busy || blocking}
              title={blocking ? `Only ${whoSaid(t.created_by)} can resolve a blocking comment` : undefined}
              onClick={() =>
                start(async () => {
                  setErr(null);
                  try {
                    await ops.resolve(t, true);
                  } catch (e) {
                    setErr(said(e));
                  }
                })
              }
            >
              <Check size={13} aria-hidden="true" />
              <span>Resolve</span>
            </button>
          )}
        </div>
      )}
    </div>
  );
}
