/**
 * bp-g3 step 6 (plan 01a1a832eb2f711f9c6f51d88effe89d): the browser's one socket, shared by its tabs,
 * in its lock fallback: two simulated tabs (one process's `Live`s, Node's own BroadcastChannel between
 * them, a Web Locks stand-in granting in order), the network a test carrier. Exactly one owner at any
 * time; both tabs' subscriptions to one topic ride one subscription; each change reaches each tab once;
 * and when the owner goes, the other takes over and subscribes again from its cursor (resumed, not
 * reopened), applying nothing twice.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

const { Live } = await import("../src/api/bp/shared.ts");
const { TopicState } = await import("../src/api/bp/state.ts");
type Hooks = import("../src/api/bp/carrier.ts").CarrierHooks;
type CarrierLike = import("../src/api/bp/carrier.ts").CarrierLike;
type Cursor = import("../src/api/bp/gen.ts").Cursor;
type Topic = import("../src/api/bp/gen.ts").Topic;
type Delta = import("../src/api/bp/gen.ts").Delta;

/** Web Locks as the spec has them for one exclusive name: granted in the order asked, the next once
 * the holder's callback's promise settles. */
class Locks {
  private held = false;
  private queue: (() => void)[] = [];
  request(_name: string, f: () => Promise<void>): Promise<void> {
    return new Promise((resolve) => {
      const run = () => {
        this.held = true;
        void f().finally(() => {
          this.held = false;
          resolve();
          this.queue.shift()?.();
        });
      };
      if (this.held) this.queue.push(run);
      else run();
    });
  }
}
Object.defineProperty(globalThis, "navigator", { value: { locks: new Locks() }, configurable: true });

/** The network, as each owner's carrier sees it. */
type Sub = { sub: number; topic: Topic; cursor: Cursor | null };
class TestCarrier implements CarrierLike {
  opened = 1;
  live = false;
  subs: Sub[] = [];
  states = new Map<number, InstanceType<typeof TopicState>>();
  constructor(readonly hooks: Hooks) {}
  start(): void {
    this.live = true;
    this.hooks.state("open");
  }
  stop(): void {
    this.live = false;
  }
  state(sub: number) {
    return this.states.get(sub);
  }
  subscribe(sub: number, topic: Topic, cursor: Cursor | null): void {
    this.subs.push({ sub, topic, cursor });
  }
  unsubscribe(): void {}
  request(): void {}
  /** The server's side of a subscription: a snapshot of `deltas` at `rev`, or a resume. */
  start_(sub: number, born: number, rev: number, deltas: Delta[]): void {
    const st = new TopicState();
    this.states.set(sub, st);
    this.hooks.event({ t: "snapshot", sub, cursor: { born, rev } });
    for (const d of deltas) {
      st.apply(d);
      this.hooks.event({ t: "change", sub, rev, delta: d });
    }
    this.hooks.event({ t: "ready", sub, cursor: { born, rev } });
  }
  change(sub: number, rev: number, d: Delta): void {
    this.states.get(sub)?.apply(d);
    this.hooks.event({ t: "change", sub, rev, delta: d });
  }
}

const settle = () => new Promise((r) => setTimeout(r, 20));
const text = (s: string): Delta => ({ t: "Append", at: 0, data: new TextEncoder().encode(s) });

test("bp_g3_s6 one owner, one subscription a topic, and a handover that resumes", async () => {
  const carriers: TestCarrier[] = [];
  const make = (hooks: Hooks) => {
    const c = new TestCarrier(hooks);
    carriers.push(c);
    return c;
  };
  const owners = () => carriers.filter((c) => c.live).length;
  const a = new Live({ mode: "lock", carrier: make });
  await settle();
  const b = new Live({ mode: "lock", carrier: make });
  await settle();
  assert.equal(owners(), 1, "exactly one owner");
  assert.ok(a.owns() && !b.owns(), "the first tab owns the socket");

  const topic: Topic = { kind: 10, key: "leg/1" };
  const sa = a.open(topic);
  await settle();
  const first = carriers[0]!;
  assert.equal(first.subs.length, 1);
  first.start_(first.subs[0]!.sub, 7, 1, [text("hello ")]);
  await settle();
  const sb = b.open(topic);
  await settle();
  assert.equal(first.subs.length, 1, "the second tab joined the first's subscription");
  assert.equal(new TextDecoder().decode(b.sub(sb)!.state.textBytes()), "hello ", "it was given the owner's copy");

  first.change(first.subs[0]!.sub, 2, { t: "Append", at: 6, data: new TextEncoder().encode("world") });
  await settle();
  for (const [tab, id] of [
    [a, sa],
    [b, sb],
  ] as const) {
    assert.equal(new TextDecoder().decode(tab.sub(id)!.state.textBytes()), "hello world", "each tab has the change once");
    assert.deepEqual(tab.sub(id)!.cursor, { born: 7, rev: 2 });
  }

  // The owner's tab goes: the other takes the lock, and resumes from its cursor.
  a.dispose();
  await settle();
  assert.equal(owners(), 1, "still exactly one owner");
  assert.ok(b.owns(), "the other tab took over");
  const second = carriers[1]!;
  assert.equal(second.subs.length, 1);
  assert.deepEqual(second.subs[0]!.cursor, { born: 7, rev: 2 }, "subscribed again from its cursor");
  // The server resumes: the changes since, nothing twice (a repeat of rev 2 is passed over).
  const sub = second.subs[0]!.sub;
  second.hooks.event({ t: "resumed", sub, cursor: { born: 7, rev: 2 } });
  second.hooks.event({ t: "ready", sub, cursor: { born: 7, rev: 2 } });
  second.change(sub, 2, { t: "Append", at: 6, data: new TextEncoder().encode("world") });
  second.change(sub, 3, { t: "Append", at: 11, data: new TextEncoder().encode("!") });
  await settle();
  assert.equal(new TextDecoder().decode(b.sub(sb)!.state.textBytes()), "hello world!", "resumed, nothing applied twice");
  b.dispose();
});
