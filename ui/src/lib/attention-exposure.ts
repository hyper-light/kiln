/** Rendered paired visits for plan 01a199e1b177752e84dad5dc990e6a15; no clocks, timers or network. */
export interface AttentionPair {
  key: string;
  order_a: string[];
  order_b: string[];
  arm: 0 | 1;
  model_order: boolean;
}
export interface AttentionFrame {
  exposure: string;
  frame_revision: number;
  context: string;
  order_a: string[];
  order_b: string[];
  shown: string[];
  model_order: boolean;
}
export interface AttentionSelection {
  exposure: string;
  frame_revision: number;
  chosen_position: number;
}
export interface AttentionClose {
  exposure: string;
  frame_revision: number;
  key: string;
}
export const sameOrder = (a: readonly string[], b: readonly string[]) => a.length === b.length && a.every((id, i) => id === b[i]);

/** Only the learned rest can move. Needs, pins and other sections retain their actual slots. */
export function displayedPair(pair: AttentionPair, shown: readonly string[], eligible: ReadonlySet<string>, automatic: boolean) {
  const selected = pair.arm === 0 ? pair.order_a : pair.order_b;
  const other = pair.arm === 0 ? pair.order_b : pair.order_a;
  const slots = shown.filter((id) => eligible.has(id));
  const chosenRest = selected.filter((id) => eligible.has(id));
  const otherRest = other.filter((id) => eligible.has(id));
  const unique = new Set(shown).size === shown.length && new Set(selected).size === selected.length && new Set(other).size === other.length;
  const known = shown.every((id) => selected.includes(id) && other.includes(id));
  const consistent = unique && known && sameOrder(slots, chosenRest) && otherRest.length === slots.length && otherRest.every((id) => slots.includes(id));
  let at = 0;
  const alternate = consistent && automatic ? shown.map((id) => (eligible.has(id) ? (otherRest[at++] ?? id) : id)) : [...shown];
  return {
    order_a: pair.arm === 0 ? [...shown] : alternate,
    order_b: pair.arm === 1 ? [...shown] : alternate,
    consistent,
    model_order: pair.model_order && automatic && consistent && !sameOrder(shown, alternate),
  };
}

/** One open presentation. Ref updates do not manufacture no-click visits. */
export class AttentionPresentation {
  private frame: AttentionFrame | undefined;
  private pairKey: string | undefined;
  private controls: string | undefined;
  private invalid = false;
  private closed = false;
  constructor(private readonly send: { render: (frame: AttentionFrame) => void; close: (close: AttentionClose) => void; key: () => string }) {}

  render(input: { pair: AttentionPair; shown: string[]; eligible: ReadonlySet<string>; context: string; controls: string; automatic: boolean; allowed: boolean }) {
    if (this.pairKey !== input.pair.key) {
      this.close();
      this.pairKey = input.pair.key;
      this.controls = input.controls;
      this.frame = undefined;
      this.closed = false;
      this.invalid = !input.allowed;
    }
    if (this.closed) return;
    if (this.controls !== input.controls || !input.allowed) this.invalid = true;
    const projected = displayedPair(input.pair, input.shown, input.eligible, input.automatic);
    if (!projected.consistent) this.invalid = true;
    const body = {
      exposure: input.pair.key,
      context: input.context,
      order_a: projected.order_a,
      order_b: projected.order_b,
      shown: [...input.shown],
      model_order: projected.model_order && !this.invalid,
    };
    if (
      this.frame &&
      sameOrder(this.frame.shown, body.shown) &&
      sameOrder(this.frame.order_a, body.order_a) &&
      sameOrder(this.frame.order_b, body.order_b) &&
      this.frame.context === body.context &&
      this.frame.model_order === body.model_order
    )
      return;
    this.frame = { ...body, frame_revision: (this.frame?.frame_revision ?? 0) + 1 };
    this.send.render(this.frame);
  }

  choose(agent: string, actual: readonly string[]): AttentionSelection | undefined {
    if (this.closed || !this.frame) return;
    const position = this.frame.shown.indexOf(agent);
    if (position < 0 || !sameOrder(actual, this.frame.shown)) {
      this.invalidate();
      this.close();
      return;
    }
    this.closed = true;
    return { exposure: this.frame.exposure, frame_revision: this.frame.frame_revision, chosen_position: position };
  }

  invalidate() {
    this.invalid = true;
    if (!this.closed && this.frame?.model_order) {
      this.frame = { ...this.frame, frame_revision: this.frame.frame_revision + 1, model_order: false };
      this.send.render(this.frame);
    }
  }

  close() {
    if (this.closed || !this.frame) return;
    this.closed = true;
    this.send.close({ exposure: this.frame.exposure, frame_revision: this.frame.frame_revision, key: this.send.key() });
  }
}
