/**
 * Seeded fuzz inputs made exactly as kiln-bp makes them (crates/kiln-bp/src/fuzz.rs): input `i`
 * of seed `s` from its own generator, so both languages make the same bytes, and the same digest
 * of what reading them came to. Used by the differential fuzzing (tests/bp-vectors.test.ts).
 */
import { arbFrame, KIND_INDEX, VERSION } from "./gen";
import { encodeFrame, Frames, decodeBody } from "./frames";
import { fnv64, Gen, Writer } from "./runtime";

export type Made = "random" | "valid" | "truncated" | "extended" | "flipped";
const MADE: Made[] = ["random", "valid", "truncated", "extended", "flipped"];

/** Input `i` of the run seeded `seed`. */
export function input(seed: bigint, i: number): { made: Made; msg: Uint8Array } {
  const g = Gen.forInput(seed, i);
  const made = MADE[Math.min(g.below(5), 4)]!;
  if (made === "random") {
    const n = g.below(64);
    const msg = new Uint8Array(n);
    for (let j = 0; j < n; j++) msg[j] = g.byte();
    return { made, msg };
  }
  const count = g.below(3) + 1;
  const parts: Uint8Array[] = [];
  for (let j = 0; j < count; j++) parts.push(encodeFrame(arbFrame(g), VERSION));
  let msg = concat(parts);
  if (made === "truncated") {
    msg = msg.slice(0, g.index(msg.length));
  } else if (made === "extended") {
    const n = g.below(8) + 1;
    const more = new Uint8Array(n);
    for (let j = 0; j < n; j++) more[j] = g.byte();
    msg = concat([msg, more]);
  } else if (made === "flipped") {
    const flips = g.below(3) + 1;
    for (let j = 0; j < flips; j++) {
      const at = g.index(msg.length);
      const bit = g.below(8);
      if (at < msg.length) msg[at] = msg[at]! ^ (1 << bit);
    }
  }
  return { made, msg };
}

function concat(parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/** What reading `msg` came to, written as kiln-bp's `fuzz::outcome` writes it. */
export function outcome(msg: Uint8Array): Uint8Array {
  const w = new Writer();
  const frames = new Frames(msg);
  for (let raw = frames.nextRaw(); raw !== null; raw = frames.nextRaw()) {
    const res = "err" in raw ? raw : decodeBody(raw.kind, frames.buf, raw.from, raw.to);
    if ("ok" in res) {
      w.u8(0);
      w.varint(KIND_INDEX[res.ok.frame.kind]);
      w.varint(res.ok.extra);
      w.bytes(encodeFrame(res.ok.frame, VERSION));
    } else {
      w.u8(1);
      w.varint(res.err.kind === null ? 0 : res.err.kind + 1);
      w.varint(res.err.why);
    }
  }
  return w.bytesOut();
}

const MASK64 = (1n << 64n) - 1n;

/** The digest of inputs `from` to `to` (not included) of a seed, as kiln-bp's `fuzz::digest`. */
export function digest(seed: bigint, from: number, to: number): bigint {
  let h = 0xcbf29ce484222325n;
  for (let i = from; i < to; i++) {
    let o = fnv64(outcome(input(seed, i).msg));
    for (let b = 0; b < 8; b++) {
      h = ((h ^ (o & 0xffn)) * 0x100000001b3n) & MASK64;
      o >>= 8n;
    }
  }
  return h;
}
