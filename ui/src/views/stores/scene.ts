/**
 * KilnDB's machines, drawn the way hyperlight's articles draw them (components/slates' failover
 * cabinets): each machine a lit cabinet seen a little from the side, its stores the trays of its
 * drive bay, its name on its header, its numbers on its readout; cables between their ports. Pure:
 * the live status in, the paths and labels out. Nothing moves for show: a light runs on a cable only
 * while batches move on it, and a tray's light is lit only while its store writes.
 */
import type { ConvoState, ConvoStatus, ConvoStore } from "@/api/types";

export type Tone = "neutral" | "pass" | "fail" | "pending" | "error";
export type Material = "paper" | "circuit" | "metal" | "silicon" | "shadow" | "emissive";

export interface ScenePath {
  id: string;
  d: string;
  kind: "glass" | "edge" | "fine" | "light" | "rear";
  opacity: number;
  tone?: Tone;
  /** A solid surface: its fill, lit as hyperlight's materials are. */
  material?: Material;
  /** A fixed dash: a machine that keeps no store, a cable that is down. */
  dash?: string;
  /** Which part of the figure it belongs to: its tabs bring a part forward. */
  part: Part;
}

export interface SceneLabel {
  id: string;
  text: string;
  x: number;
  y: number;
  kind: "heading" | "name" | "label" | "small" | "status" | "plate" | "tray" | "readout";
  tone?: Tone;
  anchor?: "start" | "middle" | "end";
  part: Part;
}

/** A store's place in the scene: a click there opens its dashboard. */
export interface SceneHit {
  key: string;
  title: string;
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

/** A cable moved data, or a store wrote, this recently (ms): its light runs. */
export const ACTIVE_MS = 6_000;

const coord = (p: P) => `${p[0].toFixed(2)} ${p[1].toFixed(2)}`;
const pathOf = (pts: readonly P[], close = false) => pts.map((p, i) => `${i ? "L" : "M"}${coord(p)}`).join(" ") + (close ? " Z" : "");

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

/** What the scene draws: the server's store, each runner's (and the machines that keep none), the mirrors. */
export function storesOf(status: ConvoStatus) {
  const server = status.stores.find((s) => s.role === "server") ?? null;
  const rank = (s: ConvoStore) => (s.id ? (s.connected ? 0 : 1) : 2);
  const runners = status.stores.filter((s) => s.role === "runner").sort((a, b) => rank(a) - rank(b) || a.machine.localeCompare(b.machine) || a.key.localeCompare(b.key));
  const mirrors = status.stores.filter((s) => s.role === "mirror");
  return { server, runners, mirrors };
}

/**
 * A machine's name as its header can hold it: its host without its domain (an address stays whole),
 * cut to fit, keeping a short last part (the -01 of build-farm-01) that tells machines of a kind apart.
 */
export function plateName(machine: string, room: number): string {
  const host = /^[^.]*[a-z][^.]*\./i.test(machine) ? machine.slice(0, machine.indexOf(".")) : machine;
  if (host.length <= room) return host;
  const last = /-[^-]{1,4}$/.exec(host)?.[0] ?? "";
  const head = room - 1 - last.length;
  return head >= 4 ? `${host.slice(0, head)}…${last}` : `${host.slice(0, room - 1)}…`;
}

/** A tray of a cabinet's drive bay: a store the machine keeps. */
interface Tray {
  key: string;
  label: string;
  used: number | null;
  writing: boolean;
  tone: Tone;
  /** Its numbers are known (its machine reports them). */
  known: boolean;
}

/** A cabinet to draw: a machine and the stores it keeps. */
interface Machine {
  key: string;
  /** What its header says. */
  name: string;
  /** Its whole name. */
  title: string;
  tone: Tone;
  connected: boolean;
  /** Keeps no store at all: its bay is empty. */
  empty: boolean;
  trays: Tray[];
  readout: [string, string];
  status: string;
  part: Part;
  /** Slots in its drive bay. */
  slots: number;
}

/** Where a cabinet stands, and its ports. */
interface Placed {
  m: Machine;
  x: number;
  top: number;
  w: number;
  h: number;
  /** Its ports: on its top, its bottom, and each side at its drive bay. */
  topPort: P;
  bottomPort: P;
  left: P;
  right: P;
}

class Drawing {
  paths: ScenePath[] = [];
  labels: SceneLabel[] = [];
  hits: SceneHit[] = [];

  line(id: string, pts: readonly P[], o: { kind?: ScenePath["kind"]; opacity?: number; tone?: Tone; material?: Material; dash?: string; part: Part }, close = false) {
    this.paths.push({ id, d: pathOf(pts, close), kind: o.kind ?? "fine", opacity: o.opacity ?? 0.6, tone: o.tone, material: o.material, dash: o.dash, part: o.part });
  }

  face(id: string, pts: readonly P[], material: Material, opacity: number, part: Part, tone?: Tone, dash?: string) {
    this.line(id, pts, { kind: "glass", material, opacity, tone, part, dash }, true);
  }

  frame(id: string, x: number, y: number, w: number, h: number, o: { material?: Material; opacity?: number; tone?: Tone; kind?: ScenePath["kind"]; dash?: string; part: Part }) {
    const pts: P[] = [
      [x, y],
      [x + w, y],
      [x + w, y + h],
      [x, y + h],
    ];
    this.line(id, pts, { kind: o.kind ?? (o.material ? "glass" : "fine"), opacity: o.opacity ?? 0.8, tone: o.tone, material: o.material, dash: o.dash, part: o.part }, true);
  }

  circle(id: string, cx: number, cy: number, r: number, o: { material?: Material; opacity?: number; tone?: Tone; kind?: ScenePath["kind"]; part: Part }) {
    const pts: P[] = Array.from({ length: 24 }, (_, i) => [cx + Math.cos((i * Math.PI) / 12) * r, cy + Math.sin((i * Math.PI) / 12) * r]);
    this.line(id, pts, { kind: o.kind ?? (o.material ? "glass" : "fine"), opacity: o.opacity ?? 0.7, tone: o.tone, material: o.material, part: o.part }, true);
  }

  label(id: string, text: string, x: number, y: number, kind: SceneLabel["kind"], part: Part, tone: Tone = "neutral", anchor: SceneLabel["anchor"] = "middle") {
    this.labels.push({ id, text, x, y, kind, tone, anchor, part });
  }

  /**
   * A cabinet: a level front of rack-mounted units between two rails (its name on its header, two
   * fans, a drive bay whose trays are the stores it keeps, a readout), its side and top in depth,
   * vents, a plinth on four feet, a shadow under it.
   */
  cabinet(id: string, at: Placed, dim: number) {
    const { m, x, top, w, h } = at;
    const part = m.part;
    const left = x - w / 2;
    const right = x + w / 2;
    const bottom = top + h;
    const depth = w * 0.42;
    // The side and the top recede up and to the right.
    const q = (xx: number, yy: number, z: number): P => [xx + z * 0.66, yy - z * 0.5];
    const dash = m.empty ? "2 3" : undefined;
    const o = (v: number) => v * dim;
    // The body: shadow, side, top, their vents and inset; feet and plinth.
    this.face(`${id}-shadow`, [[left - 4, bottom + 7], [right + 3, bottom + 7], q(right + 3, bottom + 7, depth + 5), q(left - 4, bottom + 7, depth + 5)], "shadow", o(0.75), part);
    this.face(`${id}-side`, [q(right, top, 0), q(right, bottom, 0), q(right, bottom, depth), q(right, top, depth)], "metal", o(0.9), part, undefined, dash);
    this.face(`${id}-top`, [q(left, top, 0), q(right, top, 0), q(right, top, depth), q(left, top, depth)], "metal", o(0.96), part, undefined, dash);
    this.face(`${id}-side-inset`, [q(right, top + 10, 6), q(right, bottom - 12, 6), q(right, bottom - 12, depth - 5), q(right, top + 10, depth - 5)], "silicon", o(0.7), part);
    const vents = Math.max(8, Math.floor((h - 30) / 7));
    for (let v = 0; v < vents; v++) {
      const y = top + 16 + v * ((h - 34) / vents);
      this.line(`${id}-side-vent-${v}`, [q(right, y, 9), q(right, y, depth - 8)], { opacity: o(0.46), part });
    }
    for (let f = 0; f < 6; f++) {
      const xx = left + 12 + (f * (w - 24)) / 5;
      this.line(`${id}-top-vent-${f}`, [q(xx, top, 6), q(xx, top, depth - 5)], { opacity: o(0.5), part });
    }
    this.line(`${id}-rear-upright`, [q(right, top + 3, depth - 2), q(right, bottom - 3, depth - 2)], { kind: "edge", opacity: o(0.8), part });
    for (const side of [-1, 1]) {
      const fx = x + side * (w / 2 - 10) - 4;
      for (const rear of [false, true]) {
        const z = rear ? depth - 11 : 2;
        const k = `${id}-foot-${side}-${rear ? "rear" : "front"}`;
        this.face(`${k}-side`, [q(fx + 8, bottom + 1, z), q(fx + 8, bottom + 7, z), q(fx + 8, bottom + 7, z + 7), q(fx + 8, bottom + 1, z + 7)], "silicon", o(0.92), part);
        this.face(`${k}-face`, [q(fx, bottom + 1, z), q(fx + 8, bottom + 1, z), q(fx + 8, bottom + 7, z), q(fx, bottom + 7, z)], "metal", o(0.9), part);
      }
    }
    // The front: the face, its recess, its header with the machine's name.
    this.frame(`${id}-front`, left, top, w, h, { material: "metal", opacity: o(0.96), part, dash });
    this.frame(`${id}-recess`, left + 5, top + 21, w - 10, h - 27, { material: "silicon", opacity: o(0.95), part });
    this.frame(`${id}-header`, left + 5, top + 2, w - 10, 17, { material: "metal", opacity: o(0.9), part });
    this.label(`${id}-name`, m.name.toUpperCase(), x, top + 14, "plate", part);
    // Rails, each with its holes and two screws.
    for (const edge of [-1, 1]) {
      const ex = x + edge * (w / 2 - 8);
      this.line(
        `${id}-ear-${edge}`,
        [
          [ex - 2, top + 23],
          [ex + 2, top + 23],
          [ex + 2, bottom - 8],
          [ex - 2, bottom - 8],
        ],
        { kind: "glass", material: "metal", opacity: o(0.86), part },
        true,
      );
      const holes = Math.max(5, Math.floor((h - 40) / 12));
      for (let s = 0; s < holes; s++) this.frame(`${id}-rail-hole-${edge}-${s}`, ex - 0.8, top + 27 + s * 12, 1.6, 2.5, { material: "silicon", opacity: o(0.6), part });
      for (let s = 0; s < 2; s++) {
        const sy = top + 6 + s * (h - 12);
        this.circle(`${id}-screw-${edge}-${s}`, ex + edge * 5, sy, 1.2, { opacity: o(0.6), part });
        this.line(
          `${id}-screw-slot-${edge}-${s}`,
          [
            [ex + edge * 5 - 0.7, sy],
            [ex + edge * 5 + 0.7, sy],
          ],
          { opacity: o(0.62), part },
        );
      }
    }
    // The fan bay: two fans in their wells, grilles beneath them.
    const fanY = top + 25;
    const fanH = Math.max(26, Math.min(40, h * 0.2));
    this.frame(`${id}-fan-bay-shadow`, left + 10, fanY - 1, w - 20, fanH + 3, { material: "silicon", opacity: o(0.95), part });
    this.frame(`${id}-fan-bay`, left + 12, fanY, w - 24, fanH, { material: "metal", opacity: o(0.88), part });
    for (let f = 0; f < 2; f++) {
      const fx = x + (f ? 1 : -1) * w * 0.19;
      const fy = fanY + fanH * 0.46;
      const r = Math.min(fanH * 0.32, w * 0.12);
      this.circle(`${id}-fan-well-${f}`, fx, fy, r + 1.3, { material: "silicon", opacity: o(0.88), part });
      this.circle(`${id}-fan-${f}`, fx, fy, r, { opacity: o(m.connected ? 0.62 : 0.35), part });
      this.circle(`${id}-fan-hub-${f}`, fx, fy, r * 0.2, { opacity: o(0.55), part });
      for (let b = 0; b < 5; b++) {
        const a = (b * Math.PI * 2) / 5 + f * 0.4;
        this.line(
          `${id}-blade-${f}-${b}`,
          [
            [fx + Math.cos(a) * r * 0.2, fy + Math.sin(a) * r * 0.2],
            [fx + Math.cos(a + 0.22) * r * 0.82, fy + Math.sin(a + 0.22) * r * 0.82],
            [fx + Math.cos(a + 0.74) * r * 0.7, fy + Math.sin(a + 0.74) * r * 0.7],
            [fx + Math.cos(a + 0.7) * r * 0.3, fy + Math.sin(a + 0.7) * r * 0.3],
          ],
          { kind: "glass", material: "metal", opacity: o(m.connected ? 0.6 : 0.3), part },
          true,
        );
      }
      for (let g = 0; g < 3; g++)
        this.line(
          `${id}-fan-grille-${f}-${g}`,
          [
            [fx - r * 0.7, fanY + fanH - 6 + g * 1.6],
            [fx + r * 0.7, fanY + fanH - 6 + g * 1.6],
          ],
          { opacity: o(0.3), part },
        );
    }
    // The drive bay: a cage of slots, a tray in each for a store the machine keeps (its fill and its
    // light), blank covers in the rest.
    const bayY = fanY + fanH + 6;
    const readoutY = bottom - 30;
    const bayH = Math.max(24, readoutY - bayY - 6);
    this.frame(`${id}-bay-shadow`, left + 10, bayY - 1, w - 20, bayH + 3, { material: "silicon", opacity: o(0.95), part });
    this.frame(`${id}-bay`, left + 12, bayY, w - 24, bayH, { material: "metal", opacity: o(0.88), part });
    const slots = Math.max(m.slots, m.trays.length, 1);
    const gap = 3;
    const trayH = (bayH - 6 - (slots - 1) * gap) / slots;
    const tx = left + 16;
    const tw = w - 32;
    for (let i = 0; i < slots; i++) {
      const t = m.trays[i];
      const ty = bayY + 3 + i * (trayH + gap);
      if (!t) {
        // A blank cover: a plate with a pull slot and vents.
        this.frame(`${id}-blank-${i}`, tx, ty, tw, trayH, { material: "metal", opacity: o(0.55), part, dash: m.empty && i === 0 ? "2 3" : undefined });
        for (let v = 0; v < 5; v++) {
          const vx = tx + tw * 0.3 + v * (tw * 0.09);
          this.line(
            `${id}-blank-${i}-vent-${v}`,
            [
              [vx, ty + trayH * 0.32],
              [vx, ty + trayH * 0.68],
            ],
            { opacity: o(0.35), part },
          );
        }
        if (m.empty && i === 0) this.label(`${id}-blank-label`, "NO STORE", x, ty + trayH * 0.5 + 3.5, "small", part, "neutral");
        continue;
      }
      const tpart: Part = t.key.startsWith("copy") ? "mirror" : part;
      this.frame(`${id}-tray-${i}`, tx, ty, tw, trayH, { material: "silicon", opacity: o(0.92), part: tpart });
      this.line(
        `${id}-tray-${i}-latch`,
        [
          [tx + 2, ty + 2.5],
          [tx + tw - 2, ty + 2.5],
          [tx + tw - 2, ty + trayH - 2.5],
        ],
        { opacity: o(0.55), part: tpart },
      );
      // Its handles: short U-shapes at each end.
      for (const dir of [-1, 1]) {
        const hx = dir < 0 ? tx + 3 : tx + tw - 3;
        this.line(
          `${id}-tray-${i}-handle-${dir}`,
          [
            [hx, ty + trayH * 0.28],
            [hx - dir * 2, ty + trayH * 0.28],
            [hx - dir * 2, ty + trayH * 0.72],
            [hx, ty + trayH * 0.72],
          ],
          { kind: "edge", opacity: o(0.72), part: tpart },
        );
      }
      // Its name; a level meter of its fill (the part of its file in use) beside it, a block for
      // each part; its light (lit while it writes) at the end.
      const mid = ty + trayH / 2;
      this.label(`${id}-tray-${i}-label`, t.label, tx + 9, mid + 2.6, "tray", tpart, "neutral", "start");
      const lx = tx + tw - 12;
      const gx = Math.max(tx + tw * 0.5, tx + 9 + t.label.length * 5.4 + 7);
      const blocks = Math.max(3, Math.floor((lx - 4 - gx + 1) / 3.4));
      const lit = t.used === null ? 0 : Math.max(t.used > 0 ? 1 : 0, Math.round(t.used * blocks));
      for (let b = 0; b < blocks; b++) {
        const on = b < lit;
        this.frame(`${id}-tray-${i}-level-${b}`, gx + b * 3.4, mid - 2.2, 2.4, 4.4, {
          material: on ? "emissive" : "silicon",
          opacity: on ? o(0.88) : o(0.5),
          tone: on ? t.tone : "neutral",
          part: tpart,
        });
      }
      this.frame(`${id}-tray-${i}-led`, lx, mid - 1.7, 5, 3.4, { material: t.writing ? "emissive" : "silicon", opacity: t.writing ? 0.95 : o(0.6), tone: t.writing ? t.tone : "neutral", part: tpart });
      if (t.writing)
        this.line(
          `${id}-tray-${i}-head`,
          [
            [gx, mid + 3.6],
            [gx + blocks * 3.4 - 1, mid + 3.6],
          ],
          { kind: "light", opacity: 0.95, tone: t.tone, part: tpart },
        );
    }
    // The readout: what the machine's store says of itself.
    this.frame(`${id}-display`, left + 10, readoutY, w - 20, 18, { material: "silicon", opacity: o(0.97), part });
    this.label(`${id}-readout-0`, m.readout[0], left + 15, readoutY + 12.3, "readout", part, "neutral", "start");
    this.label(`${id}-readout-1`, m.readout[1], right - 15, readoutY + 12.3, "readout", part, m.tone, "end");
    // The service light: the machine's state, along its foot.
    this.line(
      `${id}-service-light`,
      [
        [left + 13, bottom - 6],
        [right - 13, bottom - 6],
      ],
      { opacity: m.connected ? 0.85 : 0.2, tone: m.tone, part },
    );
    this.frame(`${id}-plinth`, left + 2, bottom - 3, w - 4, 6, { material: "metal", opacity: o(0.98), part });
    // Ports where its cables meet it.
    for (const [k, p] of [
      ["top", at.topPort],
      ["bottom", at.bottomPort],
    ] as const)
      this.frame(`${id}-socket-${k}`, p[0] - 2.4, p[1] - 1.7, 4.8, 3.4, { material: "silicon", opacity: o(0.85), part });
    this.hits.push({ key: m.key, title: m.title, x: left - 6, y: top - 4, w: w + depth * 0.66 + 12, h: h + 44 });
    // Its state beneath it, a line or two.
    const [first, ...rest] = m.status.split(" · ");
    this.label(`${id}-status`, first ?? "", x + depth * 0.33, bottom + 24, "small", part, m.tone);
    if (rest.length) this.label(`${id}-status-2`, rest.join(" · "), x + depth * 0.33, bottom + 38, "small", part, "neutral");
  }
}

function machinesOf(status: ConvoStatus) {
  const now = status.at;
  const { server, runners, mirrors } = storesOf(status);
  const byMachine = new Map<string, ConvoStore[]>();
  for (const r of runners) byMachine.set(r.machine, [...(byMachine.get(r.machine) ?? []), r]);
  const machines: Machine[] = [...byMachine.entries()].map(([machine, stores]) => {
    const kept = stores.filter((s) => s.id);
    const copy = mirrors.find((m) => m.machine === machine);
    const main = kept[0] ?? stores[0]!;
    const trays: Tray[] = kept.map((s, i) => ({
      key: `own-${i}`,
      label: kept.length > 1 ? `STORE ${i + 1}` : "STORE",
      used: used(s),
      writing: writing(s, now),
      tone: toneOf(s.state),
      known: s.report !== null,
    }));
    if (copy) trays.push({ key: "copy", label: "COPY", used: used(copy), writing: writing(copy, now), tone: toneOf(copy.state), known: copy.report !== null });
    const f = main.report?.file;
    const readout: [string, string] = f
      ? [sizeText(f.bytes), main.rates.writes != null ? `${main.rates.writes < 10 ? main.rates.writes.toFixed(1) : Math.round(main.rates.writes)}/s` : ""]
      : [main.version ? `v${main.version.replace(/^v/, "")}` : "—", main.state === "feeding" ? "older" : ""];
    return {
      key: main.key,
      name: machine,
      title: machine,
      tone: toneOf(main.state),
      connected: main.connected,
      empty: kept.length === 0,
      trays,
      readout,
      status: stateText(main, now),
      part: "runners" as Part,
      slots: 3,
    };
  });
  return { server, machines, mirrors };
}

/** The scene for `status`, landscape (800 wide) or portrait (420 wide). */
export function storesScene(status: ConvoStatus, portrait: boolean): Scene {
  const g = new Drawing();
  const now = status.at;
  const { server, machines, mirrors } = machinesOf(status);
  const width = portrait ? 420 : 800;
  const n = Math.max(1, machines.length);

  // The server: a broader cabinet above the runners, its heading beside it (above it on a phone).
  const sw = portrait ? 150 : 168;
  const sh = portrait ? 180 : 188;
  const sx = portrait ? width / 2 - 22 : width / 2 + 40;
  const stop = portrait ? 124 : 70;
  const sf = server?.report?.file;
  const writes = server?.rates.writes;
  const serverMachine: Machine = {
    key: server?.key ?? "server",
    name: "KILN SERVER",
    title: "the server's store",
    tone: server ? toneOf(server.state) : "neutral",
    connected: true,
    empty: !server,
    trays: server ? [{ key: "own", label: "CONVERSATIONS.DB", used: used(server), writing: writing(server, now), tone: toneOf(server.state), known: true }] : [],
    readout: sf ? [sizeText(sf.bytes), writes != null ? `${writes < 10 ? writes.toFixed(1) : Math.round(writes)}/s` : ""] : ["off", ""],
    status: "",
    part: "server",
    slots: 4,
  };
  const serverAt: Placed = {
    m: serverMachine,
    x: sx,
    top: stop,
    w: sw,
    h: sh,
    topPort: [sx, stop - 1],
    bottomPort: [sx, stop + sh + 1],
    left: [sx - sw / 2, stop + sh * 0.6],
    right: [sx + sw / 2, stop + sh * 0.6],
  };
  const serverLines = ["every machine's store, taken in", sf ? `#${countText(sf.top)} writes · ${sf.bytes ? Math.round((sf.free / sf.bytes) * 100) : 0}% free` : "keeps no store"];
  if (portrait) {
    g.label("server-heading", "SERVER", width / 2, 40, "heading", "server");
    serverLines.forEach((t, i) => g.label(`server-line-${i}`, t, width / 2, 62 + i * 18, "small", "server"));
  } else {
    const lx = sx - sw / 2 - 34;
    g.label("server-heading", "SERVER", lx, stop + 40, "heading", "server", "neutral", "end");
    serverLines.forEach((t, i) => g.label(`server-line-${i}`, t, lx, stop + 62 + i * 18, "small", "server", "neutral", "end"));
  }

  // The runners: rows of cabinets beneath it, up to six to a row (two on a phone), each row centred.
  const cols = portrait ? 2 : Math.min(n, 6);
  const pitch = portrait ? 196 : (width - 40) / cols;
  const rw = portrait ? 132 : Math.min(124, pitch - 22);
  const rh = portrait ? 168 : 170;
  const room = Math.max(6, Math.floor((rw - 20) / 7));
  const cx = width / 2 - (portrait ? 22 : 24);
  const busY = stop + sh + 34;
  const laneY = busY + 18;
  const rowTop0 = laneY + 58;
  const rowGap = rh + 96;
  const placed: Placed[] = machines.map((machine, i) => {
    const m = { ...machine, name: plateName(machine.title, room) };
    const row = Math.floor(i / cols);
    const inRow = Math.min(cols, n - row * cols);
    const x = cx + ((i % cols) - (inRow - 1) / 2) * pitch;
    const top = rowTop0 + row * rowGap;
    return { m, x, top, w: rw, h: rh, topPort: [x, top - 1], bottomPort: [x, top + rh + 1], left: [x - rw / 2, top + rh * 0.55], right: [x + rw / 2, top + rh * 0.55] };
  });
  // On a phone the runners get a heading of their own, between the cables rising from the first row
  // (beside a lone runner's); on a wider page their lettered headers and the summary say it.
  if (portrait) {
    if (n === 1) g.label("runners-heading", "RUNNERS · A STORE EACH", cx - 14, rowTop0 - 40, "heading", "runners", "neutral", "end");
    else g.label("runners-heading", "RUNNERS · A STORE EACH", cx, rowTop0 - 40, "heading", "runners");
  }

  // The feeds: each runner's cable up to a bus beneath the server, and into it. A row further down
  // runs its cables along above it and up the figure's left edge, clear of the rows above.
  const inPort = serverAt.bottomPort;
  placed.forEach((p, i) => {
    const s = p.m;
    if (s.empty) return;
    const store = status.stores.find((x) => x.key === s.key);
    const live = store ? moving(store, now) : false;
    const lane = inPort[0] + (i - (placed.length - 1) / 2) * 5;
    const row = Math.floor(i / cols);
    const edge = 8 + (row - 1) * 3;
    const route: P[] =
      row === 0 ? [p.topPort, [p.topPort[0], busY], [lane, busY], [lane, inPort[1]]] : [p.topPort, [p.topPort[0], p.top - 36], [edge, p.top - 36], [edge, busY], [lane, busY], [lane, inPort[1]]];
    g.line(`feed-${i}`, route, { opacity: s.connected ? 0.5 : 0.22, tone: s.connected ? s.tone : "neutral", dash: s.connected ? undefined : "2 4", part: "feeds" });
    if (live) g.line(`feed-${i}-flow`, route, { kind: "light", opacity: 0.95, tone: s.tone, part: "feeds" });
  });

  // The mirror's copy: the server's cable out along a lane of its own, down into the tray it fills
  // (down the figure's right edge to a row further down).
  const mirror = mirrors[0];
  if (mirror) {
    const p = placed.find((x) => x.m.trays.some((t) => t.key === "copy"));
    if (p) {
      const tone = toneOf(mirror.state);
      const out = serverAt.right;
      const deep = p.top > rowTop0;
      const edge = portrait || deep ? width - 10 : Math.min(width - 14, out[0] + rw * 0.42 * 0.66 + 40);
      const into: P = [p.x + 16, p.top - 1];
      const route: P[] = deep ? [out, [edge, out[1]], [edge, p.top - 44], [into[0], p.top - 44], into] : [out, [edge, out[1]], [edge, laneY], [into[0], laneY], into];
      g.line("mirror-cable", route, { opacity: mirror.connected ? 0.55 : 0.22, tone: mirror.connected ? tone : "neutral", dash: mirror.connected ? undefined : "2 4", part: "mirror" });
      if (moving(mirror, now)) g.line("mirror-flow", route, { kind: "light", opacity: 0.95, tone, part: "mirror" });
      // Its name beside its cable: left of it at the figure's edge, right of it elsewhere.
      if (portrait || deep) g.label("mirror-tag", "MIRROR COPY", edge - 6, out[1] - 8, "heading", "mirror", "neutral", "end");
      else g.label("mirror-tag", "MIRROR COPY", edge + 8, (out[1] + laneY) / 2, "heading", "mirror", "neutral", "start");
    }
  } else if (status.mirrors_wanted > 0) {
    g.label("mirror-none", "no mirror chosen yet", serverAt.right[0] + 40, serverAt.right[1], "small", "mirror", "neutral", "start");
  }

  g.cabinet("server", serverAt, server ? 1 : 0.5);
  placed.forEach((p, i) => g.cabinet(`runner-${i}`, p, p.m.connected ? 1 : 0.45));

  // What it comes to, in a line.
  const withStore = machines.filter((m) => !m.empty);
  const runnerStores = status.stores.filter((s) => s.role === "runner" && s.id);
  const synced = runnerStores.filter((s) => s.state === "in_sync").length;
  const behind = runnerStores.reduce((a, s) => a + (s.connected ? (s.feed?.behind ?? 0) : 0), 0);
  const parts = [`${withStore.length} of ${machines.length} machines keep a store`, `${synced} in sync`];
  if (behind > 0) parts.push(`${countText(behind)} rows to come`);
  if (mirror) parts.push(mirror.state === "in_sync" ? "mirror in sync" : `mirror ${stateText(mirror, now)}`);
  const summaryTone: Tone = runnerStores.some((s) => s.state === "behind") ? "error" : behind > 0 ? "pending" : "pass";
  const rows = Math.ceil(n / cols);
  const height = Math.ceil(rowTop0 + (rows - 1) * rowGap + rh + (portrait ? 112 : 92));
  if (portrait) {
    // Two lines on a phone: it would run off the edge in one.
    g.label("summary", parts.slice(0, 2).join(" · "), width / 2, height - 40, "status", "all", summaryTone);
    g.label("summary-2", parts.slice(2).join(" · "), width / 2, height - 20, "status", "all", summaryTone);
  } else g.label("summary", parts.join(" · "), width / 2, height - 22, "status", "all", summaryTone);
  return { width, height, paths: g.paths, labels: g.labels, hits: g.hits };
}
