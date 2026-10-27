/**
 * A commit's states as glyphs (status-glyph-spec.ts): one shape and one word each, every one with a
 * motion of its own (status-glyph.css), and the journey strip of seven of them, review to replant.
 */
import * as React from "react";
import { cn } from "@/lib/utils";
import { arcPath, GLYPHS, journeyLabel, pipDone, pipFlows, STAGE_SHORT, STATES, type GlyphState, type Part, type Pip, type Tone } from "./status-glyph-spec";
import "./status-glyph.css";

const TONE: Record<Exclude<Tone, "prism">, string> = {
  needs: "#e2cfae",
  fail: "#e2b9bd",
  pause: "#b8c2d3",
  work: "#cdc6ea",
  done: "#bfdccf",
  warn: "#d0b591",
  idle: "#a1a4ad",
  off: "#8b8e96",
  faint: "#5c6470",
  tag: "#d8cbb0",
};
/** The states drawn in the prism a working ring turns through: they carry its gradient. */
const PRISMED = new Set<GlyphState>(STATES.filter((s) => JSON.stringify(GLYPHS[s].parts).includes('"prism"')));
const PRISM = [
  ["#b8d9d2", 0],
  ["#a6c4ed", 0.33],
  ["#b8a6d5", 0.66],
  ["#d6a2aa", 1],
] as const;

/**
 * Every looping motion keeps one clock, the page's: a glyph drawn anew (a state changed, a row
 * came back) joins its motion where every other glyph is rather than starting over.
 */
export function phaseLock(el: Element | null) {
  if (!el || typeof el.getAnimations !== "function") return;
  for (const a of el.getAnimations({ subtree: true })) if (a.playState === "running" && a.startTime !== 0) a.startTime = 0;
}

function Shape({ part, p, prism }: { part: Part; p: number; prism: string }) {
  const motion = part.m ? `sg-m sg-${part.m}` : undefined;
  if (part.k === "g") {
    return (
      <g className={motion}>
        {part.parts.map((q, i) => (
          <Shape key={i} part={q} p={p} prism={prism} />
        ))}
      </g>
    );
  }
  const paint = (t: Tone | undefined) => (t === undefined ? "none" : t === "prism" ? `url(#${prism})` : TONE[t]);
  const look = {
    stroke: paint(part.stroke),
    strokeWidth: part.w,
    fill: part.k === "arc" ? "none" : paint(part.fill),
    opacity: part.k === "arc" ? undefined : part.o,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    className: motion,
  };
  if (part.k === "arc") return <path d={arcPath(p)} {...look} />;
  if (part.k === "circle") return <circle cx={part.cx} cy={part.cy} r={part.r} strokeDasharray={part.dash} {...look} />;
  return <path d={part.d} {...look} />;
}

/** One state as its glyph: `p` is how far a wait has come; `label` names it otherwise than its word. */
export function StatusGlyph({ state, size = 14, p = 0, label, className }: { state: GlyphState; size?: 12 | 14 | 16 | 20; p?: number; label?: string; className?: string }) {
  const prism = `sg${React.useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  const glyph = GLYPHS[state];
  return (
    // A new state is a new glyph, so its motion is set on the page's clock as it's drawn.
    <svg key={state} ref={phaseLock} viewBox="0 0 14 14" width={size} height={size} className={cn("sg", className)} data-state={state} role="img" aria-label={label ?? glyph.word}>
      {PRISMED.has(state) && (
        <defs>
          <linearGradient id={prism} x1="0" x2="1" y1="0" y2="1">
            {PRISM.map(([c, at]) => (
              <stop key={c} offset={at} stopColor={c} />
            ))}
          </linearGradient>
        </defs>
      )}
      {glyph.parts.map((part, i) => (
        <Shape key={i} part={part} p={p} prism={prism} />
      ))}
    </svg>
  );
}

/** A commit's journey as seven pips joined by a line: lit where the stage before is done, light flowing where work is under way. */
export function JourneyStrip({ pips, labels = true, className }: { pips: readonly Pip[]; labels?: boolean; className?: string }) {
  return (
    <span ref={phaseLock} className={cn("js", className)} role="img" aria-label={journeyLabel(pips)}>
      {pips.map((pip, i) => {
        const before = i > 0 ? pips[i - 1] : undefined;
        return (
          <React.Fragment key={pip.stage}>
            {before && <span className="js-c" data-lit={pipDone(before.state) && pip.state !== "none" ? "" : undefined} data-flow={pipDone(before.state) && pipFlows(pip.state) ? "" : undefined} />}
            <span className="js-p" data-on={pip.state !== "none" ? "" : undefined}>
              <StatusGlyph state={pip.state} p={pip.p} />
              {labels && <span className="js-l">{STAGE_SHORT[pip.stage]}</span>}
            </span>
          </React.Fragment>
        );
      })}
    </span>
  );
}
