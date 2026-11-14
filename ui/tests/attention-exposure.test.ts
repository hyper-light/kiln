/** Exact rendered feedback, plan 01a199e1b177752e84dad5dc990e6a15: noted as the list renders, sent with what you do. */
import assert from "node:assert/strict";
import { test } from "node:test";
import { AttentionPresentation, displayedPair, type AttentionPair, type AttentionShown } from "../src/lib/attention-exposure.ts";
import { AttentionControls, AttentionOrder, keepSame } from "../src/lib/attention-order.ts";
import { sameJson } from "../src/lib/utils.ts";
import { onOwnAgentAction, ownAgentAction } from "../src/lib/own-agent-action.ts";
const pair: AttentionPair = { key: "first", order_a: ["a", "fixed", "b", "hidden"], order_b: ["b", "hidden", "a", "fixed"], arm: 0, model_order: true };
const shown = ["fixed", "a", "b"];
const eligible = new Set(["a", "b"]);
const input = (more: Partial<AttentionShown> = {}): AttentionShown => ({ shown, eligible, context: "mission/desktop", controls: "all", automatic: true, allowed: true, ...more });

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
});

test("mc_attention: rendering only notes what shows; a frame goes with an action, once per change", () => {
  const presentation = new AttentionPresentation();
  presentation.note(pair, input());
  presentation.note(pair, input());
  const first = presentation.frame();
  assert.equal(first?.frame_revision, 1);
  assert.deepEqual(first?.shown, shown);
  assert.equal(presentation.frame(), undefined, "nothing new shows: nothing to send");
  presentation.note(pair, input({ shown: ["a", "fixed", "b"] }));
  assert.equal(presentation.frame()?.frame_revision, 2);
});

test("mc_attention: leaving without a pick closes the last frame shown, sent with it if it never was, once", () => {
  const presentation = new AttentionPresentation();
  presentation.note(pair, input());
  const left = presentation.close("close-key");
  assert.equal(left.frame?.frame_revision, 1, "the frame shown goes with its close");
  assert.deepEqual(left.close, { exposure: "first", frame_revision: 1, key: "close-key" });
  assert.deepEqual(presentation.close("again"), {}, "a visit closes once");
  const sentBefore = new AttentionPresentation();
  sentBefore.note(pair, input());
  sentBefore.frame();
  const closed = sentBefore.close("k");
  assert.equal(closed.frame, undefined, "already sent: only its close");
  assert.equal(closed.close?.frame_revision, 1);
  assert.deepEqual(new AttentionPresentation().close("k"), {}, "nothing shown: nothing to close");
});

test("mc_attention: a pick from the list sends the frame it was picked from and its position, and ends the visit", () => {
  const presentation = new AttentionPresentation();
  presentation.note(pair, input());
  const picked = presentation.choose("b");
  assert.equal(picked.frame?.frame_revision, 1);
  assert.deepEqual(picked.selection, { exposure: "first", frame_revision: 1, chosen_position: 2 });
  assert.deepEqual(presentation.close("k"), {}, "the pick is the close");
  const unknown = new AttentionPresentation();
  unknown.note(pair, input());
  assert.equal(unknown.choose("elsewhere").selection, undefined, "a row it didn't show isn't a pick from it");
});

test("mc_attention: changed filters or layout cannot reuse the earlier random assignment", () => {
  const presentation = new AttentionPresentation();
  presentation.note(pair, input());
  presentation.note(pair, input({ controls: "filtered" }));
  presentation.note(pair, input({ controls: "all" }));
  assert.equal(presentation.frame()?.model_order, false, "returning controls does not remove selection confounding");
  presentation.note({ ...pair, key: "fresh" }, input({ context: "mission/phone" }));
  const fresh = presentation.frame();
  assert.equal(fresh?.model_order, true);
  assert.equal(fresh?.context, "mission/phone");
  assert.equal(fresh?.exposure, "fresh");
  assert.equal(fresh?.frame_revision, 1, "a new visit counts its own frames");
});

test("mc_attention: human commands say the rows they act on as pressed, and once done; failed ones alone move nothing", async () => {
  const started: string[][] = [];
  const done: string[][] = [];
  const stop = onOwnAgentAction(
    (ids) => done.push([...ids]),
    (ids) => started.push([...ids]),
  );
  let finish: (() => void) | undefined;
  const first = ownAgentAction(
    new Promise<void>((resolve) => {
      finish = resolve;
    }),
    ["a"],
  );
  const second = ownAgentAction(Promise.resolve({ results: [{ id: "c" }] }), ["b"], (r) => r.results.map((x) => x.id));
  assert.deepEqual(started, [["a"], ["b"]], "each row is told as it's pressed");
  await second;
  assert.equal(done.length, 0, "concurrent recipients form one completed action");
  finish?.();
  await first;
  assert.deepEqual(done, [["a", "b", "c"]]);
  await assert.rejects(ownAgentAction(Promise.reject(new Error("rejected")), ["d"]));
  assert.equal(done.length, 1);
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

test("W26-R15 what the list shows is projected in steps that grow with the list, not with its square: twice the agents, no more than twice the reads", () => {
  /** How many times the lists' ids are read to project a list of `n` agents (each list counts its own reads). */
  const reads = (n: number) => {
    let count = 0;
    const counted = (ids: string[]) =>
      new Proxy(ids, {
        get(target, key, receiver) {
          if (typeof key === "string" && /^\d+$/.test(key)) count++;
          return Reflect.get(target, key, receiver);
        },
      });
    const ids = Array.from({ length: n }, (_, i) => `agent-${i}`);
    const drawn: AttentionPair = { key: "many", order_a: counted([...ids]), order_b: counted([...ids].reverse()), arm: 0, model_order: true };
    const projected = displayedPair(drawn, counted([...ids]), new Set(ids), true);
    assert.equal(projected.consistent, true);
    assert.deepEqual(projected.order_b, [...ids].reverse());
    return count;
  };
  // Each id read a few times whatever the list's length: twice the agents, no more than twice the reads.
  const few = reads(200);
  const twice = reads(400);
  assert.ok(twice <= 2 * few, `twice the agents took ${twice} reads, ${(twice / few).toFixed(1)} times the ${few} of half as many`);
});

test("W26-R15 a snapshot read again keeps each mark and the settings that say the same as the objects they were", () => {
  const snapshot = () => ({
    order: ["a", "b", "c"],
    settings: { sort: "learned", park_later_minutes: 60 },
    agents: {
      a: { revision: 1, pinned: 2, urgency: { score: 0.5, stale: false, reason: "soon" } },
      b: { revision: 0, deferred_requests: [{ agent: "b", kind: "approval", id: "q1", at: 5 }] },
      c: { revision: 3 },
    } as Record<string, object>,
  });
  const was = snapshot();
  const next = { ...snapshot(), order: ["c", "a", "b"] };
  next.agents.c = { revision: 4, parked_until: 9 };
  next.agents.d = { revision: 0 };
  delete next.agents.b;
  const kept = keepSame(was, next);
  // Parsed anew, and yet the marks that didn't change are the ones the rows already have.
  assert.notEqual(next.agents.a, was.agents.a);
  assert.equal(kept.agents.a, was.agents.a);
  assert.equal(kept.settings, was.settings);
  assert.deepEqual(kept.agents.c, { revision: 4, parked_until: 9 });
  assert.deepEqual(Object.keys(kept.agents), ["a", "c", "d"]);
  assert.deepEqual(kept.order, ["c", "a", "b"]);
  assert.equal(keepSame(undefined, next), next);
  // Nothing changed at all: the marks are the very object they were (what's made from all of them isn't made again).
  assert.equal(keepSame(was, snapshot()).agents, was.agents);
  assert.notEqual(kept.agents, was.agents);
  assert.notEqual(keepSame(was, { ...next, settings: { sort: "yours", park_later_minutes: 60 } }).settings, was.settings);
  assert.equal(sameJson({ a: [1, { b: null }], c: "x" }, { c: "x", a: [1, { b: null }] }), true);
  assert.equal(sameJson({ a: [1, 2] }, { a: [2, 1] }), false);
  assert.equal(sameJson({ a: 1 }, { a: 1, b: undefined }), false);
  assert.equal(sameJson([], {}), false);
});
