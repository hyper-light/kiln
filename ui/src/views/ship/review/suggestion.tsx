/**
 * A comment's suggestion (docs/ci.md W26-R10): a change against the file's own line numbers, the old
 * lines (−) and the new (+), with copy and Apply. Applying is a React Action calling kiln's endpoint
 * (the caller's): the block says so while it goes, then which revision applied it.
 */
import * as React from "react";
import { Check } from "lucide-react";
import type { Suggestion } from "@/api/review";
import { CopyIcon } from "@/components/ui/copy-block";
import { CommentCode } from "@/lib/markdown";

/** Lines of a file by its own numbers: the first one's number, and their text. */
export interface Lines {
  start: number;
  text: string[];
}

/**
 * Which of the server's suggestions a comment's `n`th block is: the one in its place when it says the
 * same, else the first of the body's that does. None: kiln doesn't read it as one, so it doesn't apply.
 */
export function serverSuggestion(all: readonly Suggestion[] | undefined, n: number, lines: readonly string[]): Suggestion | undefined {
  const same = (s: Suggestion) => !s.field && s.lines.length === lines.length && s.lines.every((l, i) => l === lines[i]);
  const placed = all?.find((s) => s.index === n);
  return placed && same(placed) ? placed : all?.find(same);
}

const said = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function SuggestionBlock({
  old,
  lines,
  state,
  onApply,
}: {
  /** The lines its thread is on, as the revision shown has them (none: it shows as code). */
  old: Lines | null;
  /** What it puts in their place (none: they go). */
  lines: string[];
  /** The server's word on it: the revision that applied it, and whether its lines changed since. */
  state?: Suggestion;
  /** Apply it, answering the revision it made. None: it doesn't apply here (a draft, a preview). */
  onApply?: () => Promise<{ version: number }>;
}) {
  const [applying, startApply] = React.useTransition();
  const [made, setMade] = React.useState<number | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  if (!old) return <CommentCode body={lines.join("\n")} lang="suggestion" />;
  const applied = state?.applied?.version ?? made;
  const apply = () => {
    if (!onApply) return;
    setError(null);
    startApply(async () => {
      try {
        setMade((await onApply()).version);
      } catch (e) {
        setError(said(e));
      }
    });
  };
  return (
    <div className="md-code md-sug" data-state={applied != null ? "applied" : state?.outdated ? "outdated" : undefined}>
      <div className="md-code-head">
        <span className="md-code-lang">suggested change</span>
        <span className="md-code-acts">
          {applied != null ? (
            <span className="md-applied">
              <Check size={13} aria-hidden="true" />
              applied in v{applied}
            </span>
          ) : state?.outdated ? (
            <span className="md-outdated" title="Its lines changed since it was written: it doesn't apply">
              outdated
            </span>
          ) : (
            onApply && (
              <button type="button" className="md-apply" disabled={applying} onClick={apply}>
                <Check size={13} aria-hidden="true" />
                {applying ? "Applying…" : "Apply"}
              </button>
            )
          )}
          <CopyIcon text={lines.join("\n")} label="Copy the suggestion" />
        </span>
      </div>
      <pre className="md-code-body" data-code-scroll>
        <code>
          {old.text.map((t, i) => (
            <span key={`o${i}`} className="md-ln" data-n={old.start + i} data-sign="−">
              {t}
            </span>
          ))}
          {lines.map((t, i) => (
            <span key={`n${i}`} className="md-ln" data-n={old.start + i} data-sign="+">
              {t}
            </span>
          ))}
        </code>
      </pre>
      {error && (
        <p className="md-sug-error" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
