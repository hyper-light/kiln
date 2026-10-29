/**
 * Every state a commit's journey can be in, as one shape and one word: status-glyph.tsx draws it,
 * status-glyph.css moves it. A shape tells each state apart without its colour (WCAG 1.4.1); the
 * word is its accessible name. A code failure, CI's own error and a flake are three different
 * shapes (OpenTelemetry's CI/CD conventions keep them apart, and flakes erode trust in red).
 * Pure, so tests/status-glyph.test.ts checks it.
 */

export const STATES = [
  "none",
  "queued",
  "running",
  "passed",
  "reused",
  "flaky",
  "failed",
  "error",
  "superseded",
  "needs",
  "waiting",
  "soaking",
  "guard",
  "healthy",
  "degraded",
  "drift",
  "rode",
  "rolledback",
  "replanting",
  "replanted",
  "partial",
  "offline",
  "tag",
  "agent",
  "person",
  "flag",
  "draft",
  "requested",
  "changes",
  "blocked",
  "fixing",
  "conflict",
  "approved",
] as const;
export type GlyphState = (typeof STATES)[number];

/** A colour from Mission control's stands (stands.css), or the prism a working ring turns through. */
export type Tone = "needs" | "fail" | "pause" | "work" | "done" | "warn" | "idle" | "off" | "faint" | "tag" | "prism";

/** How a part moves (status-glyph.css): only transform and opacity. */
export type Motion =
  "turn" | "call" | "breathe" | "swap-a" | "swap-b" | "crawl" | "crawl-slow" | "glow" | "wave" | "throb" | "blink" | "hand" | "flicker" | "fade" | "rewind" | "sway" | "drift" | "twinkle" | "flutter";

export type Part =
  | { k: "circle"; cx: number; cy: number; r: number; stroke?: Tone; fill?: Tone; w?: number; dash?: string; o?: number; m?: Motion }
  | { k: "path"; d: string; stroke?: Tone; fill?: Tone; w?: number; o?: number; m?: Motion }
  /** How far a wait has come, drawn from its share (`p`, 0 to 1) round a 5-unit ring. */
  | { k: "arc"; stroke: Tone; w: number; m?: Motion }
  | { k: "g"; m: Motion; parts: Part[] };

export interface Glyph {
  word: string;
  parts: Part[];
}

const ring = (stroke: Tone, w = 1.3, extra: Partial<Extract<Part, { k: "circle" }>> = {}): Part => ({ k: "circle", cx: 7, cy: 7, r: 5, stroke, w, ...extra });
const BUBBLE = "M3.2 1.8 H10.8 Q12.6 1.8 12.6 3.6 V8.2 Q12.6 10 10.8 10 H6.6 L3.8 12.4 V10 H3.2 Q1.4 10 1.4 8.2 V3.6 Q1.4 1.8 3.2 1.8 Z";
const STEM = "M7 12.6 V6.6";
const LEAF_LOW = "M7 8.6 C4.4 8.8 2.8 7.2 2.8 4.6 C5.4 4.6 7 6.2 7 8.6 Z";
const LEAF_HIGH = "M7 7 C7 4.2 8.6 2.4 11.4 2.4 C11.4 5.2 9.6 7 7 7 Z";

export const GLYPHS: Record<GlyphState, Glyph> = {
  none: { word: "not yet", parts: [ring("faint", 1.2, { o: 0.5 })] },
  queued: { word: "queued", parts: [ring("idle", 1.3, { dash: "2.2 2.1", m: "crawl" })] },
  running: { word: "running", parts: [{ k: "g", m: "turn", parts: [ring("prism", 1.8, { dash: "23.6 7.8" })] }] },
  passed: { word: "passed", parts: [{ k: "path", d: "M2.6 7.4 L5.6 10.2 L11.4 3.8", stroke: "done", w: 1.8, m: "glow" }] },
  reused: {
    word: "reused",
    parts: [
      { k: "circle", cx: 7, cy: 7, r: 5.6, stroke: "done", w: 1, dash: "1.4 1.6", o: 0.7, m: "crawl" },
      { k: "path", d: "M4.4 7.2 L6.2 8.9 L9.7 5.1", stroke: "done", w: 1.5 },
    ],
  },
  flaky: {
    word: "flaky",
    parts: [
      { k: "circle", cx: 7, cy: 7, r: 5.2, stroke: "warn", w: 1.3 },
      { k: "path", d: "M4 7.6 C5 5.6 6 5.6 7 7 S9 8.4 10 6.4", stroke: "warn", w: 1.4, m: "wave" },
    ],
  },
  failed: {
    word: "failed",
    parts: [
      { k: "circle", cx: 7, cy: 7, r: 5.2, stroke: "fail", w: 1.3 },
      { k: "path", d: "M5.1 5.1 L8.9 8.9 M8.9 5.1 L5.1 8.9", stroke: "fail", w: 1.5, m: "throb" },
    ],
  },
  error: {
    word: "kiln error",
    parts: [
      { k: "path", d: "M7 1.8 L12.6 11.6 H1.4 Z", stroke: "warn", w: 1.3 },
      {
        k: "g",
        m: "blink",
        parts: [
          { k: "path", d: "M7 5.4 V8", stroke: "warn", w: 1.5 },
          { k: "circle", cx: 7, cy: 9.8, r: 0.8, fill: "warn" },
        ],
      },
    ],
  },
  superseded: { word: "superseded", parts: [ring("off", 1.2), { k: "path", d: "M3.9 10.1 L10.1 3.9", stroke: "off", w: 1.2 }] },
  needs: {
    word: "needs you",
    parts: [
      {
        k: "g",
        m: "call",
        parts: [
          { k: "path", d: BUBBLE, stroke: "needs", w: 1.4 },
          { k: "path", d: "M7 4 V6.6", stroke: "needs", w: 1.6 },
          { k: "circle", cx: 7, cy: 8.4, r: 0.85, fill: "needs" },
        ],
      },
    ],
  },
  waiting: { word: "waiting", parts: [ring("pause", 1.3, { o: 0.25 }), { k: "arc", stroke: "pause", w: 1.8 }, { k: "path", d: "M7 4.6 V7 L8.6 8", stroke: "pause", w: 1.2, m: "hand" }] },
  soaking: {
    word: "soaking",
    parts: [ring("done", 1.3, { o: 0.25 }), { k: "arc", stroke: "done", w: 1.8 }, { k: "path", d: "M4.8 7.6 C5.5 6.8 6.2 6.8 7 7.6 S8.5 8.4 9.2 7.6", stroke: "done", w: 1.2, m: "wave" }],
  },
  guard: { word: "guard", parts: [ring("work", 1.3, { o: 0.25 }), { k: "arc", stroke: "work", w: 1.8 }, { k: "circle", cx: 7, cy: 7, r: 1.6, fill: "work", m: "breathe" }] },
  healthy: { word: "healthy", parts: [ring("done", 1.3), { k: "circle", cx: 7, cy: 7, r: 2.3, fill: "done", m: "breathe" }] },
  degraded: { word: "degraded", parts: [ring("fail", 1.3), { k: "path", d: "M4.2 7.4 L5.9 5.8 L7.9 8.6 L9.8 6.6", stroke: "fail", w: 1.3, m: "flicker" }] },
  drift: {
    word: "drifted",
    parts: [
      { k: "circle", cx: 5.8, cy: 7, r: 4, stroke: "warn", w: 1.2, o: 0.55 },
      { k: "circle", cx: 8.2, cy: 7, r: 4, stroke: "warn", w: 1.4, m: "drift" },
    ],
  },
  rode: { word: "rode along", parts: [ring("done", 1, { dash: "1 2.1", o: 0.75, m: "crawl" }), { k: "circle", cx: 7, cy: 7, r: 1.9, fill: "done", o: 0.85 }] },
  rolledback: {
    word: "rolled back",
    parts: [
      {
        k: "g",
        m: "rewind",
        parts: [
          { k: "path", d: "M3.6 4.6 A4.8 4.8 0 1 1 2.4 8.4", stroke: "fail", w: 1.5 },
          { k: "path", d: "M2.4 2.2 V5 H5.2", stroke: "fail", w: 1.5 },
        ],
      },
    ],
  },
  replanting: {
    word: "replanting",
    parts: [
      { k: "circle", cx: 5.3, cy: 7, r: 3.9, stroke: "idle", w: 1.3, m: "swap-a" },
      { k: "circle", cx: 8.7, cy: 7, r: 3.9, stroke: "work", w: 1.6, m: "swap-b" },
    ],
  },
  replanted: {
    word: "replanted",
    parts: [
      {
        k: "g",
        m: "sway",
        parts: [
          { k: "path", d: STEM, stroke: "done", w: 1.5 },
          { k: "path", d: LEAF_LOW, fill: "done", o: 0.7 },
          { k: "path", d: LEAF_HIGH, fill: "done" },
        ],
      },
    ],
  },
  partial: {
    word: "partly replanted",
    parts: [
      {
        k: "g",
        m: "sway",
        parts: [
          { k: "path", d: STEM, stroke: "warn", w: 1.5 },
          { k: "path", d: LEAF_HIGH, fill: "warn" },
        ],
      },
      { k: "path", d: "M2.6 4.2 L5 6.6 M5 4.2 L2.6 6.6", stroke: "fail", w: 1.3, m: "blink" },
    ],
  },
  offline: { word: "offline", parts: [ring("off", 1.3, { dash: "2.2 2.1", m: "crawl-slow" }), { k: "path", d: "M4.5 9.5 L9.5 4.5", stroke: "off", w: 1, o: 0.6 }] },
  tag: {
    word: "tag",
    parts: [
      { k: "path", d: "M1.8 2.8 V7 L7.2 12.4 L12.4 7.2 L7 1.8 H2.8 Q1.8 1.8 1.8 2.8 Z", stroke: "tag", w: 1.3 },
      { k: "circle", cx: 4.7, cy: 4.7, r: 1, fill: "tag", m: "glow" },
    ],
  },
  agent: { word: "agent", parts: [{ k: "path", d: "M7 1.6 L8.3 5.7 L12.4 7 L8.3 8.3 L7 12.4 L5.7 8.3 L1.6 7 L5.7 5.7 Z", stroke: "work", w: 1.2, m: "twinkle" }] },
  person: {
    word: "person",
    parts: [
      { k: "circle", cx: 7, cy: 4.9, r: 2.4, stroke: "idle", w: 1.3 },
      { k: "path", d: "M2.6 12.2 C3.2 9.6 5 8.6 7 8.6 C9 8.6 10.8 9.6 11.4 12.2", stroke: "idle", w: 1.3 },
    ],
  },
  flag: {
    word: "environment",
    parts: [
      { k: "path", d: "M3 12.6 V1.8", stroke: "idle", w: 1.3 },
      { k: "path", d: "M3 2.2 H11 L9 4.8 L11 7.4 H3", stroke: "done", w: 1.3, m: "flutter" },
    ],
  },
  draft: {
    word: "draft",
    parts: [
      { k: "path", d: "M3.4 1.8 H8.4 L10.6 4 V12.2 H3.4 Z", stroke: "idle", w: 1.2, m: "fade" },
      { k: "path", d: "M5.2 7 H8.8 M5.2 9.4 H7.6", stroke: "idle", w: 1.1 },
    ],
  },
  requested: {
    word: "requested",
    parts: [
      { k: "path", d: BUBBLE, stroke: "pause", w: 1.3 },
      {
        k: "g",
        m: "wave",
        parts: [
          { k: "circle", cx: 4.6, cy: 5.9, r: 0.8, fill: "pause" },
          { k: "circle", cx: 7, cy: 5.9, r: 0.8, fill: "pause" },
          { k: "circle", cx: 9.4, cy: 5.9, r: 0.8, fill: "pause" },
        ],
      },
    ],
  },
  changes: {
    word: "changes asked",
    parts: [
      { k: "path", d: BUBBLE, stroke: "warn", w: 1.3 },
      { k: "path", d: "M4.4 6.2 C5.3 5 6.1 5 7 6.2 S8.7 7.4 9.6 6.2", stroke: "warn", w: 1.3, m: "wave" },
    ],
  },
  blocked: {
    word: "blocked",
    parts: [
      { k: "path", d: "M5 1.6 H9 L12.4 5 V9 L9 12.4 H5 L1.6 9 V5 Z", stroke: "fail", w: 1.3 },
      { k: "path", d: "M4.6 7 H9.4", stroke: "fail", w: 1.6, m: "throb" },
    ],
  },
  fixing: {
    word: "fixing",
    parts: [
      { k: "g", m: "turn", parts: [ring("work", 1.2, { dash: "3.2 2.4" })] },
      { k: "path", d: "M7 4.2 L7.8 6.2 L9.8 7 L7.8 7.8 L7 9.8 L6.2 7.8 L4.2 7 L6.2 6.2 Z", stroke: "work", w: 1.1, m: "twinkle" },
    ],
  },
  conflict: {
    word: "conflicts",
    parts: [
      { k: "path", d: "M3.6 12.4 V9 C3.6 6.6 7 6.6 7 4 M10.4 12.4 V9 C10.4 6.6 7 6.6 7 4", stroke: "warn", w: 1.3 },
      { k: "circle", cx: 7, cy: 2.6, r: 1.1, fill: "fail", m: "blink" },
    ],
  },
  approved: { word: "approved", parts: [ring("done", 1.3), { k: "path", d: "M4.3 7.3 L6.2 9.1 L9.8 5.2", stroke: "done", w: 1.5, m: "glow" }] },
};

/** The path of a wait's progress: from the top of a 5-unit ring, clockwise, `p` of the way round. */
export function arcPath(p: number): string {
  const q = Math.max(0.001, Math.min(0.999, Number.isFinite(p) ? p : 0));
  const a = q * Math.PI * 2;
  const x = 7 + 5 * Math.sin(a);
  const y = 7 - 5 * Math.cos(a);
  return `M7 2 A5 5 0 ${q > 0.5 ? 1 : 0} 1 ${x.toFixed(2)} ${y.toFixed(2)}`;
}

/** A state's shape without its colours: two states never share one (tests hold it). */
export function signature(state: GlyphState): string {
  const strip = (parts: readonly Part[]): unknown[] =>
    parts.map((part) => {
      if (part.k === "g") return { k: "g", parts: strip(part.parts) };
      if (part.k === "arc") return { k: "arc" };
      if (part.k === "circle") return { k: "circle", cx: part.cx, cy: part.cy, r: part.r, w: part.w, dash: part.dash, filled: part.fill !== undefined };
      return { k: "path", d: part.d, w: part.w, filled: part.fill !== undefined };
    });
  return JSON.stringify(strip(GLYPHS[state].parts));
}

/** Whether any part of a state's shape moves (a group always does: it's grouped to move together). */
export function moves(state: GlyphState): boolean {
  return GLYPHS[state].parts.some((part) => part.m !== undefined);
}

// ---- The journey: a commit from review to replanting --------------------------------------------

export const STAGES = ["review", "land", "checks", "build", "staging", "production", "replant"] as const;
export type Stage = (typeof STAGES)[number];
export const STAGE_WORD: Record<Stage, string> = { review: "Review", land: "Land", checks: "Checks", build: "Build", staging: "Staging", production: "Production", replant: "Replant" };
/** The shorter word under a pip, where room is tight. */
export const STAGE_SHORT: Record<Stage, string> = { review: "Review", land: "Land", checks: "Checks", build: "Build", staging: "Staging", production: "Prod", replant: "Replant" };

export interface Pip {
  stage: Stage;
  state: GlyphState;
  /** How far a wait has come (soaking, guard, waiting), 0 to 1. */
  p?: number;
}

/** A stage whose work is done, so the line to the next one lights. */
export function pipDone(state: GlyphState): boolean {
  return (
    state === "passed" ||
    state === "reused" ||
    state === "flaky" ||
    state === "healthy" ||
    state === "rode" ||
    state === "replanted" ||
    state === "soaking" ||
    state === "partial" ||
    state === "approved"
  );
}

/** A stage with work under way (or waiting on you), so light flows into it along the line. */
export function pipFlows(state: GlyphState): boolean {
  return state === "running" || state === "guard" || state === "replanting" || state === "needs" || state === "fixing";
}

/** The whole strip in words, as its accessible name: each stage and how it stands. */
export function journeyLabel(pips: readonly Pip[]): string {
  return pips.map((pip) => `${STAGE_WORD[pip.stage]}: ${GLYPHS[pip.state].word}`).join(", ");
}
