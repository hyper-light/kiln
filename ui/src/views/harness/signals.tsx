/** Signals: the agent console's small motion primitives (optic glyph, prism label, thinking head/body, tool beam, caret). */
import * as React from "react";
import "./signals.css";
import { ChevronRight } from "lucide-react";
import { cn } from "@/lib/utils";

export type OpticState = "idle" | "thinking" | "tool" | "waiting" | "done" | "ended";

const OPTIC_LABELS: Record<OpticState, string> = {
  idle: "Idle",
  thinking: "Thinking",
  tool: "Using a tool",
  waiting: "Waiting for you",
  done: "Done",
  ended: "Ended",
};

const RINGS = [0, 1, 2, 3];

/** Concentric elliptical rings (the site's about-optic, shrunk to a glyph). Motion and colour are pure CSS per state. */
export function Optic({ state, size = 14, className }: { state: OpticState; size?: 14 | 16; className?: string }) {
  return (
    <span className={cn("sg-optic", className)} data-state={state} data-size={size} role="img" aria-label={OPTIC_LABELS[state]}>
      {RINGS.map((i) => (
        <span key={i} style={{ "--i": i } as React.CSSProperties} />
      ))}
    </span>
  );
}

/** A mono eyebrow; live text carries the prism gradient sweeping through it. */
export function PrismLabel({ live = false, className, children }: { live?: boolean; className?: string; children: React.ReactNode }) {
  return (
    <span className={cn("sg-label", className)} data-live={live ? "true" : "false"}>
      {children}
    </span>
  );
}

/** "<1s", "12s", "1m 05s", "1h 02m". */
function formatThought(seconds: number) {
  if (!Number.isFinite(seconds) || seconds < 1) return "<1s";
  const s = Math.floor(seconds);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
}

/** The toggle above a thinking block: live it breathes ("Thinking"); settled it reads "Thought · 12s" with a chevron. */
export function ThinkingHead({
  live,
  seconds,
  open,
  onToggle,
  className,
}: {
  live: boolean;
  seconds?: number;
  open: boolean;
  onToggle: () => void;
  className?: string;
}) {
  return (
    <button type="button" className={cn("sg-think-head", className)} data-live={live ? "true" : "false"} aria-expanded={open} onClick={onToggle}>
      {/* The label already names the state; keep the glyph out of the button's accessible name. */}
      <span className="sg-think-glyph" aria-hidden="true">
        <Optic state={live ? "thinking" : "done"} />
      </span>
      {live ? (
        <PrismLabel live>Thinking</PrismLabel>
      ) : (
        <>
          <PrismLabel>
            Thought{seconds != null && <span className="sg-num"> · {formatThought(seconds)}</span>}
          </PrismLabel>
          <ChevronRight size={12} className="sg-chev" aria-hidden="true" />
        </>
      )}
    </button>
  );
}

/** The thinking text container: a prism rule runs down its left edge while live, then settles to the border colour. */
export function ThinkingBody({ live, className, ...rest }: React.HTMLAttributes<HTMLDivElement> & { live: boolean }) {
  return <div {...rest} className={cn("sg-think", className)} data-live={live ? "true" : "false"} />;
}

export type BeamStatus = "running" | "done" | "error" | "denied" | "pending" | "cancelled";

const BEAM_LABELS: Record<BeamStatus, string> = {
  running: "Running",
  done: "Done",
  error: "Error",
  denied: "Denied",
  pending: "Pending",
  cancelled: "Cancelled",
};

/** A tool call's 30px signal: a prism rule with a travelling bead while running, collapsing into an outcome chip. */
export function Beam({ status, className }: { status: BeamStatus; className?: string }) {
  return (
    <span className={cn("sg-beam", className)} data-status={status} role="img" aria-label={BEAM_LABELS[status]}>
      <span className="sg-beam-rule" />
      <span className="sg-beam-dot" />
      <span className="sg-beam-chip" />
    </span>
  );
}

/** The streaming-text caret: a thin prism bar that breathes rather than blinks. */
export function Caret({ className }: { className?: string }) {
  return <span className={cn("sg-caret", className)} aria-hidden="true" />;
}
