/**
 * What a review comment says (docs/ci.md W26-R09, R10): its markdown, each of its suggestion blocks
 * shown against the lines its thread is on, with copy and Apply; then a suggestion kept in its old
 * field, as one block more.
 */
import * as React from "react";
import type { Suggestion } from "@/api/review";
import { CommentBody } from "@/lib/markdown";
import { serverSuggestion, SuggestionBlock, type Lines } from "./suggestion";
import "./review.css";

interface Here {
  suggestions?: readonly Suggestion[];
  old: Lines | null;
  onApply?: (index: number) => Promise<{ version: number }>;
}

/** The comment shown, for its suggestion blocks. */
const HereContext = React.createContext<Here>({ old: null });

/** One of its suggestion blocks: the server's suggestion it is (by its place and its lines) says whether it applies and how it stands. */
function SuggestionHere({ n, lines }: { n: number; lines: string[] }) {
  const { suggestions, old, onApply } = React.useContext(HereContext);
  const s = serverSuggestion(suggestions, n, lines);
  return <SuggestionBlock old={old} lines={lines} state={s} onApply={s && onApply ? () => onApply(s.index) : undefined} />;
}

export function CommentText({
  body,
  suggestions,
  old = null,
  onApply,
}: {
  body: string;
  /** The server's word on its suggestions (none: a draft, a preview). */
  suggestions?: readonly Suggestion[];
  /** The lines its thread is on, as the revision shown has them. */
  old?: Lines | null;
  /** Apply one of its suggestions (by its index), answering the revision it made. None: no Apply. */
  onApply?: (index: number) => Promise<{ version: number }>;
}) {
  const here = React.useMemo(() => ({ suggestions, old, onApply }), [suggestions, old, onApply]);
  const field = suggestions?.find((s) => s.field);
  return (
    <div className="md-comment">
      <HereContext.Provider value={here}>
        <CommentBody text={body} suggestion={SuggestionHere} />
      </HereContext.Provider>
      {field && <SuggestionBlock old={old} lines={field.lines} state={field} onApply={onApply ? () => onApply(field.index) : undefined} />}
    </div>
  );
}
