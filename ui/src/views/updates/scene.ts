/**
 * The Rollout figure's scene (System → Updates): the builds kiln hands out on the left as chip
 * wafers, the server in the middle as a box with kiln's mark on its top, each runner on the right
 * as a small machine; a cable from each build into the server and one from the server to each
 * runner. On a phone: the server at the top, a trunk down from it and a branch to each runner (the
 * builds are listed under it instead). Light flows along a cable while something moves on it: a
 * runner's upgrade, the release pipeline building a platform; a connected runner's cable carries a
 * faint pulse now and then, an offline one's nothing. Pure: what to draw and where, not how.
 */
import type { CableTone, Tone } from "./words";

export type P = [number, number];

/** The isometric projection: cos 30°, sin 30°. */
const C = 0.866;
const S = 0.5;

const f = (n: number) => n.toFixed(2);
const xy = (p: P) => `${f(p[0])} ${f(p[1])}`;
export const pathOf = (pts: P[]) => "M" + pts.map(xy).join(" L");
export const polyOf = (pts: P[]) => pathOf(pts) + " Z";

/** A point of a box standing on (cx, cy): `x` along its width, `y` its depth, `z` up. */
export function iso(cx: number, cy: number, x: number, y: number, z: number): P {
  return [cx + (x - y) * C, cy + (x + y) * S - z];
}

export interface IsoBox {
  /** Its shadow on the ground, none for one stacked on another. */
  shadow: { cx: number; cy: number; rx: number; ry: number } | null;
  /** Its front-left and front-right faces, and its top. */
  left: string;
  right: string;
  top: string;
  /** The top's two front edges, catching the light. */
  ridge: string;
  /** Lines round its front faces at fractions of its height. */
  etches: string[];
}

/** A box `w` wide, `d` deep and `h` high standing on (cx, cy). */
export function isoBox(cx: number, cy: number, w: number, d: number, h: number, etch: number[] = [], shadow = true): IsoBox {
  const W = w / 2;
  const D = d / 2;
  const p = (x: number, y: number, z: number) => iso(cx, cy, x, y, z);
  return {
    shadow: shadow ? { cx: cx + 2, cy: cy + 3, rx: (W + D) * C * 1.02, ry: (W + D) * S * 0.92 } : null,
    left: polyOf([p(-W, D, h), p(W, D, h), p(W, D, 0), p(-W, D, 0)]),
    right: polyOf([p(W, D, h), p(W, -D, h), p(W, -D, 0), p(W, D, 0)]),
    top: polyOf([p(-W, -D, h), p(W, -D, h), p(W, D, h), p(-W, D, h)]),
    ridge: pathOf([p(-W, D, h), p(W, D, h), p(W, -D, h)]),
    etches: etch.map((k) => pathOf([p(-W, D, h * k), p(W, D, h * k), p(W, -D, h * k)])),
  };
}

/** kiln's mark, drawn in a 32 × 32 box: its outer loop, its inner one, its tail. */
export const MARK = {
  loop: "M6.4 22.5C2.4 17.1 7.1 6.2 14 4.5C21.4 2.7 27.4 10.1 25.1 16.5C22.8 22.9 13.1 28.8 8.8 26.6",
  inner: "M9.1 24.2C5.4 20.7 10.7 9.1 17.2 8.1C23.7 7.1 29.7 13.5 27 19.3C24.5 24.7 17.4 27.9 12.8 26.8",
  tail: "M8.8 26.6C6.9 25.6 5.6 23.6 5.2 21.3",
};

export interface Label {
  text: string;
  x: number;
  y: number;
  anchor: "start" | "middle";
  /** Set after it, smaller: a build's arch and ABI after its OS. */
  tail?: string;
}

export interface Hit {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Cable {
  d: string;
  /** A small chevron along it, the way things move. */
  arrow: string | null;
  tone: CableTone;
  /** Something moves on it now. */
  lit: boolean;
}

export interface Led {
  x: number;
  y: number;
  r: number;
}

export interface BuildNode {
  key: string;
  title: string;
  chip: IsoBox;
  /** The die on the chip's top, drawn as it is built. */
  die: string;
  building: boolean;
  tone: Tone;
  name: Label;
  sub: Label;
  hit: Hit;
  cable: Cable;
}

export interface RunnerNode {
  key: string;
  title: string;
  box: IsoBox;
  /** The new runner, stacked on the old one while it switches. */
  next: IsoBox;
  led: Led;
  tone: Tone;
  switching: boolean;
  offline: boolean;
  /** Connected with nothing moving on its cable: a faint pulse runs it now and then, the link live. */
  pulse: boolean;
  name: Label;
  sub: Label;
  hit: Hit;
  cable: Cable;
}

export interface ServerNode {
  key: "server";
  title: string;
  box: IsoBox;
  /** Where kiln's mark lies on its top: an SVG transform. */
  mark: string;
  led: Led;
  tone: Tone;
  name: Label;
  sub: Label;
  hit: Hit;
}

export interface RolloutScene {
  width: number;
  height: number;
  portrait: boolean;
  halo: { cx: number; cy: number; rx: number; ry: number };
  headings: Label[];
  builds: BuildNode[];
  server: ServerNode;
  runners: RunnerNode[];
}

export interface SceneBuild {
  key: string;
  /** Its OS, named beside its chip, and its arch and ABI after it: its target in words. */
  os: string;
  arch: string;
  line: string;
  tone: Tone;
  cable: CableTone;
  lit: boolean;
  building: boolean;
}

export interface SceneRunner {
  key: string;
  name: string;
  line: string;
  tone: Tone;
  cable: CableTone;
  lit: boolean;
  switching: boolean;
  offline: boolean;
}

export interface SceneServer {
  line: string;
  tone: Tone;
}

/** Words cut to what fits beside a node. */
export function clip(text: string, n: number) {
  return text.length > n ? `${text.slice(0, n - 1).trimEnd()}…` : text;
}

/** Spread `n` ports over `total`: the i-th one's offset (one in the middle). */
export function spread(i: number, n: number, total: number) {
  return n <= 1 ? total / 2 : (i * total) / (n - 1);
}

function cubicAt(a: P, c1: P, c2: P, b: P, t: number): { p: P; d: P } {
  const u = 1 - t;
  const p: P = [0, 1].map((i) => u * u * u * a[i]! + 3 * u * u * t * c1[i]! + 3 * u * t * t * c2[i]! + t * t * t * b[i]!) as P;
  const d: P = [0, 1].map((i) => 3 * u * u * (c1[i]! - a[i]!) + 6 * u * t * (c2[i]! - c1[i]!) + 3 * t * t * (b[i]! - c2[i]!)) as P;
  return { p, d };
}

/** The point `frac` of the way along a cubic, by its length, and its direction there. */
export function alongCubic(a: P, c1: P, c2: P, b: P, frac: number): { p: P; d: P } {
  const steps = 64;
  const lengths = [0];
  let prev = a;
  for (let i = 1; i <= steps; i++) {
    const { p } = cubicAt(a, c1, c2, b, i / steps);
    lengths.push(lengths[i - 1]! + Math.hypot(p[0] - prev[0], p[1] - prev[1]));
    prev = p;
  }
  const want = lengths[steps]! * Math.min(1, Math.max(0, frac));
  let i = 1;
  while (i < steps && lengths[i]! < want) i++;
  const span = lengths[i]! - lengths[i - 1]! || 1;
  return cubicAt(a, c1, c2, b, (i - 1 + (want - lengths[i - 1]!) / span) / steps);
}

function chevron({ p, d }: { p: P; d: P }, size = 3): string {
  const len = Math.hypot(d[0], d[1]) || 1;
  const u: P = [d[0] / len, d[1] / len];
  const v: P = [-u[1], u[0]];
  return pathOf([[p[0] - u[0] * size + v[0] * size, p[1] - u[1] * size + v[1] * size], p, [p[0] - u[0] * size - v[0] * size, p[1] - u[1] * size - v[1] * size]]);
}

/** A cable from `a` to `b`, leaving and arriving level, its chevron `at` of the way along. */
function curve(a: P, b: P, out: number, into: number, at: number, tone: CableTone, lit: boolean): Cable {
  const c1: P = [a[0] + out, a[1]];
  const c2: P = [b[0] - into, b[1]];
  return { d: `M${xy(a)} C${xy(c1)} ${xy(c2)} ${xy(b)}`, arrow: chevron(alongCubic(a, c1, c2, b, at)), tone, lit };
}

function serverNode(cx: number, cy: number, w: number, d: number, h: number, server: SceneServer, name: Label, sub: Label, hit: Hit): ServerNode {
  const s = ((w * 0.5) / 32) * 1.15;
  const led = iso(cx, cy, w / 2 - 8, d / 2, h * 0.5);
  return {
    key: "server",
    title: `kiln server: ${server.line}`,
    box: isoBox(cx, cy, w, d, h, [0.34, 0.67]),
    mark: `matrix(${f(s * C)} ${f(s * S)} ${f(-s * C)} ${f(s * S)} ${f(cx)} ${f(cy - 32 * s * S - h)})`,
    led: { x: led[0], y: led[1], r: 1.7 },
    tone: server.tone,
    name,
    sub,
    hit,
  };
}

function runnerNode(r: SceneRunner, cx: number, cy: number, name: Label, sub: Label, hit: Hit, cable: Cable): RunnerNode {
  const led = iso(cx, cy, 9, 8, 4.5);
  return {
    key: r.key,
    title: `${r.name}: ${r.line}`,
    box: isoBox(cx, cy, 24, 16, 9, [0.5]),
    next: isoBox(cx, cy - 13, 24, 16, 4, [], false),
    led: { x: led[0], y: led[1], r: 1.4 },
    tone: r.tone,
    switching: r.switching,
    offline: r.offline,
    pulse: !r.offline && !r.lit,
    name,
    sub,
    hit,
    cable,
  };
}

/** The scene, `portrait` for a phone. */
export function rolloutScene(input: { builds: SceneBuild[]; server: SceneServer; runners: SceneRunner[] }, portrait: boolean): RolloutScene {
  const { runners, server } = input;
  const m = runners.length;
  if (portrait) {
    const width = 360;
    const [sx, sy, trunk, gx, lx] = [78, 70, 58, 104, 134];
    const ys = runners.map((_, i) => 178 + i * 46);
    const height = Math.max(220, (ys[m - 1] ?? 132) + 42);
    return {
      width,
      height,
      portrait,
      halo: { cx: sx + 40, cy: sy, rx: 150, ry: 110 },
      // Over the server's box, just clear of the far corner of its top (49.5 above where it stands).
      headings: [{ text: "SERVER", x: sx, y: sy - 58.5, anchor: "middle" as const }, ...(m ? [{ text: "RUNNERS", x: lx, y: 146, anchor: "start" as const }] : [])],
      builds: [],
      server: serverNode(
        sx,
        sy,
        72,
        46,
        20,
        server,
        { text: "kiln server", x: 158, y: sy - 4, anchor: "start" },
        { text: clip(server.line, 30), x: 158, y: sy + 15, anchor: "start" },
        { x: 10, y: 28, w: 330, h: 88 },
      ),
      runners: runners.map((r, i) => {
        const y = ys[i]!;
        const at = y - 3;
        // One trunk down from under the server, a branch to each runner.
        const cable: Cable = { d: `M${trunk} 96 L${trunk} ${at - 14} Q${trunk} ${at} ${trunk + 14} ${at} L${gx - 18} ${at}`, arrow: null, tone: r.cable, lit: r.lit };
        return runnerNode(
          r,
          gx,
          y,
          { text: clip(r.name, 26), x: lx, y: y - 3, anchor: "start" },
          { text: clip(r.line, 32), x: lx, y: y + 13, anchor: "start" },
          { x: 10, y: y - 22, w: 330, h: 44 },
          cable,
        );
      }),
    };
  }
  const width = 960;
  const sx = 480;
  const n = input.builds.length;
  const rstep = m > 8 ? 44 : 52;
  const bstep = n > 7 ? 44 : 54;
  // The server stands level with the middle of both columns, each column's first row below its heading.
  const sy = Math.max(146, 42 + ((m - 1) * rstep) / 2, 65 + ((n - 1) * bstep) / 2);
  const rys = runners.map((_, i) => sy + 20 + (i - (m - 1) / 2) * rstep);
  const bys = input.builds.map((_, i) => sy + 11 + (i - (n - 1) / 2) * bstep);
  const height = Math.max(sy + 172, (rys[m - 1] ?? 0) + 48, (bys[n - 1] ?? 0) + 54);
  // Where cables meet the server: its left corner (builds in) and its right one (runners out).
  const reach = (60 + 39) * C;
  return {
    width,
    height,
    portrait,
    halo: { cx: sx, cy: sy - 6, rx: 210, ry: 150 },
    headings: [
      ...(n ? [{ text: "BUILDS", x: 30, y: 24, anchor: "start" as const }] : []),
      // Just above the server's box, whose top's far corner stands 79.5 above where it stands.
      { text: "SERVER", x: sx, y: sy - 93, anchor: "middle" as const },
      ...(m ? [{ text: "RUNNERS", x: 712, y: 24, anchor: "start" as const }] : []),
    ],
    builds: input.builds.map((b, i) => {
      const y = bys[i]!;
      const chip = isoBox(236, y, 22, 22, 4);
      const q = (x: number, z: number) => iso(236, y, x, z, 4);
      return {
        key: b.key,
        title: `${b.os} ${b.arch} build: ${b.line}`,
        chip,
        die: polyOf([q(-6, -6), q(6, -6), q(6, 6), q(-6, 6)]),
        building: b.building,
        tone: b.tone,
        name: { text: clip(b.os, 10), tail: clip(b.arch, 16), x: 30, y: y - 3, anchor: "start" as const },
        sub: { text: clip(b.line, 30), x: 30, y: y + 15, anchor: "start" as const },
        hit: { x: 20, y: y - 26, w: 262, h: 52 },
        cable: curve([255, y - 2], [sx - reach + 2, sy - 14 - spread(i, n, 22.5)], 86, 70, 0.62, b.cable, b.lit),
      };
    }),
    server: serverNode(
      sx,
      sy,
      120,
      78,
      30,
      server,
      { text: "kiln server", x: sx, y: sy + 80, anchor: "middle" },
      { text: clip(server.line, 40), x: sx, y: sy + 101, anchor: "middle" },
      { x: sx - 96, y: sy - 90, w: 192, h: 212 },
    ),
    runners: runners.map((r, i) => {
      const y = rys[i]!;
      const cable = curve([sx + reach - 2, sy + 4 - spread(i, m, 22)], [700, y - 4], 70, 78, 0.6, r.cable, r.lit);
      return runnerNode(
        r,
        716,
        y,
        { text: clip(r.name, 24), x: 746, y: y - 3, anchor: "start" },
        { text: clip(r.line, 30), x: 746, y: y + 14, anchor: "start" },
        { x: 686, y: y - 26, w: 262, h: 50 },
        cable,
      );
    }),
  };
}
