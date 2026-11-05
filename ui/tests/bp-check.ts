/**
 * The shared vectors' checks, as a page runs them (e2e/bp-vectors.spec.ts bundles this into
 * Chromium and WebKit): every vector and join read as Rust read them, a block of the seeded
 * inputs to Rust's digest, and the deflate batches inflated whole, each before the next, through
 * one long-lived DecompressionStream("deflate-raw"). Returns what failed, by name.
 */
import { decodeMessage, encodeFrame } from "../src/api/bp/frames";
import { digest } from "../src/api/bp/fuzz";
import { VERSION, takeFragmentBody } from "../src/api/bp/gen";
import { Unpacker } from "../src/api/bp/inflate";
import { Joiner } from "../src/api/bp/join";
import { hex, Reader, toJson, unhex, WHY_NAMES, Writer } from "../src/api/bp/runtime";

type Outcome = Record<string, unknown>;
type Vectors = {
  vectors: { name: string; hex: string; outcomes: Outcome[] }[];
  joins: { name: string; fragments: { hex: string; times?: number }[]; outcomes: Outcome[] }[];
  inflate: { len: number; data: string; plain: string }[];
  fuzz: { seed: string; count: number; block: number; digests: string[] };
};

function said(o: ReturnType<typeof decodeMessage>[number]): string {
  if ("ok" in o) return toJson({ frame: o.ok.frame, extra: o.ok.extra, reencoded: hex(encodeFrame(o.ok.frame, VERSION)) });
  return toJson({ kind: o.err.kind, why: WHY_NAMES[o.err.why] });
}

export async function check(V: Vectors, fuzzBlocks: number): Promise<{ checked: number; failed: string[] }> {
  const failed: string[] = [];
  let checked = 0;
  for (const v of V.vectors) {
    checked++;
    if (toJson(decodeMessage(unhex(v.hex)).map(said)) !== toJson(v.outcomes.map(toJson))) failed.push(v.name);
  }
  for (const j of V.joins) {
    checked++;
    const joiner = new Joiner();
    const outs: string[] = [];
    let waiting = 0;
    for (const f of j.fragments) {
      const r = new Reader(unhex(f.hex));
      r.raw();
      r.raw();
      const frag = takeFragmentBody(r);
      for (let i = 0; i < (f.times ?? 1); i++) {
        const o = joiner.take(frag);
        if (o === null) {
          waiting++;
          continue;
        }
        if (waiting > 0) outs.push(toJson({ waiting }));
        waiting = 0;
        outs.push(said(o));
      }
    }
    if (waiting > 0) outs.push(toJson({ waiting }));
    if (toJson(outs) !== toJson(j.outcomes.map(toJson))) failed.push(j.name);
  }
  // Each batch whole before the next is given: one long-lived inflater.
  const u = new Unpacker();
  for (const [i, b] of V.inflate.entries()) {
    checked++;
    const w = new Writer();
    w.u8(2);
    w.varint(b.len);
    w.bytes(unhex(b.data));
    const body = w.bytesOut();
    const f = new Writer();
    f.varint(19);
    f.bytes(body);
    const got = await u.unpack(f.bytesOut());
    if (got.length !== 1 || hex(got[0]!) !== b.plain) failed.push(`inflate batch ${i}`);
  }
  const seed = BigInt(V.fuzz.seed);
  for (let b = 0; b < fuzzBlocks; b++) {
    checked++;
    if (digest(seed, b * V.fuzz.block, (b + 1) * V.fuzz.block).toString() !== V.fuzz.digests[b]) failed.push(`fuzz block ${b} (seed ${V.fuzz.seed})`);
  }
  return { checked, failed };
}
