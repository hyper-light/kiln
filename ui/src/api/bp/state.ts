/**
 * A topic's state kept without its record types, as kiln-bp's `state.rs` keeps it: the one
 * record's fields, a collection's items by key, a text or byte stream. Each field is its own
 * encoding (the generated codec reads it); an empty one is its type's default. The client core
 * keeps one per subscription; the page's typed code reads from it.
 */
import type { Delta, Key } from "./gen";
import { Reader, Writer, fnv64 } from "./runtime";

/** An item's key as the server names it: `n:<number>` or `s:<string>`. */
export type ItemKey = string;

export function itemKey(k: Key): ItemKey | null {
  if (k.t === "Num") return `n:${k.n}`;
  if (k.t === "Str") return `s:${k.s}`;
  return null;
}

export function keyOf(k: ItemKey): Key {
  return k.startsWith("n:") ? { t: "Num", n: Number(k.slice(2)) } : { t: "Str", s: k.slice(2) };
}

export type Misfit = "no_item" | "offset" | "key" | "not_text";

const EMPTY = new Uint8Array(0);

function setFields(into: Uint8Array[], mask: bigint, fields: Uint8Array[]): void {
  let i = 0;
  for (let bit = 0; bit < 64; bit++) {
    if ((mask >> BigInt(bit)) & 1n) {
      const f = fields[i++];
      if (f === undefined) return;
      while (into.length <= bit) into.push(EMPTY);
      into[bit] = f;
    }
  }
}

/** Appends `data` to a field encoded as `str` or `bytes`, at offset `at` of what it holds. */
function extendField(field: Uint8Array, at: number, data: Uint8Array): Uint8Array | Misfit {
  let held: Uint8Array = EMPTY;
  if (field.length > 0) {
    try {
      const r = new Reader(field);
      held = r.bytes();
      if (!r.done()) return "not_text";
    } catch {
      return "not_text";
    }
  }
  if (held.length !== at) return "offset";
  const joined = new Uint8Array(held.length + data.length);
  joined.set(held);
  joined.set(data, held.length);
  const w = new Writer(joined.length + 4);
  w.bytes(joined);
  return w.bytesOut();
}

export class TopicState {
  record: Uint8Array[] = [];
  items = new Map<ItemKey, Uint8Array[]>();
  text: Uint8Array[] = [];
  textLen = 0;
  textFrom: number | null = null;

  textEnd(): number {
    return (this.textFrom ?? 0) + this.textLen;
  }

  /** Applies a delta whose keys are resolved; a misfit says the subscription starts again. */
  apply(d: Delta): Misfit | null {
    switch (d.t) {
      case "Set":
        setFields(this.record, d.mask, d.fields);
        return null;
      case "Put": {
        const k = itemKey(d.key);
        if (k === null) return "key";
        this.items.set(k, d.fields.slice());
        return null;
      }
      case "Patch": {
        const k = itemKey(d.key);
        if (k === null) return "key";
        const item = this.items.get(k);
        if (!item) return "no_item";
        setFields(item, d.mask, d.fields);
        return null;
      }
      case "Remove": {
        const k = itemKey(d.key);
        if (k === null) return "key";
        this.items.delete(k);
        return null;
      }
      case "Append":
        if (this.textFrom === null) this.textFrom = d.at;
        if (d.at !== this.textEnd()) return "offset";
        this.text.push(d.data);
        this.textLen += d.data.length;
        return null;
      case "Extend": {
        const k = itemKey(d.key);
        if (k === null) return "key";
        const item = this.items.get(k);
        if (!item) return "no_item";
        while (item.length <= d.field) item.push(EMPTY);
        const f = extendField(item[d.field]!, d.at, d.data);
        if (typeof f === "string") return f;
        item[d.field] = f;
        return null;
      }
    }
  }

  /** The stream's bytes, joined. */
  textBytes(): Uint8Array {
    const out = new Uint8Array(this.textLen);
    let at = 0;
    for (const p of this.text) {
      out.set(p, at);
      at += p.length;
    }
    return out;
  }

  /** A digest of the state, as kiln-bp's `TopicState::digest`. */
  digest(): bigint {
    const w = new Writer();
    w.varint(this.record.length);
    for (const f of this.record) w.bytes(f);
    const keys = [...this.items.keys()].sort(compareKeys);
    w.varint(keys.length);
    for (const k of keys) {
      if (k.startsWith("n:")) {
        w.u8(0);
        w.varint(Number(k.slice(2)));
      } else {
        w.u8(1);
        w.bytes(new TextEncoder().encode(k.slice(2)));
      }
      const fields = this.items.get(k)!;
      w.varint(fields.length);
      for (const f of fields) w.bytes(f);
    }
    w.varint(this.textFrom ?? 0);
    w.bytes(this.textBytes());
    return fnv64(w.bytesOut());
  }
}

/** Keys in kiln-bp's order: numbers first, by value; then strings, by their UTF-8 bytes. */
function compareKeys(a: ItemKey, b: ItemKey): number {
  const an = a.startsWith("n:");
  const bn = b.startsWith("n:");
  if (an !== bn) return an ? -1 : 1;
  if (an) return Number(a.slice(2)) - Number(b.slice(2));
  const x = new TextEncoder().encode(a.slice(2));
  const y = new TextEncoder().encode(b.slice(2));
  for (let i = 0; i < Math.min(x.length, y.length); i++) if (x[i] !== y[i]) return x[i]! - y[i]!;
  return x.length - y.length;
}
