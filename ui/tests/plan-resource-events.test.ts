/** W21-R02: a canonical Plan kept by another view still refreshes when it is held again. */
import assert from "node:assert/strict";
import { registerHooks } from "node:module";
import { test } from "node:test";
import type { Plan } from "../src/api/plans.ts";
import type { Research } from "../src/api/research.ts";
import type { ServerEvent } from "../src/api/types.ts";

const subscriptions: (() => void)[] = [];
Object.assign(globalThis, {
  window: Object.assign(new EventTarget(), { setTimeout, clearTimeout, setInterval, clearInterval }),
  document: new EventTarget(),
});
// Hold the real external-store subscription without mounting unrelated DOM: all reads/events,
// invalidation, request generations and cohort publication below use the production modules.
const react = `export const use = () => {}; export const useSyncExternalStore = (subscribe, snapshot) => { globalThis.__planResourceSubscribe(subscribe(() => {})); return snapshot(); };`;
Object.assign(globalThis, { __planResourceSubscribe: (release: () => void) => subscriptions.push(release) });
const loader = registerHooks({
  resolve(specifier, context, next) {
    return specifier === "react" ? { url: "data:text/javascript," + encodeURIComponent(react), shortCircuit: true } : next(specifier, context);
  },
});
const { useSuspenseResource, acquireResource, resourceData } = await import("../src/lib/resource.ts");
const { planRes, researchRes, reviewPlanRes } = await import("../src/api/plan-resources.ts");
const { plansApi } = await import("../src/api/plans.ts");
const { events } = await import("../src/api/events.ts");
loader.deregister();

await test("W21-R02: a retained ordinary Plan subscriber and same-plan re-hold publish the new coherent review revision", async () => {
  const original = plansApi.reviewContext;
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
  const previous = { WebSocket: globalThis.WebSocket, location: globalThis.location };
  Object.assign(globalThis, { WebSocket: EventSocket, location: { protocol: "http:", host: "plan-resource.test" } });
  const plan = { id: "retained-plan", version: 1, research: "retained-research" } as Plan;
  const research = { id: "retained-research", plan: plan.id, updated_at: 1 } as Research;
  let revision = 1;
  let reads = 0;
  plansApi.reviewContext = async () => {
    reads++;
    return { plan: { ...plan, version: revision }, research: { ...research, updated_at: revision }, cohort: { plan_version: revision, research_id: research.id, research_updated_at: revision } };
  };
  const emit = (heldPlan: string, since: number) => sockets.at(-1)?.onmessage?.({ data: JSON.stringify({ type: "harness", info: { id: "agent", held: { plan: heldPlan, since } } }) });
  const ordinary = planRes(plan.id);
  const review = reviewPlanRes(plan.id);
  try {
    events.start();
    await acquireResource(review);
    useSuspenseResource(ordinary.key, ordinary.fetch, ordinary.opts);
    assert.equal(subscriptions.length, 1, "the ordinary view remains subscribed throughout the re-hold");
    for (const source of [ordinary, review]) {
      const unrelated = { type: "harness", info: { held: { plan: "other-plan", since: 2 } } } as ServerEvent;
      assert.equal(source.opts?.relevant?.(unrelated), false, "unrelated agent activity keeps this Plan");
      const related = { type: "harness", info: { held: { plan: plan.id, since: 2 } } } as ServerEvent;
      assert.equal(source.opts?.relevant?.(related), true, "either canonical reader preserves same-plan freshness");
    }
    revision = 2;
    emit(plan.id, 2);
    const deadline = Date.now() + 2500;
    while (Date.now() < deadline) {
      const current = resourceData(ordinary);
      if (typeof current === "object" && current.version === 2) break;
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    const fresh = resourceData(ordinary);
    assert.equal(typeof fresh === "object" ? fresh.version : null, 2);
    const currentResearch = resourceData(researchRes(research.id));
    assert.equal(typeof currentResearch === "object" ? currentResearch.updated_at : null, 2);
    assert.equal(reads, 2, "the retained ordinary reader refreshes the context from the real event, without a new review acquisition");
    assert.equal(subscriptions.length, 1);
  } finally {
    for (const release of subscriptions) release();
    events.stop();
    plansApi.reviewContext = original;
    Object.assign(globalThis, previous);
  }
});

await test("W21-R02: unrelated harness traffic cannot invalidate or exhaust an in-flight canonical Plan cohort", async () => {
  const original = plansApi.reviewContext;
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
  const previous = { WebSocket: globalThis.WebSocket, location: globalThis.location };
  Object.assign(globalThis, { WebSocket: EventSocket, location: { protocol: "http:", host: "plan-resource.test" } });
  const plan = { id: "quiet-plan", version: 1, research: "quiet-research" } as Plan;
  const research = { id: "quiet-research", plan: plan.id, updated_at: 1 } as Research;
  const context = { plan, research, cohort: { plan_version: 1, research_id: research.id, research_updated_at: 1 } };
  let finish = (_value: typeof context) => {};
  const deferred = new Promise<typeof context>((resolve) => {
    finish = resolve;
  });
  let reads = 0;
  plansApi.reviewContext = () => {
    reads++;
    return deferred;
  };
  try {
    events.start();
    const source = planRes(plan.id);
    const loading = acquireResource(source);
    for (let i = 0; i < 12; i++) sockets.at(-1)?.onmessage?.({ data: JSON.stringify({ type: "harness", info: { id: "busy-agent", held: { plan: "unrelated", since: i } } }) });
    finish(context);
    await loading;
    await new Promise((resolve) => setTimeout(resolve, 20));
    assert.equal(reads, 1, "irrelevant events never dirty the prepared owner, so no retry or exhaustion is possible");
    assert.equal(resourceData(source), plan);
    assert.equal(resourceData(researchRes(research.id)), research);
  } finally {
    events.stop();
    plansApi.reviewContext = original;
    Object.assign(globalThis, previous);
  }
});
