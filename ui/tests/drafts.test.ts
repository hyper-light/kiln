import assert from "node:assert/strict";
import { test } from "node:test";

const values = new Map<string, string>();
const timers = new Map<number, () => void>();
let nextTimer = 0;
let unavailable = false;
const storage = {
  getItem: (key: string) => values.get(key) ?? null,
  setItem: (key: string, value: string) => {
    if (unavailable) throw new Error("Storage unavailable");
    values.set(key, value);
  },
  removeItem: (key: string) => values.delete(key),
};
const browser = Object.assign(new EventTarget(), {
  setTimeout: (fn: () => void) => {
    const id = ++nextTimer;
    timers.set(id, fn);
    return id;
  },
  clearTimeout: (id: number) => timers.delete(id),
});
Object.assign(globalThis, { window: browser, localStorage: storage, sessionStorage: storage });
const { useDrafts } = await import("../src/views/harness/state.ts");
const saved = () => JSON.parse(values.get("kiln.agents.drafts") ?? "null");
const flush = () => {
  const queued = [...timers.values()];
  timers.clear();
  queued.forEach((fn) => fn());
};

await test("rapid draft edits serialize once and the latest draft survives flush and reload", async (t) => {
  const stringify = t.mock.method(JSON, "stringify");
  const other = "another agent's large draft ".repeat(10_000);
  useDrafts.getState().setDraft("other", other);
  for (let i = 0; i < 100; i++) useDrafts.getState().setDraft("typing", `edit ${i}`);
  assert.equal(stringify.mock.calls.length, 0, "typing must not serialize any draft");
  assert.equal(timers.size, 1);
  assert.equal(saved(), null);

  await useDrafts.persist.rehydrate();
  assert.equal(useDrafts.getState().drafts.typing, "edit 99", "pending reads keep the latest text");
  flush();
  assert.equal(stringify.mock.calls.length, 1);
  assert.deepEqual(saved().state.drafts, { other, typing: "edit 99" });
  const reloaded = await import("../src/views/harness/state.ts?reload");
  assert.deepEqual(reloaded.useDrafts.getState().drafts, { other, typing: "edit 99" });
});

await test("pagehide flushes the latest edit and clearing storage cancels a pending save", () => {
  useDrafts.getState().setDraft("typing", "before leaving");
  browser.dispatchEvent(new Event("pagehide"));
  assert.equal(saved().state.drafts.typing, "before leaving");
  assert.equal(timers.size, 0);

  useDrafts.getState().setDraft("typing", "must not be saved");
  useDrafts.persist.clearStorage();
  flush();
  assert.equal(saved(), null);
});

await test("a storage failure keeps drafts in memory and does not prevent later saves", () => {
  unavailable = true;
  try {
    useDrafts.getState().setDraft("typing", "kept in memory");
    assert.doesNotThrow(flush);
    assert.equal(useDrafts.getState().drafts.typing, "kept in memory");
  } finally {
    unavailable = false;
  }
  useDrafts.getState().setDraft("typing", "saved afterward");
  flush();
  assert.equal(saved().state.drafts.typing, "saved afterward");
});
