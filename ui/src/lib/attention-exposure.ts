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

/** What the list shows as it renders: its rows in order, those in the learned rest, and what it's shown with. */
export interface AttentionShown {
  shown: string[];
  eligible: ReadonlySet<string>;
  context: string;
  controls: string;
  automatic: boolean;
  allowed: boolean;
}
type Body = Omit<AttentionFrame, "frame_revision">;
const sameBody = (a: Body, b: Body) =>
  a.exposure === b.exposure && a.context === b.context && a.model_order === b.model_order && sameOrder(a.shown, b.shown) && sameOrder(a.order_a, b.order_a) && sameOrder(a.order_b, b.order_b);

/**
 * One visit's presentation. What the list shows is noted as it renders (no request); it's sent from
 * what you do: the frame shown with a pick or an organize action (`frame`, `choose`), and the last
 * one shown, with its close, with the next snapshot request after you leave (`close`).
 */
export class AttentionPresentation {
  private key: string | undefined;
  private controls: string | undefined;
  private invalid = false;
  private closed = false;
  private body: Body | undefined;
  private sent: AttentionFrame | undefined;

  /** What shows now: no request, so rendering it again costs nothing. */
  note(pair: AttentionPair | null | undefined, input: AttentionShown) {
    if (!pair) return;
    if (this.key !== pair.key) {
      this.key = pair.key;
      this.controls = input.controls;
      this.body = undefined;
      this.sent = undefined;
      this.closed = false;
      this.invalid = !input.allowed;
    }
    if (this.closed) return;
    if (this.controls !== input.controls || !input.allowed) this.invalid = true;
    const projected = displayedPair(pair, input.shown, input.eligible, input.automatic);
    if (!projected.consistent) this.invalid = true;
    this.body = {
      exposure: pair.key,
      context: input.context,
      order_a: projected.order_a,
      order_b: projected.order_b,
      shown: [...input.shown],
      model_order: projected.model_order && !this.invalid,
    };
  }

  /** The frame to send with an action: what shows, as a new revision when it isn't what was sent last. */
  frame(): AttentionFrame | undefined {
    if (this.closed || !this.body) return;
    if (this.sent && sameBody(this.sent, this.body)) return;
    this.sent = { ...this.body, frame_revision: (this.sent?.frame_revision ?? 0) + 1 };
    return this.sent;
  }

  /** A row picked from the list: the frame it was picked from (if not yet sent) and where it was in it. The pick ends the visit. */
  choose(agent: string): { frame?: AttentionFrame; selection?: AttentionSelection } {
    if (this.closed || !this.body) return {};
    const frame = this.frame();
    const shown = this.sent;
    const position = shown?.shown.indexOf(agent) ?? -1;
    if (!shown || position < 0) return { frame };
    this.closed = true;
    return { frame, selection: { exposure: shown.exposure, frame_revision: shown.frame_revision, chosen_position: position } };
  }

  /** Left without a pick: the last frame shown (if not yet sent), and its close. Once. */
  close(key: string): { frame?: AttentionFrame; close?: AttentionClose } {
    if (this.closed) return {};
    const frame = this.frame();
    this.closed = true;
    if (!this.sent) return {};
    return { frame, close: { exposure: this.sent.exposure, frame_revision: this.sent.frame_revision, key } };
  }
}
