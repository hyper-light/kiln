import assert from "node:assert/strict";
import { test } from "node:test";

const { codecOf, initSegment, pictureSegment, units } = await import("../src/views/harness/live-source.ts");
const { typedUrl } = await import("../src/views/harness/live-of.ts");

/** A box's kind and size at `at`, and where its insides start. */
function box(b: Uint8Array, at: number): { kind: string; size: number; inside: number } {
  const size = new DataView(b.buffer, b.byteOffset).getUint32(at);
  return { kind: String.fromCharCode(...b.subarray(at + 4, at + 8)), size, inside: at + 8 };
}

/** The kinds of the boxes side by side from `from` to `to`, each as long as it says. */
function kinds(b: Uint8Array, from = 0, to = b.length): string[] {
  const out: string[] = [];
  for (let at = from; at < to; ) {
    const x = box(b, at);
    assert.ok(x.size >= 8 && at + x.size <= to, `${x.kind} at ${at} says ${x.size}`);
    out.push(x.kind);
    at += x.size;
  }
  return out;
}

const SPS = Uint8Array.from([0x67, 0x42, 0xe0, 0x1f, 0xda, 0x02, 0x80, 0xbf]);
const PPS = Uint8Array.from([0x68, 0xce, 0x3c, 0x80]);
const IDR = Uint8Array.from([0x65, 0x88, 0x84, 0x00, 0x33]);

test("a picture's units, after three- and four-byte start codes", () => {
  const annexb = Uint8Array.from([0, 0, 0, 1, ...SPS, 0, 0, 0, 1, ...PPS, 0, 0, 1, ...IDR]);
  const got = units(annexb).map((u) => Array.from(u));
  assert.deepEqual(got, [Array.from(SPS), Array.from(PPS), Array.from(IDR)]);
  assert.equal(codecOf(SPS), "avc1.42E01F");
});

test("the file's start: ftyp, then moov with its one track and its parameter sets", () => {
  const init = initSegment(SPS, PPS, 390, 844);
  assert.deepEqual(kinds(init), ["ftyp", "moov"]);
  const moov = box(init, box(init, 0).size);
  assert.deepEqual(kinds(init, moov.inside, moov.inside + moov.size - 8), ["mvhd", "trak", "mvex"]);
  // Its avcC carries both parameter sets as they are.
  const at = init.findIndex((_, i) => String.fromCharCode(...init.subarray(i, i + 4)) === "avcC");
  assert.ok(at > 0);
  const avcc = init.subarray(at + 4);
  assert.deepEqual(Array.from(avcc.subarray(0, 6)), [1, 0x42, 0xe0, 0x1f, 0xff, 0xe1]);
  assert.deepEqual(Array.from(avcc.subarray(8, 8 + SPS.length)), Array.from(SPS));
});

test("a picture as a fragment: its data where its run says, each unit after its length", () => {
  const seg = pictureSegment(3, 90_000 * 4000, true, [IDR]);
  assert.deepEqual(kinds(seg), ["moof", "mdat"]);
  const moof = box(seg, 0);
  const mdat = box(seg, moof.size);
  const view = new DataView(seg.buffer, seg.byteOffset);
  // trun's data offset (from the fragment's start) lands on mdat's insides.
  const trun = seg.findIndex((_, i) => String.fromCharCode(...seg.subarray(i, i + 4)) === "trun");
  const offset = view.getUint32(trun + 4 + 4 + 4);
  assert.equal(offset, mdat.inside);
  assert.equal(view.getUint32(offset), IDR.length);
  assert.deepEqual(Array.from(seg.subarray(offset + 4, offset + 4 + IDR.length)), Array.from(IDR));
  // Its time, past 2^32 ticks, whole.
  const tfdt = seg.findIndex((_, i) => String.fromCharCode(...seg.subarray(i, i + 4)) === "tfdt");
  assert.equal(view.getUint32(tfdt + 8) * 2 ** 32 + view.getUint32(tfdt + 12), 90_000 * 4000);
});

test("an address typed as one: its scheme kept, a host over https, a local one or a port over http, words searched", () => {
  assert.equal(typedUrl("https://github.com/a/b"), "https://github.com/a/b");
  assert.equal(typedUrl("  github.com/datavant/x/pull/3 "), "https://github.com/datavant/x/pull/3");
  assert.equal(typedUrl("localhost:3000/x"), "http://localhost:3000/x");
  assert.equal(typedUrl("localhost"), "http://localhost");
  assert.equal(typedUrl("192.168.1.77:7070"), "http://192.168.1.77:7070");
  assert.equal(typedUrl("10.0.0.5/admin"), "http://10.0.0.5/admin");
  assert.equal(typedUrl("myapp.local"), "http://myapp.local");
  assert.equal(typedUrl("devbox:8080"), "http://devbox:8080");
  assert.equal(typedUrl("example.com"), "https://example.com");
  assert.equal(typedUrl("about:blank"), "about:blank");
  assert.equal(typedUrl("kiln live pages"), "https://duckduckgo.com/?q=kiln%20live%20pages");
  assert.equal(typedUrl("intranet"), "https://duckduckgo.com/?q=intranet");
  assert.equal(typedUrl("   "), "");
});
