import assert from "node:assert/strict";
import { test } from "node:test";
import type { ConvoStatus, ConvoStore } from "../src/api/types.ts";
import { ACTIVE_MS, plateName, stateText, storesScene } from "../src/views/stores/scene.ts";

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

test("every store has its place to click, in both layouts", () => {
  const s = status([runner("fedora"), runner("jupiter", { connected: false, state: "offline" }), runner("mac", { id: null, key: "mac/", state: "no_store", report: null, feed: undefined })]);
  for (const portrait of [false, true]) {
    const scene = storesScene(s, portrait);
    assert.deepEqual(scene.hits.map((h) => h.key).sort(), ["fedora/s-fedora", "jupiter/s-jupiter", "mac/", "server"]);
    assert.ok(scene.hits.every((h) => h.w > 0 && h.h > 0));
  }
});

test("a light runs on a cable only while batches move on it, and on a file only while it writes", () => {
  const moving = runner("fedora", {}, NOW - 1000);
  const still = runner("jupiter", {}, NOW - ACTIVE_MS - 1000);
  const scene = storesScene(status([moving, still]), false);
  const lights = scene.paths.filter((p) => p.kind === "light").map((p) => p.id);
  // fedora sorts first: feed-0 is its cable.
  assert.ok(lights.includes("feed-0-flow"), lights.join());
  assert.ok(!lights.includes("feed-1-flow"), lights.join());
  // The server wrote a second ago: the head of its tray is lit.
  assert.ok(lights.includes("server-tray-0-head"), lights.join());
  const quiet = status([still]);
  const f = quiet.stores[0]!.report!.file;
  f.wrote_at = f.at - ACTIVE_MS - 1;
  assert.ok(!storesScene(quiet, false).paths.some((p) => p.kind === "light"));
});

test("a store's state reads in a few words", () => {
  assert.equal(stateText(runner("a", { state: "catching_up", feed: { rev: 1, top: 12_345, behind: 12_344, at: 0, intake: null, ship: null } }), NOW), "12k behind · catching up");
  assert.equal(stateText(runner("a", { state: "offline", connected: false, heard: NOW - 2 * 86_400_000 }), NOW), "offline · 2d");
  assert.equal(stateText(runner("a", { state: "no_store", id: null }), NOW), "keeps no store");
});

test("the scene grows with the machines, never cramming them", () => {
  const few = storesScene(status([runner("a")]), false);
  const many = storesScene(status(["a", "b", "c", "d", "e", "f", "g", "h"].map((m) => runner(m))), false);
  assert.ok(many.height > few.height);
  // Six to a row at most, each cabinet's front clear of the next.
  const rows = new Map<number, number[]>();
  for (const l of many.labels.filter((l) => /^runner-\d+-name$/.test(l.id))) rows.set(l.y, [...(rows.get(l.y) ?? []), l.x]);
  assert.deepEqual(
    [...rows.values()].map((xs) => xs.length),
    [6, 2],
  );
  for (const xs of rows.values()) for (let i = 1; i < xs.length; i++) assert.ok(xs[i]! - xs[i - 1]! >= 100, xs.join());
});

test("a machine's name is cut to its cabinet and still tells machines apart", () => {
  assert.equal(plateName("fedora.attlocal.net", 14), "fedora");
  assert.equal(plateName("gpu-box.tail1234.ts.net", 14), "gpu-box");
  assert.equal(plateName("192.168.1.20", 14), "192.168.1.20");
  assert.equal(plateName("adas-macbook-pro-2.local", 14), "adas-macboo…-2");
  assert.equal(plateName("build-farm-01", 11), "build-f…-01");
  assert.equal(plateName("raspberrypigaragehub", 11), "raspberryp…");
  const farm = Array.from({ length: 9 }, (_, i) => runner(`build-farm-0${i + 1}`));
  for (const portrait of [false, true]) {
    const plates = storesScene(status(farm), portrait).labels.filter((l) => l.kind === "plate" && l.id.startsWith("runner-"));
    assert.equal(new Set(plates.map((l) => l.text)).size, 9, plates.map((l) => l.text).join());
  }
});

test("on a phone the summary sits clear of the last cabinet's state", () => {
  for (const n of [1, 2, 5]) {
    const scene = storesScene(status(Array.from({ length: n }, (_, i) => runner(`m${i}`, { state: "offline", connected: false, heard: NOW - 60_000 }))), true);
    const states = scene.labels.filter((l) => /-status(-2)?$/.test(l.id)).map((l) => l.y);
    const summary = scene.labels.find((l) => l.id === "summary")!;
    assert.ok(summary.y - Math.max(...states) >= 24, `${n}: ${summary.y} vs ${Math.max(...states)}`);
    assert.ok(
      scene.labels.every((l) => l.y < scene.height),
      `${n}`,
    );
  }
});
