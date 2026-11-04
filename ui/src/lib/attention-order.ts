/** Visit boundary for attention plan 01a199e1b177752e84dad5dc990e6a15.
 * Reconnects refresh information without moving rows; arrivals and the person's own actions may reorder.
 */
import type { AttentionPair } from "./attention-exposure";
export class AttentionOrder {
  private choice: { order: string[]; exposure?: string | null; pair?: AttentionPair | null } | undefined;
  take(next: string[], ownAction = false): string[] {
    return this.snapshot({ order: next }, ownAction).order;
  }
  snapshot<T extends { order: string[]; exposure?: string | null; pair?: AttentionPair | null }>(next: T, ownAction = false): T {
    if (this.choice === undefined || ownAction) this.choice = { order: next.order, exposure: next.exposure, pair: next.pair };
    const value = { ...next, order: this.choice.order, exposure: this.choice.exposure };
    if (this.choice.pair) value.pair = this.choice.pair;
    else delete value.pair;
    return value;
  }
  returned() {
    this.choice = undefined;
  }
}

/** Each arrival reads a fresh Suspense key, rather than rendering a previous visit's cached order. */
export class AttentionVisits {
  private visit = 0;
  private away = true;
  arrive(): number {
    if (this.away) {
      this.visit++;
      this.away = false;
    }
    return this.visit;
  }
  subscribed() {
    this.away = false;
  }
  returned() {
    this.away = true;
  }
  current(): number {
    return this.visit;
  }
}

/** A preload can precede controls; only subsequent human changes invalidate its unseen assignment. */
export class AttentionControls {
  private current: string | undefined;
  private drawn: string | undefined;
  private changed = false;
  get value() {
    return this.current;
  }
  bind(value: string) {
    if (this.current !== undefined && this.current !== value) this.changed = true;
    this.current = value;
    if (this.drawn === undefined) this.drawn = value;
  }
  sampled(value: string | undefined) {
    this.drawn = value ?? this.current;
    this.changed = value !== undefined && value !== this.current;
  }
  invalidate() {
    this.changed = true;
  }
  get allowed() {
    return this.current !== undefined && this.drawn === this.current && !this.changed;
  }
}
