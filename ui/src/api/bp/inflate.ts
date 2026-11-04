/**
 * Compressed batches inflated in order (docs/browser-protocol.md, "Compression"): each context
 * (`ctx`) has one long-lived `DecompressionStream("deflate-raw")`, the server sync-flushes after each
 * batch, and a batch says how many bytes it inflates to (`len`), so the stream's reader is read
 * until it has them and each batch is whole before the next is taken. A carrier passes each
 * message through `Unpacker.unpack` in arrival order and hands the plain messages it gives to
 * `Client.receive`. Messages with no compressed frame pass through untouched.
 */
import { Frames } from "./frames";
import { Reader } from "./runtime";

const COMPRESSED = 19;

/** One context's inflater. */
class Context {
  private writer: WritableStreamDefaultWriter<Uint8Array>;
  private reader: ReadableStreamDefaultReader<Uint8Array>;
  /** Bytes the stream gave past the last batch (none, when the server flushes each). */
  private spare: Uint8Array[] = [];

  constructor() {
    const ds = new DecompressionStream("deflate-raw");
    this.writer = ds.writable.getWriter();
    this.reader = ds.readable.getReader();
  }

  async inflate(data: Uint8Array, len: number): Promise<Uint8Array> {
    // Not awaited: the stream takes input as its reader is read.
    void this.writer.write(data);
    const out = new Uint8Array(len);
    let at = 0;
    while (at < len) {
      const chunk = this.spare.shift() ?? (await this.reader.read()).value;
      if (chunk === undefined) throw new Error("the inflater ended");
      const take = Math.min(chunk.length, len - at);
      out.set(chunk.subarray(0, take), at);
      at += take;
      if (take < chunk.length) this.spare.push(chunk.subarray(take));
    }
    return out;
  }
}

/** Reads past a varint, returning its value (small ones only: a ctx or a length). */
function small(r: Reader): number {
  return r.u53();
}

export class Unpacker {
  private contexts = new Map<number, Context>();
  /** Most bytes one batch may inflate to. */
  private readonly max: number;
  private chain: Promise<unknown> = Promise.resolve();

  constructor(max = 4 << 20) {
    this.max = max;
  }

  /** The plain messages `msg` holds, in order: the runs of frames between compressed batches as they came, each batch inflated. */
  unpack(msg: Uint8Array): Promise<Uint8Array[]> {
    const next = this.chain.then(() => this.run(msg));
    this.chain = next.catch(() => undefined);
    return next;
  }

  private async run(msg: Uint8Array): Promise<Uint8Array[]> {
    const out: Uint8Array[] = [];
    const frames = new Frames(msg);
    let from = 0;
    for (;;) {
      const start = frames.at();
      const raw = frames.nextRaw();
      if (raw === null || "err" in raw) break;
      if (raw.kind !== COMPRESSED) continue;
      if (start > from) out.push(msg.subarray(from, start));
      from = raw.to;
      const r = new Reader(msg, raw.from, raw.to);
      try {
        const ctx = r.u8();
        const len = small(r);
        const data = r.bytes();
        if (len > this.max) {
          out.push(msg.subarray(start, raw.to));
          continue;
        }
        let c = this.contexts.get(ctx);
        if (!c) {
          c = new Context();
          this.contexts.set(ctx, c);
        }
        out.push(await c.inflate(data, len));
      } catch {
        // Unreadable: the client core refuses it as it comes.
        out.push(msg.subarray(start, raw.to));
      }
    }
    if (from < msg.length) out.push(msg.subarray(from));
    return out;
  }
}
