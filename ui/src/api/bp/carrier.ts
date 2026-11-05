/**
 * The protocol over one WebSocket, the browser's end (goal 3 of the browser protocol program, plan
 * 01a1a832eb2f711f9c6f51d88effe89d step 6; docs/browser-protocol.md, "The WebSocket carrier"): goal
 * 2's `Client` driven over `GET /api/live`, by whoever owns the browser's one socket (a SharedWorker,
 * or the tab holding the owner's Web Lock: `shared.ts`).
 *
 * - Opening: a ticket from `POST /api/live/ticket` (the session cookie goes with it), then the
 *   socket with the protocol's version as its subprotocol.
 * - Sending: one transmit at a time while `bufferedAmount` is under a chunk, so what waits waits in
 *   the client's own queue, where priorities apply (the WebSocket API has no backpressure of its own).
 * - Liveness: judged by the server's bytes arriving (the core's rule), asked when its own timeout
 *   says; a dead link is closed and opened again, resuming every subscription from its cursor and
 *   sending again every request not answered finally (`Client.reconnect`).
 */
import { Client, DEFAULT_CONFIG, type ClientConfig, type ClientEvent } from "./client";
import type { Call, Cursor, Topic } from "./gen";
import { VERSION } from "./gen";
import { Unpacker } from "./inflate";
import type { TopicState } from "./state";

/** The `Sec-WebSocket-Protocol` of a version (the server's `kiln-bp.<n>`). */
export const protocolOf = (v: number): string => `kiln-bp.${v}`;

/** The most `bufferedAmount` may hold before the next transmit waits: one chunk. */
const BUFFERED_MOST = 1200;
/** The room a transmit up is given. */
const ROOM_UP = 16 << 10;
/** Reconnects back off from half a second to five, jittered. */
const BACKOFF_MS = [500, 1000, 2000, 5000];

export type CarrierState = "connecting" | "open" | "closed";

/** What the owner of the socket hears. */
export type CarrierHooks = {
  event: (e: ClientEvent) => void;
  state: (s: CarrierState) => void;
};

/** Decodes a ticket in base64url. */
function unbase64url(s: string): Uint8Array {
  const b = atob(s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4));
  const out = new Uint8Array(b.length);
  for (let i = 0; i < b.length; i++) out[i] = b.charCodeAt(i);
  return out;
}

/** The socket's address on this origin. */
function liveUrl(path: string): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${location.host}${path}`;
}

/** What the owner's hub drives: the real `Carrier`, or a test's. */
export interface CarrierLike {
  readonly opened: number;
  start(): void;
  stop(): void;
  state(sub: number): TopicState | undefined;
  subscribe(sub: number, topic: Topic, cursor: Cursor | null): void;
  unsubscribe(sub: number): void;
  request(id: number, call: Call): void;
}

export class Carrier implements CarrierLike {
  private client: Client | null = null;
  private ws: WebSocket | null = null;
  private unpacker = new Unpacker();
  private attempts = 0;
  private wake: ReturnType<typeof setTimeout> | null = null;
  private stopped = false;
  private readonly cfg: ClientConfig;
  /** What the owner asked before the client existed, given to the first one. */
  private early: ((c: Client) => void)[] = [];
  readonly t0 = Date.now();
  /** Sockets opened (one unless the link dropped). */
  opened = 0;

  constructor(
    private readonly hooks: CarrierHooks,
    cfg: Partial<ClientConfig> = {},
  ) {
    this.cfg = { ...DEFAULT_CONFIG, ...cfg };
  }

  private now(): number {
    return Date.now() - this.t0;
  }

  /** Opens the socket (and opens it again whenever it drops, until `stop`). */
  start(): void {
    void this.open();
  }

  stop(): void {
    this.stopped = true;
    if (this.wake !== null) clearTimeout(this.wake);
    this.ws?.close(1000, "bye");
  }

  /** The client in use: what the owner subscribes and asks through. */
  with(f: (c: Client) => void): void {
    if (this.client) {
      f(this.client);
      this.pump();
    } else this.early.push(f);
  }

  /** A subscription's copy, as the client keeps it. */
  state(sub: number): TopicState | undefined {
    return this.client?.state(sub);
  }

  subscribe(sub: number, topic: Topic, cursor: Cursor | null): void {
    this.with((c) => (cursor === null ? c.subscribe(sub, topic, 0) : c.subscribeAt(sub, topic, cursor, 0)));
  }

  unsubscribe(sub: number): void {
    this.with((c) => c.unsubscribe(sub));
  }

  request(id: number, call: Call): void {
    this.with((c) => c.request(id, call));
  }

  private async open(): Promise<void> {
    if (this.stopped) return;
    this.hooks.state("connecting");
    let ticket: Uint8Array;
    try {
      const r = await fetch("/api/live/ticket", { method: "POST", credentials: "same-origin" });
      if (!r.ok) throw new Error(`ticket: ${r.status}`);
      ticket = unbase64url(((await r.json()) as { ticket: string }).ticket);
    } catch {
      return this.again();
    }
    const now = this.now();
    const client = this.client ? this.client.reconnect(this.cfg, ticket, now) : new Client(this.cfg, ticket, now);
    this.client = client;
    for (const f of this.early.splice(0)) f(client);
    this.unpacker = new Unpacker();
    const ws = new WebSocket(liveUrl("/api/live"), protocolOf(VERSION));
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.opened += 1;
    ws.onopen = () => {
      this.attempts = 0;
      this.pump();
    };
    ws.onmessage = (m) => {
      const bytes = new Uint8Array(m.data as ArrayBuffer);
      void this.unpacker.unpack(bytes).then(
        (plain) => {
          if (this.client !== client) return;
          for (const p of plain) client.receive(p, this.now());
          this.pump();
        },
        () => ws.close(4000, "a compressed batch didn't inflate"),
      );
    };
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.hooks.state("closed");
      this.again();
    };
  }

  private again(): void {
    if (this.stopped) return;
    const base = BACKOFF_MS[Math.min(this.attempts, BACKOFF_MS.length - 1)]!;
    this.attempts += 1;
    this.arm(base / 2 + Math.random() * base, () => void this.open());
  }

  private arm(ms: number, f: () => void): void {
    if (this.wake !== null) clearTimeout(this.wake);
    this.wake = setTimeout(() => {
      this.wake = null;
      f();
    }, ms);
  }

  /** Hands on what the client heard, sends what it has while the socket's buffer is low, and looks
   * again when its timeout says (an idle frame due, the link's verdict changing). */
  private pump(): void {
    const c = this.client;
    const ws = this.ws;
    if (!c) return;
    for (let e = c.pollEvent(); e !== undefined; e = c.pollEvent()) {
      if (e.t === "opened") this.hooks.state("open");
      this.hooks.event(e);
    }
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const now = this.now();
    if (c.verdict(now) === "dead") {
      ws.close(4001, "the link went silent past its probe");
      return;
    }
    while (ws.bufferedAmount < BUFFERED_MOST) {
      const t = c.pollTransmit(ROOM_UP, now);
      if (t === null) break;
      ws.send(t.bytes);
    }
    // Looked at again when the client says, or soon while the socket's buffer is high (the API says
    // nothing when it drains).
    const next = ws.bufferedAmount >= BUFFERED_MOST ? 20 : Math.max(1, c.pollTimeout(now) - now);
    this.arm(Math.min(next, 15_000), () => this.pump());
  }
}
