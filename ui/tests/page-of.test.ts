import assert from "node:assert/strict";
import { test } from "node:test";

const { enginesOf, heightAt, heightsFor, ownEngine } = await import("../src/views/harness/page-of.ts");

const SAFARI = "Mozilla/5.0 (iPhone; CPU iPhone OS 26_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Mobile/15E148 Safari/604.1";
const CHROME = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36";
const FIREFOX = "Mozilla/5.0 (X11; Linux x86_64; rv:150.0) Gecko/20100101 Firefox/150.0";

test("a browser's own engine, as pages are measured in them", () => {
  assert.equal(ownEngine(SAFARI), "webkit");
  assert.equal(ownEngine(CHROME), "chromium");
  assert.equal(ownEngine(FIREFOX), "firefox");
});

test("a frame takes its own engine's height where one reaches its width, else Chromium's", () => {
  const heights: [number, number][] = [[320, 900], [390, 800], [640, 600], [728, 560], [1000, 400]];
  // A viewer in Safari measured it at 390; its machine's Firefox at every width.
  const page = { heights, engines: { webkit: [[390, 812]] as [number, number][], firefox: heights.map(([w, h]) => [w, h + 10]) as [number, number][] } };
  // At about 390, Safari's own; far from it, Chromium's.
  assert.equal(heightAt(heightsFor(page, SAFARI, 393), 393), 812);
  assert.equal(heightAt(heightsFor(page, SAFARI, 700), 700), 600);
  // Firefox's reach every width; Chromium's own are Chromium's.
  assert.equal(heightAt(heightsFor(page, FIREFOX, 700), 700), 610);
  assert.equal(heightAt(heightsFor(page, CHROME, 393), 393), 800);
  // Two of Safari's around it, near enough: theirs.
  const two = { heights, engines: { webkit: [[640, 610], [728, 570]] as [number, number][] } };
  assert.equal(heightAt(heightsFor(two, SAFARI, 700), 700), 610);
  // Nothing of its own: Chromium's.
  assert.equal(heightAt(heightsFor({ heights }, SAFARI, 393), 393), 800);
});

test("heights as kiln answers them, and none that aren't", () => {
  assert.deepEqual(enginesOf({ webkit: [[390, 812], [0, 4], "x", [1, 2, 3]], firefox: "no" }), { webkit: [[390, 812]], firefox: [] });
  assert.deepEqual(enginesOf(null), {});
});
