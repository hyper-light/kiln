/**
 * Line drawings in hyperlight's figure language (hyper-light.dev/blog): thin prism-stroked
 * wireframes, faint glass faces, running lights and mono lettering on a dark stage. A scene is a
 * function of time that returns paths and labels with stable ids; the stage repaints only what
 * changed, so a drawing can move every frame without React re-rendering.
 */

export type Tone = "neutral" | "pass" | "fail" | "pending" | "error";
/** Stroke weights and fills, as on the site: glass faces, edges, fine detail, rear (hidden) lines, running lights. */
export type PathKind = "glass" | "edge" | "fine" | "light" | "shade" | "rear";
export type LabelKind = "heading" | "name" | "label" | "small" | "status";

export interface FigPath {
  id: string;
  d: string;
  kind: PathKind;
  opacity: number;
  tone?: Tone;
  /** A fixed dash pattern (orbits, dashed tethers). Running lights have their own. */
  dash?: string;
  /** Running-light speed, in path-length units (of 200) per second. */
  speed?: number;
}

export interface FigLabel {
  id: string;
  text: string;
  x: number;
  y: number;
  kind: LabelKind;
  anchor: "start" | "middle" | "end";
  tone?: Tone;
  opacity: number;
}

/** A pointer target that moves with the drawing (an agent's craft). */
export interface FigHit {
  id: string;
  x: number;
  y: number;
  r: number;
  label: string;
}

export interface Frame {
  paths: FigPath[];
  labels: FigLabel[];
  hits: FigHit[];
}

/** time in seconds; w × h is the stage in CSS pixels (one SVG unit is one pixel). */
export type DrawFn = (time: number, w: number, h: number) => Frame;

export const TONES: Record<Exclude<Tone, "neutral">, string> = {
  pass: "#a8c9bc",
  fail: "#d5a4ad",
  pending: "#aab8d2",
  error: "#d8bb95",
};
export const toneColor = (t?: Tone) => (t && t !== "neutral" ? TONES[t] : undefined);

export const clamp01 = (v: number) => (v < 0 ? 0 : v > 1 ? 1 : v);
/** Smoothstep: how the site eases every transition. */
export const ease = (v: number) => {
  const t = clamp01(v);
  return t * t * (3 - 2 * t);
};
export const lerp = (a: number, b: number, t: number) => a + (b - a) * t;

export type P2 = readonly [number, number];
export type P3 = readonly [number, number, number];
export type Project = (p: P3) => P2;

/** A fixed orthographic camera, as the site's spatial figures use: x/y form the floor, z is height. */
export function camera(origin: P2, { yaw = -0.32, pitch = 0.85, scale = 1 } = {}): Project {
  const cy = Math.cos(yaw);
  const sy = Math.sin(yaw);
  const sp = Math.sin(pitch);
  const cp = Math.cos(pitch);
  return ([x, y, z]) => [origin[0] + scale * (cy * x - sy * y), origin[1] + scale * (sp * (sy * x + cy * y) - cp * z)];
}

const fmt = (n: number) => (Number.isFinite(n) ? n.toFixed(1) : "0");
export function pathData(points: readonly P2[], closed = false): string {
  let d = "";
  for (let i = 0; i < points.length; i++) {
    const p = points[i]!;
    d += `${i ? "L" : "M"}${fmt(p[0])} ${fmt(p[1])}`;
  }
  return closed && d ? `${d} Z` : d;
}

/** The point a fraction t along a polyline. */
export function along(points: readonly P2[], t: number): P2 {
  if (points.length < 2) return points[0] ?? [0, 0];
  const lengths: number[] = [];
  let total = 0;
  for (let i = 1; i < points.length; i++) {
    const l = Math.hypot(points[i]![0] - points[i - 1]![0], points[i]![1] - points[i - 1]![1]);
    lengths.push(l);
    total += l;
  }
  let rest = clamp01(t) * total;
  for (let i = 0; i < lengths.length; i++) {
    const l = lengths[i]!;
    if (rest <= l || i === lengths.length - 1) {
      const k = l ? rest / l : 0;
      const a = points[i]!;
      const b = points[i + 1]!;
      return [a[0] + (b[0] - a[0]) * k, a[1] + (b[1] - a[1]) * k];
    }
    rest -= l;
  }
  return points[points.length - 1]!;
}

/** A repeating sweep that fades at its seam (pulses, radar rings). */
export function pulse(time: number, speed = 0.22, offset = 0) {
  const progress = (((Math.max(0, time) * speed + offset) % 1) + 1) % 1;
  return { progress, opacity: Math.min(1, progress * 10, (1 - progress) * 10) };
}

/** A drawing under construction. Every call adds one path or label under a stable id. */
export function sketch() {
  const paths: FigPath[] = [];
  const labels: FigLabel[] = [];
  const hits: FigHit[] = [];
  const path = (id: string, points: readonly P2[], kind: PathKind = "fine", opacity = 0.4, tone?: Tone, closed = false, dash?: string) => {
    paths.push({ id, d: pathData(points, closed), kind, opacity: clamp01(opacity), tone, dash });
  };
  const raw = (id: string, d: string, kind: PathKind, opacity: number, tone?: Tone, extra?: Partial<FigPath>) => {
    paths.push({ id, d, kind, opacity: clamp01(opacity), tone, ...extra });
  };
  const line = (id: string, a: P2, b: P2, opacity = 0.4, tone?: Tone, kind: PathKind = "fine") => path(id, [a, b], kind, opacity, tone);
  const ellipse = (id: string, cx: number, cy: number, rx: number, ry: number, kind: PathKind = "edge", opacity = 0.5, tone?: Tone, dash?: string, segments = 48) =>
    path(
      id,
      Array.from({ length: segments }, (_, i) => [cx + Math.cos((i / segments) * Math.PI * 2) * rx, cy + Math.sin((i / segments) * Math.PI * 2) * ry] as const),
      kind,
      opacity,
      tone,
      true,
      dash,
    );
  const ring = (id: string, cx: number, cy: number, r: number, kind: PathKind = "edge", opacity = 0.5, tone?: Tone) => ellipse(id, cx, cy, r, r, kind, opacity, tone, undefined, 32);
  /** A diamond marker (the site's packets and ledger points). */
  const diamond = (id: string, x: number, y: number, r = 3, opacity = 0.85, tone?: Tone) =>
    path(
      id,
      [
        [x, y - r],
        [x + r, y],
        [x, y + r],
        [x - r, y],
      ],
      "glass",
      opacity,
      tone,
      true,
    );
  /** A chamfered instrument panel with depth, as the site's ledger cards are drawn. */
  const slab = (id: string, x: number, y: number, w: number, h: number, depth = 6, opacity = 1, tone?: Tone) => {
    const cut = Math.min(7, w / 8, h / 4);
    const face: P2[] = [
      [x + cut, y],
      [x + w - cut, y],
      [x + w, y + cut],
      [x + w, y + h - cut],
      [x + w - cut, y + h],
      [x + cut, y + h],
      [x, y + h - cut],
      [x, y + cut],
    ];
    const off: P2 = [depth * 0.65, -depth];
    path(
      `${id}-back`,
      face.map(([px, py]) => [px + off[0], py + off[1]] as const),
      "rear",
      0.25 * opacity,
      tone,
      true,
    );
    for (const i of [0, 1, 2]) {
      const p = face[i]!;
      line(`${id}-depth-${i}`, p, [p[0] + off[0], p[1] + off[1]], 0.32 * opacity, tone);
    }
    path(`${id}-face`, face, "glass", 0.7 * opacity, tone, true);
    path(`${id}-edge`, face, "edge", 0.6 * opacity, tone, true);
    line(`${id}-lip`, [x + cut, y + 3], [x + w - cut, y + 3], 0.55 * opacity, tone);
  };
  /** A box in 3-D under a camera: far faces faint, near faces glass, a bright rim on top. */
  const box = (id: string, project: Project, [x, y, z]: P3, [w, d, h]: P3, opacity = 0.8, tone?: Tone) => {
    const p = (a: number, b: number, c: number) => project([x + a, y + b, z + c]);
    path(`${id}-far`, [p(0, d, 0), p(w, d, 0), p(w, d, h), p(0, d, h)], "rear", opacity * 0.4, tone, true);
    path(`${id}-side`, [p(w, 0, 0), p(w, d, 0), p(w, d, h), p(w, 0, h)], "glass", opacity * 0.62, tone, true);
    path(`${id}-front`, [p(0, 0, 0), p(w, 0, 0), p(w, 0, h), p(0, 0, h)], "glass", opacity * 0.78, tone, true);
    path(`${id}-top`, [p(0, 0, h), p(w, 0, h), p(w, d, h), p(0, d, h)], "glass", opacity * 0.9, tone, true);
    path(`${id}-rim`, [p(0, 0, h), p(w, 0, h), p(w, d, h)], "edge", Math.min(1, opacity * 1.05), tone);
  };
  const label = (id: string, text: string, x: number, y: number, kind: LabelKind = "label", tone?: Tone, anchor: FigLabel["anchor"] = "middle", opacity = 1) => {
    labels.push({ id, text, x, y, kind, anchor, tone, opacity: clamp01(opacity) });
  };
  const hit = (id: string, x: number, y: number, r: number, name: string) => hits.push({ id, x, y, r, label: name });
  return { path, raw, line, ellipse, ring, diamond, slab, box, label, hit, frame: (): Frame => ({ paths, labels, hits }) };
}

export type Sketch = ReturnType<typeof sketch>;

/** Text that fits a column of mono lettering (about 6.6px a character at 11px). */
export function clip(text: string, px: number, perChar = 6.6): string {
  const max = Math.max(3, Math.floor(px / perChar));
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}
