import assert from "node:assert/strict";
import { test } from "node:test";
import type { HEvent } from "../src/api/harness.ts";
import { relayHidden, shareTurns, turnsFrom, turnsOf, type Turn, type TurnsMemo } from "../src/views/harness/derive.ts";
import { findIn } from "../src/views/harness/find.tsx";

/** A seeded conversation that grows and changes as a live one does, event by event. */
function conversation(seed: number) {
  let s = seed;
  const r = () => (s = (s * 1103515245 + 12345) % 2147483648) / 2147483648;
  const pick = <T>(xs: T[]) => xs[Math.floor(r() * xs.length)]!;
  let seq = 0;
  const make = (): HEvent => {
    seq += 1;
    const base = { id: `e${seq}`, seq, at: 1000 + seq * 10, ...(r() < 0.12 ? { parent: "spawn" } : {}) };
    const kind = r();
    if (kind < 0.12) return { ...base, kind: "user", text: `ask ${seq}` };
    if (kind < 0.3) return { ...base, kind: "assistant", text: `said ${seq}`, streaming: r() < 0.3 };
    if (kind < 0.42) return { ...base, kind: "thinking", text: `thought ${seq}`, streaming: r() < 0.3, redacted: r() < 0.1 };
    if (kind < 0.5) return { ...base, kind: "notice", level: pick(["info", "warn", "error"] as const), text: pick(["limit", "retrying", "limit"]) };
    if (kind < 0.53) return { ...base, id: `harness-switch:${seq}`, kind: "notice", level: "info", text: "went on in Codex" };
    const tool = pick(["read", "search", "edit", "shell", "web", "agent", "todo", "plan", "question", "mcp", "other"] as const);
    return { ...base, kind: "tool", tool_id: `t${seq}`, name: tool, tool, title: `${tool} ${seq}`, input: {}, status: pick(["running", "done", "error", "pending"] as const), started: base.at };
  };
  const changed = (e: HEvent): HEvent => {
    if (e.kind === "assistant" || e.kind === "thinking") return { ...e, text: `${e.text} more`, streaming: r() < 0.5 };
    if (e.kind === "tool") return { ...e, status: pick(["done", "error", "running"] as const), ended: e.at + 5 };
    return { ...e };
  };
  return { r, make, changed };
}

test("turns worked out from the last ones are the turns worked out afresh, however the conversation changes", () => {
  for (let seed = 1; seed <= 40; seed++) {
    const c = conversation(seed);
    let events: HEvent[] = Array.from({ length: 5 + Math.floor(c.r() * 40) }, c.make);
    const memos: Record<string, TurnsMemo | null> = { main: null, spawn: null };
    let openLast = true;
    for (let step = 0; step < 120; step++) {
      const what = c.r();
      const next = events.slice();
      if (what < 0.35) next.push(...Array.from({ length: 1 + Math.floor(c.r() * 3) }, c.make));
      else if (what < 0.6 && next.length) next[next.length - 1] = c.changed(next.at(-1)!);
      else if (what < 0.75 && next.length) {
        const i = Math.floor(c.r() * next.length);
        next[i] = c.changed(next[i]!);
      } else if (what < 0.82) openLast = !openLast;
      else if (what < 0.87 && next.length > 10) next.splice(0, Math.floor(c.r() * 5) + 1);
      else if (what < 0.92 && next.length) next.splice(Math.floor(c.r() * next.length), 0, c.make());
      else if (what < 0.96 && next.length) next.splice(Math.floor(c.r() * next.length), 1);
      events = next;
      for (const parent of ["main", "spawn"] as const) {
        const p = parent === "main" ? undefined : parent;
        memos[parent] = turnsFrom(memos[parent] ?? null, events, p, openLast);
        assert.deepEqual(memos[parent]!.turns, turnsOf(events, p, openLast), `seed ${seed}, step ${step}, ${parent}`);
      }
    }
  }
});

test("a turn streaming at the end leaves the turns before it as they were", () => {
  const c = conversation(99);
  const events: HEvent[] = [];
  for (let t = 0; t < 30; t++) {
    events.push({ id: `u${t}`, seq: events.length, at: t * 100, kind: "user", text: `ask ${t}` });
    for (let i = 0; i < 6; i++) events.push(c.make());
  }
  const live: HEvent = { id: "live", seq: 9999, at: 99999, kind: "assistant", text: "Work", streaming: true };
  const first = turnsFrom(null, [...events, live], undefined, true);
  const next = turnsFrom(first, [...events, { ...live, text: "Working" }], undefined, true);
  assert.equal(next.turns.length, first.turns.length);
  next.turns.slice(0, -2).forEach((t, i) => assert.equal(t, first.turns[i], `turn ${i} is kept`));
  assert.notEqual(next.turns.at(-1), first.turns.at(-1));
  // Nothing changed: what was worked out is the answer.
  assert.equal(turnsFrom(next, next.events, undefined, true), next);
});

test("W21-R02 streaming a leaf of a giant single turn keeps all untouched rows and matches the full derivation", () => {
  const tools: HEvent[] = Array.from({ length: 2000 }, (_, i) => ({
    id: `r${i}`,
    seq: i + 1,
    at: i + 1,
    kind: "tool",
    tool_id: `r${i}`,
    name: "Read",
    tool: "read",
    title: `file${i}`,
    input: {},
    status: "done",
    started: i + 1,
  }));
  const ask: HEvent = { id: "ask", seq: 0, at: 0, kind: "user", text: "Review the tree" };
  const note: HEvent = { id: "answer", seq: 2001, at: 2001, kind: "assistant", text: "Working", streaming: true };
  const first = turnsFrom(null, [ask, ...tools, note], undefined, true);
  const changed = [...first.events.slice(0, -1), { ...note, text: "Working on the final file" }];
  const next = turnsFrom(first, changed, undefined, true);
  assert.deepEqual(next.turns, turnsOf(changed, undefined, true));
  assert.equal(next.turns[0]?.items[0], first.turns[0]?.items[0], "the2000-call step remains the same immutable object");
  assert.equal(next.turns[0]?.prompt, first.turns[0]?.prompt);
  assert.notEqual(next.turns[0]?.items.at(-1), first.turns[0]?.items.at(-1));
  const withTool = turnsFrom(null, [ask, ...tools], undefined, true);
  const last = withTool.events.at(-1)!;
  assert.equal(last.kind, "tool");
  const replaced = [...withTool.events.slice(0, -1), { ...last, status: "error" as const }];
  const toolNext = turnsFrom(withTool, replaced, undefined, true);
  assert.deepEqual(toolNext.turns, turnsOf(replaced, undefined, true));
  const a = withTool.turns[0]?.items[0];
  const b = toolNext.turns[0]?.items[0];
  assert.equal(a?.type, "step");
  assert.equal(b?.type, "step");
  if (a?.type === "step" && b?.type === "step") assert.deepEqual(b.calls.slice(0, -1), a.calls.slice(0, -1));
  // Crossing an outcome boundary, prepending history and changing group order remain exact.
  for (const events of [changed.map((e) => (e.id === "answer" ? { ...e, streaming: false } : e)), [ask, ...changed], [ask, tools[1]!, tools[0]!, ...tools.slice(2), note]]) {
    assert.deepEqual(turnsFrom(next, events, undefined, false).turns, turnsOf(events));
  }
});

test("shared turns from the last ones render as the turns worked out afresh", () => {
  const c = conversation(7);
  let events: HEvent[] = Array.from({ length: 60 }, c.make);
  const kept = new Map<string, Turn>();
  let memo: TurnsMemo | null = null;
  for (let step = 0; step < 200; step++) {
    events = c.r() < 0.5 ? [...events, c.make()] : [...events.slice(0, -1), c.changed(events.at(-1)!)];
    const next = turnsFrom(memo, events, undefined, true);
    const shared = shareTurns(kept, next.turns);
    memo = { ...next, turns: shared };
    assert.deepEqual(shared, turnsOf(events, undefined, true), `step ${step}`);
  }
});

test("W21-R02 a replaced harness-divider label is published while unchanged neighbors keep their identity", () => {
  const divider: HEvent = { id: "harness-switch:changed", seq: 1, at: 1, kind: "notice", level: "info", text: "Continued in the old harness" };
  const original = turnsOf([divider]);
  const kept = new Map(original.map((turn) => [turn.key, turn]));
  const replacement = { ...divider, text: "Continued in the current harness" };
  const fresh = turnsOf([replacement]);
  const shared = shareTurns(kept, fresh);
  assert.equal(shared[0]?.divider, replacement);
  assert.notEqual(shared[0], original[0]);
});

test("a Codex child's thread shows its own work whichever of its calls opened it, and with its row gone", () => {
  const at = (n: number) => 1_000 + n;
  const ev: HEvent[] = [
    { id: "u1", seq: 1, at: at(1), kind: "user", text: "go" },
    { id: "spawn-1", seq: 2, at: at(2), kind: "tool", tool_id: "spawn-1", name: "spawn_agent", tool: "agent", title: "Spawn", input: {}, status: "done", started: at(2), task: "T1" },
    { id: "child:T1:p", seq: 3, at: at(3), parent: "spawn-1", kind: "user", text: "audit the columns" },
    { id: "child:T1:a", seq: 4, at: at(4), parent: "spawn-1", kind: "assistant", text: "Done: the columns hold." },
    { id: "send-2", seq: 5, at: at(5), kind: "tool", tool_id: "send-2", name: "send_message", tool: "agent", title: "Send", input: {}, status: "done", started: at(5), task: "T1" },
    { id: "other", seq: 6, at: at(6), parent: "spawn-9", kind: "assistant", text: "someone else's" },
  ];
  const words = (turns: ReturnType<typeof turnsOf>) =>
    turns.flatMap((t) => [t.prompt?.text, t.outcome?.text, ...t.items.map((i) => ("event" in i && "text" in i.event ? i.event.text : undefined))]).filter(Boolean);
  // From its spawn, as before.
  assert.deepEqual(words(turnsOf(ev, "spawn-1")), ["audit the columns", "Done: the columns hold."]);
  // From a later call that addresses it (its row gone, the thread from the call): the same work.
  assert.deepEqual(words(turnsOf(ev, "send-2", false, "T1")), ["audit the columns", "Done: the columns hold."]);
  // Nothing of another child's, nor of the main thread.
  assert(!words(turnsOf(ev, "send-2", false, "T1")).includes("someone else's"));
  assert.deepEqual(words(turnsOf(ev)), ["go"]);
});

/** The same conversations with kiln's relays in them: some asks are kiln's word to pass a message on to subagent sub1, some calls the agent passing one on. */
function withRelays(e: HEvent, r: () => number): HEvent {
  if (e.parent) return e;
  if (e.kind === "user" && r() < 0.5)
    return { ...e, text: `The person wrote a message to your subagent “Tests” (agent id sub1), which you started. <message>${e.text}</message>`, client_id: `kiln-relay-${e.id}`, origin: "kiln" };
  if (e.kind === "tool" && r() < 0.3) return { ...e, name: "SendMessage", tool: "other", input: { to: "sub1", message: "use the v2 parser" } };
  return e;
}

test("turns worked out from the last ones are the turns worked out afresh with kiln's relays to subagents among them", () => {
  for (let seed = 1; seed <= 30; seed++) {
    const c = conversation(seed);
    const make = () => withRelays(c.make(), c.r);
    let events: HEvent[] = Array.from({ length: 5 + Math.floor(c.r() * 40) }, make);
    let memo: TurnsMemo | null = null;
    for (let step = 0; step < 100; step++) {
      const what = c.r();
      const next = events.slice();
      if (what < 0.45) next.push(...Array.from({ length: 1 + Math.floor(c.r() * 3) }, make));
      else if (what < 0.7 && next.length) next[next.length - 1] = c.changed(next.at(-1)!);
      else if (what < 0.85 && next.length) next.splice(Math.floor(c.r() * next.length), 0, make());
      else if (next.length) next.splice(Math.floor(c.r() * next.length), 1);
      events = next;
      memo = turnsFrom(memo, events, undefined, true);
      assert.deepEqual(memo.turns, turnsOf(events, undefined, true), `seed ${seed}, step ${step}`);
    }
  }
});

const ask = (id: string, text: string, at: number): HEvent => ({ id, seq: at, at, kind: "user", text, client_id: id, origin: "human" });
const relay = (id: string, to: string, text: string, at: number): HEvent => ({
  id,
  seq: at,
  at,
  kind: "user",
  text: `The person wrote a message to your subagent “Run the parser tests” (agent id ${to}), which you started, through kiln, which can't reach it directly. Send it with your SendMessage tool (to: "${to}"). Pass it on exactly as written and don't act on it yourself; if it can't be sent, say why.\n\n<message>\n${text}\n</message>`,
  client_id: `kiln-relay-${id}`,
  origin: "kiln",
});
const said = (id: string, text: string, at: number, parent?: string): HEvent => ({ id, seq: at, at, kind: "assistant", text, streaming: false, ...(parent ? { parent } : {}) });
const call = (id: string, name: string, at: number, more: Partial<Extract<HEvent, { kind: "tool" }>> = {}): HEvent => ({
  id,
  seq: at,
  at,
  kind: "tool",
  tool_id: id,
  name,
  tool: "other",
  title: name,
  input: {},
  status: "done",
  started: at,
  ended: at + 1,
  ...more,
});

test("a turn kiln began only to have the agent pass a message on to a subagent is drawn as nothing, and what came before keeps its answer", () => {
  const events = [
    ask("u1", "Rewrite the parser; send a subagent through its tests.", 1),
    call("spawn", "Agent", 2, { tool: "agent", task: "a575" }),
    said("a1", "A subagent is on the tests.", 3),
    // The person wrote "Use the v2 parser" to the subagent: kiln asked the agent, idle, to pass it on.
    { id: "m1", seq: 4, at: 4, parent: "spawn", kind: "user", text: "Use the v2 parser", client_id: "m1", origin: "human" } as HEvent,
    relay("r1", "a575", "Use the v2 parser", 5),
    said("a2", "Passing that on to the subagent.", 6),
    call("send", "SendMessage", 7, { input: { to: "a575", message: "Use the v2 parser" } }),
    said("a3", "Sent.", 8),
  ];
  const turns = turnsOf(events);
  assert.deepEqual(
    turns.map((t) => [t.key, t.relay ?? false, t.outcome?.text ?? null]),
    [
      ["u1", false, "A subagent is on the tests."],
      ["r1", true, "Sent."],
    ],
  );
  assert.deepEqual(turns[1]?.hidden, ["r1", "send"]);
  assert.deepEqual([...relayHidden(events)].sort(), ["a2", "a3", "r1", "send"]);
  // Found in the subagent's thread, where it shows; not in the main one, where it doesn't.
  assert.deepEqual(findIn(events, undefined, "v2 parser"), []);
  assert.deepEqual(findIn(events, "spawn", "v2 parser"), ["m1"]);
  assert.deepEqual(findIn(events, undefined, "subagent is on"), ["a1"]);
});

test("a relay steered into the agent's work is no turn of its own: the turn goes on without kiln's words or the call passing it on", () => {
  const events = [
    ask("u1", "Also bump the version.", 1),
    call("bash", "Bash", 2, { tool: "shell" }),
    relay("r2", "a575", "Then run the figure tests", 3),
    call("send", "SendMessage", 4, { input: { to: "a575", message: "Then run the figure tests" } }),
    call("edit", "Edit", 5, { tool: "edit" }),
    said("a1", "Version bumped.", 6),
  ];
  const turns = turnsOf(events);
  assert.deepEqual(
    turns.map((t) => [t.key, t.relay ?? false, t.prompt?.id ?? null, t.items.flatMap((it) => (it.type === "step" ? it.calls.map((c) => c.id) : [it.key])), t.outcome?.text ?? null]),
    [["u1", false, "u1", ["bash", "edit"], "Version bumped."]],
  );
  assert.deepEqual(turns[0]?.hidden, ["r2", "send"]);
  // Steered in just before its answer, the answer is still the turn's.
  const last = turnsOf([
    ask("u1", "Also bump the version.", 1),
    call("bash", "Bash", 2, { tool: "shell" }),
    relay("r3", "a575", "hi", 3),
    call("send", "SendMessage", 4, { input: { to: "a575", message: "hi" } }),
    said("a1", "Version bumped.", 5),
  ]);
  assert.deepEqual(
    last.map((t) => [t.key, t.relay ?? false, t.outcome?.text ?? null, t.hidden]),
    [["u1", false, "Version bumped.", ["r3", "send"]]],
  );
});

test("the agent's own messages to its subagents show; Codex's agent tools passing a relay on don't", () => {
  // Its own SendMessage, not one kiln asked it to make, is its work.
  const own = turnsOf([ask("u1", "Coordinate the tests.", 1), call("send", "SendMessage", 2, { input: { to: "a575", message: "start with the lexer" } })]);
  assert.deepEqual(
    own.map((t) => [t.relay ?? false, t.items.length]),
    [[false, 1]],
  );
  // A relay for one subagent doesn't take the agent's message to another.
  const other = turnsOf([relay("r1", "a575", "hi", 1), call("send", "SendMessage", 2, { input: { to: "b880", message: "lexer first" } })]);
  assert.deepEqual(
    other.map((t) => [t.relay ?? false, t.hidden]),
    [[false, ["r1"]]],
  );
  // Codex passes one on with send_message (or follow-up_task, send_input), or v2's interaction.
  const codex = turnsOf([
    relay("r1", "thr2", "use the v2 parser", 1),
    call("collab", "collabAgentToolCall", 2, { tool: "agent", task: "thr2", input: { tool: "sendMessage" } }),
    call("act", "subAgentActivity", 3, { tool: "agent", task: "thr2", input: { kind: "interacted" } }),
    said("a1", "Sent it to the subagent.", 4),
  ]);
  assert.deepEqual(
    codex.map((t) => [t.relay ?? false, t.hidden]),
    [[true, ["r1", "collab", "act"]]],
  );
});
