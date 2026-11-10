import assert from "node:assert/strict";
import { test } from "node:test";
import { SessionPrefetcher } from "../src/lib/session-prefetch.ts";

class Clock {
  value = 0;
  next = 0;
  timers = new Map<number, { at: number; fn: () => void }>();
  now = () => this.value;
  set = (fn: () => void, ms: number): ReturnType<typeof setTimeout> => {
    const id = ++this.next;
    this.timers.set(id, { at: this.value + ms, fn });
    return id as unknown as ReturnType<typeof setTimeout>;
  };
  clear = (id: ReturnType<typeof setTimeout>) => {
    this.timers.delete(id as unknown as number);
  };
  advance(ms: number) {
    const end = this.value + ms;
    for (;;) {
      const next = [...this.timers].filter(([, t]) => t.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break;
      this.value = next[1].at;
      this.timers.delete(next[0]);
      next[1].fn();
    }
    this.value = end;
  }
}
function fixture() {
  const time = new Clock();
  const reads: { id: string; budget: number; finish: (ready: boolean) => void; cancels: number }[] = [];
  let allowed = true;
  const prefetch = new SessionPrefetcher(
    (id, budget) => {
      let finish = (_ready: boolean) => {};
      const ready = new Promise<boolean>((resolve) => {
        finish = resolve;
      });
      const read = { id, budget, finish, cancels: 0 };
      reads.push(read);
      return {
        ready,
        cancel() {
          read.cancels++;
          finish(false);
        },
      };
    },
    () => allowed,
    time,
  );
  return {
    time,
    reads,
    prefetch,
    block: () => {
      allowed = false;
    },
  };
}
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));

await test("W21-R02: route code shares intent dwell, ownership, queue and network policy without delaying data", async () => {
  const { time, reads, prefetch, block } = fixture();
  const prepared: string[] = [];
  const prepare = (id: string) => () => {
    prepared.push(id);
    return new Promise<void>(() => {}); // Import completion is never a data/readiness barrier.
  };
  prefetch.intent("cancelled", false, prepare("cancelled"))();
  time.advance(100);
  assert.deepEqual(prepared, []);
  prefetch.intent("a", false, prepare("a"));
  time.advance(49);
  assert.deepEqual(prepared, []);
  time.advance(1);
  assert.deepEqual(prepared, ["a"]);
  assert.equal(reads.length, 1);
  const release = prefetch.intent("a", false, prepare("duplicate"));
  prefetch.intent("discarded", true, prepare("discarded"));
  prefetch.intent("latest", true, prepare("latest"));
  assert.deepEqual(prepared, ["a"]);
  reads[0]!.finish(true);
  await tick();
  release();
  assert.deepEqual(prepared, ["a", "latest"]);
  assert.equal(reads.length, 2);
  block();
  prefetch.clear();
  prefetch.intent("offline", true, prepare("offline"));
  time.advance(100);
  assert.deepEqual(prepared, ["a", "latest"]);
  await tick();
});

await test("W21-R02: failed speculative code imports leave data loading and foreground retry available", async () => {
  const { reads, prefetch } = fixture();
  prefetch.intent("throw", true, () => {
    throw new Error("chunk unavailable");
  });
  assert.equal(reads.length, 1);
  reads[0]!.finish(false);
  await tick();
  prefetch.intent("reject", true, () => Promise.reject(new Error("chunk network failure")));
  assert.equal(reads.length, 2);
  await tick();
  prefetch.clear();
});

await test("W21-R02: intent waits for dwell, deduplicates owners, and keeps only the latest queued session", async () => {
  const { time, reads, prefetch } = fixture();
  const a = prefetch.intent("a");
  time.advance(49);
  assert.equal(reads.length, 0);
  time.advance(1);
  assert.deepEqual(
    reads.map((r) => r.id),
    ["a"],
  );
  const same = prefetch.intent("a");
  a();
  assert.equal(reads[0]!.cancels, 0, "another owner still wants this session");
  prefetch.intent("b");
  prefetch.intent("c");
  time.advance(50);
  assert.equal(reads.length, 1, "one socket in flight");
  same();
  assert.equal(reads[0]!.cancels, 1);
  assert.deepEqual(
    reads.map((r) => r.id),
    ["a", "c"],
  );
  assert.equal(reads[1]!.budget, 512 * 1024);
  prefetch.clear();
  await tick();
  assert.equal(time.timers.size, 0);
});

await test("W21-R02: cancelled dwell, failed reads, and deadlines cannot start a discarded intent", async () => {
  const { time, reads, prefetch } = fixture();
  prefetch.intent("never")();
  time.advance(100);
  assert.equal(reads.length, 0);
  prefetch.intent("timeout", true);
  prefetch.intent("next", true);
  time.advance(1999);
  assert.equal(reads.length, 1);
  time.advance(1);
  assert.deepEqual(
    reads.map((r) => r.id),
    ["timeout", "next"],
  );
  assert.equal(reads[0]!.cancels, 1);
  reads[0]!.finish(true); // A late completion cannot become a retained cache entry.
  reads[1]!.finish(false);
  await tick();
  prefetch.intent("timeout", true);
  assert.equal(reads.length, 3);
  prefetch.clear();
  await tick();
  assert.equal(time.timers.size, 0);
});

await test("W21-R02: only two completed speculative snapshots survive, each expiring after fifteen seconds", async () => {
  const { time, reads, prefetch } = fixture();
  for (const id of ["a", "b", "c"]) {
    const release = prefetch.intent(id, true);
    reads.at(-1)!.finish(true);
    await tick();
    release();
    time.advance(100);
  }
  assert.equal(reads[0]!.cancels, 1, "the oldest unused snapshot was released");
  assert.equal(reads[1]!.cancels, 0);
  prefetch.intent("c", true);
  assert.equal(reads.length, 3, "a retained snapshot is reused");
  time.advance(15_000);
  assert.equal(reads[1]!.cancels, 1);
  assert.equal(reads[2]!.cancels, 1);
  assert.equal(time.timers.size, 0);
});

await test("W21-R02: auth/background invalidation drops retained, in-flight, and queued speculation", async () => {
  const { time, reads, prefetch, block } = fixture();
  prefetch.intent("kept", true);
  reads[0]!.finish(true);
  await tick();
  prefetch.intent("flight", true);
  prefetch.intent("queued", true);
  block();
  prefetch.clear();
  reads[1]!.finish(true);
  time.advance(30_000);
  await tick();
  assert.equal(reads.length, 2);
  assert.deepEqual(
    reads.map((r) => r.cancels),
    [1, 1],
  );
  prefetch.intent("signed-out", true);
  time.advance(100);
  assert.equal(reads.length, 2);
  assert.equal(time.timers.size, 0);
});

await test("W21-R02: the default clock preserves browser timer receiver semantics during intent cleanup", () => {
  const original = globalThis.clearTimeout;
  globalThis.clearTimeout = function (this: unknown, timer: ReturnType<typeof setTimeout>) {
    assert.ok(this === undefined || this === globalThis, "browser clearTimeout cannot receive a Clock object as this");
    original(timer);
  } as typeof clearTimeout;
  const prefetch = new SessionPrefetcher(
    () => {
      throw new Error("a cancelled dwell must not read");
    },
    () => true,
  );
  try {
    prefetch.intent("cancel-before-dwell")();
    prefetch.clear();
  } finally {
    globalThis.clearTimeout = original;
  }
});
