import assert from "node:assert/strict";
import { test } from "node:test";

const { Holds } = await import("../src/lib/action.ts");

const tick = () => new Promise<void>((resolve) => setImmediate(resolve));

/** A request the test answers when it says. */
function request() {
  let settle: (ok: boolean) => void = () => {};
  const p = new Promise<void>((resolve, reject) => (settle = (ok) => (ok ? resolve() : reject(new Error("refused")))));
  return { p, answer: () => settle(true), refuse: () => settle(false) };
}

await test("W21-R15: one thing's changes go one at a time: its other controls wait, another thing's go", async () => {
  let changes = 0;
  const holds = new Holds(() => changes++);
  const approve = request();
  const asked: string[] = [];
  holds.change("release-1", "approve", () => (asked.push("approve"), approve.p));
  assert.equal(holds.going("release-1"), "approve");
  // Reject on the same release, or Approve again, while Approve is on its way: nothing is asked.
  holds.change("release-1", "reject", () => (asked.push("reject"), request().p));
  holds.change("release-1", "approve", () => (asked.push("approve again"), request().p));
  // Another release is another thing: it goes.
  const other = request();
  holds.change("release-2", "reject", () => (asked.push("reject 2"), other.p));
  assert.deepEqual(asked, ["approve", "reject 2"]);
  assert.equal(holds.going("release-2"), "reject");
  approve.answer();
  await tick();
  assert.equal(holds.going("release-1"), undefined, "answered, it's no longer held");
  assert.equal(holds.going("release-2"), "reject");
  holds.change("release-1", "reject", () => (asked.push("reject after"), request().p));
  assert.deepEqual(asked, ["approve", "reject 2", "reject after"]);
  assert.ok(changes >= 4, "every start and end of a change is heard");
});

await test("W21-R15: a change that fails says so where it's asked to, and lets the thing go", async () => {
  const holds = new Holds(() => {});
  const save = request();
  const said: string[] = [];
  holds.change(
    "app",
    "save",
    () => save.p,
    (e) => said.push(e instanceof Error ? e.message : String(e)),
  );
  save.refuse();
  await tick();
  await tick();
  assert.deepEqual(said, ["refused"]);
  assert.equal(holds.going("app"), undefined);
  // One that throws before it asks anything: said, and nothing is held.
  holds.change(
    "app",
    "save",
    () => {
      throw new Error("no name");
    },
    (e) => said.push(e instanceof Error ? e.message : String(e)),
  );
  assert.deepEqual(said, ["refused", "no name"]);
  assert.equal(holds.going("app"), undefined);
  // One with nothing to wait for (done at once) holds nothing either.
  holds.change("app", "copy", () => undefined);
  assert.equal(holds.going("app"), undefined);
});
