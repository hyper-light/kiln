/**
 * One protocol socket per browser, shared by its tabs (goal 3 of the browser protocol program, plan
 * 01a1a832eb2f711f9c6f51d88effe89d step 6; docs/browser-protocol.md, "One socket per browser").
 *
 * - **Who owns the socket.** Whoever holds the Web Lock `kiln-bp-owner` for this origin: a
 *   SharedWorker where the browser has one (`worker.ts`), else one tab (Samsung Internet, Android
 *   WebView, a worker refused), the others queued on the lock. When the owner goes (its tab closed,
 *   the worker killed, a page suspended), the lock passes to the next in line, which takes over; a
 *   tab of a worker queues on the lock too, so it learns the worker died the moment it did, with no
 *   timer, and starts it again.
 * - **How tabs reach it.** The worker's port, or a `BroadcastChannel` (`kiln-bp`) naming each tab.
 * - **Joined.** The owner's [`Hub`] subscribes once per topic; a tab joining a topic another holds
 *   gets the owner's copy as a snapshot, then every change once, so each change crosses the network
 *   once whatever the tabs.
 * - **Resumed, not reopened.** Each tab keeps its own copy and cursor; after a change of owner it
 *   subscribes again from its cursor, so the server sends only what it missed (no snapshot while its
 *   log holds them), and a change at or below the cursor is never applied twice.
 *
 * React reads a topic through `useTopic` (`useSyncExternalStore`, as `events.ts` does). The lab's
 * page uses it now; the UI's reads move onto it with goal 9.
 */
import { useSyncExternalStore } from "react";
import type { CarrierHooks, CarrierLike, CarrierState } from "./carrier";
import type { Call, Cursor, EndCode, Outcome, Topic } from "./gen";
import { CHANNEL, Hub, OWNER_LOCK, topicKey, type Down, type SubEvent, type Up } from "./hub";
import { TopicState } from "./state";

/** One of a tab's subscriptions: its copy, its cursor, and who's reading it. */
class TabSub {
  state = new TopicState();
  cursor: Cursor | null = null;
  /** Changes past the snapshot or resume, before `ready`, still move the cursor. */
  starting = true;
  ended: { code: EndCode; why: string } | null = null;
  version = 0;
  /** Changes applied, and snapshots taken, all of them (the lab counts them: a resume takes none). */
  applied = 0;
  snapshots = 0;
  readonly listeners = new Set<() => void>();

  constructor(readonly topic: Topic) {}

  take(ev: SubEvent): void {
    switch (ev.t) {
      case "snapshot":
        this.snapshots += 1;
        this.state = new TopicState();
        for (const d of ev.deltas) this.state.apply(d);
        this.cursor = ev.cursor;
        this.starting = true;
        break;
      case "resumed":
        this.starting = true;
        break;
      case "change":
        // Never twice: what the tab has already (by its cursor) is passed over.
        if (!this.starting && this.cursor !== null && ev.rev <= this.cursor.rev) return;
        if (this.state.apply(ev.delta) === null) this.applied += 1;
        if (!this.starting && this.cursor !== null) this.cursor = { born: this.cursor.born, rev: ev.rev };
        break;
      case "ready":
        this.cursor = ev.cursor;
        this.starting = false;
        break;
      case "end":
        this.ended = { code: ev.code, why: ev.why };
        break;
    }
    this.version += 1;
    for (const l of this.listeners) l();
  }
}

export type LiveMode = "worker" | "lock";

export type LiveOptions = {
  /** Force the lock fallback (the lab's, and browsers without SharedWorker). */
  mode?: LiveMode;
  /** The worker's script (the lab serves its own; the app's is bundled beside it). */
  worker?: string | URL;
  /** The owner's carrier, where this tab owns the socket (a test's stands in for the network). */
  carrier?: (hooks: CarrierHooks) => CarrierLike;
};

/** A tab's handle on the browser's shared socket. */
export class Live {
  readonly tab = crypto.randomUUID();
  readonly mode: LiveMode;
  private subs = new Map<number, TabSub>();
  private asks = new Map<number, (o: Outcome) => void>();
  private next = 1;
  private post: (u: Up) => void = () => undefined;
  private channel: BroadcastChannel | null = null;
  private hub: Hub | null = null;
  private port: MessagePort | null = null;
  private release: (() => void) | null = null;
  /** The owner now, and the socket's state as it last said. */
  owner = "";
  conn: CarrierState = "connecting";
  sockets = 0;
  /** What went wrong reaching the owner (a worker refused or failing to start), as it was said. */
  readonly errors: string[] = [];
  private readonly listeners = new Set<() => void>();

  constructor(private readonly opts: LiveOptions = {}) {
    const hasWorker = typeof SharedWorker !== "undefined";
    this.mode = opts.mode ?? (hasWorker ? "worker" : "lock");
    if (this.mode === "worker") this.viaWorker();
    else this.viaLock();
    if (typeof addEventListener === "function") addEventListener("pagehide", () => this.dispose());
  }

  /** The tab going: its subscriptions let go, and, its owner, the lock passed on. */
  dispose(): void {
    this.post({ t: "leave", tab: this.tab });
    this.hub?.stop();
    this.hub = null;
    this.release?.();
    this.release = null;
    this.channel?.close();
    this.port?.close();
  }

  /** Whether this tab owns the socket (the lock fallback's owner). */
  owns(): boolean {
    return this.hub !== null;
  }

  private heard(d: Down): void {
    switch (d.t) {
      case "owner":
        if (d.owner !== this.owner) {
          this.owner = d.owner;
          // A new owner: everything again, each from its cursor.
          this.post({ t: "join", tab: this.tab });
          for (const [id, s] of this.subs) if (!s.ended) this.post({ t: "sub", tab: this.tab, id, topic: s.topic, cursor: s.cursor });
        }
        break;
      case "conn":
        if (d.tab !== null && d.tab !== this.tab) return;
        this.conn = d.state;
        this.sockets = d.sockets;
        break;
      case "ev":
        if (d.tab === this.tab) this.subs.get(d.id)?.take(d.ev);
        return;
      case "ans":
        if (d.tab !== this.tab) return;
        this.asks.get(d.id)?.(d.outcome);
        this.asks.delete(d.id);
        return;
    }
    for (const l of this.listeners) l();
  }

  private viaWorker(): void {
    const url = this.opts.worker ?? new URL("./worker.ts", import.meta.url);
    let watching = false;
    const open = () => {
      this.port?.close();
      const w = new SharedWorker(url, { type: "module", name: "kiln-bp" });
      w.onerror = (e) => this.errors.push(`worker: ${(e as ErrorEvent).message ?? "failed to start"}`);
      this.port = w.port;
      w.port.onmessage = (m: MessageEvent<Down>) => {
        this.heard(m.data);
        // Once the worker says it owns the socket (it holds the lock then), the lock is watched.
        if (m.data.t === "owner" && !watching) {
          watching = true;
          watch();
        }
      };
      w.port.start();
      this.post = (u) => this.port?.postMessage(u);
      this.owner = "";
      this.post({ t: "join", tab: this.tab });
      for (const [id, s] of this.subs) if (!s.ended) this.post({ t: "sub", tab: this.tab, id, topic: s.topic, cursor: s.cursor });
    };
    // The worker holds the owner's lock while it lives: granted here, it has died. Let go at once (a
    // new worker takes it), and start one, every subscription resumed from its cursor.
    const watch = (): void => {
      if (typeof navigator.locks === "undefined") return;
      void navigator.locks
        .request(OWNER_LOCK, async () => {
          watching = false;
          open();
        })
        .catch(() => undefined);
    };
    open();
  }

  private viaLock(): void {
    const ch = new BroadcastChannel(CHANNEL);
    this.channel = ch;
    ch.onmessage = (m: MessageEvent<Down | Up>) => {
      const msg = m.data;
      if (this.hub && (msg.t === "join" || msg.t === "sub" || msg.t === "unsub" || msg.t === "req" || msg.t === "leave")) this.hub.take(msg);
      else if (msg.t === "owner" || msg.t === "conn" || msg.t === "ev" || msg.t === "ans") this.heard(msg);
    };
    this.post = (u) => {
      if (this.hub) this.hub.take(u);
      else ch.postMessage(u);
    };
    const become = () =>
      new Promise<void>((done) => {
        // The owner: its hub in this tab, heard by itself and, over the channel, the others; the lock
        // held until the tab goes.
        this.release = done;
        const hub = new Hub(
          this.tab,
          (d) => {
            this.heard(d);
            ch.postMessage(d);
          },
          this.opts.carrier,
        );
        this.hub = hub;
        hub.start();
      });
    if (typeof navigator.locks !== "undefined") void navigator.locks.request(OWNER_LOCK, become);
    this.post({ t: "join", tab: this.tab });
  }

  /** Subscribes to `topic`: the subscription's id, read with `state` and `subscribe`. */
  open(topic: Topic): number {
    const id = this.next++;
    this.subs.set(id, new TabSub(topic));
    this.post({ t: "sub", tab: this.tab, id, topic, cursor: null });
    return id;
  }

  close(id: number): void {
    this.subs.delete(id);
    this.post({ t: "unsub", tab: this.tab, id });
  }

  sub(id: number): TabSub | undefined {
    return this.subs.get(id);
  }

  request(call: Call): Promise<Outcome> {
    const id = this.next++;
    return new Promise((resolve) => {
      this.asks.set(id, resolve);
      this.post({ t: "req", tab: this.tab, id, call });
    });
  }

  /** The lab's: the worker ends itself, as Android may end it in the background. */
  killWorker(): void {
    this.port?.postMessage({ t: "die" } satisfies Up);
  }

  /** Listens for the socket's state and the owner changing. */
  listen(l: () => void): () => void {
    this.listeners.add(l);
    return () => this.listeners.delete(l);
  }
}

let shared: Live | null = null;

/** This tab's handle on the browser's socket, made the first time it's asked for. */
export function live(opts?: LiveOptions): Live {
  shared ??= new Live(opts);
  return shared;
}

/** A topic's copy in React: re-rendered as it changes, the subscription held while mounted. */
export function useTopic(topic: Topic, l: Live = live()): { state: TopicState; cursor: Cursor | null; version: number } {
  const key = topicKey(topic);
  const hold = holdFor(l, key, topic);
  useSyncExternalStore(hold.subscribe, hold.version);
  const s = l.sub(hold.id);
  return { state: s?.state ?? new TopicState(), cursor: s?.cursor ?? null, version: s?.version ?? 0 };
}

type Hold = { id: number; count: number; subscribe: (cb: () => void) => () => void; version: () => number };
const holds = new WeakMap<Live, Map<string, Hold>>();

/** One tab subscription a topic, however many components read it; let go with the last. */
function holdFor(l: Live, key: string, topic: Topic): Hold {
  const m = holds.get(l) ?? new Map<string, Hold>();
  holds.set(l, m);
  const have = m.get(key);
  if (have) return have;
  const id = l.open(topic);
  const h: Hold = {
    id,
    count: 0,
    subscribe: (cb) => {
      h.count += 1;
      l.sub(id)?.listeners.add(cb);
      return () => {
        l.sub(id)?.listeners.delete(cb);
        h.count -= 1;
        if (h.count === 0) {
          m.delete(key);
          l.close(id);
        }
      };
    },
    version: () => l.sub(id)?.version ?? 0,
  };
  m.set(key, h);
  return h;
}
