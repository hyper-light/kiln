/**
 * page_bytes.mjs on a small made-up build (goal 8, plan 01a1a832ebfd step 2): a page is the shell
 * plus its route, its waves are its critical fetches, and a seeded +10 KiB in one route's chunk
 * fails that page's budget, naming it.
 *
 *   node --test ci/ui/page_bytes.test.mjs
 */
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import { measure, over, ratchet } from "./page_bytes.mjs";

function build(seed = "") {
  const dist = mkdtempSync(path.join(process.env.TMPDIR ?? tmpdir(), "page-bytes-"));
  const assets = path.join(dist, "client", "assets");
  mkdirSync(assets, { recursive: true });
  const w = (f, s) => writeFileSync(path.join(assets, f), s);
  writeFileSync(
    path.join(dist, "client", "_shell.html"),
    `<html><head><link rel="stylesheet" href="/assets/index-AAAAAAAA.css"/><link rel="modulepreload" href="/assets/index-BBBBBBBB.js"/></head><body><script type="module" src="/assets/index-BBBBBBBB.js"></script></body></html>`,
  );
  w("index-AAAAAAAA.css", `@font-face{src:url(./geist-latin-wght-normal-CCCCCCCC.woff2)}body{color:red}`);
  w("geist-latin-wght-normal-CCCCCCCC.woff2", "font".repeat(100));
  w("react-DDDDDDDD.js", `export const r=${JSON.stringify("react ".repeat(500))};`);
  w(
    "index-BBBBBBBB.js",
    `const __vite__mapDeps=(i,m=__vite__mapDeps,d=(m.f||(m.f=["assets/_app.agents._id-EEEEEEEE.js","assets/markdown-FFFFFFFF.js","assets/parts-GGGGGGGG.css"])))=>i.map(i=>d[i]);
import{r}from"./react-DDDDDDDD.js";const a=()=>L(()=>import(\`./_app.agents._id-EEEEEEEE.js\`),__vite__mapDeps([0,1,2]));const b=()=>L(()=>import(\`./_app.index-HHHHHHHH.js\`),__vite__mapDeps([]));`,
  );
  w("_app.agents._id-EEEEEEEE.js", `import{m}from"./markdown-FFFFFFFF.js";import{x}from"./late-IIIIIIII.js";export const c=${JSON.stringify("conversation ".repeat(300) + seed)};`);
  w("_app.index-HHHHHHHH.js", `export const o="overview";`);
  w("markdown-FFFFFFFF.js", `export const m=${JSON.stringify("markdown ".repeat(300))};`);
  w("late-IIIIIIII.js", `export const x=1;`);
  w("parts-GGGGGGGG.css", `.parts{color:blue}`);
  return dist;
}

test("a page is the shell plus its route, in waves", () => {
  const dist = build();
  try {
    const m = measure(dist);
    const c = m.conversation;
    for (const f of [
      "_shell.html",
      "index-BBBBBBBB.js",
      "react-DDDDDDDD.js",
      "index-AAAAAAAA.css",
      "_app.agents._id-EEEEEEEE.js",
      "markdown-FFFFFFFF.js",
      "parts-GGGGGGGG.css",
      "late-IIIIIIII.js",
      "geist-latin-wght-normal-CCCCCCCC.woff2",
    ])
      assert.ok(c.files.includes(f), f);
    assert.ok(!m.overview.files.includes("markdown-FFFFFFFF.js"));
    // document, what it names, the route with its preloads, the import the preloads missed, the fonts.
    assert.equal(c.waves, 5);
    assert.equal(c.total_br, c.html_br + c.js_br + c.css_br + c.font_br);
    assert.ok(c.js_br > m.overview.js_br);
  } finally {
    rmSync(dist, { recursive: true, force: true });
  }
});

test("a seeded +10 KiB in a route's chunk fails that page's budget, naming it", () => {
  const before = build();
  const after = build(
    randomBytes(10 * 1024)
      .toString("base64url")
      .slice(0, 10 * 1024),
  );
  try {
    const budgets = ratchet(measure(before), { pages: {} }, 2048);
    assert.deepEqual(over(measure(before), budgets), []);
    const misses = over(measure(after), budgets);
    const pages = new Set(misses.map((m) => m[0]));
    assert.deepEqual([...pages], ["conversation"]);
    assert.ok(misses.some(([, k, b, v]) => k === "js_br" && v > b));
  } finally {
    rmSync(before, { recursive: true, force: true });
    rmSync(after, { recursive: true, force: true });
  }
});

test("budgets only move down", () => {
  const m = { p: { total_br: 100, js_br: 50, css_br: 10, font_br: 10, html_br: 30, requests: 5, waves: 3 } };
  const b = ratchet(m, { pages: { p: { first: { total_br: 90, js_br: 500 } } } }, 0);
  assert.equal(b.pages.p.first.total_br, 90);
  assert.equal(b.pages.p.first.js_br, 50);
});
