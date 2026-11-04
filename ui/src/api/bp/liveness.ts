/**
 * Liveness by bytes arriving (docs/browser-protocol.md, "Liveness"), as kiln-bp's `liveness.rs`
 * decides it, number for number: the peer promises bytes at least every idle period; how late
 * they come beyond it is learned (RFC 6298's smoothed mean and mean deviation), and so is the
 * round trip. Live while bytes are due, suspect once they're late (a probe goes first), dead only
 * when a suspect link stays silent a round trip past the probe. Judged from the arrival times
 * given, whenever asked: a page that was frozen judges from what arrived, not from timers that
 * didn't fire. Times are milliseconds on the caller's clock.
 */

/** RFC 6298's estimator, the mean times 8 and the deviation times 4, as TCP keeps them. */
export class Estimate {
  private srtt8 = 0;
  private var4 = 0;
  samples = 0;

  sample(x: number): void {
    if (this.samples === 0) {
      this.srtt8 = x * 8;
      this.var4 = x * 2;
    } else {
      const srtt = Math.floor(this.srtt8 / 8);
      const err = Math.abs(srtt - x);
      this.var4 = this.var4 - Math.floor(this.var4 / 4) + err;
      this.srtt8 = this.srtt8 - Math.floor(this.srtt8 / 8) + x;
    }
    this.samples++;
  }

  mean(): number {
    return Math.floor(this.srtt8 / 8);
  }

  deviation(): number {
    return Math.floor(this.var4 / 4);
  }

  /** The mean plus four deviations. */
  bound(): number {
    return this.mean() + this.var4;
  }
}

export type Verdict = "live" | "suspect" | "dead";

/** The floor on the learned spread and round trip: the clock's grain. */
export const GRAIN_MS = 1;

export class Liveness {
  private idle: number;
  private last: number;
  private readonly late = new Estimate();
  readonly rtt = new Estimate();
  private probed: number | null = null;

  constructor(idleMs: number, now: number) {
    this.idle = Math.max(idleMs, GRAIN_MS);
    this.last = now;
  }

  setIdle(idleMs: number): void {
    this.idle = Math.max(idleMs, GRAIN_MS);
  }

  idleMs(): number {
    return this.idle;
  }

  /** Bytes arrived at `now`. */
  arrived(now: number): void {
    const gap = Math.max(0, now - this.last);
    if (gap * 2 >= this.idle) this.late.sample(Math.max(0, gap - this.idle));
    this.last = Math.max(this.last, now);
    this.probed = null;
  }

  roundTrip(ms: number): void {
    this.rtt.sample(ms);
  }

  spread(): number {
    return this.late.samples === 0 ? this.idle : Math.max(this.late.bound(), GRAIN_MS);
  }

  rttBound(): number {
    return this.rtt.samples === 0 ? this.idle : Math.max(this.rtt.bound(), GRAIN_MS);
  }

  expectedBy(): number {
    return this.last + this.idle + this.spread();
  }

  probeSent(now: number): void {
    if (this.probed === null) this.probed = now;
  }

  probing(): boolean {
    return this.probed !== null;
  }

  deadAt(): number {
    const from = Math.max(this.probed ?? this.expectedBy(), this.expectedBy());
    return from + this.rttBound();
  }

  verdict(now: number): Verdict {
    if (now < this.expectedBy()) return "live";
    if (this.probed === null || now < this.deadAt()) return "suspect";
    return "dead";
  }

  /** When the verdict next changes if nothing arrives. */
  nextChange(now: number): number {
    const v = this.verdict(now);
    if (v === "live") return this.expectedBy();
    if (v === "suspect" && this.probed !== null) return this.deadAt();
    return now;
  }

  lastArrival(): number {
    return this.last;
  }
}
