/**
 * A list a page at a time, as Ship's commits turn (W21-R15): the page's sizes, for the list's head,
 * and under the list Back, where it is and On. A control that can't act is dimmed, never gone; the
 * one whose page is on its way says so.
 */
import * as React from "react";
import { sendingProps } from "@/lib/action";
import { cn } from "@/lib/utils";
import "./pager.css";

/** A page's sizes, as a list offers them. */
export const PER_PAGE = [5, 10, 25, 50, 100] as const;
export type PerPage = (typeof PER_PAGE)[number];

/** The page's sizes ("per page 5 · 10 · 25 · 50 · 100"); `onPer` null: dimmed. */
export function PerPicker({ per, onPer, label, turning, className }: { per: PerPage; onPer: ((n: PerPage) => void) | null; label: string; turning?: PerPage | null; className?: string }) {
  return (
    <span className={cn("pager-per", className)} role="group" aria-label={label}>
      <span>per page</span>
      {PER_PAGE.map((n, i) => (
        <React.Fragment key={n}>
          {i > 0 && <span aria-hidden="true">·</span>}
          <button type="button" aria-pressed={per === n} aria-label={`${n} a page`} disabled={onPer === null} {...sendingProps(turning === n)} onClick={() => onPer?.(n)}>
            {n}
          </button>
        </React.Fragment>
      ))}
    </span>
  );
}

/** Back, where it is, On: `back`/`on` null when there's no page that way. */
export function Pager({
  back,
  range,
  on,
  words = ["Previous", "Next"],
  turning = null,
  label,
  className,
}: {
  back: (() => void) | null;
  range: string;
  on: (() => void) | null;
  /** What Back and On say ("Newer", "Older" for commits). */
  words?: [string, string];
  /** Which press's page is on its way. */
  turning?: "back" | "on" | null;
  label?: string;
  className?: string;
}) {
  return (
    <div className={cn("pager", className)} role="group" aria-label={label}>
      <button type="button" disabled={back === null} {...sendingProps(turning === "back")} onClick={() => back?.()}>
        {words[0]}
      </button>
      <span>{range}</span>
      <button type="button" disabled={on === null} {...sendingProps(turning === "on")} onClick={() => on?.()}>
        {words[1]}
      </button>
    </div>
  );
}
