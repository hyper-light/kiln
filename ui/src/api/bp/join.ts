/**
 * Fragments joined back into the frame they cut, as kiln-bp's `join.rs`: a lane's fragments come
 * in its order, `last` ends one frame, and the bytes joined are exactly one frame, never a
 * fragment or a compressed batch itself. A lane past the bound is let go and refused.
 */
import type { Fragment } from "./gen";
import { decodeBody, Frames, MAX_FRAME, type Outcome } from "./frames";
import { Why } from "./runtime";

const FRAGMENT = 14;
const COMPRESSED = 19;

export class Joiner {
  private lanes = new Map<number, { parts: Uint8Array[]; len: number }>();
  private readonly max: number;

  constructor(max = MAX_FRAME) {
    this.max = max;
  }

  held(): number {
    let n = 0;
    for (const l of this.lanes.values()) n += l.len;
    return n;
  }

  /** Takes a fragment: the frame it ends when it's `last`, else null. */
  take(f: Fragment): Outcome | null {
    let lane = this.lanes.get(f.lane);
    if (!lane) {
      lane = { parts: [], len: 0 };
      this.lanes.set(f.lane, lane);
    }
    if (lane.len + f.data.length > this.max + 16) {
      this.lanes.delete(f.lane);
      return { err: { kind: FRAGMENT, why: Why.TooLarge } };
    }
    lane.parts.push(f.data);
    lane.len += f.data.length;
    if (!f.last) return null;
    this.lanes.delete(f.lane);
    const whole = new Uint8Array(lane.len);
    let at = 0;
    for (const p of lane.parts) {
      whole.set(p, at);
      at += p.length;
    }
    return readOne(whole, this.max);
  }
}

/** Bytes that must be one frame, not a fragment or a compressed batch. */
export function readOne(whole: Uint8Array, max = MAX_FRAME): Outcome {
  const it = new Frames(whole, max);
  const first = it.nextRaw();
  if (first === null) return { err: { kind: FRAGMENT, why: Why.CutShort } };
  if ("err" in first) return first;
  if (it.nextRaw() !== null) return { err: { kind: first.kind, why: Why.LeftOver } };
  if (first.kind === FRAGMENT || first.kind === COMPRESSED) return { err: { kind: first.kind, why: Why.TooDeep } };
  return decodeBody(first.kind, whole, first.from, first.to);
}
