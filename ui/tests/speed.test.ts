import assert from "node:assert/strict";
import { test } from "node:test";
import type { ControlOption } from "../src/api/harness.ts";
import { boltLabel, boltState, lit, nextSpeed, speedName } from "../src/views/harness/speed.ts";

const fast: ControlOption[] = [{ id: "fast", label: "Fast", description: "Up to 2.5x faster" }];
const tiers: ControlOption[] = [
  { id: "priority", label: "Fast", description: "2x speed, increased usage" },
  { id: "ultrafast", label: "Ultrafast", description: "The fastest" },
];

test("a click goes to the model's next speed, and from its fastest back to standard", () => {
  assert.deepEqual([nextSpeed(fast, ""), nextSpeed(fast, "fast")], ["fast", ""]);
  assert.deepEqual([nextSpeed(tiers, ""), nextSpeed(tiers, "priority"), nextSpeed(tiers, "ultrafast")], ["priority", "ultrafast", ""]);
  // A speed the model doesn't list (another model's, a slower tier): standard next.
  assert.equal(nextSpeed(tiers, "flex"), "");
});

test("off, on, two bolts for the next speed up, cooling down, and an account that can't", () => {
  assert.equal(boltState({ speeds: tiers, speed: "" }), "off");
  assert.equal(boltState({ speeds: tiers, speed: "priority" }), "on");
  assert.deepEqual([lit(tiers, ""), lit(tiers, "priority"), lit(tiers, "ultrafast"), lit(tiers, "flex")], [0, 1, 2, 0]);
  assert.equal(boltState({ speeds: fast, speed: "fast", cooldown: true }), "cooldown");
  // Turned off while it cooled down: off.
  assert.equal(boltState({ speeds: fast, speed: "", cooldown: true }), "off");
  assert.equal(boltState({ speeds: fast, speed: "fast", blocked: "Fast mode requires usage credits" }), "blocked");
  assert.equal(boltState({ speeds: fast, speed: "", blocked: "Fast mode requires usage credits" }), "blocked");
});

test("the bolt says what it is and what a click does", () => {
  assert.equal(boltLabel({ speeds: tiers, speed: "" }), "Speed: Standard. Click for Fast: 2x speed, increased usage");
  assert.equal(boltLabel({ speeds: tiers, speed: "priority" }), "Speed: Fast (2x speed, increased usage). Click for Ultrafast");
  assert.equal(boltLabel({ speeds: tiers, speed: "ultrafast" }), "Speed: Ultrafast (The fastest). Click for Standard");
  assert.equal(boltLabel({ speeds: fast, speed: "fast", cooldown: true }), "Fast, cooling down: standard speed until its rate limit resets, then fast again by itself. Click for Standard");
  assert.equal(boltLabel({ speeds: fast, speed: "fast", blocked: "Fast mode requires usage credits" }), "Fast mode requires usage credits");
  assert.deepEqual([speedName(tiers, ""), speedName(tiers, "ultrafast"), speedName([], "turbo")], ["Standard", "Ultrafast", "Turbo"]);
});
