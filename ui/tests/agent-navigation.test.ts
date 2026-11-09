import assert from "node:assert/strict";
import { test } from "node:test";
import { loadAgentDestination } from "../src/lib/agent-navigation.ts";

function deferred<T>() {
  let resolve = (_value: T) => {};
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

await test("W21-R02: first held Plan navigation starts selected code, canonical session, goal stack and review together", async () => {
  const code = deferred<void>();
  const session = deferred<void>();
  const goals = deferred<void>();
  const first = deferred<void>();
  const agents = deferred<{ id: string; held: { plan: string } }[]>();
  const starts: string[] = [];
  const navigating = loadAgentDestination(
    { id: "a", tab: "plan" },
    {
      code: (tab) => {
        starts.push(`code/${tab}`);
        return code.promise;
      },
      session: (id) => {
        starts.push(`session/${id}`);
        return session.promise;
      },
      goals: (id) => {
        starts.push(`goals/${id}`);
        return goals.promise;
      },
      review: (id) => {
        starts.push(`review/${id}`);
        return first.promise;
      },
      held: () => "held-plan",
      agents: () => {
        starts.push("agents");
        return agents.promise;
      },
    },
  );
  assert.deepEqual(starts, ["code/plan", "session/a", "goals/a", "review/held-plan", "agents"]);
  agents.resolve([{ id: "a", held: { plan: "changed-plan" } }]);
  await Promise.resolve();
  assert.equal(starts.at(-1), "review/changed-plan", "fresh canonical metadata can change the required plan");
  code.resolve();
  session.resolve();
  goals.resolve();
  first.resolve();
  await navigating;
});

await test("W21-R02: cancellation prevents late plan discovery and Work navigation never reads review data", async () => {
  const controller = new AbortController();
  const agents = deferred<{ id: string; held: { plan: string } }[]>();
  const starts: string[] = [];
  const source = {
    code: async () => {
      starts.push("code");
    },
    session: async () => {
      starts.push("session");
    },
    goals: async () => {
      starts.push("goals");
    },
    review: async () => {
      starts.push("review");
    },
    held: () => undefined,
    agents: () => agents.promise,
  };
  const navigating = loadAgentDestination({ id: "a", tab: "plan", signal: controller.signal }, source);
  controller.abort();
  agents.resolve([{ id: "a", held: { plan: "late-plan" } }]);
  await navigating;
  assert.deepEqual(starts, ["code", "session", "goals"]);
  starts.length = 0;
  await loadAgentDestination({ id: "a", tab: "changes" }, source);
  assert.deepEqual(starts, ["code", "session"]);
});
