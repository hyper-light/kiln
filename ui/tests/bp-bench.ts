/**
 * The browser's decode cost per frame (bp-g2 step 10): each recorded frame read by the protocol's
 * codec, against JSON.parse of what today's endpoint sends for the same operation, timed over many
 * repetitions (a browser's clock is coarse) in the page itself (e2e/bp-decode.spec.ts).
 */
import { decodeMessage } from "../src/api/bp/frames";
import { unhex } from "../src/api/bp/runtime";

type Frame = { seq: string; bp: string; json: string };
export type Row = { seq: string; frames: number; bp: [number, number, number]; json: [number, number, number] };

function pct(v: number[], q: number): number {
  const s = [...v].sort((a, b) => a - b);
  return s[Math.round((s.length - 1) * q)] ?? 0;
}

export function bench(frames: Frame[], reps = 200): Row[] {
  const by = new Map<string, { bp: number[]; json: number[] }>();
  let sink = 0;
  for (const f of frames) {
    const b = unhex(f.bp);
    const t0 = performance.now();
    for (let i = 0; i < reps; i++) sink += decodeMessage(b).length;
    const t1 = performance.now();
    for (let i = 0; i < reps; i++) sink += f.json.startsWith("{") ? Object.keys(JSON.parse(f.json) as object).length : f.json.length;
    const t2 = performance.now();
    const row = by.get(f.seq) ?? { bp: [], json: [] };
    row.bp.push(((t1 - t0) * 1000) / reps);
    row.json.push(((t2 - t1) * 1000) / reps);
    by.set(f.seq, row);
  }
  if (sink < 0) throw new Error("unreachable");
  return [...by.entries()].map(([seq, r]) => ({ seq, frames: r.bp.length, bp: [pct(r.bp, 0.5), pct(r.bp, 0.99), pct(r.bp, 0.999)], json: [pct(r.json, 0.5), pct(r.json, 0.99), pct(r.json, 0.999)] }));
}
