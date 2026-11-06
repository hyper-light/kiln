/**
 * Frames in a carrier message (docs/browser-protocol.md, "Frames"): kind, length, body, back to
 * back, read as kiln-bp reads them (crates/kiln-bp/src/frames.rs). Each frame is read whole or
 * refused with why; an unknown or malformed one is skipped by its length and the next read; a
 * length past the message refuses the rest of it. Nothing thrown escapes.
 */
import { KIND_INDEX, KINDS, putFrameBody, takeFrameBody, type Frame } from "./gen";
import { BpError, Reader, Why, Writer } from "./runtime";

/** A frame's body, whole, is at most this long. */
export const MAX_FRAME = 1 << 20;

/** A frame read, with how many bytes of a newer build's trailing fields it carried. */
export type Read = { frame: Frame; extra: number };
/** A frame that couldn't be read: its kind when that much was read, and why. */
export type Refused = { kind: number | null; why: Why };
export type Outcome = { ok: Read } | { err: Refused };

function whyOf(e: unknown): Why {
  if (e instanceof BpError) return e.why;
  throw e;
}

/** Reads the body of the kind at `kind`: whole or refused. */
export function decodeBody(kind: number, buf: Uint8Array, from: number, to: number): Outcome {
  if (kind >= KINDS.length) return { err: { kind, why: Why.UnknownKind } };
  const r = new Reader(buf, from, to);
  try {
    const frame = takeFrameBody(kind, r);
    return { ok: { frame, extra: r.left() } };
  } catch (e) {
    return { err: { kind, why: whyOf(e) } };
  }
}

/** The frames of one message, one at a time, as kiln-bp's `Frames`. */
export class Frames {
  private r: Reader;
  private stopped = false;
  private readonly max: number;

  constructor(msg: Uint8Array, max = MAX_FRAME) {
    this.r = new Reader(msg);
    this.max = max;
  }

  /** The next frame's kind and where its body is, unread; null at the end. */
  nextRaw(): { kind: number; from: number; to: number } | { err: Refused } | null {
    if (this.stopped || this.r.done()) return null;
    let kind: number;
    try {
      kind = this.r.raw();
    } catch (e) {
      this.stopped = true;
      return { err: { kind: null, why: whyOf(e) } };
    }
    if (kind < 0 || kind > 0xffffffff) {
      this.stopped = true;
      return { err: { kind: null, why: Why.BadVarint } };
    }
    let len: number;
    try {
      len = this.r.raw();
    } catch (e) {
      this.stopped = true;
      return { err: { kind, why: whyOf(e) } };
    }
    if (len < 0 || len > this.max) {
      this.stopped = true;
      return { err: { kind, why: Why.TooLarge } };
    }
    if (len > this.r.left()) {
      this.stopped = true;
      return { err: { kind, why: Why.CutShort } };
    }
    const from = this.r.pos;
    this.r.pos += len;
    return { kind, from, to: from + len };
  }

  /** Where the next frame starts. */
  at(): number {
    return this.r.pos;
  }

  get buf(): Uint8Array {
    return this.r.buf;
  }

  next(): Outcome | null {
    const raw = this.nextRaw();
    if (raw === null) return null;
    if ("err" in raw) return raw;
    return decodeBody(raw.kind, this.r.buf, raw.from, raw.to);
  }
}

/** Every frame of a message, read. */
export function decodeMessage(msg: Uint8Array, max = MAX_FRAME): Outcome[] {
  const f = new Frames(msg, max);
  const out: Outcome[] = [];
  for (let o = f.next(); o !== null; o = f.next()) out.push(o);
  return out;
}

/** Writes a frame (kind, length, body) for a peer speaking `version`. */
export function encodeInto(w: Writer, f: Frame, version: number): void {
  const body = new Writer();
  putFrameBody(body, f, version);
  w.varint(KIND_INDEX[f.kind]);
  w.bytes(body.bytesOut());
}

export function encodeFrame(f: Frame, version: number): Uint8Array {
  const w = new Writer();
  encodeInto(w, f, version);
  return w.bytesOut();
}

export function encodeMessage(frames: Frame[], version: number): Uint8Array {
  const w = new Writer();
  for (const f of frames) encodeInto(w, f, version);
  return w.bytesOut();
}
