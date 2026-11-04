#!/usr/bin/env node
/**
 * What each page of a UI build costs on the wire, from the build alone (no browser), held to its
 * budget (goal 8, plan 01a1a832ebfd step 2). A page is the shell (what _shell.html names, with what
 * that imports) plus its route: the route's chunk, the files its import preloads (`__vite__mapDeps`)
 * and what they import, plus the Latin fonts its stylesheets name. Sizes are Brotli's, as kiln sends
 * them (the build's own `.br` beside a file when there is one, else Brotli 11 made here).
 *
 *   node ci/ui/page_bytes.mjs [--dist ui/dist] [--json] [--check] [--budgets ci/ui/page_budgets.json]
 *                             [--ratchet]
 *
 * --check fails (exit 1) on any page over its budget, naming the page, the measure, its budget and
 * its bytes. --ratchet writes the budgets down to what this build measures (never up). Waves are the
 * page's critical path in fetches: the document, then what it names, then the route's code (asked as
 * the entry runs), then what that imports that wasn't asked with it, and the fonts after the code
 * renders text (the signed-in app renders only in the browser).
 */
import { existsSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { brotliCompressSync, constants } from "node:zlib";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(HERE, "..", "..");

/** The pages goal 8's lab loads (step 1), each as the routes it opens and the lazy chunks its first view loads. */
export const PAGES = {
  signin: { routes: ["_app.index"], lazy: ["login"], about: "/ signed out: the sign-in form" },
  overview: { routes: ["_app.index"], lazy: [], about: "/ signed in" },
  mission: { routes: ["_app.agents.index"], lazy: [], about: "/agents: Mission control" },
  conversation: { routes: ["_app.agents._id"], lazy: [], about: "/agents/:id: an agent's conversation" },
  runs: { routes: ["_app.ship"], lazy: [], about: "/runs, which lands on Ship (/ship)" },
  run: { routes: ["_app.runs._id.index"], lazy: [], about: "/runs/:id: a run with its jobs' logs" },
};

const STATIC_IMPORT = /(?:\bimport|\bexport)\s*(?:[\w*{}\s,$]*?\s*from\s*)?["'`]([^"'`]+?\.js)["'`]/g;
const DYNAMIC_IMPORT = /import\(\s*["'`]([^"'`]+?\.js)["'`]\s*\)\s*,\s*__vite__mapDeps\(\s*\[([\d,\s]*)\]\s*\)/g;
const DYNAMIC_BARE = /import\(\s*["'`]([^"'`]+?\.js)["'`]\s*\)/g;
const MAP_DEPS = /m\.f\s*=\s*(\[[^\]]*\])/;

/** `x` as a file of assets/: `./a.js`, `/assets/a.js`, `assets/a.js`, or a name the import map maps. */
function fileOf(spec, map) {
  const to = map?.[spec] ?? spec;
  return path.basename(to.split("?")[0]);
}

/** The import map the shell carries (goal 8 step 4), if it has one. */
export function importMap(html) {
  const m = html.match(/<script type="importmap"[^>]*>([\s\S]*?)<\/script>/);
  if (!m) return {};
  try {
    return JSON.parse(m[1]).imports ?? {};
  } catch {
    return {};
  }
}

/** Brotli 11 of a file as kiln sends it: the build's `.br` when it has one. */
function brSize(file, data) {
  const pre = `${file}.br`;
  if (existsSync(pre)) return readFileSync(pre).length;
  return brotliCompressSync(data, { params: { [constants.BROTLI_PARAM_QUALITY]: 11, [constants.BROTLI_PARAM_SIZE_HINT]: data.length } }).length;
}

/** Every JS chunk's static imports, dynamic imports (with the files each preloads) and size. */
export function graph(assets, map, { read = readFileSync } = {}) {
  const chunks = new Map();
  for (const f of readdirSync(assets)) {
    if (!f.endsWith(".js")) continue;
    const data = read(path.join(assets, f));
    const src = data.toString("utf8");
    const deps = (() => {
      const m = src.match(MAP_DEPS);
      if (!m) return [];
      try {
        return JSON.parse(m[1]).map((d) => fileOf(d, map));
      } catch {
        return [];
      }
    })();
    const imports = [...src.matchAll(STATIC_IMPORT)].map((m) => fileOf(m[1], map)).filter((d) => d !== f);
    const dynamic = new Map();
    for (const m of src.matchAll(DYNAMIC_IMPORT)) {
      const preloads = m[2]
        .split(",")
        .map((s) => s.trim())
        .filter(Boolean)
        .map((i) => deps[Number(i)])
        .filter(Boolean);
      dynamic.set(fileOf(m[1], map), preloads);
    }
    for (const m of src.matchAll(DYNAMIC_BARE)) if (!dynamic.has(fileOf(m[1], map))) dynamic.set(fileOf(m[1], map), []);
    chunks.set(f, { imports: [...new Set(imports)], dynamic, data });
  }
  return chunks;
}

/** A chunk's file by its name before the hash (`_app.agents._id` -> `_app.agents._id-AbC123xy.js`). */
function named(chunks, name) {
  const re = new RegExp(`^${name.replace(/[.$]/g, (c) => `\\${c}`)}-[\\w-]{8}\\.js$`);
  return [...chunks.keys()].filter((f) => re.test(f));
}

/** The files `roots` need, each at the wave it's asked in (a static import not asked already is a wave after its importer's). */
function waves(roots, wave, chunks, out) {
  const stack = roots.map((f) => [f, wave]);
  while (stack.length) {
    const [f, w] = stack.pop();
    if (out.has(f) && out.get(f) <= w) continue;
    out.set(f, w);
    for (const d of chunks.get(f)?.imports ?? []) if (!out.has(d) || out.get(d) > w + 1) stack.push([d, w + 1]);
  }
  return out;
}

/** Each page's files with their waves, and its bytes by type. */
export function measure(dist) {
  const client = path.join(dist, "client");
  const assets = path.join(client, "assets");
  const htmlPath = path.join(client, "_shell.html");
  const html = readFileSync(htmlPath, "utf8");
  const map = importMap(html);
  const chunks = graph(assets, map);
  const named_ = [...html.matchAll(/(?:href|src)="\/assets\/([^"]+?\.(?:js|css))"/g)].map((m) => m[1]);
  const size = new Map();
  const sizeOf = (f) => {
    if (!size.has(f)) {
      const p = f === "_shell.html" ? htmlPath : path.join(assets, f);
      size.set(f, existsSync(p) ? brSize(p, readFileSync(p)) : 0);
    }
    return size.get(f);
  };
  const shell = new Map([["_shell.html", 1]]);
  for (const f of named_) if (f.endsWith(".css")) shell.set(f, 2);
  waves(
    named_.filter((f) => f.endsWith(".js")),
    2,
    chunks,
    shell,
  );
  const out = {};
  for (const [page, def] of Object.entries(PAGES)) {
    const files = new Map(shell);
    // The routes' (and their first view's lazy parts') code is asked as the entry runs, with what
    // its import preloads.
    const asked = [];
    for (const name of [...def.routes, ...def.lazy]) {
      for (const f of named(chunks, name)) {
        asked.push(f);
        for (const c of chunks.values()) for (const d of c.dynamic.get(f) ?? []) asked.push(d);
      }
    }
    const fresh = asked.filter((f) => !files.has(f));
    for (const f of fresh) if (f.endsWith(".css")) files.set(f, 3);
    waves(
      fresh.filter((f) => f.endsWith(".js")),
      3,
      chunks,
      files,
    );
    const codeWave = Math.max(...files.values());
    // Fonts: the Latin cuts its stylesheets name (what English text uses), once the code renders.
    for (const f of [...files.keys()].filter((x) => x.endsWith(".css"))) {
      const css = readFileSync(path.join(assets, f), "utf8");
      for (const m of css.matchAll(/url\(\s*["']?(?:\.\/|\/assets\/)?([\w.-]+-latin-wght-normal-[\w-]+\.woff2)/g)) files.set(m[1], codeWave + 1);
    }
    const by = { html: 0, js: 0, css: 0, font: 0 };
    for (const f of files.keys()) {
      const kind = f === "_shell.html" ? "html" : f.endsWith(".js") ? "js" : f.endsWith(".css") ? "css" : "font";
      by[kind] += sizeOf(f);
    }
    out[page] = {
      html_br: by.html,
      js_br: by.js,
      css_br: by.css,
      font_br: by.font,
      total_br: by.html + by.js + by.css + by.font,
      requests: files.size,
      waves: Math.max(...files.values()),
      files: [...files.keys()].sort(),
    };
  }
  return out;
}

const MEASURES = ["total_br", "js_br", "css_br", "font_br", "html_br", "requests", "waves"];

/** Every page over its budget: [page, measure, budget, measured]. */
export function over(measured, budgets) {
  const misses = [];
  for (const [page, b] of Object.entries(budgets.pages ?? {})) {
    const m = measured[page];
    if (!m) {
      misses.push([page, "missing", 0, 0]);
      continue;
    }
    for (const k of MEASURES) if (typeof b.first?.[k] === "number" && m[k] > b.first[k]) misses.push([page, k, b.first[k], m[k]]);
  }
  return misses;
}

/** The budgets moved down to what was measured plus `headroom` bytes (never up). */
export function ratchet(measured, budgets, headroom) {
  const pages = { ...(budgets.pages ?? {}) };
  for (const [page, m] of Object.entries(measured)) {
    const first = { ...(pages[page]?.first ?? {}) };
    for (const k of MEASURES) {
      const want = k.endsWith("_br") ? m[k] + headroom : m[k];
      first[k] = typeof first[k] === "number" ? Math.min(first[k], want) : want;
    }
    pages[page] = { ...(pages[page] ?? {}), first };
  }
  return { ...budgets, pages };
}

const kib = (n) => `${(n / 1024).toFixed(1)} KiB`;

function main(argv) {
  const flag = (k, d) => {
    const i = argv.indexOf(k);
    return i < 0 ? d : argv[i + 1];
  };
  const dist = path.resolve(flag("--dist", path.join(ROOT, "ui", "dist")));
  const budgetsPath = path.resolve(flag("--budgets", path.join(HERE, "page_budgets.json")));
  const measured = measure(dist);
  if (argv.includes("--json")) console.log(JSON.stringify(measured, null, 1));
  else
    for (const [page, m] of Object.entries(measured))
      console.log(
        `${page.padEnd(13)} total ${kib(m.total_br).padStart(10)}  js ${kib(m.js_br).padStart(10)}  css ${kib(m.css_br).padStart(9)}  fonts ${kib(m.font_br).padStart(8)}  html ${kib(m.html_br).padStart(8)}  ${String(m.requests).padStart(3)} requests  ${m.waves} waves`,
      );
  const budgets = existsSync(budgetsPath) ? JSON.parse(readFileSync(budgetsPath, "utf8")) : { pages: {} };
  if (argv.includes("--ratchet")) {
    writeFileSync(budgetsPath, `${JSON.stringify(ratchet(measured, budgets, budgets.headroom_br ?? 2048), null, 2)}\n`);
    console.log(`budgets ratcheted: ${budgetsPath}`);
  }
  if (!argv.includes("--check")) return 0;
  const misses = over(measured, budgets);
  for (const [page, k, b, m] of misses) console.error(`page ${page} over budget: ${k} ${k.endsWith("_br") ? `${kib(m)} (${m} bytes) against ${kib(b)} (${b} bytes)` : `${m} against ${b}`}`);
  // Today's distance from the time targets' arithmetic (docs/browser-lab.md): reported, not failed.
  for (const [page, m] of Object.entries(measured)) {
    const t = budgets.targets?.first_ready_50k_bytes;
    if (typeof t === "number" && m.total_br > t) console.log(`page ${page}: ${kib(m.total_br)} is ${kib(m.total_br - t)} over the first visit's target arithmetic at 50 kbit/s (${kib(t)})`);
  }
  if (misses.length === 0) console.log(`every page within its budget (${budgetsPath})`);
  return misses.length ? 1 : 0;
}

// exitCode, not exit(): piped output is written after main returns.
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) process.exitCode = main(process.argv.slice(2));
