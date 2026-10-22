/** Signals: the agent console's small motion primitives (optic glyph, prism label, thinking head/body, tool beam, caret). */
import * as React from "react";
import "./signals.css";
import { ChevronRight } from "lucide-react";
import { cn } from "@/lib/utils";
import { useNow } from "@/lib/hooks";
import { thinkingWords } from "./thinking";

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

/** A mono eyebrow; live text carries the prism gradient sweeping through it (`sweep`: the thinking
 *  line's, a strip of the prism the GPU slides across its words; signals.css). */
export function PrismLabel({ live = false, sweep = false, className, children }: { live?: boolean; sweep?: boolean; className?: string; children: React.ReactNode }) {
  return (
    <span className={cn("sg-label", className)} data-live={live ? "true" : "false"} data-sweep={live && sweep ? "" : undefined}>
      {children}
      {live && sweep && <span className="sg-sweep" aria-hidden="true" />}
    </span>
  );
}

/** What says a subagent is running, the prism sweeping through its words (`on`: else as it is; signals.css). */
export function Shimmer({ on, className, children }: { on: boolean; className?: string; children: React.ReactNode }) {
  return <span className={cn(className, on && "sg-shimmer")}>{children}</span>;
}

/** "<1s", "12s", "1m 05s", "1h 02m". */
export function formatThought(seconds: number) {
  if (!Number.isFinite(seconds) || seconds < 1) return "<1s";
  const s = Math.floor(seconds);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
}

/** A live phase's words and clock share the same start, in a thought block or between steps. */
export function ThinkingLabel({ since, doing }: { since?: number; doing?: string }) {
  const now = useNow();
  const [observed] = React.useState(Date.now);
  const seconds = Math.max(0, (now - (since ?? observed)) / 1000);
  const what = doing && /^(compact|retr|wait|summar)/i.test(doing) ? doing : thinkingWords(seconds);
  return <><PrismLabel live sweep>{what}</PrismLabel><span className="sg-num">{formatThought(seconds)}</span></>;
}

/** The toggle above a thinking block: live it breathes ("Thinking"); settled it reads "Thought · 12s" with a chevron. */
export function ThinkingHead({
  live,
  seconds,
  open,
  onToggle,
  className,
  since,
}: {
  live: boolean;
  seconds?: number;
  open: boolean;
  onToggle: () => void;
  className?: string;
  since?: number;
}) {
  return (
    <button type="button" className={cn("sg-think-head", "sg-thinking-line", className)} data-live={live ? "true" : "false"} aria-expanded={open} onClick={onToggle}>
      {/* The label already names the state; keep the glyph out of the button's accessible name. */}
      <span className="sg-think-glyph" aria-hidden="true">
        <Optic state={live ? "thinking" : "done"} />
      </span>
      {live ? (
        <ThinkingLabel since={since} />
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

// hyperlight's mark (hyperlight-site lib/brand.ts): the open loop of light, and the refraction where
// it leaves. (signals.css draws the same two paths as the running light's mask.)
const LOOP = "M6.4 22.5C2.4 17.1 7.1 6.2 14 4.5C21.4 2.7 27.4 10.1 25.1 16.5C22.8 22.9 13.1 28.8 8.8 26.6";
const TAIL = "M8.8 26.6C6.9 25.6 5.6 23.6 5.2 21.3";

/**
 * A tool call's state, drawn as hyperlight's mark. While it runs, a spectral light sweeps the loop
 * and out through the refraction: a gradient turning behind the mark's shape, which the GPU turns
 * (nothing on the page is painted again for it). Once settled, the loop is drawn whole in the
 * outcome's tone: broken for an error, dashed when denied, faint and breathing while pending.
 */
export function Beam({ status, className }: { status: BeamStatus; className?: string }) {
  return (
    <span className={cn("sg-beam", className)} data-status={status} role="img" aria-label={BEAM_LABELS[status]}>
      <svg className="sg-trace" data-status={status} viewBox="0 0 32 32" aria-hidden="true">
        <path className="sg-trace-loop" d={LOOP} pathLength={100} />
        <path className="sg-trace-tail" d={TAIL} />
      </svg>
      {status === "running" && (
        <span className="sg-trace-glow" aria-hidden="true">
          <span />
        </span>
      )}
    </span>
  );
}

const PRISM = ["#b8d9d2", "#a6c4ed", "#b8a6d5", "#d6a2aa", "#d8cbb0"];

/**
 * A conversation loading: hyperlight's mark, a short spectral light travelling the loop, changing
 * hue as it goes, and leaving through the refraction. Only one is ever on the page, so its moving
 * dash costs little (a tool call's Beam turns a gradient instead: there are many of those).
 */
export function LoadingMark({ className }: { className?: string }) {
  // useId's punctuation isn't safe inside url(#…).
  const gradient = `sg-load-${React.useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  return (
    <svg className={cn("sg-load", className)} viewBox="0 0 32 32" aria-hidden="true">
      <defs>
        <linearGradient id={gradient} x1="5" y1="27" x2="27" y2="5" gradientUnits="userSpaceOnUse">
          {PRISM.map((c, i) => (
            <stop key={c} offset={i / (PRISM.length - 1)} stopColor={c} />
          ))}
        </linearGradient>
      </defs>
      <path className="sg-load-loop" d={LOOP} pathLength={100} />
      <path className="sg-load-light" d={LOOP} pathLength={100} stroke={`url(#${gradient})`} />
      <path className="sg-load-tail" d={TAIL} />
    </svg>
  );
}

/** The streaming-text caret: a thin prism bar that breathes rather than blinks. */
export function Caret({ className }: { className?: string }) {
  return <span className={cn("sg-caret", className)} aria-hidden="true" />;
}
