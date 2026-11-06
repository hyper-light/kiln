#!/usr/bin/env node
/**
 * Which source modules a page's critical JavaScript is made of (goal 8, plan 01a1a832ebfd step 6):
 * each chunk's bytes split by the source each stretch of it came from (its source map), its Brotli
 * size shared out in the same proportions, summed over the page's files (ci/ui/page_bytes.mjs).
 * Needs a build with source maps: `KILN_UI_SOURCEMAP=1 npx vite build` (in ui/).
 *
 *   node ci/ui/attribute.mjs --dist ui/dist --page conversation [--top 40] [--by package|file]
 */
import { existsSync, readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { measure } from "./page_bytes.mjs";

const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const DIGIT = new Map([...B64].map((c, i) => [c, i]));

/** A source map's mappings as [line][segment] = [column, source] (only what attribution needs). */
function segments(mappings) {
  const lines = [];
  let source = 0;
  for (const line of mappings.split(";")) {
    const segs = [];
    let col = 0;
    for (const seg of line.split(",")) {
      if (!seg) continue;
      const vals = [];
      let v = 0;
      let shift = 0;
      for (const c of seg) {
        const d = DIGIT.get(c) ?? 0;
        v += (d & 31) << shift;
        if (d & 32) shift += 5;
        else {
          vals.push(v & 1 ? -(v >> 1) : v >> 1);
          v = 0;
          shift = 0;
        }
      }
      col += vals[0] ?? 0;
      if (vals.length > 1) source += vals[1];
      segs.push([col, vals.length > 1 ? source : -1]);
      // The rest of the fields (original line, column, name) carry no bytes.
    }
    lines.push(segs);
  }
  return lines;
}

/** Each source's raw bytes in a chunk, by its map. */
export function bySource(code, map) {
  const out = new Map();
  const lines = code.split("\n");
  const segs = segments(map.mappings);
  for (let l = 0; l < lines.length; l++) {
    const s = segs[l] ?? [];
    const len = lines[l].length + 1;
    if (s.length === 0) {
      out.set("(unmapped)", (out.get("(unmapped)") ?? 0) + len);
      continue;
    }
    if (s[0][0] > 0) out.set("(unmapped)", (out.get("(unmapped)") ?? 0) + s[0][0]);
    for (let i = 0; i < s.length; i++) {
      const end = i + 1 < s.length ? s[i + 1][0] : len;
      const src = s[i][1] < 0 ? "(unmapped)" : (map.sources[s[i][1]] ?? "(unknown)");
      out.set(src, (out.get(src) ?? 0) + Math.max(0, end - s[i][0]));
    }
  }
  return out;
}

const pkgOf = (src) => {
  const m = /node_modules\/((?:@[^/]+\/)?[^/]+)/.exec(src);
  return m ? m[1] : src.replace(/^(\.\.\/)+/, "").replace(/^ui\//, "");
};

function main(argv) {
  const flag = (k, d) => {
    const i = argv.indexOf(k);
    return i < 0 ? d : argv[i + 1];
  };
  const dist = path.resolve(flag("--dist", "ui/dist"));
  const page = flag("--page", "conversation");
  const top = Number(flag("--top", "40"));
  const by = flag("--by", "file");
  const m = measure(dist)[page];
  const assets = path.join(dist, "client", "assets");
  const total = new Map();
  for (const f of m.files.filter((x) => x.endsWith(".js"))) {
    const p = path.join(assets, f);
    const mp = `${p}.map`;
    if (!existsSync(mp)) {
      total.set(`(no map) ${f}`, (total.get(`(no map) ${f}`) ?? 0) + m.sizes[f]);
      continue;
    }
    const code = readFileSync(p, "utf8");
    const parts = bySource(code, JSON.parse(readFileSync(mp, "utf8")));
    const raw = [...parts.values()].reduce((a, b) => a + b, 0) || 1;
    for (const [src, n] of parts) {
      const key = by === "package" ? pkgOf(src) : src.replace(/^(\.\.\/)+/, "");
      total.set(key, (total.get(key) ?? 0) + (m.sizes[f] * n) / raw);
    }
  }
  const rows = [...total.entries()].sort((a, b) => b[1] - a[1]);
  const sum = rows.reduce((a, r) => a + r[1], 0);
  console.log(`${page}: ${(sum / 1024).toFixed(1)} KiB Brotli of JavaScript, by ${by}`);
  for (const [k, v] of rows.slice(0, top)) console.log(`${(v / 1024).toFixed(1).padStart(7)} KiB  ${k}`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main(process.argv.slice(2));
