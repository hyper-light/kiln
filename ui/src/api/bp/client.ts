/**
 * The browser's half of a connection (docs/browser-protocol.md), sans-IO, as kiln-bp's
 * `client.rs`: a carrier (goal 3's WebSocket, goal 5's WebTransport) hands it the bytes that came
 * (`receive`, after `Unpacker` has inflated any compressed batch) and asks it for bytes when it
 * has room (`pollTransmit`). It opens with a ticket, sends requests and keeps them until a final
 * answer, subscribes from its cursors, notices gaps and subscribes again, joins fragments, grants
 * credit, and keeps liveness by bytes arriving. Time is the caller's (milliseconds); nothing here
 * reads a clock or starts a timer. What it hears goes out through `pollEvent`.
 */
import { CAP_DEFLATE_RAW } from "./caps";
import { encodeFrame, Frames, decodeBody, type Outcome as ReadOutcome } from "./frames";
import {
  KIND_INDEX,
  OLDEST,
  SCHEMA_HASH,
  VERSION,
  type Call,
  type CloseCode,
  type Cursor,
  type Delta,
  type EndCode,
  type Frame,
  type Key,
  type Mark,
  type OpenRefusal,
  type Outcome,
  type Topic,
  type UnreadCode,
  type UnreadWhy,
} from "./gen";
import { Joiner } from "./join";
import { Liveness, type Verdict } from "./liveness";
import { itemKey, keyOf, TopicState, type ItemKey } from "./state";
import { codeOf, Why } from "./runtime";

export type ClientConfig = {
  version: number;
  oldest: number;
  caps: bigint;
  idleMs: number;
  /** The connection's credit, granted at open and topped up while `autoCredit`. */
  window: number;
  autoCredit: boolean;
  build: string;
  /** Keep each subscription's state (a `TopicState`). */
  mirror: boolean;
};

export const DEFAULT_CONFIG: ClientConfig = { version: VERSION, oldest: OLDEST, caps: CAP_DEFLATE_RAW, idleMs: 15_000, window: 1 << 20, autoCredit: true, build: "", mirror: true };

export type ClientEvent =
  | { t: "opened"; version: number; caps: bigint; born: number }
  | { t: "refused"; code: OpenRefusal; why: string; build: string }
  | { t: "answer"; id: number; outcome: Outcome; at: Mark | null; secret: boolean }
  | { t: "snapshot"; sub: number; cursor: Cursor }
  | { t: "resumed"; sub: number; cursor: Cursor }
  | { t: "change"; sub: number; rev: number; delta: Delta }
  | { t: "ready"; sub: number; cursor: Cursor }
  | { t: "end"; sub: number; code: EndCode; why: string }
  | { t: "gap"; sub: number; cursor: Cursor | null }
  | { t: "unreadable"; kind: number | null; why: Why }
  | { t: "unread"; kind: number; code: UnreadCode; why: UnreadWhy }
  | { t: "closed"; code: CloseCode; why: string; build: string };

/** Whether an answer is final, for now, or not this time (docs/wire.md, "The UI's sockets"). */
export function finality(o: Outcome): "final" | "for_now" | "not_this_time" {
  if (o.t === "Parked") return "for_now";
  if (o.t === "Pending" || (o.t === "Nack" && o.again)) return "not_this_time";
  return "final";
}

/** How many `Str` keys a subscription's alias table holds at most (kiln-bp's `MAX_ALIASES`). */
export const MAX_ALIASES = 4096;
const MIN_FRAGMENT = 256;

type Target = { t: "stream" } | { t: "field"; key: ItemKey; field: number };

type Sub = {
  topic: Topic;
  cursor: Cursor | null;
  rev: number;
  born: number;
  starting: boolean;
  waiting: boolean;
  aliases: string[];
  tail: { target: Target; end: number } | null;
  credit: number;
  received: number;
  state: TopicState;
  ended: boolean;
};

type Prio = "control" | "interactive";

/** One frame of a transmit: where it is in the bytes, and its lane. */
export type Sent = { kind: Frame["kind"]; lane: "control" | "datagram"; start: number; len: number };
export type Transmit = { bytes: Uint8Array; frames: Sent[] };

const URGENT_UP = new Set(["Open", "Request", "Close", "Probe", "ProbeAck", "Input", "Resize"]);

export class Client {
  private readonly cfg: ClientConfig;
  private phase: "opening" | "open" | "closed" = "opening";
  version = OLDEST;
  caps = 0n;
  born = 0;
  private readonly openedAt: number;
  readonly live: Liveness;
  private events: ClientEvent[] = [];
  private up: { f: Frame; prio: Prio }[] = [];
  private credits = new Map<number, number>();
  private resizes = new Map<number, { cols: number; rows: number }>();
  private probeWant = false;
  private probeOut: { nonce: number; at: number } | null = null;
  private acks: { nonce: number; got: number }[] = [];
  private nonce = 0;
  private subs = new Map<number, Sub>();
  /** Requests not yet answered finally, by id: sent again on the next connection. */
  readonly pending = new Map<number, Call>();
  private joiner = new Joiner();
  private fragment = 1200;
  private got = 0;
  private granted: number;
  private lastSent: number;
  private idleDue = false;
  /** Frames read, by kind. */
  readonly counts = new Map<string, number>();

  constructor(cfg: ClientConfig, ticket: Uint8Array, now: number) {
    this.cfg = cfg;
    this.openedAt = now;
    this.lastSent = now;
    this.granted = cfg.window;
    this.live = new Liveness(cfg.idleMs, now);
    this.up.push({
      f: { kind: "Open", body: { version: cfg.version, oldest: cfg.oldest, caps: cfg.caps, schema: SCHEMA_HASH, ticket, idle_ms: cfg.idleMs, window: cfg.window, build: cfg.build } },
      prio: "control",
    });
  }

  /** A new connection carrying this one's subscriptions (from their cursors) and unanswered requests. */
  reconnect(cfg: ClientConfig, ticket: Uint8Array, now: number): Client {
    const c = new Client(cfg, ticket, now);
    for (const [id, call] of this.pending) c.request(id, call);
    for (const [id, s] of this.subs) if (!s.ended) c.subscribeFrom(id, s.topic, s.cursor, s.credit, s.state);
    return c;
  }

  pollEvent(): ClientEvent | undefined {
    return this.events.shift();
  }

  isOpen(): boolean {
    return this.phase === "open";
  }

  isClosed(): boolean {
    return this.phase === "closed";
  }

  state(sub: number): TopicState | undefined {
    return this.subs.get(sub)?.state;
  }

  cursor(sub: number): Cursor | null {
    return this.subs.get(sub)?.cursor ?? null;
  }

  received(sub: number): number {
    return this.subs.get(sub)?.received ?? 0;
  }

  private queue(f: Frame, prio: Prio = "control"): void {
    this.up.push({ f, prio });
  }

  /** Sends a request under `id`, kept until a final answer and sent again on a reconnect. */
  request(id: number, call: Call): void {
    this.pending.set(id, call);
    this.queue({ kind: "Request", body: { id, call } });
  }

  subscribe(sub: number, topic: Topic, credit = 0): void {
    this.subscribeFrom(sub, topic, null, credit, new TopicState());
  }

  /** Subscribes from a cursor a page already has (another tab's, the shared socket's owner having
   * changed: goal 3's `shared.ts`): resumed, no snapshot, while the server's log still holds it. */
  subscribeAt(sub: number, topic: Topic, cursor: Cursor, credit = 0): void {
    this.subscribeFrom(sub, topic, cursor, credit, new TopicState());
  }

  private subscribeFrom(sub: number, topic: Topic, cursor: Cursor | null, credit: number, state: TopicState): void {
    this.subs.set(sub, { topic, cursor, rev: cursor?.rev ?? 0, born: cursor?.born ?? 0, starting: false, waiting: true, aliases: [], tail: null, credit, received: 0, state, ended: false });
    this.queue({ kind: "Subscribe", body: { sub, topic, cursor, credit } });
  }

  unsubscribe(sub: number): void {
    const s = this.subs.get(sub);
    if (s) s.ended = true;
    this.queue({ kind: "Unsubscribe", body: { sub } });
  }

  /** Grants credit up to `upto` bytes since the subscription began (sub 0: the connection). */
  grant(sub: number, upto: number): void {
    if (sub === 0) this.granted = Math.max(this.granted, upto);
    else {
      const s = this.subs.get(sub);
      if (s) s.credit = Math.max(s.credit, upto);
    }
    this.credits.set(sub, Math.max(this.credits.get(sub) ?? 0, upto));
  }

  input(sub: number, data: Uint8Array): void {
    this.queue({ kind: "Input", body: { sub, data } }, "interactive");
  }

  resize(sub: number, cols: number, rows: number): void {
    this.resizes.set(sub, { cols, rows });
  }

  close(code: CloseCode, why: string): void {
    if (this.phase === "open") this.queue({ kind: "Close", body: { code, why, build: "" } });
    this.phase = "closed";
  }

  /** The bytes that came (plain: compressed batches already inflated by `Unpacker`), at `now`. `wire`:
   * the bytes they came as on the carrier, which `got` (what the page says it has, in idle frames and
   * probes' answers) counts, so a server's window over the bytes it sent isn't thrown by inflation. */
  receive(bytes: Uint8Array, now: number, wire = bytes.length): void {
    if (bytes.length === 0) return;
    this.got += wire;
    this.live.arrived(now);
    const frames = new Frames(bytes);
    for (let raw = frames.nextRaw(); raw !== null; raw = frames.nextRaw()) {
      if ("err" in raw) {
        this.refuse(raw.err.kind, raw.err.why);
        continue;
      }
      const o = decodeBody(raw.kind, bytes, raw.from, raw.to);
      if ("ok" in o) this.take(o.ok.frame, raw.to - raw.from, now);
      else this.refuse(o.err.kind, o.err.why);
    }
    if (this.cfg.autoCredit && this.phase === "open" && this.got * 2 >= this.granted) this.grant(0, this.got + this.cfg.window);
  }

  private refuse(kind: number | null, why: Why): void {
    this.events.push({ t: "unreadable", kind, why });
    const code: UnreadCode = codeOf(why) === "unknown" ? { t: "Unknown" } : { t: "Malformed" };
    this.queue({ kind: "Unread", body: { kind: kind ?? 0xffffffff, code, why: { t: UNREAD_WHY[why] ?? "CutShort" } } });
  }

  private take(f: Frame, len: number, now: number): void {
    this.counts.set(f.kind, (this.counts.get(f.kind) ?? 0) + 1);
    switch (f.kind) {
      case "Accepted": {
        const a = f.body;
        this.phase = "open";
        this.version = a.version;
        this.caps = a.caps;
        this.born = a.born;
        this.fragment = Math.max(a.fragment || 1200, MIN_FRAGMENT);
        this.live.setIdle(a.idle_ms);
        this.live.roundTrip(Math.max(0, now - this.openedAt));
        this.events.push({ t: "opened", version: a.version, caps: a.caps, born: a.born });
        return;
      }
      case "Refused":
        this.phase = "closed";
        this.events.push({ t: "refused", code: f.body.code, why: f.body.why, build: f.body.build });
        return;
      case "Answer":
      case "SecretAnswer": {
        const a = f.body;
        if (finality(a.outcome) === "final") this.pending.delete(a.id);
        this.events.push({ t: "answer", id: a.id, outcome: a.outcome, at: a.at, secret: f.kind === "SecretAnswer" });
        return;
      }
      case "Snapshot":
        return this.begin(f.body.sub, f.body.cursor, true);
      case "Resumed":
        return this.begin(f.body.sub, f.body.cursor, false);
      case "Change":
        this.count(f.body.sub, len);
        return this.change(f.body.sub, f.body.rev, f.body.step, f.body.delta);
      case "Tail":
        this.count(f.body.sub, len);
        return this.tail(f.body.sub, f.body.data);
      case "Ready": {
        const s = this.subs.get(f.body.sub);
        if (!s || s.waiting || s.ended) return;
        s.starting = false;
        s.rev = f.body.cursor.rev;
        s.born = f.body.cursor.born;
        s.cursor = f.body.cursor;
        this.events.push({ t: "ready", sub: f.body.sub, cursor: f.body.cursor });
        return;
      }
      case "End": {
        const s = this.subs.get(f.body.sub);
        if (s) s.ended = true;
        this.events.push({ t: "end", sub: f.body.sub, code: f.body.code, why: f.body.why });
        return;
      }
      case "Fragment": {
        const o: ReadOutcome | null = this.joiner.take(f.body);
        if (o === null) return;
        if ("ok" in o) this.take(o.ok.frame, len, now);
        else this.refuse(o.err.kind, o.err.why);
        return;
      }
      case "Idle":
        return;
      case "Probe":
        this.acks.push({ nonce: f.body.nonce, got: this.got });
        return;
      case "ProbeAck":
        if (this.probeOut && this.probeOut.nonce === f.body.nonce) {
          this.live.roundTrip(Math.max(0, now - this.probeOut.at));
          this.probeOut = null;
        }
        return;
      case "Unread":
        this.events.push({ t: "unread", kind: f.body.kind, code: f.body.code, why: f.body.why });
        return;
      case "Compressed":
        // Inflated before it gets here (`Unpacker`); one that does is a batch inside a batch.
        return this.refuse(19, Why.TooDeep);
      case "Close":
        this.phase = "closed";
        this.events.push({ t: "closed", code: f.body.code, why: f.body.why, build: f.body.build });
        return;
      case "Open":
      case "Request":
      case "Subscribe":
      case "Unsubscribe":
      case "Credit":
      case "Input":
      case "Resize":
        return this.refuse(KIND_INDEX[f.kind], Why.OutOfOrder);
    }
  }

  private count(sub: number, len: number): void {
    const s = this.subs.get(sub);
    if (s) s.received += len;
  }

  private begin(sub: number, cursor: Cursor, snapshot: boolean): void {
    const s = this.subs.get(sub);
    if (!s || s.ended) return;
    if (!snapshot && (s.cursor === null || s.cursor.born !== cursor.born || s.cursor.rev !== cursor.rev)) return this.gap(sub);
    s.waiting = false;
    s.starting = true;
    s.aliases = [];
    s.tail = null;
    s.rev = cursor.rev;
    s.born = cursor.born;
    if (snapshot) {
      s.state = new TopicState();
      this.events.push({ t: "snapshot", sub, cursor });
    } else {
      this.events.push({ t: "resumed", sub, cursor });
    }
  }

  /** Subscribes again from the cursor the page has. */
  private gap(sub: number): void {
    const s = this.subs.get(sub);
    if (!s) return;
    s.waiting = true;
    s.received = 0;
    this.events.push({ t: "gap", sub, cursor: s.cursor });
    this.queue({ kind: "Subscribe", body: { sub, topic: s.topic, cursor: s.cursor, credit: s.credit } });
  }

  private resolve(s: Sub, k: Key): Key | null {
    if (k.t === "Ref") {
      const name = s.aliases[k.n];
      return name === undefined ? null : { t: "Str", s: name };
    }
    if (k.t === "Str" && s.aliases.length < MAX_ALIASES) s.aliases.push(k.s);
    return k;
  }

  private change(sub: number, rev: number, step: number, delta: Delta): void {
    const s = this.subs.get(sub);
    if (!s || s.waiting || s.ended) return;
    const snapshotItem = step === 0;
    if (!snapshotItem) {
      if (rev <= s.rev) return; // a repeat
      if (rev - step !== s.rev) return this.gap(sub);
    } else if (!s.starting) {
      return this.gap(sub);
    }
    let d: Delta | null;
    switch (delta.t) {
      case "Put":
      case "Patch":
      case "Remove":
      case "Extend": {
        const key = this.resolve(s, delta.key);
        d = key === null ? null : { ...delta, key };
        break;
      }
      default:
        d = delta;
    }
    if (d === null) return this.gap(sub);
    if (d.t === "Append") s.tail = { target: { t: "stream" }, end: d.at + d.data.length };
    else if (d.t === "Extend") {
      const k = itemKey(d.key);
      s.tail = k === null ? null : { target: { t: "field", key: k, field: d.field }, end: d.at + d.data.length };
    } else s.tail = null;
    if (this.cfg.mirror && s.state.apply(d) !== null) return this.gap(sub);
    if (!snapshotItem) {
      s.rev = rev;
      if (!s.starting) s.cursor = { born: s.born, rev };
    }
    this.events.push({ t: "change", sub, rev, delta: d });
  }

  private tail(sub: number, data: Uint8Array): void {
    const s = this.subs.get(sub);
    if (!s || s.waiting || s.ended) return;
    if (s.tail === null) return this.gap(sub);
    const { target, end } = s.tail;
    const d: Delta = target.t === "stream" ? { t: "Append", at: end, data } : { t: "Extend", key: keyOf(target.key), field: target.field, at: end, data };
    if (this.cfg.mirror && s.state.apply(d) !== null) return this.gap(sub);
    s.tail = { target, end: end + data.length };
    s.rev += 1;
    if (!s.starting) s.cursor = { born: s.born, rev: s.rev };
    this.events.push({ t: "change", sub, rev: s.rev, delta: d });
  }

  /** The page ran again after a freeze (visibilitychange, pageshow, resume): the gap is its own, not the link's. Call before reading what came meanwhile. */
  resumed(now: number): void {
    this.live.resumed(now);
    this.probeOut = null;
  }

  /** The link's verdict now; a suspect link gets a probe. */
  verdict(now: number): Verdict {
    const v = this.live.verdict(now);
    if (v === "suspect" && this.phase === "open" && !(this.probeOut !== null && this.live.probing())) this.probeWant = true;
    return v;
  }

  /** When the connection next needs looking at. */
  pollTimeout(now: number): number {
    return Math.min(this.lastSent + this.cfg.idleMs, this.live.nextChange(now));
  }

  /** Bytes to send, at most `room`; null when nothing is due. */
  pollTransmit(room: number, now: number): Transmit | null {
    if (this.phase === "open") {
      this.verdict(now);
      for (const [sub, upto] of this.credits) this.queue({ kind: "Credit", body: { sub, upto } });
      this.credits.clear();
      for (const [sub, r] of this.resizes) this.queue({ kind: "Resize", body: { sub, cols: r.cols, rows: r.rows } }, "interactive");
      this.resizes.clear();
      if (this.up.length === 0 && now - this.lastSent >= this.cfg.idleMs) this.idleDue = true;
    }
    const parts: { kind: Frame["kind"]; lane: Sent["lane"]; b: Uint8Array }[] = [];
    const version = this.phase === "opening" ? this.cfg.version : this.version;
    if (this.probeWant && this.phase === "open") {
      this.nonce = (this.nonce + 1) >>> 0;
      parts.push({ kind: "Probe", lane: "datagram", b: encodeFrame({ kind: "Probe", body: { nonce: this.nonce } }, version) });
      this.probeWant = false;
      this.probeOut = { nonce: this.nonce, at: now };
      this.live.probeSent(now);
    }
    for (const a of this.acks.splice(0)) parts.push({ kind: "ProbeAck", lane: "datagram", b: encodeFrame({ kind: "ProbeAck", body: a }, version) });
    let used = parts.reduce((n, p) => n + p.b.length, 0);
    const order = [...this.up.keys()].sort((a, b) => Number(this.up[a]!.prio !== "interactive") - Number(this.up[b]!.prio !== "interactive") || a - b);
    const taken = new Set<number>();
    for (const i of order) {
      const { f } = this.up[i]!;
      if (this.phase === "opening" && f.kind !== "Open") continue;
      const b = encodeFrame(f, version);
      if (b.length > this.fragment && f.kind !== "Open") {
        taken.add(i);
        const room = Math.max(1, this.fragment - 13);
        const n = Math.max(1, Math.ceil(b.length / room));
        for (let j = 0; j < n; j++) {
          const fb = encodeFrame({ kind: "Fragment", body: { lane: 0, last: j === n - 1, data: b.slice(j * room, (j + 1) * room) } }, version);
          parts.push({ kind: "Fragment", lane: "control", b: fb });
          used += fb.length;
        }
        continue;
      }
      if (used + b.length > room && used > 0) break;
      parts.push({ kind: f.kind, lane: f.kind === "Idle" ? "datagram" : "control", b });
      used += b.length;
      taken.add(i);
    }
    this.up = this.up.filter((_, i) => !taken.has(i));
    if (parts.length === 0 && this.idleDue && this.phase === "open") parts.push({ kind: "Idle", lane: "datagram", b: encodeFrame({ kind: "Idle", body: { got: this.got } }, version) });
    this.idleDue = false;
    if (parts.length === 0) return null;
    const bytes = new Uint8Array(parts.reduce((n, p) => n + p.b.length, 0));
    const frames: Sent[] = [];
    let at = 0;
    for (const p of parts) {
      bytes.set(p.b, at);
      frames.push({ kind: p.kind, lane: p.lane, start: at, len: p.b.length });
      at += p.b.length;
    }
    this.lastSent = now;
    return { bytes, frames };
  }
}

/** Whether a kind going up is urgent (the frame table's column). */
export function urgentUp(kind: Frame["kind"]): boolean {
  return URGENT_UP.has(kind);
}

/** kiln-bp's `Why`, by index, as `UnreadWhy`'s variants. */
const UNREAD_WHY: UnreadWhy["t"][] = ["CutShort", "TooLarge", "BadVarint", "BadValue", "TooDeep", "UnknownKind", "UnknownVariant", "LeftOver", "OutOfOrder"];
