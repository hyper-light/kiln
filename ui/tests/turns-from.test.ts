import assert from "node:assert/strict";
import { test } from "node:test";
import type { HEvent } from "../src/api/harness.ts";
import { shareTurns, turnsFrom, turnsOf, type Turn, type TurnsMemo } from "../src/views/harness/derive.ts";

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
  const words = (turns: ReturnType<typeof turnsOf>) => turns.flatMap((t) => [t.prompt?.text, t.outcome?.text, ...t.items.map((i) => ("event" in i && "text" in i.event ? i.event.text : undefined))]).filter(Boolean);
  // From its spawn, as before.
  assert.deepEqual(words(turnsOf(ev, "spawn-1")), ["audit the columns", "Done: the columns hold."]);
  // From a later call that addresses it (its row gone, the thread from the call): the same work.
  assert.deepEqual(words(turnsOf(ev, "send-2", false, "T1")), ["audit the columns", "Done: the columns hold."]);
  // Nothing of another child's, nor of the main thread.
  assert(!words(turnsOf(ev, "send-2", false, "T1")).includes("someone else's"));
  assert.deepEqual(words(turnsOf(ev)), ["go"]);
});
