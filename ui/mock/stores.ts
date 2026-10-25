/**
 * The mock's conversation stores (`GET /api/conversations/status`, `GET /api/conversations/series`;
 * docs/conversations.md, "Telemetry"): the server's store, each runner's feeding it, a mirror's copy
 * fed from it. Each second the runners write, their feeds deliver in batches (one falls behind now
 * and then and catches up), the mirror follows the server, and every ten minutes the server's file
 * is made compact over a minute and a half. An hour of 5 s points and a day of minute points are
 * made up at start, so the charts have a past.
 */
import type http from "node:http";
import type { ConvoLog, ConvoLogLine, ConvoNote, ConvoSeries, ConvoShip, ConvoStatus, ConvoStore, ConvoUpkeep } from "../src/api/types.ts";
import { hashStr, mulberry32 } from "./data.ts";

interface Ctx {
  json: (res: http.ServerResponse, status: number, body: unknown) => void;
  err: (res: http.ServerResponse, status: number, message: string) => void;
  /** The runners: whether each is connected, the kiln it runs, when it was last seen (unix s). */
  runners: () => { name: string; connected: boolean; version: string; last_seen: number }[];
}

/** One store's counters, as the simulation keeps them. */
interface Sim {
  key: string;
  machine: string;
  id: string;
  role: "server" | "runner" | "mirror";
  /** Writes so far, and what its taker has of them (a runner's: the server; a mirror's copy: of the server's). */
  top: number;
  rev: number;
  rate: number;
  bytes: number;
  free: number;
  wal: number;
  sessions: number;
  commits: number;
  busyMs: number;
  waits: number[];
  batches: number;
  /** Rows its feed delivered (a runner's to the server, the server's to the mirror). */
  rows: number;
  wire: number;
  lastBatch: number;
  wroteAt: number;
  /** Too old to report (it feeds, says nothing of itself). */
  silent: boolean;
  /** A runner that falls behind: until when its feed holds back. */
  stallUntil: number;
}

interface Point {
  t: number;
  bytes: number | null;
  free: number | null;
  wal: number | null;
  sessions: number | null;
  writes: number | null;
  commits: number | null;
  busy: number | null;
  p50: number | null;
  p99: number | null;
  behind: number | null;
  batches: number | null;
  wire: number | null;
  up: number;
}

const rng = mulberry32(hashStr("conversation-stores"));
const GB = 1 << 30;
const VERSION = "8a2dc8c";
const sims = new Map<string, Sim>();
const fine = new Map<string, Point[]>();
const coarse = new Map<string, Point[]>();
const notes: ConvoNote[] = [];
let compact: { began: number; phase: number } | null = null;
let compacted: { at: number; before: number; after: number; ms: number } | undefined;
let lastCompactAt = Date.now() - 7 * 60_000;
let started = false;
let ctxRunners: Ctx["runners"] = () => [];

const idOf = (name: string) => (hashStr(`store-${name}`) >>> 0).toString(16).padStart(8, "0") + (hashStr(`id-${name}`) >>> 0).toString(16).padStart(8, "0");

function note(key: string, level: ConvoNote["level"], text: string, at = Date.now()) {
  notes.push({ at, key, level, text });
  if (notes.length > 200) notes.shift();
  logged({ at, key, kind: "note", level, text });
}

/** The replication log, as the server keeps it (`GET /api/conversations/log`): oldest first. */
const log: ConvoLogLine[] = [];
const counted = new Map<string, [number, number, number]>();
function logged(line: ConvoLogLine) {
  log.push(line);
  if (log.length > 5_000) log.shift();
}
const kb = (b: number) => (b >= 1 << 20 ? `${(b / (1 << 20)).toFixed(1)} MB` : b >= 1 << 10 ? `${Math.round(b / 1024)} KB` : `${b} B`);

/** What each feed moved since the last sample, as the server's board logs it every 5 s. */
function sampleLog(now: number) {
  for (const s of sims.values()) {
    if (s.role === "server") continue;
    const was = counted.get(s.key);
    counted.set(s.key, [s.batches, s.rows, s.wire]);
    if (!was || s.batches <= was[0]) continue;
    const moved = { batches: s.batches - was[0], rows: s.rows - was[1], bytes: s.wire - was[2], rev: s.rev };
    const said = `${moved.batches} ${moved.batches === 1 ? "batch" : "batches"} · ${moved.rows.toLocaleString("en-US")} rows · ${kb(moved.bytes)}`;
    logged(
      s.role === "mirror"
        ? { at: now, key: s.key, kind: "ship", level: "info", text: `server → ${s.machine}: ${said} · acknowledged through ${s.rev}`, moved }
        : { at: now, key: s.key, kind: "batch", level: "info", text: `${s.machine} → server: ${said} · through ${s.rev}`, moved },
    );
  }
}

function logPage(q: string, limit: number): ConvoLog {
  const words = q.trim().toLowerCase();
  const has = (l: ConvoLogLine) => !words || l.text.toLowerCase().includes(words) || l.key.toLowerCase().includes(words);
  const matched = log.filter(has);
  return { at: Date.now(), total: log.length, matched: matched.length, lines: matched.slice(-limit).reverse() };
}

function sim(key: string, machine: string, role: Sim["role"], bytes: number, free: number, top: number, sessions: number, rate: number, silent = false): Sim {
  return {
    key,
    machine,
    id: idOf(machine + role),
    role,
    top,
    rev: top,
    rate,
    bytes: Math.round(bytes),
    free: Math.round(free),
    wal: 4 << 20,
    sessions,
    commits: Math.round(top / 9),
    busyMs: Math.round(top / 40),
    waits: [Math.round(top / 3), Math.round(top / 30), Math.round(top / 300), Math.round(top / 3000), 40, 6, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    batches: 0,
    rows: 0,
    wire: 0,
    lastBatch: 0,
    wroteAt: Date.now(),
    silent,
    stallUntil: 0,
  };
}

/** The runners' stores as the mock's runners are: the old Pi feeds without reporting, the Mac mini keeps none, the old NUC is away. */
function start() {
  started = true;
  const runners = ctxRunners();
  let total = 0;
  for (const r of runners) {
    if (r.name === "metal-mac-mini") continue;
    const big = hashStr(r.name) % 5;
    const s = sim(
      `${r.name}/${idOf(r.name + "runner")}`,
      r.name,
      "runner",
      (0.4 + big * 0.35) * GB,
      (0.05 + big * 0.04) * GB,
      180_000 + big * 210_000,
      40 + big * 25,
      r.connected ? 6 + big * 4 : 0,
      r.version !== "0.1.0",
    );
    total += s.top;
    sims.set(s.key, s);
  }
  const server = sim("server", "server", "server", 3.42 * GB, 1.21 * GB, total + 52_000, 1_284, 1.5);
  sims.set("server", server);
  const mirror = sim("mirror:metal-epyc-01", "metal-epyc-01", "mirror", 3.1 * GB, 0.3 * GB, server.top, 1_284, 0);
  sims.set(mirror.key, mirror);
  // A past: a day at a minute, an hour at 5 s, quiet at night, busy by day.
  const now = Date.now();
  for (const s of sims.values()) {
    const c: Point[] = [];
    const f: Point[] = [];
    const wave = (t: number) => 0.55 + 0.45 * Math.sin((t / 86_400_000) * 2 * Math.PI - 1.2);
    for (let i = 1440; i > 0; i--) c.push(madeUp(s, now - i * 60_000, wave(now - i * 60_000), i / 1440));
    for (let i = 720; i > 0; i--) f.push(madeUp(s, now - i * 5_000, wave(now - i * 5_000), i / 17280));
    coarse.set(s.key, c);
    fine.set(s.key, f);
  }
  note("server", "info", "telemetry began: 6 stores", now - 3_600_000);
  note("mirror:metal-epyc-01", "info", "metal-epyc-01 keeps a copy of the server's store", now - 3_590_000);
  note("metal-nuc-old/" + idOf("metal-nuc-oldrunner"), "warn", "metal-nuc-old's store is offline", now - 3_000_000);
  note("server", "info", "the server's store: 2,000 transcript lines of agents that ended long ago were let go", now - 2_400_000);
  setInterval(step, 1000);
}

/** A point of the made-up past: `busy` (0–1) how busy the hour was, `ago` (0–1) how far back. */
function madeUp(s: Sim, t: number, busy: number, ago: number): Point {
  const all = [...sims.values()].filter((x) => x.role === "runner").reduce((a, x) => a + x.rate, 0);
  const base = s.role === "runner" ? s.rate : all + 2;
  const writes = Math.max(0, base * busy * (0.7 + rng() * 0.6));
  const has = !s.silent;
  return {
    t: Math.floor(t / 1000),
    bytes: has ? Math.round(s.bytes * (1 - ago * 0.08)) : null,
    free: has ? Math.round(s.free * (1 - ago * 0.5)) : null,
    wal: has ? Math.round((2 + rng() * 6) * (1 << 20)) : null,
    sessions: has ? Math.round(s.sessions * (1 - ago * 0.1)) : null,
    writes: has ? writes : null,
    commits: has ? Math.max(0, writes / 9) : null,
    busy: has ? Math.min(1, 0.01 + busy * 0.05 + rng() * 0.02) : null,
    p50: has ? 1 : null,
    p99: has ? (rng() < 0.05 ? 64 : 4) : null,
    behind: s.role === "server" ? null : rng() < 0.04 ? Math.round(rng() * 900) : 0,
    batches: s.role === "server" ? 1.2 * busy : 0.3 * busy,
    wire: s.role === "server" ? 48_000 * busy : 9_000 * busy,
    up: 1,
  };
}

/** A second of the simulation. */
function step() {
  const now = Date.now();
  const runners = new Map(ctxRunners().map((r) => [r.name, r]));
  const server = sims.get("server")!;
  let delivered = 0;
  for (const s of sims.values()) {
    if (s.role !== "runner") continue;
    const r = runners.get(s.machine);
    if (!r?.connected) continue;
    s.rate = Math.max(0, Math.min(80, s.rate + (rng() - 0.5) * 3));
    const wrote = Math.round(s.rate * (0.5 + rng()));
    if (wrote > 0) {
      s.top += wrote;
      s.commits += Math.ceil(wrote / 12);
      s.wroteAt = now;
      grow(s, wrote);
    }
    s.busyMs += Math.round(8 + rng() * 25);
    s.waits[0]! += 12;
    s.waits[1]! += 2;
    if (rng() < 0.08) s.waits[3]! += 1;
    // Now and then a feed holds back for a while, then catches up.
    if (s.stallUntil === 0 && rng() < 0.004) {
      s.stallUntil = now + 25_000;
    }
    if (s.stallUntil && now >= s.stallUntil) s.stallUntil = 0;
    const lag = s.top - s.rev;
    if (lag > 0 && !s.stallUntil && rng() < 0.75) {
      const n = Math.min(lag, 1_500);
      s.rev += n;
      s.batches += 1;
      s.rows += n;
      s.wire += n * 310;
      s.lastBatch = now;
      delivered += n;
    }
  }
  const own = Math.round(server.rate * rng() * 2);
  server.top += delivered + own;
  if (delivered + own > 0) {
    server.wroteAt = now;
    server.commits += Math.ceil(own / 8) + (delivered ? 1 : 0);
    grow(server, delivered + own);
  }
  server.busyMs += Math.round(15 + delivered / 40 + rng() * 20);
  server.waits[0]! += 30;
  server.waits[1]! += 6;
  server.waits[2]! += 1;
  if (rng() < 0.05) server.waits[5]! += 1;
  // The mirror follows the server.
  const mirror = sims.get("mirror:metal-epyc-01")!;
  const mlag = server.top - mirror.rev;
  if (mlag > 0 && rng() < 0.8) {
    const n = Math.min(mlag, 2_500);
    mirror.rev += n;
    mirror.top += n;
    mirror.batches += 1;
    mirror.rows += n;
    mirror.wire += n * 290;
    mirror.lastBatch = now;
    mirror.wroteAt = now;
    mirror.commits += 1;
    grow(mirror, n);
  }
  mirror.busyMs += Math.round(6 + rng() * 10);
  mirror.waits[0]! += 4;
  // Every ten minutes the server's file is made compact, over a minute and a half.
  if (!compact && now - lastCompactAt > 10 * 60_000) {
    compact = { began: now, phase: 0 };
    note("server", "info", `the server's store: making its file compact (${(server.bytes / GB).toFixed(2)} GB, ${(server.free / GB).toFixed(2)} GB free)`);
  }
  if (compact && now - compact.began > 90_000) {
    const before = server.bytes;
    server.bytes = Math.round(server.bytes - server.free * 0.96);
    server.free = Math.round(server.free * 0.04);
    compacted = { at: now, before, after: server.bytes, ms: now - compact.began };
    note("server", "info", `the server's store: its file was made compact, ${(before / GB).toFixed(2)} GB to ${(server.bytes / GB).toFixed(2)} GB, in 90 s`);
    compact = null;
    lastCompactAt = now;
    // The free part grows back as old transcripts go, for the next time.
    server.free += Math.round(0.9 * GB);
    server.bytes += Math.round(0.9 * GB);
  }
  // Notes of feeds that fell behind and caught up.
  for (const s of sims.values()) {
    if (s.role === "server") continue;
    const behind = s.role === "mirror" ? server.top - s.rev : s.top - s.rev;
    const was = lastBehind.get(s.key) ?? 0;
    if (was === 0 && behind > 2_000) note(s.key, "info", `${s.machine}'s ${s.role === "mirror" ? "copy" : "store"} is ${behind.toLocaleString("en-US")} rows behind`);
    if (was > 2_000 && behind === 0) note(s.key, "info", `${s.machine}'s ${s.role === "mirror" ? "copy" : "store"} caught up`);
    lastBehind.set(s.key, behind > 2_000 ? behind : behind === 0 ? 0 : was);
  }
  if (now - lastPoint >= 5_000) {
    lastPoint = now;
    for (const s of sims.values()) push(s, now);
    sampleLog(now);
  }
}
const lastBehind = new Map<string, number>();
let lastPoint = 0;
const previous = new Map<string, Sim>();

/** A store took `rows` rows: its free part first, then the file grows. */
function grow(s: Sim, rows: number) {
  const b = rows * 300;
  if (s.free > b) s.free -= b;
  else s.bytes += b;
  s.wal = Math.min(64 << 20, Math.round(s.wal + b / 4));
  if (rng() < 0.1) s.wal = 2 << 20;
  if (rng() < 0.002) s.sessions += 1;
}

function push(s: Sim, now: number) {
  const prev = previous.get(s.key);
  const r = ctxRunners().find((x) => x.name === s.machine);
  const up = s.role === "server" || r?.connected ? 1 : 0;
  const dt = prev ? 5 : 0;
  const d = (a: number, b: number | undefined) => (dt && b !== undefined ? (a - b) / dt : null);
  const behind = s.role === "server" ? null : s.role === "mirror" ? sims.get("server")!.top - s.rev : s.top - s.rev;
  const p: Point = {
    t: Math.floor(now / 1000),
    bytes: s.silent ? null : s.bytes,
    free: s.silent ? null : s.free,
    wal: s.silent ? null : s.wal,
    sessions: s.silent ? null : s.sessions,
    writes: s.silent ? null : d(s.top, prev?.top),
    commits: s.silent ? null : d(s.commits, prev?.commits),
    busy: s.silent ? null : prev ? Math.min(1, (s.busyMs - prev.busyMs) / 5000) : null,
    p50: s.silent ? null : 1,
    p99: s.silent ? null : rng() < 0.06 ? 32 : 4,
    behind: up ? behind : null,
    batches: s.role === "server" ? d(totalBatches(), prevTotal.batches) : d(s.batches, prev?.batches),
    wire: s.role === "server" ? d(totalWire(), prevTotal.wire) : d(s.wire, prev?.wire),
    up,
  };
  if (s.role === "server") {
    prevTotal.batches = totalBatches();
    prevTotal.wire = totalWire();
  }
  previous.set(s.key, { ...s, waits: [...s.waits] });
  const f = fine.get(s.key) ?? [];
  f.push(p);
  if (f.length > 720) f.shift();
  fine.set(s.key, f);
  const c = coarse.get(s.key) ?? [];
  if (!c.length || p.t - c[c.length - 1]!.t >= 60) {
    c.push(p);
    if (c.length > 1440) c.shift();
  }
  coarse.set(s.key, c);
}
const prevTotal: { batches?: number; wire?: number } = {};
const totalBatches = () => [...sims.values()].filter((s) => s.role === "runner").reduce((a, s) => a + s.batches, 0);
const totalWire = () => [...sims.values()].filter((s) => s.role === "runner").reduce((a, s) => a + s.wire, 0);

/** The last minute's rates, from its 5 s points. */
function rates(key: string): ConvoStore["rates"] {
  const f = (fine.get(key) ?? []).slice(-12);
  const avg = (xs: (number | null)[]) => {
    const v = xs.filter((x): x is number => x !== null);
    return v.length ? v.reduce((a, b) => a + b, 0) / v.length : null;
  };
  return { writes: avg(f.map((p) => p.writes)), batches: avg(f.map((p) => p.batches)), wire: avg(f.map((p) => p.wire)), busy: avg(f.map((p) => p.busy)) };
}

function upkeepOf(s: Sim, now: number): ConvoUpkeep {
  if (s.role !== "server" || !compact) return { state: "idle" };
  const e = (now - compact.began) / 90_000;
  const phase = e < 0.85 ? "copy" : e < 0.93 ? "catch_up" : e < 0.98 ? "count" : "swap";
  const table = e < 0.15 ? "sessions" : e < 0.55 ? "events" : e < 0.82 ? "records" : "drops";
  const of = s.bytes - s.free;
  return { state: "compacting", phase, table: phase === "copy" ? table : undefined, copied: Math.round(of * Math.min(1, e * 1.05)), of, began: compact.began };
}

function ship(s: Sim, up: boolean): ConvoShip {
  return {
    up,
    acked: up ? s.rev : null,
    sent: s.rev,
    flight: up && s.top > s.rev ? 2 : 0,
    batches: s.batches,
    rows: s.rev,
    bytes: s.wire,
    last_sent: s.lastBatch || null,
    last_ack: s.lastBatch ? s.lastBatch + 40 : null,
    hellos: 1,
    stalls: 0,
  };
}

function report(s: Sim, now: number): ConvoStore["report"] {
  if (s.silent) return null;
  return {
    id: s.id,
    commits: s.commits,
    rows: s.top,
    jobs: Math.round(s.top / 5),
    busy_ms: s.busyMs,
    dropped: 0,
    timeouts: 0,
    waits: s.waits,
    file: {
      at: now - 800,
      bytes: s.bytes,
      free: s.free,
      wal: s.wal,
      top: s.top,
      wrote_at: s.wroteAt,
      sessions: s.sessions,
      upkeep: upkeepOf(s, now),
      compacted: s.role === "server" ? compacted : undefined,
    },
  };
}

export function status(): ConvoStatus {
  const now = Date.now();
  const runners = ctxRunners();
  const server = sims.get("server")!;
  const stores: ConvoStore[] = [];
  const spark = (key: string) => (fine.get(key) ?? []).slice(-120).map((p) => p.writes);
  stores.push({
    key: "server",
    role: "server",
    machine: "server",
    id: server.id,
    version: VERSION,
    connected: true,
    state: "live",
    heard: now - 800,
    report: report(server, now),
    rates: rates("server"),
    spark: spark("server"),
  });
  for (const r of runners) {
    const s = [...sims.values()].find((x) => x.role === "runner" && x.machine === r.name);
    if (!s) {
      stores.push({
        key: `${r.name}/`,
        role: "runner",
        machine: r.name,
        id: null,
        version: r.version,
        connected: r.connected,
        state: r.connected ? "no_store" : "offline",
        heard: null,
        report: null,
        rates: { writes: null, batches: null, wire: null, busy: null },
        spark: [],
      });
      continue;
    }
    const behind = s.top - s.rev;
    const state: ConvoStore["state"] = !r.connected ? "offline" : s.silent ? "feeding" : behind === 0 ? "in_sync" : now - s.lastBatch < 15_000 ? "catching_up" : "behind";
    stores.push({
      key: s.key,
      role: "runner",
      machine: s.machine,
      id: s.id,
      version: s.silent ? r.version : VERSION,
      connected: r.connected,
      state,
      heard: r.connected ? now - 1_200 : r.last_seen * 1000,
      report: report(s, now),
      feed: {
        rev: s.rev,
        top: s.silent ? null : s.top,
        behind: s.silent ? null : behind,
        at: s.lastBatch,
        intake: {
          machine: s.machine,
          store: s.id,
          rev: s.rev,
          hello_at: now - 3_500_000,
          batches: s.batches,
          rows: s.rev,
          fenced: 0,
          left: 0,
          bytes: s.wire,
          gaps: 0,
          errors: 0,
          last_batch: s.lastBatch,
          take_ms: 3,
        },
        ship: s.silent ? null : ship(s, r.connected),
      },
      rates: rates(s.key),
      spark: spark(s.key),
    });
  }
  const m = sims.get("mirror:metal-epyc-01")!;
  const mb = server.top - m.rev;
  stores.push({
    key: m.key,
    role: "mirror",
    machine: m.machine,
    id: m.id,
    version: VERSION,
    connected: true,
    state: mb === 0 ? "in_sync" : "catching_up",
    heard: now - 1_000,
    report: report(m, now),
    mirror: { chosen_at: now - 86_400_000 * 3, last_seen: now, ship: ship(m, true), behind: mb },
    rates: rates(m.key),
    spark: spark(m.key),
  });
  return { at: now, on: true, mirrors_wanted: 1, restore: { state: "idle" }, stores, notes: [...notes].reverse() };
}

function series(key: string, window: number): ConvoSeries | null {
  const tier = window <= 3600 ? fine.get(key) : coarse.get(key);
  if (!tier) return null;
  const since = Math.floor(Date.now() / 1000) - window;
  const pts = tier.filter((p) => p.t >= since);
  const col = <K extends keyof Point>(k: K) => pts.map((p) => p[k]);
  return {
    key,
    step: window <= 3600 ? 5 : 60,
    t: col("t") as number[],
    bytes: col("bytes"),
    free: col("free"),
    wal: col("wal"),
    sessions: col("sessions"),
    writes: col("writes"),
    commits: col("commits"),
    busy: col("busy"),
    p50: col("p50"),
    p99: col("p99"),
    timeouts: pts.map(() => 0),
    dropped: pts.map(() => 0),
    behind: col("behind"),
    batches: col("batches"),
    wire: col("wire"),
    up: col("up") as number[],
  };
}

export async function handleStores(req: http.IncomingMessage, res: http.ServerResponse, url: URL, ctx: Ctx): Promise<boolean> {
  if (!url.pathname.startsWith("/api/conversations/") || req.method !== "GET") return false;
  ctxRunners = ctx.runners;
  if (!started) start();
  if (url.pathname === "/api/conversations/status") return (ctx.json(res, 200, status()), true);
  if (url.pathname === "/api/conversations/log") {
    const limit = Math.min(5_000, Math.max(1, Number(url.searchParams.get("limit") ?? 1000) || 1000));
    return (ctx.json(res, 200, logPage(url.searchParams.get("q") ?? "", limit)), true);
  }
  if (url.pathname === "/api/conversations/series") {
    const key = url.searchParams.get("key") ?? "server";
    const window = Math.min(86_400, Math.max(60, Number(url.searchParams.get("window") ?? 3600) || 3600));
    const s = series(key, window) ?? {
      key,
      step: 5,
      t: [],
      bytes: [],
      free: [],
      wal: [],
      sessions: [],
      writes: [],
      commits: [],
      busy: [],
      p50: [],
      p99: [],
      timeouts: [],
      dropped: [],
      behind: [],
      batches: [],
      wire: [],
      up: [],
    };
    return (ctx.json(res, 200, s), true);
  }
  return false;
}
