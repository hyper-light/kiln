/**
 * Runners' engines and what their microVMs hold (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, P2.3,
 * P3.2): GET /api/microvm/engines (each runner's engine, why it can't, its host check, its Hex node's
 * room); GET /api/microvm/runners/:name (its microVMs, each a harness session of the mock's own, its
 * warm pools, the images built there); PUT /api/microvm/runners/:name/engine {engine} (a switch: the
 * other engine's microVMs drain first, then it's done; `end: true` ends them now).
 */
import type http from "node:http";
import { MAC_MINI } from "./data.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  body: <T>(req: http.IncomingMessage) => Promise<T>;
}

type Engine = "host" | "microvm";
interface EngineView {
  engine: Engine;
  /** Why it runs this engine: the default for one that can, its owner's choice, or it can't. */
  origin: "default" | "owner" | "cannot";
  can: boolean;
  hypervisor: string | null;
  why: string | null;
  checked_at: number;
  hex_class: string[];
  room: { memory_free: number; memory_total: number; vcpus_free: number; vcpus_total: number } | null;
  switching: { to: Engine; draining: string[]; since: number } | null;
  /** The workspace's default engine (Settings → Runners), and the projects that ask for another here. */
  default: Engine;
  overrides: { project: string; engine: Engine }[];
}

const G = 2 ** 30;
const now = () => Math.floor(Date.now() / 1000);
const ENGINES: Record<string, EngineView> = {
  [MAC_MINI]: { engine: "microvm", origin: "default", can: true, hypervisor: "Hypervisor.framework", why: null, checked_at: now() - 120, hex_class: ["shards-image"], room: { memory_free: 21 * G, memory_total: 32 * G, vcpus_free: 7, vcpus_total: 10 }, switching: null, default: "microvm", overrides: [] },
  "metal-ryzen-01": { engine: "microvm", origin: "default", can: true, hypervisor: "KVM", why: null, checked_at: now() - 300, hex_class: ["shards-image"], room: { memory_free: 41 * G, memory_total: 64 * G, vcpus_free: 7, vcpus_total: 16 }, switching: null, default: "microvm", overrides: [{ project: "hyper-light/kiln", engine: "host" }] },
  "metal-ryzen-02": { engine: "host", origin: "owner", can: true, hypervisor: "KVM", why: null, checked_at: now() - 240, hex_class: ["shards-image"], room: null, switching: null, default: "microvm", overrides: [] },
  "metal-epyc-01": { engine: "microvm", origin: "default", can: true, hypervisor: "KVM", why: null, checked_at: now() - 60, hex_class: ["shards-image"], room: { memory_free: 388 * G, memory_total: 512 * G, vcpus_free: 52, vcpus_total: 64 }, switching: null, default: "microvm", overrides: [] },
  "metal-pi-04": { engine: "host", origin: "cannot", can: false, hypervisor: null, why: "KVM unavailable: this board's kernel has no /dev/kvm (CONFIG_KVM is off)", checked_at: now() - 900, hex_class: [], room: null, switching: null, default: "microvm", overrides: [] },
  "metal-nuc-old": { engine: "host", origin: "cannot", can: false, hypervisor: null, why: "kiln on this runner is from before microVMs: upgrade it to say", checked_at: now() - 86400, hex_class: [], room: null, switching: null, default: "microvm", overrides: [] },
};

/** A microVM: the harness session it holds (the mock's own, so it opens), what it boots, how it started and what it uses now. */
interface Vm {
  id: string;
  session: string;
  agent: string;
  image: string;
  start: { kind: "warm" | "template" | "cold"; ms: number };
  up_s: number;
  state: "running" | "idle" | "paused" | "failed" | "draining";
  cpu: number[];
  memory: { used: number; total: number };
  disk: { used: number; total: number };
}
const series = (base: number, spread: number, seed: number) => {
  let s = seed;
  let v = base;
  return Array.from({ length: 60 }, () => {
    s = (s * 16807) % 2147483647;
    v = Math.max(0, Math.min(100, v + (s / 2147483647 - 0.5) * spread));
    return Math.round(v);
  });
};
const vm = (id: string, session: string, agent: string, image: string, kind: Vm["start"]["kind"], ms: number, up_s: number, state: Vm["state"], cpu: number, mem: [number, number], disk: [number, number], seed: number): Vm => ({
  id,
  session,
  agent,
  image,
  start: { kind, ms },
  up_s,
  state,
  cpu: series(cpu, cpu / 2 + 4, seed),
  memory: { used: mem[0] * G, total: mem[1] * G },
  disk: { used: disk[0] * G, total: disk[1] * G },
});

const RUNNERS: Record<string, { vms: Vm[]; pools: { agent: string; image: string; cpus: number; memory: number; warm: number; bound: number; hits: number; starts: number; idle_s: number; leaving_s: number | null }[]; images: { name: string; from: string; digest: string; size: number; built_s: number; state: "built" | "building" | "failed"; why?: string }[]; starts: { warm: number; template: number; cold: number }; net: number[] }> = {
  "metal-ryzen-01": {
    vms: [
      vm("vm-r1-1", "fam-tests", "pr-triage", "rust-dev v8", "warm", 205, 180, "running", 44, [2.8, 4], [2.2, 8], 41),
      vm("vm-r1-2", "fan-s2", "reviewer", "rust-dev v8", "template", 431, 1100, "running", 31, [2.3, 4], [1.1, 8], 43),
      vm("vm-r1-3", "sub-busy", "release-notes", "docs-site v3", "warm", 198, 2460, "idle", 4, [1.1, 4], [0.4, 8], 47),
      vm("vm-r1-4", "att-ci", "ci-diagnose", "harness-claude", "cold", 1912, 840, "running", 62, [3.4, 4], [1.4, 8], 53),
    ],
    pools: [
      { agent: "reviewer", image: "rust-dev v8", cpus: 2, memory: 4 * G, warm: 2, bound: 3, hits: 41, starts: 47, idle_s: 40, leaving_s: null },
      { agent: "ci-diagnose", image: "harness-claude", cpus: 2, memory: 2 * G, warm: 1, bound: 1, hits: 9, starts: 12, idle_s: 120, leaving_s: null },
      { agent: "pr-triage", image: "rust-dev v8", cpus: 1, memory: 2 * G, warm: 0, bound: 1, hits: 0, starts: 1, idle_s: 1860, leaving_s: 420 },
    ],
    images: [
      { name: "rust-dev", from: "rust-dev v8", digest: "sha256:7c1e0b9a44d2", size: 1.42 * G, built_s: 240, state: "built" },
      { name: "rust-dev", from: "rust-dev v9", digest: "", size: 0, built_s: 0, state: "building" },
      { name: "harness-claude", from: "kiln's", digest: "sha256:3a90d1c2fe07", size: 0.61 * G, built_s: 172800, state: "built" },
      { name: "docs-site", from: "docs-site v3", digest: "sha256:91fe72b04c3d", size: 0.88 * G, built_s: 86400, state: "built" },
    ],
    starts: { warm: 64, template: 12, cold: 6 },
    net: series(30, 18, 59),
  },
  "metal-epyc-01": {
    vms: [
      vm("vm-e1-1", "fam-migr", "reviewer", "rust-dev v8", "warm", 212, 360, "running", 38, [2.3, 4], [1.1, 8], 61),
      vm("vm-e1-2", "fan-s3", "pr-triage", "rust-dev v8", "template", 455, 1080, "paused", 1, [1.9, 2], [0.6, 8], 67),
      vm("vm-e1-3", "rev-big", "reviewer", "rust-dev v8", "warm", 221, 40, "failed", 2, [2.0, 2], [0.5, 8], 71),
    ],
    pools: [{ agent: "reviewer", image: "rust-dev v8", cpus: 4, memory: 8 * G, warm: 1, bound: 2, hits: 12, starts: 15, idle_s: 300, leaving_s: null }],
    images: [
      { name: "rust-dev", from: "rust-dev v8", digest: "sha256:7c1e0b9a44d2", size: 1.42 * G, built_s: 360, state: "built" },
      { name: "rust-dev", from: "rust-dev v9", digest: "", size: 0, built_s: 0, state: "failed", why: "Shards refused the build at line 13 (HARNESS codex): the download for linux/amd64 has SHA-256 4b07e1a9c3d5…, and --checksum=sha256:9f2c55d0b7a1… names another." },
    ],
    starts: { warm: 22, template: 6, cold: 3 },
    net: series(22, 14, 73),
  },
  [MAC_MINI]: {
    vms: [],
    pools: [{ agent: "reviewer", image: "rust-dev v8", cpus: 2, memory: 4 * G, warm: 1, bound: 2, hits: 7, starts: 9, idle_s: 60, leaving_s: null }],
    images: [
      { name: "rust-dev", from: "rust-dev v8", digest: "sha256:5d2a1e9c07f3", size: 1.39 * G, built_s: 300, state: "built" },
      { name: "harness-claude", from: "kiln's", digest: "sha256:b1c4e8a20d6f", size: 0.6 * G, built_s: 172800, state: "built" },
    ],
    starts: { warm: 7, template: 2, cold: 1 },
    net: series(8, 6, 79),
  },
};

/** Handles the routes above; false when it doesn't know the route. */
export async function handleMicrovms(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  const p = url.pathname;
  const m = req.method ?? "GET";
  if (p === "/api/microvm/engines" && m === "GET") {
    // A switch drains: its microVMs end one by one, then it's done.
    for (const [name, e] of Object.entries(ENGINES)) {
      if (!e.switching) continue;
      const r = RUNNERS[name];
      const gone = Math.floor((Date.now() / 1000 - e.switching.since) / 20);
      e.switching.draining = e.switching.draining.slice(Math.min(gone, e.switching.draining.length));
      if (r) r.vms = r.vms.filter((v) => v.state !== "draining" || e.switching!.draining.includes(v.id));
      if (e.switching.draining.length === 0) {
        e.engine = e.switching.to;
        e.origin = e.switching.to === "host" ? "owner" : "default";
        e.switching = null;
        if (r && e.engine === "host") r.pools = [];
      }
    }
    return (ctx.json(res, 200, ENGINES), true);
  }
  const one = p.match(/^\/api\/microvm\/runners\/([^/]+)$/);
  if (one && m === "GET") {
    const r = RUNNERS[decodeURIComponent(one[1]!)] ?? { vms: [], pools: [], images: [], starts: { warm: 0, template: 0, cold: 0 }, net: [] };
    return (ctx.json(res, 200, r), true);
  }
  const sw = p.match(/^\/api\/microvm\/runners\/([^/]+)\/engine$/);
  if (sw && m === "PUT") {
    const name = decodeURIComponent(sw[1]!);
    const e = ENGINES[name];
    const b = await ctx.body<{ engine?: Engine; end?: boolean; cancel?: boolean }>(req);
    if (!e) return (ctx.json(res, 404, { error: "no such runner" }), true);
    if (b.cancel && e.switching) {
      const r = RUNNERS[name];
      r?.vms.forEach((v) => {
        if (v.state === "draining") v.state = "running";
      });
      e.switching = null;
      return (ctx.json(res, 200, e), true);
    }
    if (b.end && e.switching) {
      const r = RUNNERS[name];
      if (r) r.vms = r.vms.filter((v) => !e.switching!.draining.includes(v.id));
      e.engine = e.switching.to;
      e.origin = e.engine === "host" ? "owner" : "default";
      e.switching = null;
      if (r && e.engine === "host") r.pools = [];
      return (ctx.json(res, 200, e), true);
    }
    if (b.engine !== "host" && b.engine !== "microvm") return (ctx.json(res, 400, { error: "engine is host or microvm" }), true);
    if (b.engine === "microvm" && !e.can) return (ctx.json(res, 409, { error: e.why ?? "this runner can't run microVMs" }), true);
    if (b.engine === e.engine) return (ctx.json(res, 200, e), true);
    const r = RUNNERS[name];
    const live = (r?.vms ?? []).filter((v) => v.state !== "failed");
    if (live.length === 0) {
      e.engine = b.engine;
      e.origin = b.engine === "host" ? "owner" : "default";
      return (ctx.json(res, 200, e), true);
    }
    live.forEach((v) => (v.state = "draining"));
    e.switching = { to: b.engine, draining: live.map((v) => v.id), since: Date.now() / 1000 };
    return (ctx.json(res, 200, e), true);
  }
  return false;
}
