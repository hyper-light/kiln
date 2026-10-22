/**
 * The replication figure's scene, from the board as it stands: each runner's store on the left as
 * a small database, the server's in the middle, the mirror's copy of it on the right (below it on a
 * phone); a cable from each store into the server's, and one from the server's to the mirror. A
 * light runs on a cable while batches move on it (at about their pace), and a backlog of rows to
 * come waits at its start; a cable whose store is away is faint. Pure: what to draw, not how.
 */
import type { ConvoStatus, ConvoStore } from "@/api/types";
import { agoText, countText, moving, plateName, sizeText, storesOf } from "./scene";

export type P = [number, number];
/** Where a database stands and how big it is drawn. */
export interface Plate {
  x: number;
  y: number;
  w: number;
  h: number;
}
/** The drawn cylinder: its axis, its ellipse's radii, its top and its height. */
export interface Cylinder {
  x: number;
  rx: number;
  ry: number;
  top: number;
  depth: number;
}
export type Tone = "ok" | "warn" | "off";

export interface Cable {
  d: string;
  /** Its direction, a small chevron along it. */
  arrow: string;
  /** Seconds between lights on it (about one per batch); null: nothing moves on it. */
  period: number | null;
  /** Rows to come, marks waiting at its start. */
  marks: string[];
  tone?: Tone;
}

export interface Node {
  key: string;
  plate: Plate;
  /** The small one of a runner's store, or the server's or the mirror's. */
  big: boolean;
  name: { text: string; at: P; anchor: "start" | "middle" };
  sub: { text: string; at: P; anchor: "start" | "middle"; tone?: Tone };
  heading?: { text: string; at: P; anchor: "start" | "middle" };
  hit: { x: number; y: number; w: number; h: number };
  title: string;
}

export interface ReplicationScene {
  width: number;
  height: number;
  headings: { text: string; at: P; anchor: "start" | "middle" }[];
  sources: (Node & { cable: Cable })[];
  server: Node;
  mirror: (Node & { cable: Cable }) | null;
}

/** About one light per batch: a store sending a batch a second gets one a second; never so many they blur, never so few they read as stopped. */
export function periodOf(batchesPerSecond: number | null | undefined): number | null {
  if (!batchesPerSecond || batchesPerSecond <= 0) return null;
  return Math.min(6, Math.max(1.2, 1 / batchesPerSecond));
}

/** A store's cable's pace while batches move on it: about a light a batch, the slowest while its rate isn't known yet; none when nothing moves. */
export function paceOf(s: ConvoStore, now: number): number | null {
  return moving(s, now) ? (periodOf(s.rates.batches) ?? 6) : null;
}

/** Rows to come as marks at a cable's start: one for a few, one more for each ten times as many, at most six. */
export function marksOf(behind: number): number {
  return behind > 0 ? Math.min(6, 1 + Math.floor(Math.log10(behind))) : 0;
}

const f = (n: number) => n.toFixed(2);
const at = (p: P) => `${f(p[0])} ${f(p[1])}`;

export function cylinder(p: Plate): Cylinder {
  return { x: p.x, rx: p.w * 0.68, ry: p.h * 0.25, top: p.y - p.h * 0.67 * 0.46, depth: p.h * 0.67 };
}

/** Where a database's heading sits: over its middle, just clear of the top of its cap. */
export function above(p: Plate, gap = 16): P {
  const d = cylinder(p);
  return [p.x, d.top - d.ry - gap];
}

/** Where a cable meets a database: its left or right side, `level` of the way down. */
export function port(p: Plate, side: "left" | "right", level = 0.5): P {
  const d = cylinder(p);
  return [d.x + (side === "left" ? -d.rx : d.rx), d.top + d.depth * level];
}

/** The front half of the database's ellipse `level` of the way down (`progress` of it, from the left). */
export function frontArc(d: Cylinder, level = 0, progress = 1, inset = 1): string {
  const pts: P[] = [];
  for (let i = 0; i <= 28; i++) {
    const a = Math.PI * (1 - (progress * i) / 28);
    pts.push([d.x + Math.cos(a) * d.rx * inset, d.top + d.depth * level + Math.sin(a) * d.ry * inset]);
  }
  return "M" + pts.map(at).join(" L");
}

/** Its wall: from the back of its top ellipse round the front, down, and back along its bottom. */
export function wall(d: Cylinder): string {
  const l = d.x - d.rx;
  const r = d.x + d.rx;
  const b = d.top + d.depth;
  return `M${f(l)} ${f(d.top)} A${f(d.rx)} ${f(d.ry)} 0 0 0 ${f(r)} ${f(d.top)} L${f(r)} ${f(b)} A${f(d.rx)} ${f(d.ry)} 0 0 1 ${f(l)} ${f(b)} Z`;
}

/** A point of a cubic and its direction there (`t`: 0 to 1 along it, by its parameter). */
function cubic(a: P, c1: P, c2: P, b: P, t: number): { p: P; d: P } {
  const u = 1 - t;
  const p: P = [0, 1].map((i) => u * u * u * a[i]! + 3 * u * u * t * c1[i]! + 3 * u * t * t * c2[i]! + t * t * t * b[i]!) as P;
  const d: P = [0, 1].map((i) => 3 * u * u * (c1[i]! - a[i]!) + 6 * u * t * (c2[i]! - c1[i]!) + 3 * t * t * (b[i]! - c2[i]!)) as P;
  return { p, d };
}

function chevron({ p, d }: { p: P; d: P }, size = 3): string {
  const len = Math.hypot(d[0], d[1]) || 1;
  const u: P = [d[0] / len, d[1] / len];
  const v: P = [-u[1], u[0]];
  return `M${at([p[0] - u[0] * size + v[0] * size, p[1] - u[1] * size + v[1] * size])} L${at(p)} L${at([p[0] - u[0] * size - v[0] * size, p[1] - u[1] * size - v[1] * size])}`;
}

/** A cable from `a` to `b`, bowed out by `bow` at each end. */
function curve(a: P, b: P, bowOut: number, bowIn: number, rows: number, period: number | null, tone?: Tone): Cable {
  const c1: P = [a[0] + bowOut, a[1]];
  const c2: P = [b[0] - bowIn, b[1]];
  const marks = Array.from({ length: marksOf(rows) }, (_, i) => {
    const { p } = cubic(a, c1, c2, b, 0.05 + i * 0.035);
    return `M${at([p[0] - 1, p[1] - 4])} L${at([p[0] + 1, p[1] + 3])}`;
  });
  return { d: `M${at(a)} C${at(c1)} ${at(c2)} ${at(b)}`, arrow: chevron(cubic(a, c1, c2, b, 0.67)), period, marks, tone };
}

const rate = (v: number) => (v < 10 ? v.toFixed(1) : Math.round(v).toLocaleString("en-US"));

/** A runner's words as a phone has room for, beside its database (its tone says the rest). */
const short = (text: string) => text.replace(" · catching up", "").replace(" · stalled", "").replace(" writes/s", "/s").replace("Feeding · older kiln", "Older kiln");

/** What a runner's store is doing, in a few words, and its tone. */
function sourceSub(s: ConvoStore, now: number): { text: string; tone?: Tone } {
  const behind = s.feed?.behind ?? 0;
  if (!s.connected) return { text: s.heard ? `Offline · ${agoText(now - s.heard)}` : "Offline", tone: "off" };
  if (s.state === "behind") return { text: `${countText(behind)} behind · stalled`, tone: "warn" };
  if (behind > 0 && s.state === "catching_up") return { text: `${countText(behind)} behind · catching up`, tone: "warn" };
  if (s.state === "feeding") return { text: "Feeding · older kiln" };
  const writes = s.rates.writes ?? 0;
  return { text: writes > 0.05 ? `${rate(writes)} writes/s` : "Idle · in sync" };
}

function mirrorSub(m: ConvoStore, now: number): { text: string; tone?: Tone } {
  const size = m.report?.file ? `${sizeText(m.report.file.bytes)} · ` : "";
  if (!m.connected) return { text: `${size}offline${m.heard ? ` ${agoText(now - m.heard)}` : ""}`, tone: "off" };
  const behind = m.mirror?.behind ?? 0;
  if (behind > 0) return { text: `${size}${countText(behind)} behind`, tone: "warn" };
  return { text: `${size}current`, tone: "ok" };
}

/** The scene: `portrait` for a phone (the mirror below the server, the names shortened). */
export function replicationScene(status: ConvoStatus, portrait: boolean): ReplicationScene {
  const now = status.at;
  const { server, runners, mirrors } = storesOf(status);
  const stores = runners.filter((s) => s.id);
  const mirror = mirrors[0] ?? null;
  const n = Math.max(1, stores.length);
  const step = 55;
  const top = portrait ? 60 : 64;
  const ys = stores.map((_, i) => top + i * step);
  const last = top + (n - 1) * step;
  const mid = (top + last) / 2;
  const level = (i: number) => (stores.length < 2 ? 0.5 : 0.1 + (i * 0.78) / (stores.length - 1));
  const serverName = "conversations.db";
  const serverSize = server?.report?.file ? sizeText(server.report.file.bytes) : "—";
  const serverWrites = server?.rates.writes;

  if (!portrait) {
    const width = 960;
    const height = Math.max(300, last + 71);
    // Level with the stores' middle, low enough for their headings over them however few stores there are.
    const sp: Plate = { x: 520, y: Math.max(mid - 9, 80), w: 110, h: 66 };
    const mp: Plate = { x: 794, y: sp.y, w: 88, h: 55 };
    const sources = stores.map((s, i) => {
      const plate: Plate = { x: 250, y: ys[i]!, w: 36, h: 25 };
      const sub = sourceSub(s, now);
      const behind = s.connected ? (s.feed?.behind ?? 0) : 0;
      const tone: Tone | undefined = !s.connected ? "off" : sub.tone === "warn" ? "warn" : undefined;
      return {
        key: s.key,
        plate,
        big: false,
        name: { text: s.machine, at: [30, ys[i]! - 2] as P, anchor: "start" as const },
        sub: { text: sub.text, at: [30, ys[i]! + 16] as P, anchor: "start" as const, tone: sub.tone },
        hit: { x: 22, y: ys[i]! - 25, w: 262, h: 54 },
        title: `${s.machine}: its store, ${sub.text.toLowerCase()}`,
        cable: curve(port(plate, "right"), port(sp, "left", level(i)), 76, 66, behind, paceOf(s, now), tone),
      };
    });
    // The server's and the mirror's headings just over their databases, on one line.
    const heads = above(sp)[1];
    return {
      width,
      height,
      headings: [
        { text: "RUNNER STORES", at: [30, 22], anchor: "start" },
        { text: "SERVER", at: [sp.x, heads], anchor: "middle" },
        { text: "MIRROR", at: [mp.x, heads], anchor: "middle" },
        { text: "MIRROR FEED", at: [(sp.x + sp.w * 0.68 + mp.x - mp.w * 0.68) / 2, sp.y - 32], anchor: "middle" },
      ],
      sources,
      server: {
        key: server?.key ?? "server",
        plate: sp,
        big: true,
        name: { text: serverName, at: [sp.x, sp.y + 91], anchor: "middle" },
        sub: { text: `${serverSize}${serverWrites != null ? ` · ${rate(serverWrites)} writes/s` : ""}`, at: [sp.x, sp.y + 113], anchor: "middle" },
        hit: { x: sp.x - 97, y: sp.y - 71, w: 194, h: 201 },
        title: `The server's store, ${serverName}, ${serverSize}`,
      },
      mirror: mirror
        ? (() => {
            const sub = mirrorSub(mirror, now);
            return {
              key: mirror.key,
              plate: mp,
              big: true,
              name: { text: mirror.machine, at: [mp.x, mp.y + 91] as P, anchor: "middle" as const },
              sub: { text: sub.text, at: [mp.x, mp.y + 113] as P, anchor: "middle" as const, tone: sub.tone },
              hit: { x: mp.x - 93, y: mp.y - 71, w: 186, h: 201 },
              title: `The mirror's copy on ${mirror.machine}: ${sub.text}`,
              cable: curve(
                port(sp, "right"),
                port(mp, "left"),
                64,
                64,
                mirror.connected ? (mirror.mirror?.behind ?? 0) : 0,
                paceOf(mirror, now),
                mirror.connected ? (sub.tone === "warn" ? "warn" : undefined) : "off",
              ),
            };
          })()
        : null,
    };
  }

  // A phone: the stores down the left, the server to their right, the mirror below it all, each
  // heading just over its database: the server low enough for its own, the mirror's under its words.
  const width = 360;
  const sp: Plate = { x: 267, y: Math.max(mid - 22, 62), w: 66, h: 50 };
  const mp: Plate = { x: 267, y: Math.max(last + 67, sp.y + 141), w: 54, h: 42 };
  const height = mp.y + 62;
  const sources = stores.map((s, i) => {
    const plate: Plate = { x: 150, y: ys[i]!, w: 24, h: 18 };
    const sub = sourceSub(s, now);
    const behind = s.connected ? (s.feed?.behind ?? 0) : 0;
    const tone: Tone | undefined = !s.connected ? "off" : sub.tone === "warn" ? "warn" : undefined;
    return {
      key: s.key,
      plate,
      big: false,
      name: { text: plateName(s.machine, 14), at: [20, ys[i]! - 2] as P, anchor: "start" as const },
      sub: { text: short(sub.text), at: [20, ys[i]! + 16] as P, anchor: "start" as const, tone: sub.tone },
      hit: { x: 13, y: ys[i]! - 25, w: 160, h: 54 },
      title: `${s.machine}: its store, ${sub.text.toLowerCase()}`,
      cable: curve(port(plate, "right"), port(sp, "left", level(i)), 28, 32, behind, paceOf(s, now), tone),
    };
  });
  let mirrorNode: ReplicationScene["mirror"] = null;
  if (mirror) {
    const a = port(sp, "right");
    const b = port(mp, "right");
    const x = 334;
    const sub = mirrorSub(mirror, now);
    const tone: Tone | undefined = mirror.connected ? (sub.tone === "warn" ? "warn" : undefined) : "off";
    const midY = (a[1] + 39 + b[1] - 49) / 2;
    mirrorNode = {
      key: mirror.key,
      plate: mp,
      big: true,
      heading: { text: "MIRROR", at: above(mp, 14), anchor: "middle" },
      name: { text: plateName(mirror.machine, 24), at: [20, mp.y + 8], anchor: "start" },
      sub: { text: sub.text, at: [20, mp.y + 29], anchor: "start", tone: sub.tone },
      hit: { x: 13, y: mp.y - 30, w: 300, h: 77 },
      title: `The mirror's copy on ${mirror.machine}: ${sub.text}`,
      cable: {
        d: `M${at(a)} C${f(x - 4)} ${f(a[1])} ${f(x)} ${f(a[1] + 14)} ${f(x)} ${f(a[1] + 39)} L${f(x)} ${f(b[1] - 49)} C${f(x)} ${f(b[1] - 16)} ${f(x - 19)} ${f(b[1])} ${at(b)}`,
        arrow: chevron({ p: [x, midY], d: [0, 1] }),
        period: paceOf(mirror, now),
        marks: [],
        tone,
      },
    };
  }
  return {
    width,
    height,
    headings: [
      { text: "RUNNER STORES", at: [20, 22], anchor: "start" },
      { text: "SERVER", at: above(sp, 14), anchor: "middle" },
    ],
    sources,
    server: {
      key: server?.key ?? "server",
      plate: sp,
      big: true,
      name: { text: serverName, at: [sp.x, sp.y + 67], anchor: "middle" },
      sub: { text: `${serverSize}${serverWrites != null ? ` · ${rate(serverWrites)}/s` : ""}`, at: [sp.x, sp.y + 85], anchor: "middle" },
      hit: { x: sp.x - 63, y: sp.y - 44, w: 126, h: 139 },
      title: `The server's store, ${serverName}, ${serverSize}`,
    },
    mirror: mirrorNode,
  };
}

/** What the figure's head says of replication as a whole, and its tone. */
export function healthOf(status: ConvoStatus): { text: string; tone: Tone } {
  const { runners, mirrors } = storesOf(status);
  const stores = runners.filter((s) => s.id);
  const off = stores.find((s) => !s.connected);
  const stalled = stores.find((s) => s.connected && s.state === "behind");
  const catching = stores.find((s) => s.connected && s.state === "catching_up" && (s.feed?.behind ?? 0) > 0);
  const mirror = mirrors[0];
  if (stalled) return { text: `${stalled.machine} is stalled`, tone: "warn" };
  if (catching) return { text: `${catching.machine} is catching up`, tone: "warn" };
  if (mirror && (!mirror.connected || (mirror.mirror?.behind ?? 0) > 0)) return { text: mirror.connected ? "The mirror is catching up" : "The mirror is offline", tone: "warn" };
  if (!mirror && status.mirrors_wanted > 0) return { text: "No mirror yet", tone: "warn" };
  if (off) return { text: `${off.machine} is offline`, tone: "off" };
  return { text: "All stores current", tone: "ok" };
}

/** The figure's three numbers: stores in sync of those connected, rows still to come, the mirror's copy. */
export function metricsOf(status: ConvoStatus): { synced: number; of: number; pending: number; mirror: { text: string; tone: Tone } } {
  const { runners, mirrors } = storesOf(status);
  const stores = runners.filter((s) => s.id);
  const synced = stores.filter((s) => s.connected && (s.state === "in_sync" || (s.state === "catching_up" && (s.feed?.behind ?? 0) === 0))).length;
  const pending = stores.reduce((a, s) => a + (s.connected ? (s.feed?.behind ?? 0) : 0), 0);
  const m = mirrors[0];
  const behind = m?.mirror?.behind ?? 0;
  const mirror: { text: string; tone: Tone } = !m
    ? { text: "None", tone: "off" }
    : !m.connected
      ? { text: "Offline", tone: "off" }
      : behind > 0
        ? { text: `${countText(behind)} behind`, tone: "warn" }
        : { text: "Current", tone: "ok" };
  return { synced, of: stores.length, pending, mirror };
}
