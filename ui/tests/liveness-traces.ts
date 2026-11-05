// bp-g9 step 2 (goal 9): seeded link traces through the liveness rule (src/api/bp/liveness.ts,
// kiln-bp's liveness.rs number for number). crates/kiln-bp/tests/ui_liveness.rs runs the same
// traces, from the same generator, through the Rust rule, and both must end with the same digest.
//
// The link: 50 kbit/s down a FIFO bottleneck that holds up to 6 s of bytes, a one-way delay with
// jitter, and 5% of packets lost each way. The server opens with 1 MB in flight (written as the
// bottleneck has room), then sends bursts now and then and an idle frame whenever it has written
// nothing for its idle period. TCP below: a lost packet holds everything after it (the page sees
// bytes in order); it's sent again once a later packet's arrival reaches the sender (a round trip,
// plus a quarter of it for reordering), or else on its retransmission timer, which doubles while
// it keeps being lost. Retransmissions queue behind the bottleneck like anything else. The page
// records each arrival and judges when the rule's next change comes (or when its timers run: a
// hidden tab's once a minute; a frozen page's not at all until it resumes, when what came while
// it slept is read at once). A suspect link gets a probe, which the server answers at once; the
// answer's arrival is a round trip. Blackholed traces lose everything both ways from a moment on.
import { Liveness, type Verdict } from "../src/api/bp/liveness.ts";

/** A 32-bit generator both languages compute the same (mulberry32). */
export class Rng {
  private s: number;
  constructor(seed: number) {
    this.s = seed >>> 0;
  }
  next(): number {
    this.s = (this.s + 0x6d2b79f5) >>> 0;
    let t = this.s;
    t = Math.imul(t ^ (t >>> 15), t | 1) >>> 0;
    t = (t ^ (t + (Math.imul(t ^ (t >>> 7), t | 61) >>> 0))) >>> 0;
    return (t ^ (t >>> 14)) >>> 0;
  }
  /** 0 ..< n. */
  below(n: number): number {
    return n <= 1 ? 0 : this.next() % n;
  }
}

export type Page = "shown" | "hidden" | "frozen";

export interface Outcome {
  /** Dead verdicts while the link still delivered (each a reconnect). */
  falseDead: number;
  /** For a blackholed trace: when the page found it dead, and the rule's bound at the last arrival. */
  foundAt: number | null;
  bound: number | null;
  blackholeAt: number | null;
  /** Every verdict the page reached, as letters, folded into a digest both languages compute. */
  digest: number;
  arrivals: number;
}

const MTU = 1200;
const RATE_DIV = 25; // 50 kbit/s: a byte takes 4/25 ms.
const QUEUE_MS = 6000;
const LOSS_PCT = 5;
const END_MS = 600_000;
const BULK = 1 << 20;

/** Time on the bottleneck for `bytes`. */
const ser = (bytes: number) => Math.floor((bytes * 4 + RATE_DIV - 1) / RATE_DIV);

interface Pkt {
  seq: number;
  bytes: number;
  /** The probe it answers, when it's an answer. */
  answers: number | null;
  arrival: number | null;
  /** When it last went onto the bottleneck. */
  sentAt: number;
  tries: number;
  /** A retransmission is scheduled (so later arrivals don't schedule another this round). */
  again: boolean;
}

type Ev =
  | { t: number; n: number; k: "arrive"; p: Pkt }
  | { t: number; n: number; k: "rto"; p: Pkt; tries: number }
  | { t: number; n: number; k: "resend"; p: Pkt }
  | { t: number; n: number; k: "probe-at-server"; probe: number }
  | { t: number; n: number; k: "check" }
  | { t: number; n: number; k: "server" };

/** A small binary heap by (t, n). */
class Heap {
  private a: Ev[] = [];
  private n = 0;
  push(e: Omit<Ev, "n"> & { n?: number }): void {
    const ev = { ...e, n: this.n++ } as Ev;
    const a = this.a;
    a.push(ev);
    let i = a.length - 1;
    while (i > 0) {
      const p = (i - 1) >> 1;
      if (less(a[p]!, a[i]!)) break;
      [a[p], a[i]] = [a[i]!, a[p]!];
      i = p;
    }
  }
  pop(): Ev | undefined {
    const a = this.a;
    if (a.length === 0) return undefined;
    const top = a[0]!;
    const last = a.pop()!;
    if (a.length > 0) {
      a[0] = last;
      let i = 0;
      for (;;) {
        const l = 2 * i + 1;
        const r = l + 1;
        let m = i;
        if (l < a.length && less(a[l]!, a[m]!)) m = l;
        if (r < a.length && less(a[r]!, a[m]!)) m = r;
        if (m === i) break;
        [a[m], a[i]] = [a[i]!, a[m]!];
        i = m;
      }
    }
    return top;
  }
}
const less = (x: Ev, y: Ev) => x.t < y.t || (x.t === y.t && x.n < y.n);

const LETTER: Record<Verdict, number> = { live: 108, suspect: 115, dead: 100 };

/** One seeded trace. */
export function trace(seed: number, opts: { blackhole: boolean; page: Page }): Outcome {
  const g = new Rng(seed);
  const idle = 1000 + g.below(29_000);
  const owd = 30 + g.below(400);
  const jitter = 1 + g.below(120);
  const blackholeAt = opts.blackhole ? 20_000 + g.below(500_000) : null;
  const frozenFrom = opts.page === "frozen" ? 30_000 + g.below(300_000) : null;
  const frozenTo = frozenFrom === null ? null : frozenFrom + 10_000 + g.below(240_000);
  const gone = (t: number) => blackholeAt !== null && t >= blackholeAt;

  const q = new Heap();
  // The bottleneck: when it's next free, and the last arrival it gave (FIFO).
  let linkFree = 0;
  let lastArrive = 0;
  let bulkLeft = BULK;
  let lastWrite = 0;
  let nextBurst = 1000 + g.below(idle * 3);
  let seq = 0;
  // The server's view of the round trip (for its retransmission timer).
  let srtt = 2 * owd + ser(MTU);
  let rttvar = Math.floor(srtt / 2);
  const sent: Pkt[] = [];
  const lost = new Set<Pkt>();

  const write = (t: number, bytes: number, answers: number | null) => {
    const p: Pkt = { seq: seq++, bytes, answers, arrival: null, sentAt: 0, tries: 0, again: false };
    sent.push(p);
    transmit(t, p);
    lastWrite = t;
  };
  const transmit = (t: number, p: Pkt) => {
    const start = Math.max(t, linkFree);
    p.sentAt = start;
    linkFree = start + ser(p.bytes);
    const at = Math.max(linkFree + owd + g.below(jitter), lastArrive);
    lastArrive = at;
    if (gone(t) || g.below(100) < LOSS_PCT) {
      lost.add(p);
      p.again = false;
      const rto = Math.max(200, srtt + 4 * rttvar) * 2 ** Math.min(p.tries, 10);
      q.push({ t: linkFree + rto, k: "rto", p, tries: p.tries });
    } else q.push({ t: at, k: "arrive", p });
  };
  const resend = (t: number, p: Pkt) => {
    if (!lost.has(p) || p.again) return;
    p.again = true;
    q.push({ t, k: "resend", p });
  };

  // The page.
  let live = new Liveness(idle, 0);
  live.roundTrip(2 * owd + ser(200));
  let delivered = 0; // next seq the page waits for
  const held = new Map<number, Pkt>();
  let probeAt: number | null = null;
  let probeN = 0;
  let falseDead = 0;
  let foundAt: number | null = null;
  let bound: number | null = null;
  let digest = 0x811c9dc5;
  let arrivals = 0;
  let checkAt = -1;
  // Arrivals a frozen page reads when it resumes.
  const pendingAt: [number, number | null][] = [];
  let resumedOnce = false;
  const fold = (v: Verdict) => (digest = Math.imul(digest ^ LETTER[v], 0x01000193) >>> 0);
  const running = (t: number) => frozenFrom === null || frozenTo === null || t < frozenFrom || t >= frozenTo;
  // When the page's timer for `want` actually runs.
  const runs = (want: number) => {
    let t = want;
    if (opts.page === "hidden") t = Math.ceil(t / 60_000) * 60_000;
    if (!running(t) && frozenTo !== null) t = frozenTo;
    return t;
  };
  const schedule = (now: number) => {
    const want = runs(Math.max(now + 1, live.nextChange(now)));
    if (want !== checkAt) {
      checkAt = want;
      q.push({ t: want, k: "check" });
    }
  };
  const judge = (now: number) => {
    const v = live.verdict(now);
    fold(v);
    if (v === "suspect" && !live.probing()) {
      live.probeSent(now);
      probeAt = now;
      const n = ++probeN;
      // Up: lost probes are sent again on the page's own timer, doubling.
      let at = now;
      let rto = Math.max(200, live.rttBound());
      while (g.below(100) < LOSS_PCT || gone(at)) {
        at += rto;
        rto *= 2;
        if (at > END_MS * 2) break;
      }
      if (at <= END_MS * 2) q.push({ t: at + owd + g.below(jitter), k: "probe-at-server", probe: n });
    }
    if (v === "dead") {
      if (blackholeAt !== null && now >= blackholeAt) {
        if (foundAt === null) foundAt = now;
        return false;
      }
      falseDead++;
      // Reconnect and resume: a new link judged afresh.
      live = new Liveness(idle, now);
      live.roundTrip(2 * owd + ser(200));
      probeAt = null;
    }
    schedule(now);
    return true;
  };

  q.push({ t: 0, k: "server" });
  schedule(0);
  // A blackholed trace runs on until the page finds it (or twice the trace's length).
  const end = blackholeAt === null ? END_MS : 2 * END_MS;
  for (let e = q.pop(); e && e.t <= end && foundAt === null; e = q.pop()) {
    const t = e.t;
    switch (e.k) {
      case "server": {
        // Bulk as the bottleneck has room; a burst when it's due; else an idle frame when one is.
        if (bulkLeft > 0 && linkFree - t <= QUEUE_MS) {
          const b = Math.min(MTU, bulkLeft);
          bulkLeft -= b;
          write(t, b, null);
        } else if (bulkLeft === 0 && t >= nextBurst) {
          const n = 1 + g.below(12);
          for (let i = 0; i < n; i++) write(t, i + 1 === n ? 100 + g.below(MTU - 100) : MTU, null);
          nextBurst = t + 1 + g.below(idle * 3);
        } else if (t - lastWrite >= idle) write(t, 24, null);
        const next = bulkLeft > 0 ? Math.max(t + 1, linkFree - QUEUE_MS) : Math.min(nextBurst, lastWrite + idle);
        q.push({ t: Math.max(t + 1, next), k: "server" });
        break;
      }
      case "probe-at-server":
        if (!gone(t)) write(t, 24, e.probe);
        break;
      case "rto":
        if (lost.has(e.p) && e.p.tries === e.tries) resend(t, e.p);
        break;
      case "resend":
        if (lost.has(e.p)) {
          lost.delete(e.p);
          e.p.tries++;
          transmit(t, e.p);
        }
        break;
      case "arrive": {
        const p = e.p;
        p.arrival = t;
        // A later packet's arrival tells the sender what's missing (SACK): it sends those again.
        const back = t + owd + Math.floor(srtt / 4);
        for (const m of lost) if (m.seq < p.seq) resend(back, m);
        if (p.tries === 0) {
          const sample = t + owd - p.sentAt;
          rttvar = Math.floor((3 * rttvar + Math.abs(sample - srtt)) / 4);
          srtt = Math.floor((7 * srtt + sample) / 8);
        }
        held.set(p.seq, p);
        const page = running(t) ? t : frozenTo!;
        let got = false;
        let answered: number | null = null;
        while (held.has(delivered)) {
          const d = held.get(delivered)!;
          held.delete(delivered);
          if (d.answers !== null && d.answers === probeN) answered = d.answers;
          delivered++;
          got = true;
        }
        if (got) {
          if (page === t) arrive(t, answered);
          else q.push({ t: page, k: "check" });
          if (page !== t) pendingAt.push([page, answered]);
        }
        break;
      }
      case "check": {
        if (e.t !== checkAt && pendingAt.length === 0) break;
        // A page that was frozen says it ran again before it reads what came meanwhile (goal 2's
        // `Liveness.resumed`): its own gap isn't the link's.
        if (frozenTo !== null && t >= frozenTo && !resumedOnce) {
          resumedOnce = true;
          live.resumed(frozenTo);
          probeAt = null;
        }
        for (const [at, ans] of pendingAt.splice(0)) arrive(at, ans);
        if (!judge(t)) break;
        break;
      }
    }
  }
  function arrive(t: number, answered: number | null) {
    arrivals++;
    if (answered !== null && probeAt !== null) live.roundTrip(t - probeAt);
    live.arrived(t);
    probeAt = null;
    // What the rule promises from its last arrival: found dead by then (in-flight bytes that land after the cut count).
    if (blackholeAt !== null) bound = live.deadAt();
    schedule(t);
  }
  return { falseDead, foundAt, bound, blackholeAt, digest, arrivals };
}
