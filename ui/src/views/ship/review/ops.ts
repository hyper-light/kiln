/**
 * What a Ship item's review can do (docs/ci.md W26-R07..R13): the mutations the Files section and
 * the Conversation call, one shape for a kiln change (its own endpoints), a host's pull request
 * (the forge seam's) and a commit with no change (reads only). The item and the viewer come from
 * the row; `kiln` says which actions a host can't answer stay out rather than fail.
 */
import type { Thread, ThreadKind } from "@/api/review";
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
