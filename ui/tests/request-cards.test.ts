import assert from "node:assert/strict";
import { test } from "node:test";
import { registerHooks } from "node:module";
import type { AgentInfo, Pause } from "../src/api/harness.ts";
import type { AiSession } from "../src/api/types.ts";
import { cardsOf, heldWords } from "../src/views/harness/cards.ts";

const agent = (id: string, more: Partial<AgentInfo> = {}): AgentInfo => ({
  id,
  kind: "claude",
  kind_name: "Claude Code",
  target: "local",
  name: id,
  cwd: "/work",
  created_at: 0,
  status: "idle",
  title: "",
  model: "",
  mode: "default",
  approvals: [],
  queued: 0,
  tasks_running: 0,
  cost_usd: 0,
  last_activity: 1_000,
  ...more,
});
const limit = (since: number): Pause => ({ reason: "limit", label: "Session", said: "", since, auto: true, by: "harness" });
const none = new Set<string>();
const kinds = (list: AgentInfo[], path = "/runs", away = none, answered = none) => cardsOf(list, path, away, answered).map((c) => `${c.kind}:${c.a.id}`);

const fleet = [
  agent("lim", { pause: limit(50) }),
  agent("crash", { status: "exited", exit: { code: 127, error: "codex: command not found" }, last_activity: 40 }),
  agent("plan", { held: { why: 'Paused by the agent: plan "Q" is ready for you to look at', plan: "pl_q", since: 30 } }),
  agent("ask", { status: "waiting", approvals: [{ id: "q1", tool: "question", tool_name: "", title: "Postgres or SQLite?", at: 60 }] }),
  agent("both", { held: { why: "Paused by the agent.", since: 10 }, approvals: [{ id: "b1", tool: "shell", tool_name: "Bash", title: "ls", at: 70 }] }),
  agent("idle"),
  agent("mine", { status: "exited", exit: { code: 0 } }),
];

test("one card an agent, the most pressing first: requests, goals paused for you, stops of their own, usage limits", () => {
  assert.deepEqual(kinds(fleet), ["request:ask", "request:both", "held:plan", "stopped:crash", "limit:lim"]);
});

test("none where the page shows it itself: Mission control, and the agent's own pages", () => {
  assert.deepEqual(kinds(fleet, "/agents"), []);
  assert.deepEqual(kinds(fleet, "/agents/plan"), ["request:ask", "request:both", "stopped:crash", "limit:lim"]);
  assert.deepEqual(kinds(fleet, "/agents/lim/changes"), ["request:ask", "request:both", "held:plan", "stopped:crash"]);
});

test("a subagent kiln started has no request card on its agent's page, which has its request over the composer", () => {
  const ask = { id: "s1", tool: "question" as const, tool_name: "AskUserQuestion", title: "Which first?", at: 20 };
  const step = agent("step", { parent: "lead", subagent: true, status: "waiting", approvals: [ask] });
  assert.deepEqual(kinds([agent("lead"), step], "/agents/lead"), []);
  // Anywhere else its card shows, as any agent's does; so does one only started by an agent.
  assert.deepEqual(kinds([agent("lead"), step], "/runs"), ["request:step"]);
  assert.deepEqual(kinds([agent("lead"), agent("child", { parent: "lead", approvals: [ask] })], "/agents/lead"), ["request:child"]);
});

test("a limit under a turn still going isn't one yet, nor a limit of a machine gone quiet", () => {
  assert.deepEqual(kinds([agent("a", { status: "running", pause: limit(5) }), agent("b", { status: "disconnected", pause: limit(5) })]), []);
});

test("put away, a card stays away until there's something new", () => {
  const away = new Set(["held:plan:30", "limit:lim:50"]);
  assert.deepEqual(kinds(fleet, "/runs", away), ["request:ask", "request:both", "stopped:crash"]);
  const again = [agent("plan", { held: { why: "Paused by the agent: once more", since: 31 } }), agent("lim", { pause: limit(51) })];
  assert.deepEqual(kinds(again, "/runs", away), ["held:plan", "limit:lim"]);
});

test("a request answered here goes at once; its agent's next one shows, and then what else it has", () => {
  const two = agent("two", {
    held: { why: "Paused by the agent.", since: 1 },
    approvals: [
      { id: "x1", tool: "shell", tool_name: "Bash", title: "make", at: 5 },
      { id: "x2", tool: "edit", tool_name: "Edit", title: "a.rs", at: 6 },
    ],
  });
  const first = cardsOf([two], "/runs", none, none);
  assert.deepEqual(
    first.map((c) => [c.kind, c.key, c.more]),
    [["request", "x1", 1]],
  );
  assert.deepEqual(
    cardsOf([two], "/runs", none, new Set(["x1"])).map((c) => [c.kind, c.key, c.more]),
    [["request", "x2", 0]],
  );
  assert.deepEqual(kinds([two], "/runs", none, new Set(["x1", "x2"])), ["held:two"]);
});

test("a held stack's why, without who paused it", () => {
  assert.equal(heldWords('Paused by the agent: plan "Q" is ready for you to look at'), 'plan "Q" is ready for you to look at');
  assert.equal(heldWords("Paused by Release prep: waiting on the staging password"), "waiting on the staging password");
  assert.equal(heldWords("Paused by the agent."), "");
});

test("a stop is known by when it ended: put away, it stays away as the agent's activity moves on; a later stop is new", () => {
  const stopped = (last: number, at: number) => agent("crash", { status: "exited", exit: { code: 1, error: "boom", at }, last_activity: last });
  const [card] = cardsOf([stopped(40, 35)], "/runs", none, none);
  assert.equal(card?.key, "stopped:crash:35");
  const away = new Set([card?.key ?? ""]);
  assert.deepEqual(kinds([stopped(90, 35)], "/runs", away), [], "a refresh, a restart, its activity since: the same stop");
  assert.deepEqual(kinds([stopped(120, 110)], "/runs", away), ["stopped:crash"], "it stopped again later");
});

test("U13 waiting cards share the initial roster load, helper removal and stable notifications", async (t) => {
  // Keep the actual resource, shared descriptor and waiting projection; replace browser/API boundaries.
  const handlers = new Set<(ev: unknown) => void>();
  const authListeners = new Set<() => void>();
  let allowed = false;
  let reads = 0;
  let aiReads = 0;
  const external: AiSession = { machine: "runner-a", tool: "codex", id: "conversation", pid: 42, name: "", title: "", cwd: "/work", branch: "", model: "", started_at: 1, last_activity: 1, state: "", last_prompt: "", last_reply: "", terminal: null, transcript: false };
  let aiProjection = [external];
  const first = agent("first", { held: { why: "Review", since: 10 } });
  const second = agent("second", { held: { why: "Review", since: 20 } });
  const initialHelper = agent("initial-helper", { helper: "about", held: { why: "Helper only", since: 5 }, target: "agent:runner-a", kind: "codex", harness_session: "helper-conversation" });
  const fixture = {
    events: {
      on: (fn: (ev: unknown) => void) => {
        handlers.add(fn);
        return () => handlers.delete(fn);
      },
      onReopen: () => () => undefined,
    },
    auth: {
      getState: () => ({ kind: "authed", me: { can: { "agents.run": allowed ? "server" : "none" } } }),
      subscribe: (fn: () => void) => {
        authListeners.add(fn);
        return () => authListeners.delete(fn);
      },
    },
    list: async (options?: { includeHelpers?: boolean }) => {
      reads++;
      assert.equal(options?.includeHelpers, true, "the one resource loads its full authorized roster");
      return [second, initialHelper, first];
    },
    aiSessions: async () => {
      aiReads++;
      return aiProjection;
    },
  };
  const globals = globalThis as typeof globalThis & { __u13Waiting?: typeof fixture };
  globals.__u13Waiting = fixture;
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  Object.defineProperty(globalThis, "window", { configurable: true, value: { setTimeout: (...args: Parameters<typeof setTimeout>) => setTimeout(...args), clearTimeout } });
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const loader = registerHooks({
    load(url, context, nextLoad) {
      let source: string | undefined;
      if (url.endsWith("/src/api/events.ts")) source = "export const events = globalThis.__u13Waiting.events; export const useServerEvents = () => {};";
      if (url.endsWith("/src/api/client.ts")) source = "export const api = {aiSessions: globalThis.__u13Waiting.aiSessions};";
      if (url.endsWith("/src/api/harness.ts")) source = "export const harnessApi = {list: globalThis.__u13Waiting.list}; export const listed = a => !a.helper;";
      if (url.endsWith("/src/lib/auth.ts")) source = "export const useAuth = globalThis.__u13Waiting.auth;";
      return source === undefined ? nextLoad(url, context) : { format: "module", source, shortCircuit: true };
    },
  });
  let offWaiting: (() => void) | undefined;
  let offRoster: (() => void) | undefined;
  let offAiList: (() => void) | undefined;
  let offAiFound: (() => void) | undefined;
  try {
    const { subscribeWaiting, getWaiting } = await import("../src/views/harness/waiting.ts");
    const { harnessSessionsRes, aiSessionsRes, listedHarnessSessions } = await import("../src/lib/data.ts");
    const { observeResource } = await import("../src/lib/resource.ts");
    const { missionFoundSessions } = await import("../src/views/harness/mission-rows.ts");
    const flush = async () => {
      for (let turn = 0; turn < 8; turn++) await Promise.resolve();
    };
    let notifications = 0;
    const empty = getWaiting();
    offWaiting = subscribeWaiting(() => notifications++);
    assert.equal(reads, 0, "refused auth never starts a roster load");
    assert.equal(getWaiting(), empty);
    allowed = true;
    authListeners.forEach((fn) => fn());
    let roster: AgentInfo[] = [];
    offRoster = observeResource(harnessSessionsRes.key, harnessSessionsRes.fetch, harnessSessionsRes.opts, (data) => { roster = data; });
    await Promise.resolve();
    await Promise.resolve();
    assert.equal(reads, 1, "waiting cards and another reader share the initial fetch");
    assert.equal(roster.some((a) => a.id === initialHelper.id), true, "the raw snapshot retains an already-hidden helper");
    assert.deepEqual(listedHarnessSessions(roster).map((a) => a.id), ["second", "first"], "the read projection excludes helpers");
    assert.deepEqual(
      getWaiting().map((a) => a.id),
      ["first", "second"],
    );
    let listReads = 0;
    let foundReads = 0;
    let listView: AiSession[] = [];
    let foundView: [AiSession[], string[]] = [[], []];
    offAiList = observeResource(aiSessionsRes.key, (signal) => { listReads++; return aiSessionsRes.fetch(signal); }, aiSessionsRes.opts, (data) => { listView = data; });
    offAiFound = observeResource("ai-sessions-found", async (signal) => { foundReads++; return [await aiSessionsRes.fetch(signal), []] as [AiSession[], string[]]; }, { on: ["ai_sessions"] }, (data) => { foundView = data; });
    const assertAi = (count: number, visible: boolean) => {
      assert.deepEqual([listReads, foundReads, aiReads], [count, count, count * 2], "both real resource keys read the authoritative AI boundary");
      const ids = visible ? [external.id] : [];
      assert.deepEqual(missionFoundSessions(listView, []).map((s) => s.id), ids);
      assert.deepEqual(missionFoundSessions(foundView[0], []).map((s) => s.id), ids);
    };
    await flush();
    assertAi(1, true);
    const initial = getWaiting();
    const emit = (info: AgentInfo) => handlers.forEach((fn) => fn({ type: "harness", info }));
    emit({ ...first, cost_usd: 123 });
    t.mock.timers.tick(250);
    assert.equal(getWaiting(), initial, "irrelevant updates preserve snapshot identity");
    assert.equal(notifications, 1);
    await flush();
    assertAi(1, true);
    emit({ ...first, helper: "background" });
    t.mock.timers.tick(250);
    assert.deepEqual(
      getWaiting().map((a) => a.id),
      ["second"],
      "shared helper removal reaches waiting cards",
    );
    assert.equal(notifications, 2);
    await flush();
    assertAi(1, true); // Visibility alone did not change its ownership tuple.
    allowed = false;
    authListeners.forEach((fn) => fn());
    assert.deepEqual(getWaiting(), [], "revoking access clears the projection");
    assert.equal(notifications, 3);
    assert.equal(reads, 1);

    // The API's ownership projection can change while runner process discovery stays identical.
    const owned = { ...second, target: "agent:runner-a", kind: "codex", harness_session: external.id };
    aiProjection = [{ ...external, managed_by: owned.id }];
    emit(owned);
    t.mock.timers.tick(250);
    await flush();
    assertAi(2, false);
    emit({ ...owned, status: "running", title: "More work", cost_usd: 123, context_pct: 50, last_activity: 9_000, approvals: [{ id: "q", tool: "question", tool_name: "", title: "Continue?", at: 30 }] });
    t.mock.timers.tick(250);
    await flush();
    assertAi(2, false);

    aiProjection = [external];
    let changed = { ...owned, harness_session: "another-conversation" };
    emit(changed);
    t.mock.timers.tick(250);
    await flush();
    assertAi(3, true);
    changed = { ...changed, target: "agent:other-runner" };
    emit(changed);
    t.mock.timers.tick(250);
    await flush();
    assertAi(4, true);
    changed = { ...changed, kind: "future-harness" };
    emit(changed);
    t.mock.timers.tick(250);
    await flush();
    assertAi(5, true);

    const added = { ...owned, id: "added" };
    aiProjection = [{ ...external, managed_by: added.id }];
    emit(added);
    t.mock.timers.tick(250);
    await flush();
    assertAi(6, false);
    aiProjection = [external];
    handlers.forEach((fn) => fn({ type: "harness_closed", id: added.id }));
    t.mock.timers.tick(250);
    await flush();
    assertAi(7, true);
    handlers.forEach((fn) => fn({ type: "harness_closed", id: added.id }));
    emit({ ...first, helper: "background" });
    t.mock.timers.tick(250);
    await flush();
    assertAi(7, true);
    // Hidden helpers use the same raw resource and the same existing events: no discovery event is needed.
    const hidden = { ...owned, id: "hidden-helper", helper: "about", held: { why: "Helper only", since: 1 } };
    aiProjection = [{ ...external, managed_by: hidden.id }];
    emit(hidden);
    t.mock.timers.tick(250);
    await flush();
    assertAi(8, false);
    assert.deepEqual(listedHarnessSessions(roster).map((a) => a.id), ["second"], "an added helper has no row or count");
    emit({ ...hidden, status: "running", title: "Still helping", cost_usd: 12, last_activity: 10_000 });
    t.mock.timers.tick(250);
    await flush();
    assertAi(8, false); // Ordinary hidden status snapshots do not refetch either AI query.

    aiProjection = [external];
    emit({ ...hidden, harness_session: "another-helper-conversation" });
    t.mock.timers.tick(250);
    await flush();
    assertAi(9, true);
    aiProjection = [{ ...external, managed_by: hidden.id }];
    emit(hidden);
    t.mock.timers.tick(250);
    await flush();
    assertAi(10, false);
    aiProjection = [external];
    handlers.forEach((fn) => fn({ type: "harness_closed", id: hidden.id }));
    t.mock.timers.tick(250);
    await flush();
    assertAi(11, true);
    handlers.forEach((fn) => fn({ type: "harness_closed", id: hidden.id }));
    emit({ ...initialHelper, cost_usd: 42 });
    t.mock.timers.tick(250);
    await flush();
    assertAi(11, true); // Repeated close and an unchanged hidden tuple stay quiet.

    // This helper was already hidden in the initial snapshot, not learned from an earlier live event.
    aiProjection = [{ ...external, managed_by: initialHelper.id }];
    emit({ ...initialHelper, harness_session: external.id });
    t.mock.timers.tick(250);
    await flush();
    assertAi(12, false);
    aiProjection = [external];
    handlers.forEach((fn) => fn({ type: "harness_closed", id: initialHelper.id }));
    t.mock.timers.tick(250);
    await flush();
    assertAi(13, true);
    assert.deepEqual(listedHarnessSessions(roster).map((a) => a.id), ["second"], "helpers stayed out of the visible projection through every transition");
    assert.equal(reads, 1, "identity invalidation never creates another roster load");
    assert.equal(notifications, 3, "unsubscribed waiting projection stays quiet");
  } finally {
    offWaiting?.();
    offRoster?.();
    offAiList?.();
    offAiFound?.();
    loader.deregister();
    delete globals.__u13Waiting;
    if (previousWindow) Object.defineProperty(globalThis, "window", previousWindow);
    else Reflect.deleteProperty(globalThis, "window");
  }
});
