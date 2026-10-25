import assert from "node:assert/strict";
import { test } from "node:test";
import { alongCubic, clip, iso, isoBox, rolloutScene, spread, type SceneBuild, type SceneRunner } from "../src/views/updates/scene.ts";

const build = (key: string, over: Partial<SceneBuild> = {}): SceneBuild => ({
  key,
  os: "linux",
  arch: "x86_64 · musl",
  line: "b0de36d · run #412",
  tone: "ok",
  cable: undefined,
  lit: false,
  building: false,
  ...over,
});
const runner = (key: string, over: Partial<SceneRunner> = {}): SceneRunner => ({
  key,
  name: key,
  line: "b0de36d · current",
  tone: "ok",
  cable: undefined,
  lit: false,
  switching: false,
  offline: false,
  ...over,
});
const server = { line: "0.2.0 · b0de36d · newest", tone: "ok" as const };

/** The numbers of an SVG path, in order. */
const nums = (d: string) => (d.match(/-?\d+(?:\.\d+)?/g) ?? []).map(Number);

test("the study's layout: four builds and five runners sit where the design drew them", () => {
  const s = rolloutScene({ builds: ["a", "b", "c", "d"].map((k) => build(k)), server, runners: ["1", "2", "3", "4", "5"].map((k) => runner(k)) }, false);
  assert.equal(s.width, 960);
  assert.equal(s.height, 318);
  assert.deepEqual(
    s.runners.map((r) => r.name.y + 3),
    [62, 114, 166, 218, 270],
  );
  assert.deepEqual(
    s.builds.map((b) => b.name.y + 3),
    [76, 130, 184, 238],
  );
  assert.equal(s.server.name.y, 146 + 80);
  assert.deepEqual(
    s.headings.map((h) => h.text),
    ["BUILDS", "SERVER", "RUNNERS"],
  );
});

test("the server's heading stands just above its box, not up with the columns' headings", () => {
  for (const runners of [1, 5, 12]) {
    const s = rolloutScene({ builds: ["a", "b"].map((k) => build(k)), server, runners: Array.from({ length: runners }, (_, i) => runner(`r${i}`)) }, false);
    const sy = s.server.name.y - 80;
    const heading = s.headings.find((h) => h.text === "SERVER")!;
    // The box's top: its far corner, the highest point of any of its faces.
    const top = Math.min(...[s.server.box.top, s.server.box.left, s.server.box.right].flatMap((d) => nums(d).filter((_, i) => i % 2 === 1)));
    assert.ok(Math.abs(top - (sy - 79.5)) < 0.01, `the box's top is where the heading reckons it (${top})`);
    assert.ok(heading.y < top && top - heading.y <= 16, `${runners} runners: the heading at ${heading.y}, the box's top at ${top}`);
    assert.ok(heading.y > s.headings.find((h) => h.text === "BUILDS")!.y, "below the columns' headings");
    assert.equal(heading.x, 480);
  }
  // On a phone, over its box too.
  const p = rolloutScene({ builds: [], server, runners: [runner("a")] }, true);
  const ph = p.headings.find((h) => h.text === "SERVER")!;
  const ptop = Math.min(...nums(p.server.box.top).filter((_, i) => i % 2 === 1));
  assert.ok(ph.y < ptop && ptop - ph.y <= 12 && ph.y > 8, `the phone's heading at ${ph.y}, its box's top at ${ptop}`);
  assert.equal(ph.anchor, "middle");
});

test("a build is named by its target: its OS, then its arch and ABI after it", () => {
  const s = rolloutScene({ builds: [build("a"), build("b", { os: "macos", arch: "aarch64" }), build("c", { os: "linux", arch: "x86_64 · gnu" })], server, runners: [] }, false);
  assert.deepEqual(
    s.builds.map((b) => [b.name.text, b.name.tail]),
    [
      ["linux", "x86_64 · musl"],
      ["macos", "aarch64"],
      ["linux", "x86_64 · gnu"],
    ],
  );
  assert.equal(s.builds[0]!.title, "linux x86_64 · musl build: b0de36d · run #412");
});

test("a connected runner's cable pulses now and then; one upgrading flows instead; an offline one does neither", () => {
  const s = rolloutScene(
    {
      builds: [],
      server,
      runners: [runner("idle"), runner("moving", { lit: true, tone: "run" }), runner("away", { offline: true, cable: "off", tone: "off" }), runner("failed", { tone: "bad", cable: "bad" })],
    },
    false,
  );
  const by = Object.fromEntries(s.runners.map((r) => [r.key, r]));
  assert.equal(by.idle!.pulse, true);
  assert.equal(by.failed!.pulse, true, "still connected: its link is live");
  assert.equal(by.moving!.pulse, false);
  assert.equal(by.moving!.cable.lit, true);
  assert.equal(by.away!.pulse, false);
  assert.equal(by.away!.cable.lit, false);
  // The same on a phone.
  const phone = rolloutScene({ builds: [], server, runners: [runner("idle"), runner("away", { offline: true })] }, true);
  assert.deepEqual(
    phone.runners.map((r) => r.pulse),
    [true, false],
  );
});

test("a runner's cable leaves the server's right corner and ends at its machine", () => {
  const s = rolloutScene({ builds: [], server, runners: [runner("a"), runner("b")] }, false);
  const reach = 99 * 0.866;
  for (const [i, r] of s.runners.entries()) {
    const n = nums(r.cable.d);
    assert.ok(Math.abs(n[0]! - (480 + reach - 2)) < 0.01, "starts at the server's right corner");
    assert.ok(Math.abs(n[n.length - 2]! - 700) < 0.01, "ends at the machine's left side");
    assert.ok(Math.abs(n[n.length - 1]! - (r.name.y + 3 - 4)) < 0.01, `ends level with runner ${i}`);
    assert.ok(r.cable.arrow, "it has a chevron");
  }
});

test("a build's cable runs into the server's left corner, and what's lit stays lit", () => {
  const s = rolloutScene({ builds: [build("a", { lit: true, building: true }), build("b", { cable: "warn" })], server, runners: [] }, false);
  const n = nums(s.builds[0]!.cable.d);
  assert.equal(n[0], 255);
  assert.ok(Math.abs(n[n.length - 2]! - (480 - 99 * 0.866 + 2)) < 0.01);
  assert.equal(s.builds[0]!.cable.lit, true);
  assert.equal(s.builds[0]!.building, true);
  assert.equal(s.builds[1]!.cable.tone, "warn");
  assert.equal(s.builds[1]!.cable.lit, false);
  // No runners: no runners' heading.
  assert.deepEqual(
    s.headings.map((h) => h.text),
    ["BUILDS", "SERVER"],
  );
});

test("many runners: the rows close up, stay apart, and the server stays level with their middle", () => {
  const runners = Array.from({ length: 12 }, (_, i) => runner(`r${i}`));
  const s = rolloutScene({ builds: [build("a")], server, runners }, false);
  const ys = s.runners.map((r) => r.name.y + 3);
  for (let i = 1; i < ys.length; i++) assert.equal(ys[i]! - ys[i - 1]!, 44);
  assert.ok(ys[0]! >= 62, "the first row is under the heading");
  const sy = s.server.name.y - 80;
  assert.ok(Math.abs((ys[0]! + ys[ys.length - 1]!) / 2 - (sy + 20)) < 0.01);
  assert.ok(s.height >= ys[ys.length - 1]! + 48);
});

test("a few runners sit round the server's middle, not at the top", () => {
  const s = rolloutScene({ builds: [], server, runners: [runner("only")] }, false);
  assert.equal(s.runners[0]!.name.y + 3, 146 + 20);
  assert.equal(s.height, 318);
});

test("on a phone: the server at the top, a trunk down and a branch to each runner, no builds", () => {
  const s = rolloutScene({ builds: [build("a")], server, runners: ["1", "2", "3", "4", "5"].map((k) => runner(k)) }, true);
  assert.equal(s.width, 360);
  assert.equal(s.height, 404);
  assert.equal(s.builds.length, 0);
  assert.deepEqual(
    s.runners.map((r) => r.name.y + 3),
    [178, 224, 270, 316, 362],
  );
  for (const r of s.runners) {
    assert.match(r.cable.d, /^M58 96 L58 /, "down the trunk");
    assert.equal(r.cable.arrow, null);
    assert.ok(r.cable.d.endsWith(`L86 ${r.name.y}`), "into its machine");
  }
  assert.deepEqual(
    s.headings.map((h) => h.text),
    ["SERVER", "RUNNERS"],
  );
});

test("long names and lines are cut to fit beside their node", () => {
  const long = "a-runner-with-a-really-quite-long-name-indeed";
  const s = rolloutScene({ builds: [], server, runners: [runner(long, { line: "waits for the windows build of something very long" })] }, false);
  assert.ok(s.runners[0]!.name.text.length <= 24);
  assert.ok(s.runners[0]!.name.text.endsWith("…"));
  assert.ok(s.runners[0]!.sub.text.length <= 30);
  assert.equal(s.runners[0]!.title.startsWith(long), true, "its whole name is still its label");
  assert.equal(clip("short", 10), "short");
  assert.equal(clip("exactly ten", 11), "exactly ten");
});

test("a switching runner draws its new runner on top; an offline one is faint", () => {
  const s = rolloutScene({ builds: [], server, runners: [runner("a", { switching: true }), runner("b", { offline: true, cable: "off" })] }, false);
  assert.equal(s.runners[0]!.switching, true);
  assert.equal(s.runners[1]!.offline, true);
  assert.equal(s.runners[1]!.cable.tone, "off");
  // The new runner's box stands 13 above the old one's.
  assert.notEqual(s.runners[0]!.next.top, s.runners[0]!.box.top);
});

test("iso boxes: faces, top and ridge are closed paths round the right points", () => {
  assert.deepEqual(iso(0, 0, 0, 0, 0), [0, 0]);
  assert.deepEqual(iso(10, 20, 2, 0, 0), [10 + 2 * 0.866, 20 + 1]);
  assert.deepEqual(iso(10, 20, 0, 0, 5), [10, 15]);
  const b = isoBox(100, 100, 24, 16, 9, [0.5]);
  assert.ok(b.top.endsWith("Z") && b.left.endsWith("Z") && b.right.endsWith("Z"));
  assert.equal(b.etches.length, 1);
  assert.ok(b.shadow);
  assert.equal(isoBox(0, 0, 4, 4, 4, [], false).shadow, null);
});

test("ports spread evenly, one in the middle; a point along a curve by its length", () => {
  assert.equal(spread(0, 1, 22), 11);
  assert.deepEqual(
    [0, 1, 2, 3].map((i) => spread(i, 4, 22.5)),
    [0, 7.5, 15, 22.5],
  );
  // A straight cubic: half its length is its middle.
  const mid = alongCubic([0, 0], [10, 0], [20, 0], [30, 0], 0.5);
  assert.ok(Math.abs(mid.p[0] - 15) < 0.05);
  assert.ok(mid.d[0] > 0 && Math.abs(mid.d[1]) < 1e-9);
});
