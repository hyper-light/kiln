/**
 * The Conversation (docs/ci.md W26-R07, R15): the change's PR-style list — its description in
 * markdown, then every comment in order (line threads' named where they sit), with the comment
 * box at the end. Flat like the inline threads: no cards, each entry its line and its markdown.
 */
import * as React from "react";
import type { Thread } from "@/api/review";
import { whoSaid } from "@/api/review";
import { relativeTime } from "@/lib/format";
import { CommentText } from "./comment";
import { ReviewComposer } from "./composer";
import type { LinesOf, Ops } from "./ops";
import { ThreadView } from "./threads";
import "./review.css";

/** The change's description, its first entry. */
export function Description({ author, title, body, at }: { author: string; title?: string; body: string; at?: number }) {
  return (
    <li className="rv-m" data-who={author.startsWith("agent:") ? "agent" : "person"}>
      <div className="rv-meta">
        <span className="rv-who">{whoSaid(author)}</span>
        {at != null && <span className="rv-when">{relativeTime(at)}</span>}
      </div>
      {title && <p className="rv-title">{title}</p>}
      <div className="rv-body">
        <CommentText body={body || "No description."} />
      </div>
    </li>
  );
}

export function Conversation({
  ops,
  linesOf,
  threads,
  children,
  draft,
  setDraft,
}: {
  ops: Ops;
  linesOf: LinesOf;
  threads: Thread[];
  children: React.ReactNode;
  draft: string | null;
  setDraft: (thread: string | null) => void;
}) {
  const open = threads.filter((t) => t.state !== "resolved");
  const done = threads.filter((t) => t.state === "resolved");
  return (
    <ul className="rv-convo">
      {children}
      {[...open, ...done].map((t) => (
        <li key={t.id}>
          <ThreadView
            t={t}
            ops={ops}
            linesOf={linesOf}
            where={t.file ? `${t.file.split("/").pop()}:${t.end_line ?? t.start_line ?? ""}` : undefined}
            draft={draft === t.id}
            setDraft={(d) => setDraft(d ? t.id : null)}
          />
        </li>
      ))}
    </ul>
  );
}

/** The comment box at the Conversation's end. */
export function ConvoBox({ ops, onSent }: { ops: Ops; onSent?: () => void }) {
  return (
    <li className="rv-convo-box">
      <ReviewComposer at={{ item: ops.item }} placeholder="Leave a comment" onSend={(body) => ops.converse(body).then(() => onSent?.())} />
    </li>
  );
}
