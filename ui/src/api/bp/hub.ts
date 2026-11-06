/**
 * The owner's side of the browser's one protocol socket (goal 3 of the browser protocol program, plan
 * 01a1a832eb2f711f9c6f51d88effe89d step 6; `shared.ts` has the whole): one subscription a topic, its
 * events handed to every tab that holds it, a joining tab given the owner's copy, and requests'
 * answers to the tab that asked. Runs in the SharedWorker (`worker.ts`) or in the tab holding the lock.
 */
import { Carrier, type CarrierHooks, type CarrierLike, type CarrierState } from "./carrier";
import type { ClientEvent } from "./client";
import type { Call, Cursor, Delta, EndCode, Outcome, Topic } from "./gen";
import { keyOf, type TopicState } from "./state";

/** The lock whose holder owns the browser's socket. */
export const OWNER_LOCK = "kiln-bp-owner";
/** The channel tabs and a tab owner talk over. */
export const CHANNEL = "kiln-bp";

/** What a subscription hears, as the owner hands it to a tab. */
export type SubEvent =
  | { t: "snapshot"; cursor: Cursor; deltas: Delta[] }
  | { t: "resumed"; cursor: Cursor }
  | { t: "change"; rev: number; delta: Delta }
  | { t: "ready"; cursor: Cursor }
  | { t: "end"; code: EndCode; why: string };

/** Up, a tab to the owner. */
export type Up =
  | { t: "join"; tab: string }
  | { t: "sub"; tab: string; id: number; topic: Topic; cursor: Cursor | null }
  | { t: "unsub"; tab: string; id: number }
  | { t: "req"; tab: string; id: number; call: Call }
  | { t: "leave"; tab: string }
  /** The lab's: the worker ends itself, as Android may end it. */
  | { t: "die" };

/** Down, the owner to tabs (`tab` null: every tab). */
export type Down =
  | { t: "owner"; owner: string }
  | { t: "conn"; tab: string | null; state: CarrierState; sockets: number }
  | { t: "ev"; tab: string; id: number; ev: SubEvent }
  | { t: "ans"; tab: string; id: number; outcome: Outcome };

export const topicKey = (t: Topic): string => `${t.kind}\u0000${t.key}`;

/** A topic's state as the deltas that make it from nothing: what a joining tab is given. */
export function snapshotOf(s: TopicState): Delta[] {
  const out: Delta[] = [];
  if (s.record.length > 0) {
    const n = Math.min(64, s.record.length);
    out.push({ t: "Set", mask: n === 64 ? (1n << 64n) - 1n : (1n << BigInt(n)) - 1n, fields: s.record.slice(0, 64) });
  }
  for (const [k, fields] of s.items) out.push({ t: "Put", key: keyOf(k), fields });
  if (s.textFrom !== null) out.push({ t: "Append", at: s.textFrom, data: s.textBytes() });
  return out;
}

type Holder = { tab: string; id: number };

type ProtoSub = {
  topic: Topic;
  holders: Holder[];
  /** The cursor it started from (null: a snapshot). */
  from: Cursor | null;
  /** Started from a snapshot, so the owner's copy is whole: a joining tab can be given it. */
  whole: boolean;
  ready: boolean;
  cursor: Cursor | null;
};

/** The owner's side: one subscription a topic, its events handed to every tab that holds it, and
 * requests' answers to the tab that asked. Runs in the worker, or in the tab that owns the lock. */
export class Hub {
  private subs = new Map<number, ProtoSub>();
  private byTopic = new Map<string, number>();
  private ofTab = new Map<string, Map<number, number>>();
  private asks = new Map<number, Holder>();
  private next = 1;
  private nextAsk = 1;
  private readonly carrier: CarrierLike;
  private state: CarrierState = "connecting";

  constructor(
    readonly name: string,
    private readonly send: (d: Down) => void,
    carrier: (hooks: CarrierHooks) => CarrierLike = (hooks) => new Carrier(hooks),
  ) {
    this.carrier = carrier({ event: (e) => this.heard(e), state: (s) => this.conn(s) });
  }

  start(): void {
    this.carrier.start();
    this.send({ t: "owner", owner: this.name });
  }

  stop(): void {
    this.carrier.stop();
  }

  private conn(s: CarrierState): void {
    this.state = s;
    this.send({ t: "conn", tab: null, state: s, sockets: this.carrier.opened });
  }

  take(u: Up): void {
    switch (u.t) {
      case "join":
        this.ofTab.set(u.tab, this.ofTab.get(u.tab) ?? new Map());
        this.send({ t: "conn", tab: u.tab, state: this.state, sockets: this.carrier.opened });
        return;
      case "sub":
        return this.subscribe(u.tab, u.id, u.topic, u.cursor);
      case "unsub":
        return this.unsubscribe(u.tab, u.id);
      case "req": {
        const id = this.nextAsk++;
        this.asks.set(id, { tab: u.tab, id: u.id });
        this.carrier.request(id, u.call);
        return;
      }
      case "leave":
        for (const id of [...(this.ofTab.get(u.tab)?.keys() ?? [])]) this.unsubscribe(u.tab, id);
        this.ofTab.delete(u.tab);
        return;
      case "die":
        return;
    }
  }

  private subscribe(tab: string, id: number, topic: Topic, cursor: Cursor | null): void {
    const mine = this.ofTab.get(tab) ?? new Map<number, number>();
    this.ofTab.set(tab, mine);
    if (mine.has(id)) this.unsubscribe(tab, id);
    const key = topicKey(topic);
    const held = this.byTopic.get(key);
    const p = held === undefined ? undefined : this.subs.get(held);
    // Joined: the owner's copy as a snapshot, then the changes as they come.
    if (held !== undefined && p && p.whole && p.ready && p.cursor !== null && cursor === null) {
      p.holders.push({ tab, id });
      mine.set(id, held);
      const s = this.carrier.state(held);
      const deltas: Delta[] = s ? snapshotOf(s) : [];
      this.send({ t: "ev", tab, id, ev: { t: "snapshot", cursor: p.cursor, deltas } });
      this.send({ t: "ev", tab, id, ev: { t: "ready", cursor: p.cursor } });
      return;
    }
    // Joined, resuming: the tabs agree where they are (a new owner hears each again from its cursor),
    // so a tab at the cursor another's resume started from, or that it has reached, rides it.
    const same = (a: Cursor | null, b: Cursor | null) => a !== null && b !== null && a.born === b.born && a.rev === b.rev;
    if (held !== undefined && p && cursor !== null && ((!p.ready && same(p.from, cursor)) || (p.ready && same(p.cursor, cursor)))) {
      p.holders.push({ tab, id });
      mine.set(id, held);
      if (p.ready && p.cursor) this.send({ t: "ev", tab, id, ev: { t: "ready", cursor: p.cursor } });
      return;
    }
    // A subscription of its own: none to join, one under way, or this tab resumes from its cursor.
    const sub = this.next++;
    this.subs.set(sub, { topic, holders: [{ tab, id }], from: cursor, whole: false, ready: false, cursor });
    if (held === undefined) this.byTopic.set(key, sub);
    mine.set(id, sub);
    this.carrier.subscribe(sub, topic, cursor);
  }

  private unsubscribe(tab: string, id: number): void {
    const mine = this.ofTab.get(tab);
    const sub = mine?.get(id);
    if (sub === undefined) return;
    mine?.delete(id);
    const p = this.subs.get(sub);
    if (!p) return;
    p.holders = p.holders.filter((h) => !(h.tab === tab && h.id === id));
    if (p.holders.length > 0) return;
    this.subs.delete(sub);
    if (this.byTopic.get(topicKey(p.topic)) === sub) this.byTopic.delete(topicKey(p.topic));
    this.carrier.unsubscribe(sub);
  }

  private each(sub: number, ev: SubEvent): void {
    for (const h of this.subs.get(sub)?.holders ?? []) this.send({ t: "ev", tab: h.tab, id: h.id, ev });
  }

  /** What the carrier's client heard (a test's carrier calls it as the real one does). */
  heard(e: ClientEvent): void {
    switch (e.t) {
      case "snapshot": {
        const p = this.subs.get(e.sub);
        if (p) {
          p.whole = true;
          p.ready = false;
          p.cursor = e.cursor;
        }
        return this.each(e.sub, { t: "snapshot", cursor: e.cursor, deltas: [] });
      }
      case "resumed": {
        const p = this.subs.get(e.sub);
        if (p) p.ready = false;
        return this.each(e.sub, { t: "resumed", cursor: e.cursor });
      }
      case "change": {
        const p = this.subs.get(e.sub);
        if (p && p.ready && p.cursor) p.cursor = { born: p.cursor.born, rev: e.rev };
        return this.each(e.sub, { t: "change", rev: e.rev, delta: e.delta });
      }
      case "ready": {
        const p = this.subs.get(e.sub);
        if (p) {
          p.ready = true;
          p.cursor = e.cursor;
        }
        return this.each(e.sub, { t: "ready", cursor: e.cursor });
      }
      case "end":
        return this.each(e.sub, { t: "end", code: e.code, why: e.why });
      case "answer": {
        const h = this.asks.get(e.id);
        if (!h) return;
        this.asks.delete(e.id);
        this.send({ t: "ans", tab: h.tab, id: h.id, outcome: e.outcome });
        return;
      }
      default:
        return;
    }
  }
}
