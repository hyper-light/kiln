/** Exact rendered feedback, plan 01a199e1b177752e84dad5dc990e6a15. */
import assert from "node:assert/strict";
import { test } from "node:test";
import { AttentionPresentation, displayedPair, type AttentionFrame, type AttentionClose, type AttentionPair } from "../src/lib/attention-exposure.ts";
import { AttentionControls, AttentionOrder } from "../src/lib/attention-order.ts";
import { onOwnAgentAction, ownAgentAction } from "../src/lib/own-agent-action.ts";
const pair: AttentionPair = { key: "first", order_a: ["a", "fixed", "b", "hidden"], order_b: ["b", "hidden", "a", "fixed"], arm: 0, model_order: true };
const shown = ["fixed", "a", "b"];
const eligible = new Set(["a", "b"]);
function rig() {
  const frames: AttentionFrame[] = [];
  const closes: AttentionClose[] = [];
  const presentation = new AttentionPresentation({ render: (f) => frames.push(f), close: (c) => closes.push(c), key: () => "close-key" });
  const render = (options = {}) => presentation.render({ pair, shown, eligible, context: "mission/desktop", controls: "all", automatic: true, allowed: true, ...options });
  return { frames, closes, presentation, render };
}
test("mc_attention: actual constrained DOM slots and filtered counterfactual retain fixed rows", () => {
  const projected = displayedPair(pair, shown, eligible, true);
  assert.deepEqual(projected.order_a, shown);
  assert.deepEqual(projected.order_b, ["fixed", "b", "a"]);
  assert.equal(projected.model_order, true);
  const otherArm = displayedPair({ ...pair, arm: 1 }, ["fixed", "b", "a"], eligible, true);
  assert.deepEqual(otherArm.order_a, shown);
  assert.deepEqual(otherArm.order_b, ["fixed", "b", "a"]);
  assert.equal(otherArm.model_order, true);
  assert.equal(displayedPair(pair, ["fixed", "b", "a"], eligible, true).model_order, false, "held order must agree with selected arm");
  assert.equal(displayedPair(pair, shown, eligible, false).model_order, false, "manual order is not randomized");
});
test("mc_attention: frozen refresh preserves both proposals, exposure and selected arm", () => {
  const order = new AttentionOrder();
  order.snapshot({ order: pair.order_a, exposure: pair.key, pair });
  const nextPair = { ...pair, key: "later", arm: 1 as const };
  const held = order.snapshot({ order: nextPair.order_b, exposure: nextPair.key, pair: nextPair });
  assert.equal(held.pair, pair);
  assert.equal(held.exposure, "first");
  assert.deepEqual(held.order, pair.order_a);
  assert.equal(order.snapshot({ order: nextPair.order_b, exposure: nextPair.key, pair: nextPair }, true).pair, nextPair);
});
test("mc_attention: rerenders stay open, no-click departure closes exact final revision once", () => {
  const { frames, closes, presentation, render } = rig();
  render();
  render();
  assert.equal(frames.length, 1);
  assert.equal(closes.length, 0, "an open presentation is censored");
  render({ shown: ["a", "fixed", "b"] });
  assert.equal(frames.at(-1)?.frame_revision, 2);
  presentation.close();
  presentation.close();
  assert.deepEqual(closes, [{ exposure: "first", frame_revision: 2, key: "close-key" }]);
});
test("mc_attention: list choice records actual position and blocks no-click cleanup", () => {
  const { closes, presentation, render } = rig();
  render();
  assert.deepEqual(presentation.choose("b", shown), { exposure: "first", frame_revision: 1, chosen_position: 2 });
  presentation.close();
  assert.equal(closes.length, 0, "selected navigation is the close");
  const mismatched = rig();
  mismatched.render();
  assert.equal(mismatched.presentation.choose("b", ["b", "a", "fixed"]), undefined);
  assert.equal(mismatched.frames.at(-1)?.model_order, false);
  assert.equal(mismatched.closes.length, 1, "unknown displayed order is closed unlearnable");
});
test("mc_attention: changed filters or layout cannot reuse the earlier random assignment", () => {
  const { frames, closes, render } = rig();
  render();
  render({ controls: "filtered" });
  render({ controls: "all" });
  assert.equal(frames.at(-1)?.model_order, false, "returning controls does not remove selection confounding");
  render({ pair: { ...pair, key: "fresh" }, controls: "all", context: "mission/phone" });
  assert.equal(frames.at(-1)?.model_order, true);
  assert.equal(frames.at(-1)?.context, "mission/phone");
  assert.equal(closes.length, 1);
});
test("mc_attention: human command completion coalesces concurrent recipients but failed commands do not redraw", async () => {
  let updates = 0;
  const stop = onOwnAgentAction(() => {
    updates++;
  });
  let finish: (() => void) | undefined;
  const first = ownAgentAction(
    new Promise<void>((resolve) => {
      finish = resolve;
    }),
  );
  const second = ownAgentAction(Promise.resolve());
  await second;
  assert.equal(updates, 0);
  finish?.();
  await first;
  assert.equal(updates, 1);
  await assert.rejects(ownAgentAction(Promise.reject(new Error("rejected"))));
  assert.equal(updates, 1);
  stop();
});

test("mc_attention: preload before controls binds the first presentation without treating it as a filter change", () => {
  for (const responseFirst of [true, false]) {
    const controls = new AttentionControls();
    const atRequest = controls.value;
    if (responseFirst) controls.sampled(atRequest);
    controls.bind("all");
    if (!responseFirst) controls.sampled(atRequest);
    assert.equal(controls.allowed, true);
    controls.bind("filtered");
    assert.equal(controls.allowed, false);
    controls.bind("all");
    assert.equal(controls.allowed, false, "returning controls cannot remove conditioning on a seen order");
    controls.sampled(controls.value);
    assert.equal(controls.allowed, true, "a fresh assignment uses the current controls");
    const requested = controls.value;
    controls.bind("filtered");
    controls.sampled(requested);
    assert.equal(controls.allowed, false, "a control change while a fresh request is pending stays conservative");
  }
});
