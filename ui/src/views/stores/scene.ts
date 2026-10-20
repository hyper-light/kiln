/**
 * The conversation stores, drawn the way hyperlight's articles draw machines: wireframe chassis in
 * a fixed projection, cables between their ports, lettering in mono. Pure: the live status in, the
 * paths and labels out. Nothing moves for show: a light runs on a cable only while batches move on
 * it, and on a store's file only while it writes.
 */
import type { ConvoState, ConvoStatus, ConvoStore } from "@/api/types";

export type Tone = "neutral" | "pass" | "fail" | "pending" | "error";

export interface ScenePath {
  id: string;
  d: string;
  kind: "glass" | "edge" | "fine" | "light" | "rear";
  opacity: number;
  tone?: Tone;
  /** A fixed dash: a machine that keeps no store is drawn so. */
  dash?: string;
  /** Which part of the figure it belongs to: its tabs bring a part forward. */
  part: Part;
}

export interface SceneLabel {
  id: string;
  text: string;
  x: number;
  y: number;
  kind: "heading" | "label" | "small" | "status";
  tone?: Tone;
  anchor?: "start" | "middle" | "end";
  part: Part;
}

/** A store's place in the scene: a click there opens its dashboard. */
export interface SceneHit {
  key: string;
  x: number;
  y: number;
  w: number;
  h: number;
}

export type Part = "runners" | "feeds" | "server" | "mirror" | "upkeep" | "all";

export interface Scene {
  width: number;
  height: number;
  paths: ScenePath[];
  labels: SceneLabel[];
  hits: SceneHit[];
}

type P = readonly [number, number];
type V = readonly [number, number, number];

const YAW = 0.5;
const PITCH = 0.46;
/** A cable moved data, or a store wrote, this recently (ms): its light runs. */
export const ACTIVE_MS = 6_000;

const coord = (p: P) => `${p[0].toFixed(1)} ${p[1].toFixed(1)}`;
const pathOf = (pts: readonly P[], close = false) => pts.map((p, i) => `${i ? "L" : "M"}${coord(p)}`).join(" ") + (close ? " Z" : "");

function projector(cx: number, cy: number, scale: number) {
  const [cy_, sy, cp, sp] = [Math.cos(YAW), Math.sin(YAW), Math.cos(PITCH), Math.sin(PITCH)];
  return ([x, y, z]: V): P => {
    const depth = -x * sy + z * cy_;
    return [cx + (x * cy_ + z * sy) * scale, cy + (y * cp + depth * sp) * scale];
  };
}

/** The tone of a store's state. */
export function toneOf(state: ConvoState): Tone {
  switch (state) {
    case "live":
    case "in_sync":
      return "pass";
    case "catching_up":
    case "feeding":
      return "pending";
    case "behind":
      return "error";
    default:
      return "neutral";
  }
}

export function countText(n: number): string {
  if (n >= 1e9) return `${(n / 1e9).toFixed(n >= 1e10 ? 0 : 1)}B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(n >= 1e7 ? 0 : 1)}M`;
  if (n >= 1e4) return `${(n / 1e3).toFixed(0)}k`;
  return n.toLocaleString("en-US");
}

export function sizeText(bytes: number): string {
  if (bytes >= 1 << 30) return `${(bytes / (1 << 30)).toFixed(bytes >= 10 * (1 << 30) ? 1 : 2)} GB`;
  if (bytes >= 1 << 20) return `${(bytes / (1 << 20)).toFixed(0)} MB`;
  if (bytes >= 1 << 10) return `${(bytes / (1 << 10)).toFixed(0)} KB`;
  return `${bytes} B`;
}

export function agoText(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.round(s / 60)}m`;
  if (s < 86_400) return `${Math.round(s / 3600)}h`;
  return `${Math.round(s / 86_400)}d`;
}

/** What a store's state says, in a few words. */
export function stateText(s: ConvoStore, now: number): string {
  const behind = s.feed?.behind ?? s.mirror?.behind ?? null;
  switch (s.state) {
    case "live":
      return "live";
    case "in_sync":
      return "in sync";
    case "catching_up":
      return `${countText(behind ?? 0)} behind · catching up`;
    case "behind":
      return `${countText(behind ?? 0)} behind`;
    case "feeding":
      return "feeding · older kiln";
    case "no_store":
      return "keeps no store";
    case "offline":
      return s.heard ? `offline · ${agoText(now - s.heard)}` : "offline";
  }
}

/** The part of a store's file in use (0–1; null: not known). */
function used(s: ConvoStore): number | null {
  const f = s.report?.file;
  return f && f.bytes > 0 ? Math.min(1, Math.max(0, (f.bytes - f.free) / f.bytes)) : null;
}

/** Whether a store wrote lately: by its own clock, how long before its last look at its file it last wrote, that look being recent. */
export function writing(s: ConvoStore, now: number): boolean {
  const f = s.report?.file;
  if (!f || !f.wrote_at) return false;
  const current = s.role === "server" ? now - f.at < 10_000 : s.heard !== null && now - s.heard < 15_000;
  return current && f.at - f.wrote_at < ACTIVE_MS;
}

/** Whether batches moved on a store's cable lately (by the server's clock). */
export function moving(s: ConvoStore, now: number): boolean {
  const at = s.feed?.intake?.last_batch ?? s.mirror?.ship.last_sent ?? 0;
  return at > 0 && now - at < ACTIVE_MS && s.connected;
}

function clip(text: string, max: number) {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

interface Box {
  /** Projected bounds. */
  left: number;
  right: number;
  top: number;
  bottom: number;
  project: (v: V) => P;
}

class Drawing {
  paths: ScenePath[] = [];
  labels: SceneLabel[] = [];
  hits: SceneHit[] = [];

  line(id: string, pts: readonly P[], kind: ScenePath["kind"], opacity: number, part: Part, tone: Tone = "neutral", close = false, dash?: string) {
    this.paths.push({ id, d: pathOf(pts, close), kind, opacity, tone, part, dash });
  }

  label(id: string, text: string, x: number, y: number, kind: SceneLabel["kind"], part: Part, tone: Tone = "neutral", anchor: SceneLabel["anchor"] = "middle") {
    this.labels.push({ id, text, x, y, kind, tone, anchor, part });
  }

  /**
   * A chassis of half-extents `[w, h, d]`: its frame, glass faces and rails, vents in its side,
   * and ports to its left and right at `portY`. `dim`: drawn faint (offline); `dash`: drawn so (no store).
   */
  chassis(id: string, project: (v: V) => P, [w, h, d]: V, tone: Tone, part: Part, dim: number, dash?: string, fins = 0): Box {
    const front: V[] = [
      [-w, -h, d],
      [w, -h, d],
      [w, h, d],
      [-w, h, d],
    ];
    const rear: V[] = front.map(([x, y]) => [x, y, -d]);
    const L = (sfx: string, pts: V[], kind: ScenePath["kind"], o: number, close = false) => this.line(`${id}-${sfx}`, pts.map(project), kind, o * dim, part, tone, close, dash);
    L("rear", rear, "rear", 0.5, true);
    L(
      "floor",
      [
        [-w, h, -d],
        [w, h, -d],
        [w, h, d],
        [-w, h, d],
      ],
      "glass",
      0.26,
      true,
    );
    L(
      "side",
      [
        [w, -h, -d],
        [w, h, -d],
        [w, h, d],
        [w, -h, d],
      ],
      "glass",
      0.3,
      true,
    );
    L(
      "top",
      [
        [-w, -h, -d],
        [w, -h, -d],
        [w, -h, d],
        [-w, -h, d],
      ],
      "glass",
      0.16,
      true,
    );
    for (let e = 0; e < 4; e++) L(`rail-${e}`, [front[e]!, rear[e]!], "edge", 0.66);
    L("front", front, "glass", 0.36, true);
    L("front-edge", front, "edge", 0.72, true);
    const vents = Math.max(2, Math.floor((h * 2 - 8) / 7));
    for (let v = 0; v < vents; v++) {
      const y = -h + 6 + (v * (h * 2 - 12)) / Math.max(1, vents - 1);
      L(
        `vent-${v}`,
        [
          [w + 0.5, y, -d + 8],
          [w + 0.5, y, d - 10],
        ],
        "fine",
        0.44,
      );
    }
    // A heat sink on its top, its fins standing up in a row.
    for (let i = 0; i < fins; i++) {
      const x = -w * 0.34 + (i * w * 0.68) / Math.max(1, fins - 1);
      L(
        `fin-${i}`,
        [
          [x, -h, -d * 0.55],
          [x, -h - 13, -d * 0.55],
          [x, -h - 13, d * 0.12],
          [x, -h, d * 0.12],
        ],
        i % 3 ? "fine" : "edge",
        0.5,
      );
    }
    const pts = [...front, ...rear, ...(fins ? ([[0, -h - 13, -d * 0.55]] as V[]) : [])].map(project);
    return { left: Math.min(...pts.map((p) => p[0])), right: Math.max(...pts.map((p) => p[0])), top: Math.min(...pts.map((p) => p[1])), bottom: Math.max(...pts.map((p) => p[1])), project };
  }

  /**
   * A store's file on a chassis' front: `n` slots, as many filled as the part of it in use, the
   * last of them lit while it writes; and, while it is made compact, a bar of how far the copy is.
   */
  file(id: string, b: Box, s: ConvoStore, at: { x0: number; x1: number; y: number; rows: number; cols: number; z: number }, part: Part, dim: number, now: number) {
    const tone = toneOf(s.state);
    const u = used(s);
    const n = at.rows * at.cols;
    const filled = u === null ? 0 : Math.max(u > 0 ? 1 : 0, Math.round(u * n));
    const gap = 3;
    const sw = (at.x1 - at.x0 - gap * (at.cols - 1)) / at.cols;
    const sh = 9;
    const lit = writing(s, now);
    for (let i = 0; i < n; i++) {
      const [r, c] = [Math.floor(i / at.cols), i % at.cols];
      const x = at.x0 + c * (sw + gap);
      const y = at.y + r * (sh + 4);
      const rect: V[] = [
        [x, y, at.z],
        [x + sw, y, at.z],
        [x + sw, y + sh, at.z],
        [x, y + sh, at.z],
      ];
      const on = i < filled;
      this.line(`${id}-slot-${i}`, rect.map(b.project), on ? "glass" : "fine", (on ? 0.62 : 0.3) * dim, part, on ? tone : "neutral", true);
      if (on) {
        for (let bit = 0; bit < 3; bit++) {
          const bx = x + 3 + bit * ((sw - 6) / 2);
          this.line(`${id}-slot-${i}-bit-${bit}`, [b.project([bx, y + 3, at.z + 0.5]), b.project([bx, y + sh - 3, at.z + 0.5])], "fine", 0.5 * dim, part, tone);
        }
      }
      // The head of the file, lit while the store writes.
      if (lit && i === Math.max(0, filled - 1)) this.line(`${id}-head`, [...rect, rect[0]!].map(b.project), "light", 0.95, part, tone);
    }
    const up = s.report?.file.upkeep;
    if (up?.state === "compacting") {
      const y = at.y + at.rows * (sh + 4) + 3;
      const frac = up.of > 0 ? Math.min(1, up.copied / up.of) : 0;
      const bar = (x1: number): V[] => [
        [at.x0, y, at.z],
        [x1, y, at.z],
        [x1, y + 5, at.z],
        [at.x0, y + 5, at.z],
      ];
      this.line(`${id}-compact`, bar(at.x1).map(b.project), "fine", 0.5 * dim, "upkeep", "pending", true);
      this.line(`${id}-compact-done`, bar(at.x0 + (at.x1 - at.x0) * frac).map(b.project), "glass", 0.8 * dim, "upkeep", "pending", true);
    }
  }

  /** A port: a short stub out of a chassis' side, where its cable plugs in. */
  port(id: string, b: Box, side: -1 | 1, y: number, w: number, part: Part, dim: number): P {
    const a = b.project([side * w, y, 0]);
    const out = b.project([side * (w + 8), y, 0]);
    this.line(`${id}-port`, [a, out], "edge", 0.76 * dim, part);
    return out;
  }
}

/** What the scene draws: the server's store, each runner's (and the machines that keep none), the mirrors. */
export function storesOf(status: ConvoStatus) {
  const server = status.stores.find((s) => s.role === "server") ?? null;
  const rank = (s: ConvoStore) => (s.id ? (s.connected ? 0 : 1) : 2);
  const runners = status.stores.filter((s) => s.role === "runner").sort((a, b) => rank(a) - rank(b) || a.machine.localeCompare(b.machine) || a.key.localeCompare(b.key));
  const mirrors = status.stores.filter((s) => s.role === "mirror");
  return { server, runners, mirrors };
}

/** A runner's store's name in the scene: its machine, and its store's id where a machine has more than one. */
function runnerName(s: ConvoStore, all: ConvoStore[]) {
  const many = all.filter((x) => x.machine === s.machine && x.id).length > 1;
  return many && s.id ? `${s.machine} · ${s.id.slice(0, 6)}` : s.machine;
}

/** The scene for `status`, landscape (800 wide) or portrait (420 wide). */
export function storesScene(status: ConvoStatus, portrait: boolean): Scene {
  const g = new Drawing();
  const now = status.at;
  const { server, runners, mirrors } = storesOf(status);
  const width = portrait ? 420 : 800;
  const spacing = portrait ? 66 : 64;
  const n = Math.max(1, runners.length);

  // Runners' stores: a slab each, its name and state beside it.
  const runnerTop = portrait ? 84 : 100;
  const rx = portrait ? 300 : 262;
  if (!portrait) g.label("runners-heading", "RUNNERS", 28, 52, "heading", "runners", "neutral", "start");
  else g.label("runners-heading", "RUNNERS", 20, 40, "heading", "runners", "neutral", "start");
  const ports: { s: ConvoStore; at: P }[] = [];
  runners.forEach((s, i) => {
    const cy = runnerTop + i * spacing;
    const dim = s.connected ? 1 : 0.42;
    const dash = s.id ? undefined : "2 3";
    const project = projector(rx, cy, portrait ? 0.92 : 1);
    const id = `runner-${i}`;
    const box = g.chassis(id, project, [56, 9, 34], toneOf(s.state), "runners", dim, dash);
    // Its file: a gauge along its front, as full as the part of it in use.
    if (s.id) {
      const u = used(s);
      const gauge = (x1: number): V[] => [
        [-38, -3, 34.5],
        [x1, -3, 34.5],
        [x1, 3, 34.5],
        [-38, 3, 34.5],
      ];
      g.line(`${id}-gauge`, gauge(42).map(project), "fine", 0.45 * dim, "runners", "neutral", true);
      if (u !== null) g.line(`${id}-gauge-used`, gauge(-38 + 80 * u).map(project), "glass", 0.75 * dim, "runners", toneOf(s.state), true);
      if (writing(s, now)) g.line(`${id}-gauge-head`, [project([-38, -3.5, 35]), project([42, -3.5, 35])], "light", 0.95, "runners", toneOf(s.state));
    }
    g.line(
      `${id}-led`,
      (
        [
          [-50, -2, 34.5],
          [-45, -2, 34.5],
          [-45, 2, 34.5],
          [-50, 2, 34.5],
        ] as V[]
      ).map(project),
      "edge",
      0.85 * dim,
      "runners",
      toneOf(s.state),
      true,
    );
    const out = g.port(id, box, 1, 0, 56, "runners", dim);
    if (s.id) ports.push({ s, at: out });
    const name = clip(runnerName(s, runners), portrait ? 20 : 22);
    const lx = portrait ? 20 : 28;
    g.label(`${id}-name`, name, lx, cy - 3, "label", "runners", s.connected ? "neutral" : "neutral", "start");
    g.label(`${id}-state`, stateText(s, now), lx, cy + 13, "small", "runners", toneOf(s.state), "start");
    g.hits.push({ key: s.key, x: lx - 6, y: cy - 22, w: box.right - lx + 12, h: 40 });
  });
  const lastRunner = runnerTop + (n - 1) * spacing;

  // The server's store: a chassis, its file on its front.
  const sx = portrait ? 286 : 500;
  const sy = portrait ? lastRunner + 140 : Math.max(200, (runnerTop + lastRunner) / 2 + 10);
  const sProject = projector(sx, sy, portrait ? 0.86 : 1);
  const sTone = server ? toneOf(server.state) : "neutral";
  const sBox = g.chassis("server", sProject, [72, 44, 50], sTone, "server", server ? 1 : 0.42, server ? undefined : "2 3", 12);
  if (server) g.file("server-file", sBox, server, { x0: -62, x1: 62, y: -34, rows: 2, cols: 8, z: 50.5 }, "server", 1, now);
  for (let p = 0; p < 3; p++) {
    const x = -62 + p * 26;
    g.line(
      `server-io-${p}`,
      (
        [
          [x, 24, 50.5],
          [x + 18, 24, 50.5],
          [x + 18, 32, 50.5],
          [x, 32, 50.5],
        ] as V[]
      ).map(sProject),
      "edge",
      0.6,
      "server",
      "neutral",
      true,
    );
  }
  const f = server?.report?.file;
  const sizeLine = f ? `${sizeText(f.bytes)} · ${f.bytes ? Math.round((f.free / f.bytes) * 100) : 0}% free` : "keeps no store";
  const writes = server?.rates.writes;
  if (portrait) {
    g.label("server-heading", "SERVER", 20, sy - 30, "heading", "server", "neutral", "start");
    g.label("server-file-name", "conversations.db", 20, sy - 8, "label", "server", sTone, "start");
    g.label("server-size", sizeLine, 20, sy + 12, "small", "server", "neutral", "start");
    if (writes != null) g.label("server-writes", `${writes < 10 ? writes.toFixed(1) : Math.round(writes)} writes/s`, 20, sy + 30, "small", "server", "neutral", "start");
  } else {
    g.label("server-heading", "SERVER", sx, sBox.top - 30, "heading", "server");
    g.label("server-file-name", "conversations.db", sx, sBox.top - 12, "label", "server", sTone);
    g.label("server-size", sizeLine, sx, sBox.bottom + 20, "small", "server");
    if (writes != null) g.label("server-writes", `${writes < 10 ? writes.toFixed(1) : Math.round(writes)} writes/s`, sx, sBox.bottom + 36, "small", "server");
  }
  if (server) g.hits.push({ key: server.key, x: portrait ? 14 : sBox.left - 8, y: sBox.top - 44, w: portrait ? sBox.right - 6 : sBox.right - sBox.left + 16, h: sBox.bottom - sBox.top + 90 });

  // Feeds: a cable from each runner's store to the server's, lit while batches move on it.
  const inPort = portrait ? g.port("server-in", sBox, 1, 2, 72, "server", 1) : g.port("server-in", sBox, -1, 10, 72, "server", 1);
  ports.forEach(({ s, at }, i) => {
    const tone = toneOf(s.state);
    const route: P[] = portrait
      ? [at, [384 + i * 4, at[1]], [384 + i * 4, inPort[1] - 8 + i * 2], [inPort[0] + 6, inPort[1] - 8 + i * 2], inPort]
      : [at, [352 + i * 4, at[1]], [352 + i * 4, inPort[1]], inPort];
    const dim = s.connected ? 1 : 0.35;
    g.line(`feed-${i}`, route, "fine", 0.4 * dim, "feeds", s.connected ? tone : "neutral", false, s.connected ? undefined : "2 4");
    if (moving(s, now)) g.line(`feed-${i}-flow`, route, "light", 0.95, "feeds", tone);
  });

  // Mirrors: a chassis each, the server's feed to it.
  const outPort = portrait ? g.port("server-out", sBox, 1, 22, 72, "mirror", 1) : g.port("server-out", sBox, 1, 10, 72, "mirror", 1);
  let bottom = portrait ? sy + 80 : Math.max(lastRunner + 40, sBox.bottom + 50);
  if (mirrors.length === 0 && status.mirrors_wanted > 0) {
    const label = "no mirror chosen yet";
    if (portrait) g.label("mirror-none", label, 20, sy + 120, "small", "mirror", "neutral", "start");
    else g.label("mirror-none", label, 690, sy, "small", "mirror");
  }
  mirrors.forEach((m, i) => {
    const mx = portrait ? 290 : 690;
    const my = portrait ? sy + 150 + i * 130 : sy + (i - (mirrors.length - 1) / 2) * 150;
    const project = projector(mx, my, portrait ? 0.82 : 0.92);
    const tone = toneOf(m.state);
    const dim = m.connected ? 1 : 0.42;
    const box = g.chassis(`mirror-${i}`, project, [62, 38, 44], tone, "mirror", dim, undefined, 9);
    g.file(`mirror-${i}-file`, box, m, { x0: -52, x1: 52, y: -28, rows: 2, cols: 7, z: 44.5 }, "mirror", dim, now);
    const inAt = portrait ? g.port(`mirror-${i}-in`, box, 1, 0, 62, "mirror", dim) : g.port(`mirror-${i}-in`, box, -1, 10, 62, "mirror", dim);
    const route: P[] = portrait ? [outPort, [406, outPort[1]], [406, inAt[1]], inAt] : [outPort, [598 + i * 4, outPort[1]], [598 + i * 4, inAt[1]], inAt];
    g.line(`mirror-${i}-cable`, route, "fine", 0.42 * dim, "mirror", m.connected ? tone : "neutral", false, m.connected ? undefined : "2 4");
    if (moving(m, now)) g.line(`mirror-${i}-flow`, route, "light", 0.95, "mirror", tone);
    const copy = m.report?.file;
    const sub = copy ? `${sizeText(copy.bytes)} copy` : "copy";
    if (portrait) {
      g.label(`mirror-${i}-heading`, "MIRROR", 20, my - 30, "heading", "mirror", "neutral", "start");
      g.label(`mirror-${i}-name`, clip(m.machine, 20), 20, my - 8, "label", "mirror", "neutral", "start");
      g.label(`mirror-${i}-state`, stateText(m, now), 20, my + 12, "small", "mirror", tone, "start");
      g.label(`mirror-${i}-copy`, sub, 20, my + 30, "small", "mirror", "neutral", "start");
      bottom = Math.max(bottom, my + 70);
      g.hits.push({ key: m.key, x: 14, y: my - 44, w: box.right - 6, h: 100 });
    } else {
      g.label(`mirror-${i}-heading`, "MIRROR", mx, box.top - 30, "heading", "mirror");
      g.label(`mirror-${i}-name`, clip(m.machine, 22), mx, box.top - 12, "label", "mirror");
      g.label(`mirror-${i}-state`, stateText(m, now), mx, box.bottom + 20, "small", "mirror", tone);
      g.label(`mirror-${i}-copy`, sub, mx, box.bottom + 36, "small", "mirror");
      bottom = Math.max(bottom, box.bottom + 50);
      g.hits.push({ key: m.key, x: box.left - 8, y: box.top - 44, w: box.right - box.left + 16, h: box.bottom - box.top + 90 });
    }
  });

  // What it comes to, in a line.
  const withStore = runners.filter((s) => s.id);
  const synced = withStore.filter((s) => s.state === "in_sync").length;
  const behind = withStore.reduce((a, s) => a + (s.connected ? (s.feed?.behind ?? 0) : 0), 0);
  const mirror = mirrors[0];
  const parts = [`${synced} of ${withStore.length} in sync`];
  if (behind > 0) parts.push(`${countText(behind)} rows to come`);
  if (mirror) parts.push(mirror.state === "in_sync" ? "mirror in sync" : `mirror ${stateText(mirror, now)}`);
  const summaryTone: Tone = withStore.some((s) => s.state === "behind") ? "error" : behind > 0 ? "pending" : "pass";
  const height = Math.ceil(bottom + (portrait ? 40 : 52));
  g.label("summary", parts.join(" · "), width / 2, height - (portrait ? 18 : 26), "status", "all", summaryTone);
  return { width, height, paths: g.paths, labels: g.labels, hits: g.hits };
}
