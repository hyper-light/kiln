import assert from "node:assert/strict";
import { test } from "node:test";

// The toast store counts down on the window's clock: a fake one, run by hand.
const pending = new Map<number, () => void>();
let next = 0;
(globalThis as { window?: unknown }).window = {
  setTimeout: (f: () => void) => (pending.set(++next, f), next),
  clearTimeout: (h: number) => pending.delete(h),
  addEventListener: () => {},
  removeEventListener: () => {},
};
const { toast, act, shownToasts } = await import("../src/components/ui/toast.tsx");

test("a toast's action runs once when pressed, and the toast goes", () => {
  let undone = 0;
  const id = toast({ title: "Paused reviewer", tone: "info", action: { label: "Undo", run: () => undone++ } });
  assert.equal(shownToasts().find((t) => t.id === id)?.action?.label, "Undo");
  act(id);
  act(id);
  assert.equal(undone, 1);
  assert.equal(shownToasts().find((t) => t.id === id)?.closing, true);
});

test("a toast without an action does nothing when pressed", () => {
  const id = toast({ title: "Saved" });
  act(id);
  assert.notEqual(shownToasts().find((t) => t.id === id)?.closing, true);
});
