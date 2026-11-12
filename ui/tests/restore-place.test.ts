import assert from "node:assert/strict";
import { test } from "node:test";
import { restoreCanonicalPlace } from "../src/views/harness/restore-place.tsx";
import type { HEvent, SessionHistory } from "../src/api/harness.ts";

const event = (seq: number): HEvent => ({ id: `e${seq}`, seq, at: seq, kind: "assistant", text: `row${seq}` });
function fixture() {
  const events = new Map<number, HEvent>([[100, event(100)]]);
  let history: SessionHistory | null = { cursor: "generation-a", before: 100, hasMore: true, oversized: false, loading: false, error: null };
  let ready = true;
  let current: Promise<void> | null = null;
  let release = () => {};
  let page: (() => void) | null = null;
  let calls = 0;
  let cancelled = 0;
  const store = {
    get: () => ({ ready, history }),
    event: (id: string) => [...events.values()].find((e) => e.id === id),
    loadOlder: () => {
      if (current) return current;
      calls++;
      if (history) history = { ...history, loading: true };
      current = new Promise<void>(
        (resolve) =>
          (release = () => {
            current = null;
            resolve();
          }),
      );
      return current;
    },
    cancelOlder: (owned: Promise<void>) => {
      if (owned === current) {
        cancelled++;
        release();
      }
    },
  } as unknown as Parameters<typeof restoreCanonicalPlace>[0];
  return {
    store,
    events,
    get calls() {
      return calls;
    },
    get cancelled() {
      return cancelled;
    },
    finish: (next: number, more = true) => {
      const before = history?.before ?? 100;
      page = () => {
        for (let i = next; i < before; i++) events.set(i, event(i));
      };
      if (history) history = { ...history, before: next, hasMore: more, loading: false };
      release();
    },
    publish: async () => {
      page?.();
      page = null;
    },
    reset: () => {
      if (history) history = { ...history, cursor: "generation-b", loading: false };
      release();
    },
    logout: () => {
      history = null;
      ready = false;
      release();
    },
  };
}

test("W21-R02 a saved position automatically demands sequential canonical pages and waits for their publication", async () => {
  const f = fixture();
  const progress: number[] = [];
  const result = restoreCanonicalPlace(f.store, { id: "e25", offset: -8, seq: 25, row: true, neighbors: [{ id: "e100" }] }, new AbortController().signal, (n) => progress.push(n), f.publish);
  for (const before of [80, 60, 40, 20]) {
    assert.equal(f.calls, progress.length + 1);
    f.finish(before);
    await new Promise((resolve) => setImmediate(resolve));
  }
  assert.deepEqual(await result, { type: "found", position: { id: "e25", offset: -8, seq: 25, row: true, neighbors: [{ id: "e100" }] } });
  assert.equal(f.calls, 4, "a published target stops before requesting an unnecessary page");
  assert.deepEqual(progress, [1, 2, 3, 4]);
});

test("W21-R02 Latest/navigation aborts only the restore's owned page and prevents another fetch", async () => {
  const f = fixture();
  const controller = new AbortController();
  const result = restoreCanonicalPlace(f.store, { id: "e20", offset: 0 }, controller.signal, () => {}, f.publish);
  controller.abort();
  assert.deepEqual(await result, { type: "cancelled" });
  assert.equal(f.cancelled, 1);
  assert.equal(f.calls, 1);
});

test("W21-R02 reset/logout/no-progress terminate restoration without consuming its saved ID", async () => {
  for (const change of ["reset", "logout", "no-progress"] as const) {
    const f = fixture();
    const position = { id: "e20", offset: 3, row: true as const };
    const result = restoreCanonicalPlace(f.store, position, new AbortController().signal, () => {}, f.publish);
    if (change === "reset") f.reset();
    else if (change === "logout") f.logout();
    else f.finish(100);
    assert.equal((await result).type, "error", change);
    assert.equal(f.calls, 1);
    assert.equal(position.id, "e20");
  }
});

test("W21-R02 saved seq proves a deleted row's range is covered before falling back to its concrete neighbor", async () => {
  const f = fixture();
  f.events.set(99, event(99));
  const result = await restoreCanonicalPlace(f.store, { id: "deleted", seq: 101, offset: -10, row: true, neighbors: [{ id: "e99", row: true }] }, new AbortController().signal, () => {}, f.publish);
  assert.equal(result.type, "found");
  if (result.type === "found") assert.equal(result.position.id, "e99");
  assert.equal(f.calls, 0);
});

test("W21-R02 a published legacy full-snapshot position restores without requiring a history capability", async () => {
  const f = fixture();
  const store = { ...f.store, get: () => ({ ready: true, history: null }) } as unknown as Parameters<typeof restoreCanonicalPlace>[0];
  const position = { id: "e100", offset: 0 };
  assert.deepEqual(await restoreCanonicalPlace(store, position, new AbortController().signal, () => {}, f.publish), { type: "found", position });
  assert.equal(f.calls, 0);
  const beforeReady = { ...store, get: () => ({ ready: false, history: null }) } as unknown as Parameters<typeof restoreCanonicalPlace>[0];
  assert.equal((await restoreCanonicalPlace(beforeReady, position, new AbortController().signal, () => {}, f.publish)).type, "error", "a pre-ready snapshot is never adopted");
});
