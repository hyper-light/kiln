/**
 * The rule under Mission control's head (stands.tsx), as geometry. A stretch for each way your
 * agents stand, as long as its share of them, with a gap between each two. One wave runs across
 * all of them, as lively in each stretch as its agents: working the most, then ready, needs you,
 * paused, finished and stopped, and offline lies flat. So the wave goes on across each gap, and at
 * each gap both ends reach for the point where it crosses: they bend toward its height and stretch
 * into the gap and back, never touching. Each stretch keeps its own colour and mark (a line, beads
 * or dashes). Pure, apart from `draw`, so tests/stands-wave.test.ts checks it.
 */
import type { MissionStand } from "./mission-rows";

export type Shown = Exclude<MissionStand, "ended">;

/** A stretch's mark: a line on the wave, beads on it (`every` points apart, of radius `r`), or dashes. */
export type Mark = { kind: "line" } | { kind: "beads"; every: number; r: number } | { kind: "dashes" };

/** Each way agents stand: its colour (working's line is the prism), its mark, and its waves (amplitude, length, and the seconds a length takes to pass). */
export const LOOK: Record<Shown, { colour: string; mark: Mark; waves: readonly (readonly [number, number, number])[] }> = {
  working: { colour: "#bab0e3", mark: { kind: "line" }, waves: [[2.9, 18, 1.15], [1.05, 31, 2.05]] },
  ready: { colour: "#6f8f84", mark: { kind: "line" }, waves: [[2.2, 26, 2]] },
  needs: { colour: "#d8bb95", mark: { kind: "line" }, waves: [[1.7, 32, 2.8]] },
  paused: { colour: "#a6b2c6", mark: { kind: "beads", every: 5, r: 1.1 }, waves: [[1.2, 40, 4]] },
  finished: { colour: "#a8c9bc", mark: { kind: "line" }, waves: [[0.8, 56, 6]] },
  crashed: { colour: "#d1aaad", mark: { kind: "dashes" }, waves: [[0.45, 72, 8]] },
  offline: { colour: "#7e818a", mark: { kind: "beads", every: 7, r: 1.05 }, waves: [] },
};
/** Liveliest first. */
export const LIVELINESS: readonly Shown[] = ["working", "ready", "needs", "paused", "finished", "crashed", "offline"];
export const PRISM = ["#b8d9d2", "#a6c4ed", "#b8a6d5", "#d6a2aa", "#d8cbb0"] as const;

/** The rule's box is HEIGHT tall, its line at Y. */
export const HEIGHT = 12;
export const Y = 6;
/** Points between two stretches, and the least a stretch is. */
const GAP = 5;
const MIN = 12;
/** How far an end stretches into its gap at most, and over how many points it bends toward where it meets the next. */
const REACH = 1.15;
const BEND = 10;
/** How long a change of counts takes to settle into the stretches' lengths (ms). */
export const GROW_MS = 600;

const smoother = (x: number) => (x <= 0 ? 0 : x >= 1 ? 1 : x * x * x * (x * (6 * x - 15) + 10));
export const easeOut = (x: number) => 1 - (1 - Math.min(1, Math.max(0, x))) ** 3;

/** A stretch: where its mark is drawn (`x0` to `x1`), and its region of the wave (`r0` to `r1`: from the middle of the gap before it to the middle of the gap after, or the rule's end). */
export type Part = { stand: Shown; x0: number; x1: number; r0: number; r1: number };

/**
 * The stretches across `w`, in order. A count under 1 is a stretch coming or going: it grows from
 * nothing, and so does one gap beside it, so once it's gone its neighbours are one gap apart, as
 * they'll stay.
 */
export function stretches(stands: readonly Shown[], counts: readonly number[], w: number): Part[] {
  const present = stands.map((_, i) => Math.min(1, Math.max(0, counts[i] ?? 0)));
  // The gap before each stretch: as there as it is, and as the most there of those before it.
  let before = 0;
  const gaps = present.map((p, i) => {
    const g = i === 0 ? 0 : GAP * Math.min(p, before);
    before = Math.max(before, p);
    return g;
  });
  const fixed = present.reduce((s, p) => s + MIN * p, 0) + gaps.reduce((s, g) => s + g, 0);
  const total = stands.reduce((s, _, i) => s + Math.max(0, counts[i] ?? 0), 0) || 1;
  const free = Math.max(0, w - fixed);
  let x = 0;
  const parts = stands.map((stand, i): Part => {
    x += gaps[i] ?? 0;
    const len = MIN * (present[i] ?? 0) + (free * Math.max(0, counts[i] ?? 0)) / total;
    const p = { stand, x0: x, x1: x + len, r0: 0, r1: w };
    x += len;
    return p;
  });
  let prev: Part | undefined;
  for (const p of parts) {
    if (prev) {
      const mid = (prev.x1 + p.x0) / 2;
      prev.r1 = mid;
      p.r0 = mid;
    }
    prev = p;
  }
  return parts;
}

/** How far each join's crossfade reaches either side of its gap's middle: up to 13 points, at most two-fifths of either region. */
export function reaches(parts: readonly Part[]): number[] {
  const out: number[] = [];
  for (let i = 1; i < parts.length; i++) {
    const a = parts[i - 1];
    const b = parts[i];
    if (a && b) out.push(Math.min(13, (a.r1 - a.r0) / 2.5, (b.r1 - b.r0) / 2.5));
  }
  return out;
}

/** Each stretch's share of the wave at `x`: 1 inside its region, crossfading with its neighbour's across each gap. */
export function shares(parts: readonly Part[], h: readonly number[], x: number): number[] {
  const out = parts.map(() => 0);
  let i = parts.findIndex((p) => x < p.r1);
  if (i < 0) i = parts.length - 1;
  const p = parts[i];
  if (!p) return out;
  out[i] = 1;
  const left = h[i - 1] ?? 0;
  const right = h[i] ?? 0;
  if (i > 0 && left > 0 && x < p.r0 + left) {
    const k = smoother((x - (p.r0 - left)) / (2 * left));
    out[i] = k;
    out[i - 1] = 1 - k;
  }
  if (i < parts.length - 1 && right > 0 && x > p.r1 - right) {
    const k = smoother((x - (p.r1 - right)) / (2 * right));
    out[i] = 1 - k;
    out[i + 1] = k;
  }
  return out;
}

/** The wave's height at `x` and `t` (seconds): every stretch's waves weighted by its share there, held at the line at the rule's two ends. */
export function wave(parts: readonly Part[], h: readonly number[], w: number, x: number, t: number): number {
  const sh = shares(parts, h, x);
  const edge = smoother(Math.min(x, w - x) / 10);
  let y = 0;
  parts.forEach((p, i) => {
    const s = sh[i] ?? 0;
    if (!s) return;
    for (const [amp, len, period] of LOOK[p.stand].waves) y += s * amp * Math.sin(2 * Math.PI * (x / len - t / period));
  });
  return Y + edge * y;
}

/** Where two stretches meet: the middle of the gap between them, at the height the wave crosses it, and how far each end reaches into the gap at `t` (each gap at its own moment). */
export type Meet = { x: number; y: number; e: number };
export function meets(parts: readonly Part[], h: readonly number[], w: number, t: number): Meet[] {
  const out: Meet[] = [];
  for (let j = 1; j < parts.length; j++) {
    const a = parts[j - 1];
    const b = parts[j];
    if (!a || !b) continue;
    const x = (a.x1 + b.x0) / 2;
    out.push({ x, y: wave(parts, h, w, x, t), e: REACH * (0.5 - 0.5 * Math.cos(2 * Math.PI * (t / 2.6 + (j - 1) * 0.37))) });
  }
  return out;
}

/** Stretch `i`'s height at `x`: the wave, bent toward the meeting point beside each of its ends (`m[i - 1]` before it, `m[i]` after). */
export function drawn(parts: readonly Part[], h: readonly number[], w: number, i: number, x: number, t: number, m: readonly Meet[]): number {
  let y = wave(parts, h, w, x, t);
  const p = parts[i];
  if (!p) return y;
  const R = Math.min(BEND, (p.x1 - p.x0) / 2.5);
  if (R <= 0) return y;
  const before = i > 0 ? m[i - 1] : undefined;
  const after = i < parts.length - 1 ? m[i] : undefined;
  if (before) y += (before.y - y) * smoother(1 - Math.max(0, x - p.x0) / R);
  if (after) y += (after.y - y) * smoother(1 - Math.max(0, p.x1 - x) / R);
  return y;
}

const f = (v: number) => (Math.round(v * 100) / 100).toString();

/** Stretch `i`'s line on the wave, as an SVG path: its ends reach into the gaps beside it. */
export function linePath(parts: readonly Part[], h: readonly number[], w: number, i: number, t: number, m: readonly Meet[]): string {
  const p = parts[i];
  if (!p) return "";
  const a = p.x0 - (i > 0 ? (m[i - 1]?.e ?? 0) : 0);
  const b = p.x1 + (i < parts.length - 1 ? (m[i]?.e ?? 0) : 0);
  const steps = Math.max(2, Math.ceil(b - a));
  let d = "";
  for (let k = 0; k <= steps; k++) {
    const x = a + ((b - a) * k) / steps;
    d += `${k ? " L" : "M"}${f(x)} ${f(drawn(parts, h, w, i, Math.min(p.x1, Math.max(p.x0, x)), t, m))}`;
  }
  return d;
}

/** Stretch `i`'s beads on the wave: `every` points apart, centred; the ones at its ends reach too. */
export function beads(parts: readonly Part[], h: readonly number[], w: number, i: number, t: number, m: readonly Meet[], every: number, r: number): [number, number][] {
  const p = parts[i];
  if (!p) return [];
  const n = Math.max(1, Math.floor((p.x1 - p.x0 - 2 * r) / every) + 1);
  const x0 = (p.x0 + p.x1 - (n - 1) * every) / 2;
  return Array.from({ length: n }, (_, k): [number, number] => {
    let x = x0 + k * every;
    const y = drawn(parts, h, w, i, x, t, m);
    if (k === 0 && i > 0) x -= m[i - 1]?.e ?? 0;
    if (k === n - 1 && i < parts.length - 1) x += m[i]?.e ?? 0;
    return [x, y];
  });
}

/**
 * A count's places as it goes from `from` to `to`: two at least, so 9 going to 10 has room. Each
 * place's digit then and now (blank for none), and the way a changed one rolls: up as the count
 * rises, down as it falls. `start`: the digits begin the slot (a phone, its room trailing); `end`:
 * they end it, against the state's name (a desktop), each place under its own.
 */
export function cells(from: number, to: number, align: "start" | "end"): { was: string; now: string; dir: "up" | "down" }[] {
  const places = Math.max(2, String(from).length, String(to).length);
  const pad = (v: number) => (align === "end" ? String(v).padStart(places, " ") : String(v).padEnd(places, " "));
  const a = pad(from);
  const b = pad(to);
  const dir = to >= from ? "up" : "down";
  return [...b].map((now, i) => ({ was: (a[i] ?? " ").trim(), now: now.trim(), dir }));
}

/** Needs you brightens and dims, so it's seen (its line's opacity at `t`). */
export const calling = (t: number) => 0.8 + 0.2 * (0.5 - 0.5 * Math.cos((2 * Math.PI * t) / 1.6));

const NS = "http://www.w3.org/2000/svg";
type Built = { sig: string; items: (SVGPathElement | SVGCircleElement[])[]; grads: (SVGLinearGradientElement | null)[] };
const built = new WeakMap<SVGSVGElement, Built>();

/** The rule drawn into `svg` at `t` (seconds). Its elements are made again only when what's drawn changes (a stretch, or a bead more or fewer); otherwise only their positions move. `id` keeps its gradients apart from another rule's. */
export function draw(svg: SVGSVGElement, parts: readonly Part[], w: number, t: number, id: string): void {
  const h = reaches(parts);
  const m = meets(parts, h, w, t);
  const dots = parts.map((p, i) => {
    const mk = LOOK[p.stand].mark;
    return mk.kind === "beads" ? beads(parts, h, w, i, t, m, mk.every, mk.r) : null;
  });
  const sig = parts.map((p, i) => `${p.stand}:${dots[i]?.length ?? "-"}`).join(",");
  let b = built.get(svg);
  if (!b || b.sig !== sig) {
    b = build(svg, parts, dots, id, sig);
    built.set(svg, b);
  }
  svg.setAttribute("viewBox", `0 0 ${f(w)} ${HEIGHT}`);
  parts.forEach((p, i) => {
    const it = b.items[i];
    if (!it) return;
    if (Array.isArray(it)) {
      const at = dots[i] ?? [];
      it.forEach((c, k) => {
        const xy = at[k];
        if (!xy) return;
        c.setAttribute("cx", f(xy[0]));
        c.setAttribute("cy", f(xy[1]));
      });
    } else {
      it.setAttribute("d", linePath(parts, h, w, i, t, m));
      if (p.stand === "needs") it.setAttribute("opacity", f(calling(t)));
    }
    const g = b.grads[i];
    if (g) {
      g.setAttribute("x1", f(p.x0));
      g.setAttribute("x2", f(p.x1));
    }
  });
}

function build(svg: SVGSVGElement, parts: readonly Part[], dots: readonly ([number, number][] | null)[], id: string, sig: string): Built {
  svg.replaceChildren();
  const defs = document.createElementNS(NS, "defs");
  svg.append(defs);
  const grads: Built["grads"] = [];
  const items: Built["items"] = parts.map((p, i) => {
    const look = LOOK[p.stand];
    const d = dots[i];
    grads[i] = null;
    if (d) {
      return d.map(() => {
        const c = document.createElementNS(NS, "circle");
        c.setAttribute("r", String(look.mark.kind === "beads" ? look.mark.r : 1));
        c.setAttribute("fill", look.colour);
        svg.append(c);
        return c;
      });
    }
    const path = document.createElementNS(NS, "path");
    let stroke = look.colour;
    if (p.stand === "working") {
      const g = document.createElementNS(NS, "linearGradient");
      g.id = `${id}-p${i}`;
      g.setAttribute("gradientUnits", "userSpaceOnUse");
      PRISM.forEach((colour, k) => {
        const s = document.createElementNS(NS, "stop");
        s.setAttribute("offset", String(k / (PRISM.length - 1)));
        s.setAttribute("stop-color", colour);
        g.append(s);
      });
      defs.append(g);
      grads[i] = g;
      stroke = `url(#${g.id})`;
    }
    path.setAttribute("stroke", stroke);
    path.setAttribute("stroke-width", "2");
    path.setAttribute("fill", "none");
    path.setAttribute("stroke-linejoin", "round");
    if (look.mark.kind === "dashes") {
      path.setAttribute("stroke-dasharray", "3.6 2.6");
      path.setAttribute("stroke-linecap", "butt");
    } else path.setAttribute("stroke-linecap", "round");
    svg.append(path);
    return path;
  });
  return { sig, items, grads };
}
