/**
 * What a Ship item's review can do (docs/ci.md W26-R07..R13): the mutations the Files section and
 * the Conversation call, one shape for a kiln change (its own endpoints), a host's pull request
 * (the forge seam's) and a commit with no change (reads only). The item and the viewer come from
 * the row; `kiln` says which actions a host can't answer stay out rather than fail.
 */
import type { Thread, ThreadComment, ThreadKind } from "@/api/review";
import type { Side } from "./fold";
import type { Lines } from "./suggestion";

/** Where a comment is being written (its draft kept by draft.ts's key, however this goes away). */
export type DraftPlace = { kind: "line"; file: string; line: number; side: Side } | { kind: "reply"; thread: string } | { kind: "convo"; thread?: string };

export interface Ops {
  /** The caller's principal (`user:ada`), for whose menu rows are theirs. */
  me: string;
  /** The draft's item: `${repo}/${change.name}` or `${repo}/${sha}`. */
  item: string;
  /** The diff's key (`D41`), when the item is a kiln change. */
  key: string | null;
  /** The revision shown (threads carry theirs; a new thread gets this one). */
  version: number;
  /** A kiln change: kinds, resolve, edit, delete, Apply. A host's: comment, reply, approve. A commit's: nothing. */
  kiln: boolean;
  /** A host's pull request: its number and repository, for the forge seam's calls. */
  pull?: { repo: string; n: number };
  /** A new thread on a line (kiln: with the kind picked). */
  commentAt: (file: string, line: number, side: Side, body: string, kind: ThreadKind) => Promise<void>;
  /** A reply under a thread (a host's: `reply_to` the comment). */
  reply: (thread: Thread, body: string) => Promise<void>;
  /** A comment on the change itself (the Conversation's box). */
  converse: (body: string) => Promise<void>;
  /** Resolve or reopen a thread (kiln only). */
  resolve: (thread: Thread, on: boolean) => Promise<void>;
  /** Edit a comment's words (its author's, kiln only). */
  edit: (commentId: string, body: string) => Promise<void>;
  /** Delete a comment (its author's, kiln only): a thread's first stays a `deleted` note while replies remain. */
  remove: (commentId: string) => Promise<void>;
  /** Change a thread's kind (its author's, kiln only). */
  setKind: (thread: Thread, kind: ThreadKind) => Promise<void>;
  /** Apply a comment's `index`th suggestion (kiln only): a revision of whoever applies it. */
  apply: (commentId: string, index: number) => Promise<{ version: number }>;
}

/** The lines a thread's suggestions and new-thread suggestions go against. */
export type LinesOf = (file: string, side: Side, start: number, end: number) => Lines | null;

/* ------------------------------------------------------------- optimistic (W26-R15, AC06) */

/** A change to the threads shown, applied while its mutation runs (useOptimistic's reducer's input). */
export type ThreadPatch =
  | { type: "reply"; thread: string; c: ThreadComment }
  | { type: "thread"; t: Thread }
  | { type: "resolve"; thread: string; on: boolean; me: string }
  | { type: "edit"; id: string; body: string }
  | { type: "remove"; id: string }
  | { type: "kind"; thread: string; kind: ThreadKind }
  | { type: "retagThread"; id: string; t: Thread }
  | { type: "retag"; thread: string; id: string; c: ThreadComment }
  | { type: "unthread"; id: string }
  | { type: "unreply"; thread: string; id: string };

/** `threads` with `p` applied: the optimistic state a mutation shows until the server answers. */
export function applyThreadPatch(threads: Thread[], p: ThreadPatch): Thread[] {
  switch (p.type) {
    case "reply":
      return threads.map((t) => (t.id === p.thread ? { ...t, comments: [...t.comments, p.c] } : t));
    case "thread":
      return [...threads, p.t];
    case "resolve":
      return threads.map((t) => (t.id === p.thread ? { ...t, state: p.on ? "resolved" : "open", resolved_by: p.on ? p.me : null, resolved_at: p.on ? now() : null } : t));
    case "edit":
      return threads.map((t) => ({ ...t, comments: t.comments.map((c) => (c.id === p.id ? { ...c, body: p.body } : c)) }));
    case "remove":
      return threads.map((t) =>
        t.comments.some((c) => c.id === p.id) ? (t.comments.length === 1 ? { ...t, comments: [] } : { ...t, comments: t.comments.map((c) => (c.id === p.id ? { ...c, deleted: true } : c)) }) : t,
      );
    case "kind":
      return threads.map((t) => (t.id === p.thread ? { ...t, kind: p.kind } : t));
    case "retagThread":
      return threads.map((t) => (t.id === p.id ? p.t : t));
    case "retag":
      return threads.map((t) => (t.id === p.thread ? { ...t, comments: t.comments.map((c) => (c.id === p.id ? p.c : c)) } : t));
    case "unthread":
      return threads.filter((t) => t.id !== p.id);
    case "unreply":
      return threads.map((t) => (t.id === p.thread ? { ...t, comments: t.comments.filter((c) => c.id !== p.id) } : t));
  }
}

const now = () => Math.floor(Date.now() / 1000);
let optSeq = 0;
const optId = (p: string) => `local:${p}${++optSeq}`;

/** A comment that's still going out, shown at once (its id says so). */
export function pendingComment(me: string, body: string): ThreadComment {
  return { id: optId("c"), author: me, body, suggestion: null, created_at: now(), draft: false };
}

/** A thread that's still going out: `at` names its file and line, or the diff as a whole. */
export function pendingThread(me: string, version: number, at: { file: string | null; side: "old" | "new"; line: number | null }, kind: ThreadKind, c: ThreadComment): Thread {
  return {
    id: optId("t"),
    version,
    file: at.file,
    side: at.side,
    start_line: at.line,
    end_line: at.line,
    state: "open",
    created_by: me,
    created_at: c.created_at,
    outdated: false,
    now: null,
    comments: [c],
    kind,
  };
}
