/**
 * What a long reasoning block costs the page's main thread, measured as T3 Code measured theirs
 * (pingdotgg/t3code#30038: "long reasoning previews format in linear time", 600 ms to 1.3 ms on a
 * 150,220-character entry; formatter time only, Node, 21 interleaved repeats, the median).
 *
 * kiln's counterparts:
 * - collapsed: a collapsed thought's row decides only whether it has anything to show
 *   (views/harness/thinking.ts); it formats no preview of its text;
 * - streaming: each update of a reply or thought still coming is cut into blocks (lib/markdown-blocks.ts),
 *   and only a block that changed is parsed again;
 * - open: the whole text through markdown (react-markdown's pipeline: remark-parse, remark-gfm, remark-rehype).
 *
 *   npx tsx --tsconfig tsconfig.app.json bench/reasoning.bench.ts [--json]
 */
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkGfm from "remark-gfm";
import remarkRehype from "remark-rehype";
import { BlockCutter, streamBlocks } from "../src/lib/markdown-blocks.ts";
import { thoughtShown, type ThoughtItem } from "../src/views/harness/thinking.ts";

const REPEATS = 21;

/** A deterministic stream of numbers: the same text every run. */
function lcg(seed: number) {
  let s = BigInt(seed);
  return () => {
    s = (s * 6364136223846793005n + 1442695040888963407n) & 0xffffffffffffffffn;
    return Number(s >> 33n);
  };
}
const WORDS =
  "the retry loop reads its deadline from the same clock as the backoff so a slow tick can no longer put the deadline before the first attempt which means the socket comes back within one interval and every viewer keeps its place while the snapshot is written".split(
    " ",
  );

/** Reasoning as a model writes it: paragraphs of prose, now and then a list, inline code, a short fenced block. `code` adds a regular expression's character class in code. */
function reasoning(chars: number, code = false): string {
  const r = lcg(7);
  const out: string[] = [];
  let n = 0;
  while (n < chars) {
    const kind = r() % 10;
    let block: string;
    if (kind < 7) {
      const words = 40 + (r() % 80);
      block = Array.from({ length: words }, (_, i) => (i % 17 === 5 ? "`" + WORDS[r() % WORDS.length] + "()`" : WORDS[r() % WORDS.length])).join(" ") + ".";
    } else if (kind < 9) {
      block = Array.from({ length: 3 + (r() % 4) }, () => "- " + Array.from({ length: 6 + (r() % 12) }, () => WORDS[r() % WORDS.length]).join(" ")).join("\n");
    } else {
      block = "```rust\nlet wait = backoff.next(now).min(deadline - now);\n" + (code ? 'let re = Regex::new(r"[^a-z0-9]+")?;\n' : "") + "```";
    }
    out.push(block);
    n += block.length + 2;
  }
  return out.join("\n\n").slice(0, chars);
}

const median = (xs: number[]) => {
  const s = [...xs].sort((a, b) => a - b);
  return s[(s.length - 1) >> 1]!;
};
/** `fn`'s time in ms: the median of REPEATS runs, the cases interleaved by the caller. */
function timed(fn: () => unknown): number {
  const t0 = performance.now();
  fn();
  return performance.now() - t0;
}

const pipeline = unified().use(remarkParse).use(remarkGfm).use(remarkRehype);
const parse = (md: string) => pipeline.runSync(pipeline.parse(md));

const thought = (text: string) => ({ type: "thought", event: { id: "t", seq: 1, at: 0, kind: "thinking", text, streaming: false } }) as unknown as ThoughtItem;

/** A text arriving in steps: each prefix cut into blocks, only the blocks that changed parsed (what React's memo of a block leaves). */
function stream(text: string, step: number, parseToo: boolean): { cut: number; parsed: number; blocksParsed: number } {
  let cut = 0;
  let parsed = 0;
  let blocksParsed = 0;
  let before: string[] = [];
  // One cutter for the text's whole coming, as the page's streaming body keeps one (views/harness/prose.tsx).
  const cutter = new BlockCutter();
  for (let n = step; n <= text.length; n += step) {
    const prefix = text.slice(0, n);
    const t0 = performance.now();
    const blocks = cutter.cut(prefix);
    cut += performance.now() - t0;
    if (parseToo) {
      const t1 = performance.now();
      blocks.forEach((b, i) => {
        if (before[i] !== b) {
          parse(b);
          blocksParsed++;
        }
      });
      parsed += performance.now() - t1;
    }
    before = blocks;
  }
  return { cut, parsed, blocksParsed };
}

const cases: { name: string; run: () => number; note: string }[] = [];
const real = reasoning(150_220);
const prose96 = reasoning(98_304);
const code96 = reasoning(98_304, true);
const item = thought(real);

cases.push({ name: "collapsed thought, 150,220 chars: what its row formats", run: () => timed(() => thoughtShown(item)), note: "T3 Code: 1.3 ms (was 599.8)" });
cases.push({ name: "streaming 96 KiB in 4 KiB steps, cutting into blocks (sum of 24)", run: () => stream(prose96, 4096, false).cut, note: "T3 Code: 12.9 ms (was 8,658.4)" });
cases.push({ name: "the same, with a regex's character class in a code block", run: () => stream(code96, 4096, false).cut, note: "" });
cases.push({
  name: "streaming 96 KiB in 4 KiB steps, cut and the changed blocks parsed (sum)",
  run: () => {
    const s = stream(prose96, 4096, true);
    return s.cut + s.parsed;
  },
  note: "",
});
cases.push({
  name: "the same, with a regex's character class in a code block",
  run: () => {
    const s = stream(code96, 4096, true);
    return s.cut + s.parsed;
  },
  note: "",
});
cases.push({ name: "opening it: 150,220 chars parsed whole", run: () => timed(() => parse(real)), note: "T3 Code: about 39 ms to parse (not in their figure)" });
cases.push({ name: "opening it: cut into blocks, the first 12 parsed", run: () => timed(() => streamBlocks(real).slice(0, 12).forEach(parse)), note: "" });

// Warm up, then every case once a round, REPEATS rounds.
for (const c of cases) c.run();
const times = cases.map(() => [] as number[]);
for (let i = 0; i < REPEATS; i++) cases.forEach((c, k) => times[k]!.push(c.run()));
const rows = cases.map((c, k) => ({ case: c.name, median_ms: Number(median(times[k]!).toFixed(4)), min_ms: Number(Math.min(...times[k]!).toFixed(4)), note: c.note }));
if (process.argv.includes("--json")) console.log(JSON.stringify({ node: process.version, repeats: REPEATS, rows }));
else {
  console.log(`node ${process.version}, ${REPEATS} interleaved repeats, median (min)`);
  for (const r of rows) console.log(`${r.median_ms.toFixed(3).padStart(10)} ms (${r.min_ms.toFixed(3)})  ${r.case}${r.note ? `   [${r.note}]` : ""}`);
  const s = stream(code96, 4096, true);
  const p = stream(prose96, 4096, true);
  console.log(`blocks parsed over the stream: prose ${p.blocksParsed}, with the character class ${s.blocksParsed}; blocks in the 150 KB text: ${streamBlocks(real).length}`);
}
