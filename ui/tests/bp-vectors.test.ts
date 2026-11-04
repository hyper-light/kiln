/**
 * bp-g2 steps 3 and 4 (plan 01a1a832eb04756a9f5a6c3fef8cf63a): the browser codec against kiln-bp's
 * shared vectors (bp-vectors.json, written by Rust): every vector reads to the same frames, extra
 * bytes, re-encodings and refusals; every frame kind round-trips byte for byte; the joins come out
 * the same; and a million seeded inputs, made here as Rust makes them, read to the same digests.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const { decodeMessage, encodeFrame } = await import("../src/api/bp/frames.ts");
const { Joiner } = await import("../src/api/bp/join.ts");
const { digest } = await import("../src/api/bp/fuzz.ts");
const { KINDS, SCHEMA_HASH, VERSION, takeFragmentBody } = await import("../src/api/bp/gen.ts");
const { Reader, hex, toJson, unhex, WHY_NAMES } = await import("../src/api/bp/runtime.ts");

type Outcome = { frame: unknown; extra: number; reencoded: string } | { kind: number | null; why: string } | { waiting: number };
type Vectors = {
  version: number;
  schema: string;
  vectors: { name: string; hex: string; outcomes: Outcome[] }[];
  joins: { name: string; fragments: { hex: string; times?: number }[]; outcomes: Outcome[] }[];
  fuzz: { seed: string; count: number; block: number; digests: string[] };
};

const V: Vectors = JSON.parse(readFileSync(new URL("./bp-vectors.json", import.meta.url), "utf8"));

/** What reading came to, in the vectors' own words. */
function said(o: ReturnType<typeof decodeMessage>[number]): string {
  if ("ok" in o) return toJson({ frame: o.ok.frame, extra: o.ok.extra, reencoded: hex(encodeFrame(o.ok.frame, VERSION)) });
  return toJson({ kind: o.err.kind, why: WHY_NAMES[o.err.why] });
}

test("the vectors are this build's: its version and schema", () => {
  assert.equal(V.version, VERSION);
  assert.equal(V.schema, SCHEMA_HASH.toString());
});

test("every vector reads as Rust read it", () => {
  let n = 0;
  for (const v of V.vectors) {
    const got = decodeMessage(unhex(v.hex)).map(said);
    assert.deepEqual(got, v.outcomes.map(toJson), v.name);
    n++;
  }
  assert.ok(n >= KINDS.length);
});

test("every frame kind round-trips byte for byte", () => {
  const seen = new Set<string>();
  for (const v of V.vectors) {
    for (const o of decodeMessage(unhex(v.hex))) {
      if (!("ok" in o)) continue;
      seen.add(o.ok.frame.kind);
      const again = decodeMessage(encodeFrame(o.ok.frame, VERSION));
      assert.equal(again.length, 1);
      assert.ok("ok" in again[0]!);
      assert.equal(hex(encodeFrame((again[0] as { ok: { frame: typeof o.ok.frame } }).ok.frame, VERSION)), hex(encodeFrame(o.ok.frame, VERSION)), v.name);
    }
  }
  for (const k of KINDS) assert.ok(seen.has(k), `no vector of ${k}`);
});

test("refusals carry Rust's codes", () => {
  const whys = new Set<string>();
  for (const v of V.vectors) for (const o of v.outcomes) if ("why" in o) whys.add(o.why);
  for (const j of V.joins) for (const o of j.outcomes) if ("why" in o) whys.add(o.why);
  for (const w of WHY_NAMES) if (w !== "out_of_order") assert.ok(whys.has(w), `no vector refused ${w}`);
});

test("fragments join as Rust joins them", () => {
  for (const j of V.joins) {
    const joiner = new Joiner();
    const outs: string[] = [];
    let waiting = 0;
    for (const f of j.fragments) {
      const b = unhex(f.hex);
      const r = new Reader(b);
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
    assert.deepEqual(outs, j.outcomes.map(toJson), j.name);
  }
});

test("a million seeded inputs read to Rust's digests (differential fuzzing)", () => {
  const seed = BigInt(V.fuzz.seed);
  const blocks = V.fuzz.count / V.fuzz.block;
  for (let b = 0; b < blocks; b++) {
    const from = b * V.fuzz.block;
    const got = digest(seed, from, from + V.fuzz.block).toString();
    assert.equal(
      got,
      V.fuzz.digests[b],
      `seed ${V.fuzz.seed}, inputs ${from} to ${from + V.fuzz.block - 1} read differently from Rust: replay them with ` +
        `KILN_BP_FUZZ_FROM=${from} KILN_BP_FUZZ_TO=${from + V.fuzz.block} cargo test -p kiln-bp --test vectors replay -- --nocapture`,
    );
  }
});
