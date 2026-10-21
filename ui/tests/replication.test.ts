import assert from "node:assert/strict";
import { test } from "node:test";
import type { ConvoStatus, ConvoStore } from "../src/api/types.ts";
import { ACTIVE_MS, plateName, stateText } from "../src/views/stores/scene.ts";
import { healthOf, marksOf, metricsOf, periodOf, replicationScene } from "../src/views/stores/replication-scene.ts";

const NOW = 1_800_000_000_000;

function file(top: number, wroteAt = 0) {
  return { at: NOW - 1000, bytes: 1 << 30, free: 1 << 28, wal: 1 << 20, top, wrote_at: wroteAt, sessions: 10, upkeep: { state: "idle" as const } };
}

function report(id: string, top: number, wroteAt = 0) {
  return { id, commits: 1, rows: 1, jobs: 1, busy_ms: 1, dropped: 0, timeouts: 0, waits: [], file: file(top, wroteAt) };
}

const none = { writes: null, batches: null, wire: null, busy: null };

function runner(machine: string, over: Partial<ConvoStore> = {}, lastBatch = 0): ConvoStore {
  return {
    key: `${machine}/s-${machine}`,
    role: "runner",
    machine,
    id: `s-${machine}`,
    version: "abc1234",
    connected: true,
    state: "in_sync",
    heard: NOW - 2000,
    report: report(`s-${machine}`, 100),
    feed: {
      rev: 100,
      top: 100,
      behind: 0,
      at: lastBatch,
      intake: { machine, store: `s-${machine}`, rev: 100, hello_at: NOW - 60_000, batches: 3, rows: 100, fenced: 0, left: 0, bytes: 1000, gaps: 0, errors: 0, last_batch: lastBatch, take_ms: 1 },
      ship: null,
    },
    rates: none,
    spark: [],
    ...over,
  };
}

function status(stores: ConvoStore[]): ConvoStatus {
  const server: ConvoStore = {
    key: "server",
    role: "server",
    machine: "server",
    id: "srv",
    version: "abc1234",
    connected: true,
    state: "live",
    heard: NOW,
    report: report("srv", 500, NOW - 1000),
    rates: none,
    spark: [],
  };
  return { at: NOW, on: true, mirrors_wanted: 1, restore: { state: "idle" }, stores: [server, ...stores], notes: [] };
}

function mirror(over: Partial<ConvoStore> = {}, lastSent = NOW - 1000): ConvoStore {
  return {
    key: "mirror:adacachy",
    role: "mirror",
    machine: "adacachy",
    id: "copy",
    version: "abc1234",
    connected: true,
    state: "in_sync",
    heard: NOW - 2000,
    report: report("copy", 500),
    mirror: {
      chosen_at: NOW - 86_400_000,
      last_seen: NOW - 1000,
      behind: 0,
      ship: { up: true, acked: 500, sent: 500, flight: 0, batches: 9, rows: 500, bytes: 9000, last_sent: lastSent, last_ack: lastSent, hellos: 1, stalls: 0 },
    },
    rates: { writes: 2, batches: 0.5, wire: 1000, busy: 0 },
    spark: [],
    ...over,
  };
}

test("every store has its place to pick, in both layouts, inside the picture", () => {
  const s = status([runner("fedora"), runner("jupiter", { connected: false, state: "offline" }), runner("mac", { id: null, key: "mac/", state: "no_store", report: null, feed: undefined }), mirror()]);
  for (const portrait of [false, true]) {
    const scene = replicationScene(s, portrait);
    const keys = [...scene.sources.map((n) => n.key), scene.server.key, scene.mirror?.key].sort();
    // A machine that keeps no store has no database to draw.
    assert.deepEqual(keys, ["fedora/s-fedora", "jupiter/s-jupiter", "mirror:adacachy", "server"]);
    for (const n of [...scene.sources, scene.server, scene.mirror!]) {
      assert.ok(n.hit.w > 0 && n.hit.h > 0, n.key);
      assert.ok(n.hit.x >= 0 && n.hit.y >= 0 && n.hit.x + n.hit.w <= scene.width && n.hit.y + n.hit.h <= scene.height, `${portrait} ${n.key}`);
    }
  }
});

test("a light runs on a cable only while batches move on it, about once a batch", () => {
  const busy = runner("fedora", { rates: { writes: 4, batches: 2, wire: 1000, busy: 0.1 } }, NOW - 1000);
  const slow = runner("jupiter", { rates: { writes: 0.1, batches: 0.1, wire: 10, busy: 0 } }, NOW - 1000);
  const still = runner("mac", { rates: { writes: 0, batches: 0, wire: 0, busy: 0 } }, NOW - ACTIVE_MS - 1000);
  const away = runner("pi", { connected: false, state: "offline", rates: { writes: 0, batches: 1, wire: 0, busy: 0 } }, NOW - 1000);
  const scene = replicationScene(status([busy, slow, still, away]), false);
  const period = Object.fromEntries(scene.sources.map((n) => [n.key.split("/")[0], n.cable.period]));
  assert.deepEqual(period, { fedora: 1.2, jupiter: 6, mac: null, pi: null });
  assert.equal(scene.sources.find((n) => n.key.startsWith("pi/"))?.cable.tone, "off");
  assert.equal(periodOf(0.5), 2);
  assert.equal(periodOf(null), null);
});

test("rows to come wait at a cable's start, more marks for ten times as many", () => {
  assert.deepEqual([0, 1, 14, 248, 5_000, 1e9].map(marksOf), [0, 1, 2, 3, 4, 6]);
  const behind = runner("fedora", { state: "catching_up", feed: { rev: 1, top: 249, behind: 248, at: NOW - 1000, intake: null, ship: null } }, NOW - 1000);
  const scene = replicationScene(status([behind]), false);
  assert.equal(scene.sources[0]?.cable.marks.length, 3);
  assert.equal(scene.sources[0]?.cable.tone, "warn");
  assert.equal(scene.sources[0]?.sub.text, "248 behind · catching up");
});

test("the picture grows with the stores, never cramming them, and the phone's mirror sits below them", () => {
  const few = replicationScene(status([runner("a")]), false);
  const many = replicationScene(status(["a", "b", "c", "d", "e", "f", "g", "h"].map((m) => runner(m))), false);
  assert.ok(many.height > few.height);
  const ys = many.sources.map((n) => n.plate.y);
  for (let i = 1; i < ys.length; i++) assert.ok(ys[i]! - ys[i - 1]! >= 50, ys.join());
  const phone = replicationScene(status([runner("a"), runner("b"), runner("c"), mirror()]), true);
  const last = Math.max(...phone.sources.map((n) => n.plate.y));
  assert.ok(phone.mirror && phone.mirror.plate.y - last >= 50, `${phone.mirror?.plate.y} vs ${last}`);
  assert.ok(phone.mirror && phone.mirror.sub.at[1] < phone.height);
});

test("the head and the numbers say how replication stands", () => {
  assert.deepEqual(healthOf(status([runner("fedora"), mirror()])), { text: "All stores current", tone: "ok" });
  const catching = runner("fedora", { state: "catching_up", feed: { rev: 1, top: 249, behind: 248, at: NOW, intake: null, ship: null } });
  assert.deepEqual(healthOf(status([catching, mirror()])), { text: "fedora is catching up", tone: "warn" });
  assert.deepEqual(healthOf(status([runner("fedora")])), { text: "No mirror yet", tone: "warn" });
  assert.deepEqual(healthOf(status([runner("fedora"), runner("pi", { connected: false, state: "offline" }), mirror()])), { text: "pi is offline", tone: "off" });
  const m = metricsOf(
    status([runner("fedora"), catching, runner("pi", { connected: false, state: "offline" }), mirror({ mirror: { chosen_at: 0, last_seen: NOW, behind: 1_200, ship: mirror().mirror!.ship } })]),
  );
  assert.deepEqual(m, { synced: 1, of: 3, pending: 248, mirror: { text: "1,200 behind", tone: "warn" } });
});

test("a store's state reads in a few words", () => {
  assert.equal(stateText(runner("a", { state: "catching_up", feed: { rev: 1, top: 12_345, behind: 12_344, at: 0, intake: null, ship: null } }), NOW), "12k behind · catching up");
  assert.equal(stateText(runner("a", { state: "offline", connected: false, heard: NOW - 2 * 86_400_000 }), NOW), "offline · 2d");
  assert.equal(stateText(runner("a", { state: "no_store", id: null }), NOW), "keeps no store");
});

test("a machine's name is cut to fit and still tells machines apart", () => {
  assert.equal(plateName("fedora.attlocal.net", 14), "fedora");
  assert.equal(plateName("gpu-box.tail1234.ts.net", 14), "gpu-box");
  assert.equal(plateName("192.168.1.20", 14), "192.168.1.20");
  assert.equal(plateName("adas-macbook-pro-2.local", 14), "adas-macboo…-2");
  assert.equal(plateName("build-farm-01", 11), "build-f…-01");
  assert.equal(plateName("raspberrypigaragehub", 11), "raspberryp…");
  const farm = Array.from({ length: 9 }, (_, i) => runner(`build-farm-0${i + 1}`));
  const names = replicationScene(status(farm), true).sources.map((n) => n.name.text);
  assert.equal(new Set(names).size, 9, names.join());
});
