/**
 * A thread's kind (docs/ci.md W26-R11): a question, a nit, an issue or blocking, each its icon. A
 * blocking thread holds approval, merging and any automatic merge until its author resolves it; an
 * open issue asks approving to go ahead anyway; an open nit is said on approval; a question holds
 * nothing.
 */
import * as React from "react";
import { CircleQuestionMark, Feather, OctagonMinus, TriangleAlert, type LucideIcon } from "lucide-react";
import type { ThreadKind } from "@/api/review";

export const KINDS: readonly ThreadKind[] = ["question", "nit", "issue", "blocking"];

export const KIND_ICON: Record<ThreadKind, LucideIcon> = { question: CircleQuestionMark, nit: Feather, issue: TriangleAlert, blocking: OctagonMinus };

/** What each kind does, as its icon's title says it. */
export const KIND_SAYS: Record<ThreadKind, string> = {
  question: "Question: holds nothing",
  nit: "Nit: said when it's approved",
  issue: "Issue: approving asks to go ahead anyway",
  blocking: "Blocking: holds approval and merging until its author resolves it",
};

/** The kind of a new thread, one of four icons (a radio group: the arrow keys move along it). */
export const KindPicker = React.memo(function KindPicker({ value, onChange }: { value: ThreadKind; onChange: (kind: ThreadKind) => void }) {
  return (
    <span className="rv-kinds" role="radiogroup" aria-label="Kind of comment">
      {KINDS.map((k, i) => {
        const Icon = KIND_ICON[k];
        return (
          <button
            key={k}
            type="button"
            role="radio"
            className="rv-kind"
            data-k={k}
            aria-checked={value === k}
            aria-label={k}
            title={KIND_SAYS[k]}
            tabIndex={value === k ? 0 : -1}
            onClick={() => onChange(k)}
            onKeyDown={(e) => {
              const by = e.key === "ArrowRight" || e.key === "ArrowDown" ? 1 : e.key === "ArrowLeft" || e.key === "ArrowUp" ? -1 : 0;
              if (!by) return;
              e.preventDefault();
              const next = (i + by + KINDS.length) % KINDS.length;
              onChange(KINDS[next] ?? k);
              (e.currentTarget.parentElement?.children[next] as HTMLElement | undefined)?.focus();
            }}
          >
            <Icon size={14} aria-hidden="true" />
          </button>
        );
      })}
    </span>
  );
});
