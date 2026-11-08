/**
 * A thread in a Ship item's dropdown (docs/ci.md W26-R07, R08): flat like the agent chat's turns
 * (a lighter band, top and bottom borders, no inset cards). Each comment's line is as the mockup
 * draws it (Ship's mockups plan, step 12): who, with a person's or an agent's mark, an agent for its
 * person (W26-R18); the signature's shield, saying who signed it (dim while unsigned: W27-R04); the
 * kind's chip on the thread's
 * first; when; and ⋯ (reply, copy, quote, edit, delete, the kind's, what can't run shown with
 * why); a long press on the body copies it. The body is markdown (step 6's CommentText, its
 * suggestions with Apply). Reply and Resolve are aligned under the last comment; a reply is the
 * step 6 composer.
 */
import * as React from "react";
import { Check, Copy, MoreHorizontal, Pencil, Quote, Reply, RotateCcw, ShieldCheck, Trash2, X } from "lucide-react";
import { bylineOf, sealOf, whoSaid, type Thread, type ThreadComment } from "@/api/review";
import { StatusGlyph } from "@/components/ui/status-glyph";
import { sendingProps, useAction } from "@/lib/action";
import { relativeTime } from "@/lib/format";
import { copyText } from "@/views/terminal/clipboard";
import { CommentText } from "./comment";
import { ReviewComposer } from "./composer";
import { draftKey, replyAt, useDraftSet } from "./draft";
import { KINDS, KIND_ICON, KIND_SAYS } from "./kinds";
import type { LinesOf, Ops } from "./ops";
import type { Side } from "./fold";
import type { Lines } from "./suggestion";
import { counted } from "./perf";
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
    void copyText(text).then((ok) => {
      if (!ok) return;
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1400);
    });
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
      {icon("Copy", "Copy", () => void copyText(c.body), <Copy size={14} aria-hidden="true" />)}
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

/**
 * A comment's who and seal (W26-R18, W27-R04): its author in words with a person's or an agent's
 * mark, an agent for the person it acts for, and the seal, whole once signed and dim while not,
 * saying who signed it. A person's and an agent's are one shape.
 */
export function CommentWho({ c }: { c: Pick<ThreadComment, "author" | "by" | "signed"> }) {
  const agent = c.by ? c.by.kind === "agent" : c.author.startsWith("agent:");
  const seal = sealOf(c);
  return (
    <>
      <span className="rv-by">
        <StatusGlyph state={agent ? "agent" : "person"} size={12} />
        {c.by ? c.by.name : whoSaid(c.author)}
        {c.by?.for && <span className="rv-for">for {c.by.for}</span>}
      </span>
      <span className="rv-seal" data-signed={c.signed ? "" : undefined} title={seal} aria-label={seal}>
        <ShieldCheck size={13} aria-hidden="true" />
      </span>
    </>
  );
}

/** One comment: its line (who, seal, kind, when, ⋯) and its markdown, with the press to copy. */
function CommentCard({ c, t, root, ops, linesOf, where, onReplyDraft }: { c: ThreadComment; t: Thread; root: boolean; ops: Ops; linesOf: LinesOf; where?: string; onReplyDraft: () => void }) {
  const [menu, setMenu] = React.useState(false);
  const [editing, setEditing] = React.useState(false);
  const [confirm, setConfirm] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const { copied, press } = usePressCopy(c.body);
  const key = draftKey(replyAt(ops.item, t));
  // Quote writes the reply's draft but doesn't show it: the setter alone, so typing it elsewhere
  // never renders this card (W26-R15, AC06).
  const setReplyText = useDraftSet(key);
  const KindIcon = root && t.kind ? KIND_ICON[t.kind] : null;
  const [save, saving] = useAction(
    async (body: string) => {
      setErr(null);
      await ops.edit(c.id, body);
      setEditing(false);
    },
    (e) => setErr(said(e)),
  );
  // Deleted at once (files.tsx's patch), and with it this card when it's its thread's only comment:
  // refused, the comment is back and a toast says why (W21-R15).
  const [remove, removing] = useAction(async () => {
    setErr(null);
    await ops.remove(c.id);
  }, "Couldn't delete the comment");
  const quote = () => {
    setReplyText((was) => (was.trim() ? `${was.replace(/\n+$/, "")}\n\n> ${c.body}\n\n` : `> ${c.body}\n\n`));
    setMenu(false);
    onReplyDraft();
  };
  return (
    <div className="rv-m" data-who={c.author === ops.me ? "you" : c.author.startsWith("agent:") ? "agent" : "person"}>
      <div className="rv-meta">
        <span className="rv-who">
          <CommentWho c={c} />
          {KindIcon && (
            <span className="rv-k rv-ki" data-k={t.kind} title={t.kind ?? ""} aria-label={t.kind ?? ""}>
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
        <EditBody body={c.body} busy={saving} onSave={save} onCancel={() => setEditing(false)} />
      ) : (
        <div className="rv-body" {...press} onContextMenu={(e) => e.preventDefault()}>
          <CommentText body={c.body} suggestions={c.suggestions} old={oldOf(t, linesOf)} onApply={ops.kiln ? (index) => ops.apply(c.id, index) : undefined} />
        </div>
      )}
      {confirm && (
        <div className="rv-note" data-k="blocking">
          <Trash2 size={13} aria-hidden="true" />
          <span>Delete this comment?</span>
          <button type="button" className="rv-btn" {...sendingProps(removing)} onClick={remove}>
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
        <button type="button" className="rv-btn rv-btn-go" disabled={!text.trim()} {...sendingProps(busy)} onClick={() => onSave(text.trim())}>
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
/** What came of an open thread (W26-R18): the version that fixed it, as a suggestion says it was
 * applied; or, once agents started their fixes in it since a person spoke there, that it waits for
 * a person, the whole why in its title. At the end of the thread's buttons, so nothing moves. */
export function ThreadFix({ t }: { t: Pick<Thread, "fixed_in" | "waits"> }) {
  if (t.waits)
    return (
      <span className="rv-fix" data-waits="1" title={t.waits}>
        waits for a person
      </span>
    );
  if (t.fixed_in == null) return null;
  return (
    <span className="rv-fix md-applied">
      <Check size={13} aria-hidden="true" />
      fixed in v{t.fixed_in}
    </span>
  );
}

export const ThreadView = React.memo(
  function ThreadView({
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
    counted("ship/threadview");
    const resolved = t.state === "resolved";
    const blocking = t.kind === "blocking" && t.created_by !== ops.me;
    const [err, setErr] = React.useState<string | null>(null);
    // Resolved or reopened at once (files.tsx's patch, W21-R15), and back, saying why, if kiln refuses it.
    const [resolve, resolving] = useAction(
      async (on: boolean) => {
        setErr(null);
        await ops.resolve(t, on);
      },
      (e) => setErr(said(e)),
    );
    const at = replyAt(ops.item, t);
    if (resolved)
      return (
        <div className="rv-thread" data-done="1" data-k={t.kind}>
          <span className="rv-done">
            <Check size={13} aria-hidden="true" />
            <span>
              {t.comments.length} · resolved{t.resolved_by ? ` by ${whoSaid(t.resolved_by)}` : ""}
              {t.fixed_in != null ? ` · fixed in v${t.fixed_in}` : ""}
            </span>
            {err && (
              <span className="rvc-error" role="alert">
                {err}
              </span>
            )}
          </span>
          {ops.kiln && (
            <button type="button" className="rv-ib" aria-label="Reopen" title="Reopen" {...sendingProps(resolving)} onClick={() => resolve(false)}>
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
          <ReviewComposer
            at={at}
            lines={oldOf(t, linesOf)}
            placeholder={`Reply to ${t.comments[0] ? bylineOf(t.comments[0]) : ""}`}
            sendLabel="Reply"
            onSend={(body) => ops.reply(t, body)}
            onSent={() => setDraft(null)}
            onCancel={() => setDraft(null)}
            autoFocus
          />
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
                disabled={blocking}
                {...sendingProps(resolving)}
                title={blocking ? `Only ${whoSaid(t.created_by)} can resolve a blocking comment` : undefined}
                onClick={() => resolve(true)}
              >
                <Check size={13} aria-hidden="true" />
                <span>Resolve</span>
              </button>
            )}
            <ThreadFix t={t} />
          </div>
        )}
      </div>
    );
  },
  // A thread renders again only when what it draws does; each element's setDraft closes over its own
  // thread id and the draft setter, which don't change (W26-R15).
  (a, b) => a.t === b.t && a.ops === b.ops && a.linesOf === b.linesOf && a.draft === b.draft && a.where === b.where,
);
