import assert from "node:assert/strict";
import { test } from "node:test";
import { attachOrder } from "../src/views/terminal/attach-order.ts";

/** Everything queued for after this task has run. */
const settled = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

/** An order, and what attached in it, in the order they did. */
function order() {
  const went: string[] = [];
  const o = attachOrder();
  const inView = new Set<string>();
  const ask = (id: string) =>
    o.ask(
      id,
      () => inView.has(id),
      () => went.push(id),
    );
  return { o, went, inView, ask };
}

test("W21-R02 the terminal in view attaches at once, and the others after it, one at a time in the order they asked", async () => {
  const { o, went, inView, ask } = order();
  inView.add("c");
  // Tabs made together ask together, in their order: the one in view is the third.
  ask("a");
  ask("b");
  ask("c");
  ask("d");
  assert.deepEqual(went, ["c"], "the one in view doesn't wait, and none goes before it");
  await settled();
  assert.deepEqual(went, ["c"], "the others wait while it attaches");
  o.done("c");
  await settled();
  assert.deepEqual(went, ["c", "a"]);
  o.done("a");
  await settled();
  o.done("b");
  await settled();
  assert.deepEqual(went, ["c", "a", "b", "d"]);
  // A turn that's over twice, or one that never was, lets no one through early.
  o.done("b");
  o.done("nobody");
  await settled();
  assert.deepEqual(went, ["c", "a", "b", "d"]);
});

test("W21-R02 with none in view the first to ask attaches, then the rest in turn", async () => {
  const { o, went, ask } = order();
  ask("a");
  ask("b");
  assert.deepEqual(went, [], "it waits for the asks made with it");
  await settled();
  assert.deepEqual(went, ["a"]);
  o.done("a");
  await settled();
  assert.deepEqual(went, ["a", "b"]);
});

test("W21-R02 a terminal that comes into view attaches now, whatever else is attaching", async () => {
  const { o, went, inView, ask } = order();
  inView.add("a");
  ask("a");
  ask("b");
  ask("c");
  await settled();
  assert.deepEqual(went, ["a"]);
  // Its tab is picked while the first is still attaching.
  inView.delete("a");
  inView.add("c");
  o.shown("c");
  assert.deepEqual(went, ["a", "c"]);
  // It took no one's place: the one still waiting goes once both are done, and shown again it doesn't attach twice.
  o.shown("c");
  o.done("a");
  await settled();
  assert.deepEqual(went, ["a", "c"], "one is still attaching");
  o.done("c");
  await settled();
  assert.deepEqual(went, ["a", "c", "b"]);
});

test("W21-R02 a terminal that leaves gives up its place, or its turn", async () => {
  const { o, went, inView, ask } = order();
  inView.add("a");
  const leaveA = ask("a");
  const leaveB = ask("b");
  ask("c");
  // One waiting that leaves (its tab closed) never attaches.
  leaveB();
  // One attaching that leaves (its view gone before its replay came) frees its turn.
  leaveA();
  await settled();
  assert.deepEqual(went, ["a", "c"]);
  // Asking again (its socket dropped: it attaches again) while it waits keeps one place, at the back.
  ask("d");
  ask("e");
  ask("d");
  o.done("c");
  await settled();
  o.done("e");
  await settled();
  o.done("d");
  await settled();
  assert.deepEqual(went, ["a", "c", "e", "d"]);
});
