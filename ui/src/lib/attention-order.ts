/** Visit boundary for attention plan 01a199e1b177752e84dad5dc990e6a15.
 * A visit's first snapshot fixes its order: reconnects and refreshes bring news without moving rows.
 * Only your own actions move rows, and only the rows they acted on (`place`).
 */
import type { AttentionPair } from "./attention-exposure";
export class AttentionOrder {
  private choice: { order: string[]; exposure?: string | null; pair?: AttentionPair | null } | undefined;
  take(next: string[]): string[] {
    return this.snapshot({ order: next }).order;
  }
  snapshot<T extends { order: string[]; exposure?: string | null; pair?: AttentionPair | null }>(next: T): T {
    if (this.choice === undefined) this.choice = { order: next.order, exposure: next.exposure, pair: next.pair };
    const value = { ...next, order: this.choice.order, exposure: this.choice.exposure };
    if (this.choice.pair) value.pair = this.choice.pair;
    else delete value.pair;
    return value;
  }
  /**
   * The rows acted on take their places from `reference` (the server's order once it has the action,
   * or the order before it, for Undo), each just before the first row `reference` puts after it;
   * every other row keeps its place. One `reference` doesn't rank leaves the order.
   */
  place(ids: readonly string[], reference: readonly string[]) {
    if (!this.choice) return;
    const moving = new Set(ids);
    const at = new Map(reference.map((id, i) => [id, i]));
    const order = this.choice.order.filter((id) => !moving.has(id));
    for (const id of moving) {
      const mine = at.get(id);
      if (mine === undefined) continue;
      const before = order.findIndex((other) => (at.get(other) ?? Number.POSITIVE_INFINITY) > mine);
      order.splice(before < 0 ? order.length : before, 0, id);
    }
    this.choice = { ...this.choice, order };
  }
  /** The order shown now. */
  get order(): readonly string[] {
    return this.choice?.order ?? [];
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
