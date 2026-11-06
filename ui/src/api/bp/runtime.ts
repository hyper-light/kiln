/**
 * The browser protocol's runtime (docs/browser-protocol.md, "Encoding"): the cursor the generated
 * codec (`gen.ts`) reads with, the writer it writes with, and the generator its fuzz draws come
 * from. It follows kiln-bp's rules exactly (crates/kiln-bp/src/wire.rs), which the shared vectors
 * and the differential fuzzing hold it to: a varint is at most 10 bytes and the tenth at most 1,
 * each type is refused past its range, every length is checked against the bytes left before
 * anything is taken, strings are strict UTF-8.
 *
 * A refusal is thrown as a `BpError` inside a frame's reading and caught where the frame is read
 * (`frames.ts`), which returns it as a value: nothing escapes to the caller.
 */

/** Why a frame couldn't be read, by kiln-bp's `Why` index. */
export const Why = {
  CutShort: 0,
  TooLarge: 1,
  BadVarint: 2,
  BadValue: 3,
  TooDeep: 4,
  UnknownKind: 5,
  UnknownVariant: 6,
  LeftOver: 7,
  OutOfOrder: 8,
} as const;
export type Why = (typeof Why)[keyof typeof Why];

export const WHY_NAMES = ["cut_short", "too_large", "bad_varint", "bad_value", "too_deep", "unknown_kind", "unknown_variant", "left_over", "out_of_order"] as const;

/** docs/wire.md's code for a reason: a newer build's kind or variant is `unknown`, the rest `malformed`. */
export function codeOf(why: Why): "unknown" | "malformed" {
  return why === Why.UnknownKind || why === Why.UnknownVariant ? "unknown" : "malformed";
}

/** A refusal, thrown inside a frame's reading. */
export class BpError extends Error {
  readonly why: Why;
  constructor(why: Why) {
    super(WHY_NAMES[why]);
    this.why = why;
  }
}

/** The largest integer a number holds exactly. */
export const MAX_SAFE = Number.MAX_SAFE_INTEGER;
const MAX_SAFE_BIG = BigInt(Number.MAX_SAFE_INTEGER);
const MASK64 = (1n << 64n) - 1n;
/** zigzag(±(2^53 - 1)) is at most this. */
const ZIGZAG_MOST = (1n << 54n) - 2n;
/** Strings this long or longer go through TextDecoder; shorter ones are decoded here, faster. */
const DECODER_FROM = 200;

const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
const encoder = new TextEncoder();

/** A cursor over bytes being read. */
export class Reader {
  readonly buf: Uint8Array;
  pos: number;
  end: number;

  constructor(buf: Uint8Array, pos = 0, end = buf.length) {
    this.buf = buf;
    this.pos = pos;
    this.end = end;
  }

  left(): number {
    return this.end - this.pos;
  }

  done(): boolean {
    return this.pos >= this.end;
  }

  u8(): number {
    if (this.pos >= this.end) throw new BpError(Why.CutShort);
    return this.buf[this.pos++]!;
  }

  /** A varint as kiln-bp reads it: its value when it is at most 2^53 - 1, else -1. */
  raw(): number {
    let v = 0;
    let mul = 1;
    for (let i = 0; i < 10; i++) {
      const b = this.u8();
      if (i === 9 && b > 1) throw new BpError(Why.BadVarint);
      v += (b & 0x7f) * mul;
      if ((b & 0x80) === 0) return v <= MAX_SAFE ? v : -1;
      mul *= 128;
    }
    throw new BpError(Why.BadVarint);
  }

  /** A varint of all 64 bits. */
  big(): bigint {
    let v = 0n;
    for (let i = 0; i < 10; i++) {
      const b = this.u8();
      if (i === 9 && b > 1) throw new BpError(Why.BadVarint);
      v |= BigInt(b & 0x7f) << BigInt(7 * i);
      if ((b & 0x80) === 0) return v;
    }
    throw new BpError(Why.BadVarint);
  }

  u16(): number {
    const v = this.raw();
    if (v < 0 || v > 0xffff) throw new BpError(Why.BadVarint);
    return v;
  }

  u32(): number {
    const v = this.raw();
    if (v < 0 || v > 0xffffffff) throw new BpError(Why.BadVarint);
    return v;
  }

  u53(): number {
    const v = this.raw();
    if (v < 0) throw new BpError(Why.BadVarint);
    return v;
  }

  i53(): number {
    const at = this.pos;
    const v = this.raw();
    if (v >= 0) return v % 2 === 0 ? v / 2 : -(v + 1) / 2;
    this.pos = at;
    const b = this.big();
    if (b > ZIGZAG_MOST) throw new BpError(Why.BadVarint);
    return Number(b & 1n ? -((b + 1n) >> 1n) : b >> 1n);
  }

  bits(): bigint {
    return this.big();
  }

  /** An enum's variant index: -1 when it's past any number (no variant has it). */
  variant(): number {
    return this.raw();
  }

  bool(): boolean {
    const b = this.u8();
    if (b === 0) return false;
    if (b === 1) return true;
    throw new BpError(Why.BadValue);
  }

  /** A count of things each a byte at least: never past the bytes left. */
  count(): number {
    const n = this.raw();
    if (n < 0 || n > this.left()) throw new BpError(Why.CutShort);
    return n;
  }

  bytes(): Uint8Array {
    const n = this.count();
    const out = this.buf.slice(this.pos, this.pos + n);
    this.pos += n;
    return out;
  }

  str(): string {
    const n = this.count();
    const from = this.pos;
    this.pos += n;
    if (n >= DECODER_FROM) {
      try {
        return decoder.decode(this.buf.subarray(from, from + n));
      } catch {
        throw new BpError(Why.BadValue);
      }
    }
    return utf8(this.buf, from, from + n);
  }

  opt<T>(f: () => T): T | null {
    const tag = this.u8();
    if (tag === 0) return null;
    if (tag === 1) return f();
    throw new BpError(Why.BadValue);
  }

  vec<T>(f: () => T): T[] {
    const n = this.count();
    const out: T[] = [];
    for (let i = 0; i < n; i++) out.push(f());
    return out;
  }
}

/** Strict UTF-8, as Rust's `str::from_utf8` reads it: no overlongs, surrogates or code points past U+10FFFF. */
function utf8(b: Uint8Array, from: number, to: number): string {
  let s = "";
  let i = from;
  const cont = (): number => {
    if (i >= to) throw new BpError(Why.BadValue);
    const c = b[i++]!;
    if ((c & 0xc0) !== 0x80) throw new BpError(Why.BadValue);
    return c & 0x3f;
  };
  while (i < to) {
    const c = b[i++]!;
    if (c < 0x80) {
      s += String.fromCharCode(c);
    } else if (c < 0xc2) {
      throw new BpError(Why.BadValue);
    } else if (c < 0xe0) {
      s += String.fromCharCode(((c & 0x1f) << 6) | cont());
    } else if (c < 0xf0) {
      const second = i < to ? b[i]! : 0;
      if ((c === 0xe0 && second < 0xa0) || (c === 0xed && second >= 0xa0)) throw new BpError(Why.BadValue);
      const c1 = cont();
      const c2 = cont();
      s += String.fromCharCode(((c & 0x0f) << 12) | (c1 << 6) | c2);
    } else if (c < 0xf5) {
      const second = i < to ? b[i]! : 0;
      if ((c === 0xf0 && second < 0x90) || (c === 0xf4 && second >= 0x90)) throw new BpError(Why.BadValue);
      const c1 = cont();
      const c2 = cont();
      const c3 = cont();
      s += String.fromCodePoint(((c & 0x07) << 18) | (c1 << 12) | (c2 << 6) | c3);
    } else {
      throw new BpError(Why.BadValue);
    }
  }
  return s;
}

/** Where encoded bytes go. */
export class Writer {
  buf: Uint8Array;
  len: number;

  constructor(size = 64) {
    this.buf = new Uint8Array(size);
    this.len = 0;
  }

  private room(n: number): void {
    if (this.len + n <= this.buf.length) return;
    let size = this.buf.length * 2;
    while (size < this.len + n) size *= 2;
    const next = new Uint8Array(size);
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }

  /** What's been written. */
  bytesOut(): Uint8Array {
    return this.buf.slice(0, this.len);
  }

  u8(b: number): void {
    this.room(1);
    this.buf[this.len++] = b & 0xff;
  }

  /** An unsigned varint of a number up to 2^53 - 1. */
  varint(v: number): void {
    this.room(8);
    while (v >= 0x80) {
      this.buf[this.len++] = (v % 128) | 0x80;
      v = Math.floor(v / 128);
    }
    this.buf[this.len++] = v;
  }

  big(v: bigint): void {
    this.room(10);
    v &= MASK64;
    while (v >= 0x80n) {
      this.buf[this.len++] = Number(v & 0x7fn) | 0x80;
      v >>= 7n;
    }
    this.buf[this.len++] = Number(v);
  }

  u16(v: number): void {
    this.varint(v);
  }

  u32(v: number): void {
    this.varint(v);
  }

  u53(v: number): void {
    this.varint(v);
  }

  i53(v: number): void {
    if (Math.abs(v) <= 2 ** 51) {
      this.varint(v >= 0 ? v * 2 : -v * 2 - 1);
    } else {
      const n = BigInt(v);
      this.big(n >= 0n ? n << 1n : (-n << 1n) - 1n);
    }
  }

  bits(v: bigint): void {
    this.big(v);
  }

  bool(v: boolean): void {
    this.u8(v ? 1 : 0);
  }

  bytes(b: Uint8Array): void {
    this.varint(b.length);
    this.room(b.length);
    this.buf.set(b, this.len);
    this.len += b.length;
  }

  str(s: string): void {
    if (s.length >= DECODER_FROM / 3) {
      this.bytes(encoder.encode(s));
      return;
    }
    // Short: encoded here. A UTF-16 unit is at most three bytes of UTF-8.
    const at = this.len;
    this.room(1 + s.length * 3);
    let n = 0;
    const start = at + 1;
    for (let i = 0; i < s.length; i++) {
      let c = s.charCodeAt(i);
      if (c >= 0xd800 && c < 0xdc00 && i + 1 < s.length) {
        const d = s.charCodeAt(i + 1);
        if (d >= 0xdc00 && d < 0xe000) {
          c = 0x10000 + ((c - 0xd800) << 10) + (d - 0xdc00);
          i++;
        }
      }
      if (c >= 0xd800 && c < 0xe000) c = 0xfffd;
      const o = start + n;
      if (c < 0x80) {
        this.buf[o] = c;
        n += 1;
      } else if (c < 0x800) {
        this.buf[o] = 0xc0 | (c >> 6);
        this.buf[o + 1] = 0x80 | (c & 0x3f);
        n += 2;
      } else if (c < 0x10000) {
        this.buf[o] = 0xe0 | (c >> 12);
        this.buf[o + 1] = 0x80 | ((c >> 6) & 0x3f);
        this.buf[o + 2] = 0x80 | (c & 0x3f);
        n += 3;
      } else {
        this.buf[o] = 0xf0 | (c >> 18);
        this.buf[o + 1] = 0x80 | ((c >> 12) & 0x3f);
        this.buf[o + 2] = 0x80 | ((c >> 6) & 0x3f);
        this.buf[o + 3] = 0x80 | (c & 0x3f);
        n += 4;
      }
    }
    // n < 128 for strings this short (under 67 units: at most 264 bytes, so maybe two length bytes).
    if (n < 0x80) {
      this.buf[at] = n;
      this.len = start + n;
    } else {
      const body = this.buf.slice(start, start + n);
      this.len = at;
      this.bytes(body);
    }
  }

  opt<T>(v: T | null, f: (x: T) => void): void {
    if (v === null) {
      this.u8(0);
    } else {
      this.u8(1);
      f(v);
    }
  }

  vec<T>(v: T[], f: (x: T) => void): void {
    this.varint(v.length);
    for (const x of v) f(x);
  }
}

const GAMMA = 0x9e3779b97f4a7c15n;

/** SplitMix64's finalizer. */
export function mix(z: bigint): bigint {
  z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & MASK64;
  z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & MASK64;
  return z ^ (z >> 31n);
}

/** The characters drawn strings are made of, as kiln-bp's `ALPHABET`. */
const ALPHABET = ["a", "b", "z", "0", "9", " ", '"', "\\", "\n", "\u0001", "é", "中", "😀"];

/**
 * SplitMix64, as kiln-sim and kiln-bp draw it, with the draws the generated `arb*` functions make:
 * one seed gives the same values here as in Rust (crates/kiln-bp/src/wire.rs, each type's `arb`).
 */
export class Gen {
  private s: bigint;

  constructor(seed: bigint) {
    this.s = seed & MASK64;
  }

  /** The generator for input `i` of a run seeded `seed`. */
  static forInput(seed: bigint, i: number): Gen {
    return new Gen(mix((seed + BigInt(i)) & MASK64));
  }

  next(): bigint {
    this.s = (this.s + GAMMA) & MASK64;
    return mix(this.s);
  }

  /** Below `n` (0 for `n` of 0). */
  below(n: number): number {
    if (n <= 0) return 0;
    return Number(this.next() % BigInt(n));
  }

  index(n: number): number {
    return this.below(n);
  }

  coin(): boolean {
    return this.below(2) === 1;
  }

  byte(): number {
    return this.below(256) & 0xff;
  }

  u8(): number {
    return this.byte();
  }

  u16(): number {
    return this.coin() ? this.below(128) : this.below(65536);
  }

  u32(): number {
    switch (this.below(3)) {
      case 0:
        return this.below(128);
      case 1:
        return this.below(16384);
      default:
        return this.below(2 ** 32);
    }
  }

  u53(): number {
    switch (this.below(4)) {
      case 0:
        return this.below(128);
      case 1:
        return this.below(16384);
      case 2:
        return this.below(2 ** 32);
      default:
        return Number(this.next() & MAX_SAFE_BIG);
    }
  }

  i53(): number {
    const m = this.u53();
    const neg = this.coin();
    return neg && m !== 0 ? -m : m;
  }

  bits(): bigint {
    switch (this.below(3)) {
      case 0:
        return BigInt(this.below(256));
      case 1:
        return this.next();
      default:
        return 1n << BigInt(this.below(64));
    }
  }

  bool(): boolean {
    return this.coin();
  }

  str(): string {
    const n = this.below(12);
    let s = "";
    for (let i = 0; i < n; i++) s += ALPHABET[this.index(ALPHABET.length)];
    return s;
  }

  bytes(): Uint8Array {
    const n = this.below(16);
    const out = new Uint8Array(n);
    for (let i = 0; i < n; i++) out[i] = this.byte();
    return out;
  }

  opt<T>(f: () => T): T | null {
    return this.coin() ? f() : null;
  }

  vec<T>(f: () => T): T[] {
    const n = this.below(4);
    const out: T[] = [];
    for (let i = 0; i < n; i++) out.push(f());
    return out;
  }
}

/** FNV-1a, 64 bits. */
export function fnv64(b: Uint8Array): bigint {
  let h = 0xcbf29ce484222325n;
  for (let i = 0; i < b.length; i++) h = ((h ^ BigInt(b[i]!)) * 0x100000001b3n) & MASK64;
  return h;
}

const HEX = "0123456789abcdef";

export function hex(b: Uint8Array): string {
  let s = "";
  for (let i = 0; i < b.length; i++) s += HEX[b[i]! >> 4]! + HEX[b[i]! & 15]!;
  return s;
}

export function unhex(s: string): Uint8Array {
  const out = new Uint8Array(s.length >> 1);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** A value as kiln-bp's vectors write it: JSON.stringify's strings, `bits` as decimal strings, bytes as hex. */
export function toJson(v: unknown): string {
  if (v === null || v === undefined) return "null";
  switch (typeof v) {
    case "string":
      return JSON.stringify(v);
    case "number":
      return String(v);
    case "boolean":
      return v ? "true" : "false";
    case "bigint":
      return `"${v}"`;
    default:
      break;
  }
  if (v instanceof Uint8Array) return `"${hex(v)}"`;
  if (Array.isArray(v)) return `[${v.map(toJson).join(",")}]`;
  return `{${Object.entries(v as Record<string, unknown>)
    .map(([k, x]) => `${JSON.stringify(k)}:${toJson(x)}`)
    .join(",")}}`;
}
