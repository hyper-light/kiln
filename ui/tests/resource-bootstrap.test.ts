import assert from "node:assert/strict";
import { test } from "node:test";
import { createElement } from "react";
import { renderToString } from "react-dom/server";

Object.assign(globalThis, {
  window: Object.assign(new EventTarget(), {
    setTimeout,
    clearTimeout,
    setInterval,
    clearInterval,
  }),
  document: new EventTarget(),
});
const { seedResource, loaded, useSuspenseResource, acquireResource, resourceData, resourceCohort, resourceRecord, invalidate, setData, change } = await import("../src/lib/resource.ts");
const { setAuthed } = await import("../src/api/client.ts");

function ResourceValue({ resourceKey }: { resourceKey: string }) {
  return String(useSuspenseResource(resourceKey, async () => "unexpected").data);
}
const read = (key: string) => renderToString(createElement(ResourceValue, { resourceKey: key }));

await test("W21-R02: a bootstrap seeds only an unread key, never a newer value or pending fetch", async () => {
  let fetched = 0;
  const fetcher = async () => {
    fetched++;
    return "live";
  };
  assert.equal(seedResource("boot/seed", "server", fetcher), true);
  await loaded("boot/seed", fetcher);
  assert.equal(fetched, 0);
  assert.equal(read("boot/seed"), "server");
  assert.equal(seedResource("boot/seed", "older server", fetcher), false);
  assert.equal(read("boot/seed"), "server");
  let finish = (_value: string) => {};
  const pending = loaded(
    "boot/pending",
    () =>
      new Promise<string>((resolve) => {
        finish = resolve;
      }),
  );
  assert.equal(seedResource("boot/pending", "old snapshot", fetcher), false);
  finish("new snapshot");
  await pending;
  assert.equal(read("boot/pending"), "new snapshot");
});

await test("W21-R02: logout removes seeded data and late requests cannot populate the next login", async () => {
  setAuthed(true);
  assert.equal(
    seedResource("boot/private", "old private data", async () => "unused"),
    true,
  );
  let finish = (_value: string) => {};
  let oldSignal: AbortSignal | undefined;
  const old = loaded("boot/late", (signal) => {
    oldSignal = signal;
    return new Promise<string>((resolve) => {
      finish = resolve;
    });
  });
  setAuthed(false);
  assert.equal(oldSignal?.aborted, true);
  finish("old private response");
  await old;
  setAuthed(true);
  await loaded("boot/private", async () => "new person");
  await loaded("boot/late", async () => "new response");
  assert.equal(read("boot/private"), "new person");
  assert.equal(read("boot/late"), "new response");
});

function deferred<T>() {
  let resolve = (_value: T) => {};
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
const tick = () => new Promise<void>((resolve) => setImmediate(resolve));

await test("W21-R02: a canceled preparation cannot cancel a shared canonical read or publish after replacement", async () => {
  const a = new AbortController();
  const b = new AbortController();
  const waiting = deferred<string>();
  let signal: AbortSignal | undefined;
  const source = {
    key: "navigation/shared",
    fetch: (readSignal: AbortSignal) => {
      signal = readSignal;
      return waiting.promise;
    },
  };
  const first = acquireResource(source, a.signal);
  const second = acquireResource(source, b.signal);
  const canceled = assert.rejects(first, { name: "AbortError" });
  a.abort();
  await canceled;
  assert.equal(signal?.aborted, false);
  waiting.resolve("shared result");
  assert.equal(await second, "shared result");
  assert.equal(resourceData(source), "shared result");

  const old = deferred<string>();
  const next = deferred<string>();
  const cancel = new AbortController();
  let oldSignal: AbortSignal | undefined;
  const oldSource = {
    key: "navigation/replaced",
    fetch: (readSignal: AbortSignal) => {
      oldSignal = readSignal;
      return old.promise;
    },
  };
  const before = acquireResource(oldSource, cancel.signal);
  const rejected = assert.rejects(before, { name: "AbortError" });
  cancel.abort();
  await rejected;
  assert.equal(oldSignal?.aborted, true);
  const replacement = acquireResource({ ...oldSource, fetch: () => next.promise });
  old.resolve("late canceled result");
  await tick();
  assert.equal(resourceData(oldSource), undefined);
  next.resolve("new result");
  assert.equal(await replacement, "new result");
  assert.equal(read(oldSource.key), "new result", "the canceled first-read promise is not reused");
});

await test("W21-R02: a view's existing read survives an unrelated navigation cancellation", async () => {
  const waiting = deferred<string>();
  let signal: AbortSignal | undefined;
  const source = {
    key: "navigation/view-owned",
    fetch: (readSignal: AbortSignal) => {
      signal = readSignal;
      return waiting.promise;
    },
  };
  const view = loaded(source.key, source.fetch);
  const cancel = new AbortController();
  const preparation = acquireResource(source, cancel.signal);
  const rejected = assert.rejects(preparation, { name: "AbortError" });
  cancel.abort();
  await rejected;
  assert.equal(signal?.aborted, false);
  waiting.resolve("view result");
  await view;
  assert.equal(resourceData(source), "view result");
});

await test("W21-R02: related records retry a crossed canonical generation and publish only a coherent cohort", async () => {
  const initial = deferred<ReturnType<typeof resourceCohort<number>>>();
  const reread = deferred<ReturnType<typeof resourceCohort<number>>>();
  const newerResearch = deferred<number>();
  const research = { key: "cohort/research", fetch: () => newerResearch.promise };
  let requests = 0;
  const plan = { key: "cohort/plan", fetch: () => (++requests === 1 ? initial.promise : reread.promise) };
  seedResource(research.key, 1, research.fetch);
  const loading = acquireResource(plan);
  let ordinaryReads = 0;
  const shared = loaded(plan.key, async () => {
    ordinaryReads++;
    return 99;
  });
  invalidate(research.key);
  const updating = acquireResource(research);
  initial.resolve(resourceCohort(1, [resourceRecord(research, 1)]));
  await tick();
  assert.equal(resourceData(plan), undefined, "a plan never lands beside research from another pending revision");
  newerResearch.resolve(2);
  await updating;
  await tick();
  assert.equal(requests, 2);
  assert.equal(resourceData(plan), undefined);
  reread.resolve(resourceCohort(2, [resourceRecord(research, 2)]));
  assert.equal(await loading, 2);
  await shared;
  assert.equal(ordinaryReads, 0, "a concurrent ordinary Plan reader cannot replace the cohort retry's fetcher");
  assert.equal(resourceData(plan), 2);
  assert.equal(resourceData(research), 2);
});

await test("W21-R02: one auth guard rejects every record in a late private cohort", async () => {
  setAuthed(true);
  const waiting = deferred<ReturnType<typeof resourceCohort<string>>>();
  const research = { key: "cohort/private-research", fetch: async () => "new research" };
  const plan = { key: "cohort/private-plan", fetch: () => waiting.promise };
  const old = acquireResource(plan);
  const rejected = assert.rejects(old, { name: "AbortError" });
  setAuthed(false);
  setAuthed(true);
  waiting.resolve(resourceCohort("old plan", [resourceRecord(research, "old research")]));
  await rejected;
  assert.equal(resourceData(plan), undefined);
  assert.equal(resourceData(research), undefined);
  assert.equal(await acquireResource(research), "new research");
});

await test("W21-R02: review transport versions and record linkage are validated before normalization", async () => {
  const { reviewCohort } = await import("../src/api/plan-resources.ts");
  const plan = { id: "p", version: 3, research: "r" } as import("../src/api/plans.ts").Plan;
  const research = { id: "r", plan: "p", updated_at: 7 } as import("../src/api/research.ts").Research;
  const context = { plan, research, cohort: { plan_version: 3, research_id: "r", research_updated_at: 7 } };
  assert.doesNotThrow(() => reviewCohort("p", context));
  assert.throws(() => reviewCohort("other", context), /changed while loading/);
  assert.throws(() => reviewCohort("p", { ...context, cohort: { ...context.cohort, plan_version: 2 } }), /changed while loading/);
  assert.throws(() => reviewCohort("p", { ...context, research: { ...research, plan: "other" } }), /changed while loading/);
  assert.throws(() => reviewCohort("p", { ...context, research: { ...research, updated_at: 8 } }), /changed while loading/);
  assert.throws(() => reviewCohort("p", { ...context, research: null }), /changed while loading/, "an absent record must have an absent revision");
  const missing = reviewCohort("p", { ...context, research: null, cohort: { ...context.cohort, research_updated_at: null } });
  const source = { key: "cohort/missing-linked-research-plan", fetch: async () => missing };
  await acquireResource(source);
  assert.equal(resourceData({ key: "research/r", fetch: async () => "unexpected" }), "gone");
});

await test("W21-R02: a repeatedly crossed cohort stops after three reads and exposes its error", async () => {
  const research = { key: "cohort/bounded-research", fetch: async () => 0 };
  seedResource(research.key, 0, research.fetch);
  let attempts = 0;
  const plan = {
    key: "cohort/bounded-plan",
    fetch: async () => {
      attempts++;
      setData<number>(research.key, (previous) => previous + 1);
      return resourceCohort(attempts, [resourceRecord(research, 0)]);
    },
  };
  await assert.rejects(acquireResource(plan), /related records changed while loading/);
  assert.equal(attempts, 3);
  assert.equal(resourceData(plan), undefined);
  assert.equal(resourceData(research), 3);
});

await test("W21-R02: goal preparation cannot restore another login's private fallback stack", async () => {
  const { goalsRes, goalReads } = await import("../src/api/goal-resources.ts");
  const { harnessApi } = await import("../src/api/harness.ts");
  const original = harnessApi.goals;
  const waiting = deferred<import("../src/api/harness.ts").GoalStack>();
  harnessApi.goals = () => waiting.promise;
  try {
    setAuthed(true);
    const old = acquireResource(goalsRes("private-agent"));
    const rejected = assert.rejects(old, { name: "AbortError" });
    setAuthed(false);
    setAuthed(true);
    waiting.resolve({ items: [{ text: "private goal" }] } as import("../src/api/harness.ts").GoalStack);
    await rejected;
    assert.equal(goalReads.last("private-agent"), null);
    assert.equal(resourceData(goalsRes("private-agent")), undefined);
  } finally {
    harnessApi.goals = original;
  }
});

async function withServerEvents(run: (emit: () => void) => Promise<void>) {
  const { events } = await import("../src/api/events.ts");
  const previous = { WebSocket: globalThis.WebSocket, location: globalThis.location };
  const sockets: EventSocket[] = [];
  class EventSocket {
    static OPEN = 1;
    readyState = 1;
    onmessage: ((message: { data: string }) => void) | null = null;
    close() {}
    send() {}
    constructor() {
      sockets.push(this);
    }
  }
  Object.assign(globalThis, { WebSocket: EventSocket, location: { protocol: "http:", host: "resource.test" } });
  events.start();
  try {
    await run(() => sockets.at(-1)?.onmessage?.({ data: JSON.stringify({ type: "harness_closed", id: "updated" }) }));
  } finally {
    events.stop();
    Object.assign(globalThis, previous);
  }
}

await test("W21-R02: an event during an unsubscribed prepared read survives its response and revalidates", async () => {
  await withServerEvents(async (emit) => {
    const old = deferred<string>();
    const fresh = deferred<string>();
    let requests = 0;
    const source = { key: "navigation/event-during-read", fetch: () => (++requests === 1 ? old.promise : fresh.promise), opts: { on: ["harness_closed"] } };
    const loading = acquireResource(source);
    emit();
    old.resolve("before event");
    await loading;
    await tick();
    assert.equal(requests, 2, "the response cannot erase an invalidation that arrived during its read");
    fresh.resolve("after event");
    await tick();
    assert.equal(resourceData(source), "after event");
  });
});

await test("W21-R02: research invalidated during a cohort read is reread before any Plan revision is published", async () => {
  await withServerEvents(async (emit) => {
    const old = deferred<ReturnType<typeof resourceCohort<number>>>();
    const fresh = deferred<ReturnType<typeof resourceCohort<number>>>();
    const research = { key: "cohort/event-research", fetch: async () => 0, opts: { on: ["harness_closed"] } };
    seedResource(research.key, 0, research.fetch, research.opts);
    let requests = 0;
    const plan = { key: "cohort/event-plan", fetch: () => (++requests === 1 ? old.promise : fresh.promise) };
    const loading = acquireResource(plan);
    emit();
    old.resolve(resourceCohort(1, [resourceRecord(research, 1)]));
    await tick();
    assert.equal(requests, 2);
    assert.equal(resourceData(plan), undefined);
    assert.equal(resourceData(research), 0);
    fresh.resolve(resourceCohort(2, [resourceRecord(research, 2)]));
    await loading;
    assert.equal(resourceData(plan), 2);
    assert.equal(resourceData(research), 2);
  });
});

await test("W21-R15: an optimistic press stands over an already-out coherent read without replaying a newer snapshot", async () => {
  const snapshot = deferred<ReturnType<typeof resourceCohort<number>>>();
  let requests = 0;
  const research = { key: "cohort/optimistic-research", fetch: async () => 0 };
  const plan = {
    key: "cohort/optimistic-plan",
    fetch: () => {
      requests++;
      return snapshot.promise;
    },
  };
  seedResource(plan.key, 1, plan.fetch);
  const loading = acquireResource(plan);
  change<number>(plan.key, (value) => value + 1);
  assert.equal(resourceData(plan), 2, "the press is visible while the request is out");
  snapshot.resolve(resourceCohort(1, [resourceRecord(research, 7)]));
  await loading;
  assert.equal(requests, 1, "an optimistic transform does not restart against a later snapshot");
  assert.equal(resourceData(plan), 2, "the old response cannot erase the press");
  assert.equal(resourceData(research), 7);
});
