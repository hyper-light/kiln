import assert from "node:assert/strict";
import { test } from "node:test";
import { unified } from "unified";
import remarkParse from "remark-parse";
import remarkGfm from "remark-gfm";
import remarkRehype from "remark-rehype";
import { streamBlocks } from "../src/lib/markdown-blocks.ts";

/** What markdown renders to (hast, as react-markdown makes it), without positions or the line breaks between blocks. */
function rendered(md: string): unknown[] {
  const p = unified().use(remarkParse).use(remarkGfm).use(remarkRehype);
  const tree = p.runSync(p.parse(md)) as { children: unknown[] };
  const strip = (n: unknown): unknown => {
    if (!n || typeof n !== "object") return n;
    const { children, ...rest } = n as { position?: unknown; children?: unknown[] };
    delete rest.position;
    return children ? { ...rest, children: children.map(strip) } : rest;
  };
  return tree.children.filter((c) => !(typeof c === "object" && c && (c as { type: string; value?: string }).type === "text" && !(c as { value: string }).value.trim())).map(strip);
}

/** Every way the text could be cut while it streams: each prefix (every `step` characters), cut, renders as the whole prefix does. */
function rendersAlike(md: string, step = 1) {
  for (let n = 1; n <= md.length; n += n + step > md.length && n < md.length ? md.length - n : step) {
    const prefix = md.slice(0, n);
    const blocks = streamBlocks(prefix);
    assert.equal(blocks.join(""), prefix, "the pieces are the text");
    assert.deepEqual(blocks.flatMap(rendered), rendered(prefix), `cut as ${JSON.stringify(blocks)}`);
  }
}

test("paragraphs are cut at the blank lines between them", () => {
  assert.deepEqual(streamBlocks("One.\n\nTwo.\n\nThree."), ["One.\n\n", "Two.\n\n", "Three."]);
  assert.deepEqual(streamBlocks("One.\nstill one.\n\n\nTwo."), ["One.\nstill one.\n\n\n", "Two."]);
});

test("a list keeps its items, loose or not, and what's indented under them", () => {
  assert.deepEqual(streamBlocks("- a\n\n- b\n\n- c"), ["- a\n\n- b\n\n- c"]);
  assert.deepEqual(streamBlocks("1. a\n\n2. b"), ["1. a\n\n2. b"]);
  assert.deepEqual(streamBlocks("- a\n\n  more of a\n\nAfter."), ["- a\n\n  more of a\n\n", "After."]);
  assert.deepEqual(streamBlocks("Before:\n\n- a\n- b\n\nAfter."), ["Before:\n\n- a\n- b\n\n", "After."]);
});

test("a fenced code block is never cut, even before it's closed", () => {
  assert.deepEqual(streamBlocks("```rust\nlet a = 1;\n\nlet b = 2;\n```\n\nAfter."), ["```rust\nlet a = 1;\n\nlet b = 2;\n```\n\n", "After."]);
  assert.deepEqual(streamBlocks("Before.\n\n~~~\na\n\nb"), ["Before.\n\n", "~~~\na\n\nb"]);
  assert.deepEqual(streamBlocks("````\na\n```\n\nstill code\n````\n\nAfter."), ["````\na\n```\n\nstill code\n````\n\n", "After."]);
});

test("text that defines links or footnotes, or holds raw HTML that runs past blank lines, is rendered whole", () => {
  assert.deepEqual(streamBlocks("See [the docs].\n\n[the docs]: https://example.com"), ["See [the docs].\n\n[the docs]: https://example.com"]);
  assert.deepEqual(streamBlocks("A claim.[^1]\n\nMore.\n\n[^1]: Source."), ["A claim.[^1]\n\nMore.\n\n[^1]: Source."]);
  assert.deepEqual(streamBlocks("<!--\n\nhidden\n\n-->\n\nShown."), ["<!--\n\nhidden\n\n-->\n\nShown."]);
  assert.deepEqual(streamBlocks("<PRE>\na\n\nb\n</PRE>\n\nAfter."), ["<PRE>\na\n\nb\n</PRE>\n\nAfter."]);
  rendersAlike("Before.\n\n<!-- a comment\n\nthat goes on -->\n\nAfter.");
});

test("a line still coming that's too short to tell waits", () => {
  assert.deepEqual(streamBlocks("One.\n\n-"), ["One.\n\n-"]);
  assert.deepEqual(streamBlocks("One.\n\n1."), ["One.\n\n1."]);
  assert.deepEqual(streamBlocks("One.\n\nTw"), ["One.\n\nTw"]);
  assert.deepEqual(streamBlocks("One.\n\nTwo"), ["One.\n\n", "Two"]);
});

test("every prefix of a reply renders cut as it does whole", () => {
  rendersAlike(
    [
      "### What I found",
      "",
      "The retry loop in `client.rs` read the clock **twice**, so a slow tick could put the deadline before the first attempt.",
      "",
      "- The first attempt now takes its deadline from the same instant.",
      "- The jitter comes from the test's seeded source.",
      "",
      "1. one",
      "",
      "2. two",
      "   still two",
      "",
      "```rust",
      "let now = clock.now();",
      "",
      "let deadline = now + budget;",
      "```",
      "",
      "> A quote",
      "> on two lines",
      "",
      "| a | b |",
      "|---|---|",
      "| 1 | 2 |",
      "",
      "---",
      "",
      "    indented code",
      "",
      "    more of it",
      "",
      "All 124 tests pass.",
    ].join("\n"),
  );
});

test("replies made of every kind of block render cut as they do whole, as they stream", () => {
  let seed = 7;
  const r = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648;
  const pick = <T>(xs: T[]) => xs[Math.floor(r() * xs.length)]!;
  const BLOCKS = [
    "A paragraph with `code`, **bold** and a [link](https://example.com).",
    "Two lines\nin one paragraph.",
    "- tight\n- list",
    "- loose\n\n- list",
    "1. first\n2. second",
    "3. starts at three\n\n4. loose",
    "- outer\n  - inner\n\n    inner's own paragraph",
    "- item with code:\n\n  ```sh\n  cargo test\n\n  echo done\n  ```",
    "```ts\nconst a = 1;\n\nconst b = 2;\n```",
    "~~~\nunclosed until later\n\n~~~",
    "> quoted\n> twice",
    "> quoted\n\n> again",
    "| a | b |\n|---|---|\n| 1 | 2 |",
    "## A heading",
    "Setext\n---",
    "***",
    "    indented\n\n    code",
    "- [ ] a task\n- [x] done",
    "Ends with two spaces  \nand a hard break.",
    "1) parenthesised\n2) list",
  ];
  for (let doc = 0; doc < 24; doc++) {
    const md = Array.from({ length: 3 + Math.floor(r() * 5) }, () => pick(BLOCKS)).join(pick(["\n\n", "\n\n", "\n\n\n", "\n"]));
    rendersAlike(md, 3);
  }
});
