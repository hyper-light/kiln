#!/usr/bin/env node
/**
 * File names that change only when their content does (goal 8, plan 01a1a832ebfd step 4). The
 * bundler names each chunk by a hash that takes in the hashed names of the chunks it imports, and
 * nearly every chunk imports the entry, which names nearly every chunk: one changed line renamed
 * hundreds, and a visit after any deploy was a first visit again. Here, after the build, every
 * chunk names the others by a key that doesn't change (`./k/<key>.js`, `assets/k/<key>.js`) and
 * takes a name from its own content alone; an import map in every page (the shell's, and the UI
 * server's pages, ui/server/main.ts) says where each key is. A deploy renames only the chunks that
 * changed, and the shell, which carries the map.
 *
 * A chunk's key is its name, with a digest of its modules when another chunk has the same name
 * (`.kiln-chunks.json`, written by vite.config.ts's chunkKeys). The pages' manifests (the shell's
 * and the UI server's) name the new files. Writes `dist/client/_importmap.json` for the UI server.
 *
 *   node scripts/stable-names.mjs [dist]
 */
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const UI = join(dirname(fileURLToPath(import.meta.url)), "..");
const DIST = process.argv[2] ?? join(UI, "dist");
const CLIENT = join(DIST, "client");
const ASSETS = join(CLIENT, "assets");

const stemOf = (file) => file.replace(/-[\w-]{8}\.js$/, "");
const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/** Each chunk's key: its name, and its modules' digest when the name isn't the chunk's alone. */
export function keys(chunks) {
  const count = new Map();
  for (const file of Object.keys(chunks)) count.set(stemOf(file), (count.get(stemOf(file)) ?? 0) + 1);
  const out = {};
  for (const [file, c] of Object.entries(chunks)) {
    const stem = stemOf(file);
    out[file] = (count.get(stem) ?? 0) > 1 ? `${stem}.${c.modules}` : stem;
  }
  return out;
}

/** `code` with every other chunk named by its key; throws on a chunk named any other way. */
export function rewrite(code, key) {
  const names = Object.keys(key);
  if (names.length === 0) return code;
  const re = new RegExp(`(\\./|assets/)(${names.map(escape).join("|")})`, "g");
  const out = code.replace(re, (_m, pre, file) => `${pre}k/${key[file]}.js`);
  for (const f of names) if (out.includes(f)) throw new Error(`stable-names: a chunk names ${f} in a way it can't rewrite`);
  return out;
}

const hash8 = (s) => createHash("sha256").update(s).digest("base64url").slice(0, 8);

function* files(dir) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) yield* files(p);
    else if (e.isFile()) yield p;
  }
}

function main() {
  const meta = join(CLIENT, ".kiln-chunks.json");
  if (!existsSync(meta)) throw new Error(`stable-names: no ${meta} (vite.config.ts's chunkKeys)`);
  const chunks = Object.fromEntries(Object.entries(JSON.parse(readFileSync(meta, "utf8"))).map(([f, c]) => [f.replace(/^assets\//, ""), c]));
  rmSync(meta);
  const key = keys(chunks);
  const renamed = {};
  const map = {};
  for (const file of Object.keys(chunks)) {
    const path = join(ASSETS, file);
    const code = rewrite(readFileSync(path, "utf8"), key);
    const next = `${stemOf(file)}-${hash8(code)}.js`;
    writeFileSync(path, code);
    if (next !== file) renameSync(path, join(ASSETS, next));
    renamed[file] = next;
    map[`/assets/k/${key[file]}.js`] = `/assets/${next}`;
  }
  // The pages' manifests and every other file of the build that names a chunk: the new names.
  const old = Object.keys(renamed);
  const re = new RegExp(old.map(escape).join("|"), "g");
  for (const path of [...files(DIST)]) {
    if (path.startsWith(ASSETS) && path.endsWith(".js") && !path.includes("/server/")) continue;
    if (!/\.(js|mjs|html|json)$/.test(path) || statSync(path).size > 64 << 20) continue;
    const text = readFileSync(path, "utf8");
    const next = text.replace(re, (f) => renamed[f]);
    if (next !== text) writeFileSync(path, next);
  }
  const importMap = JSON.stringify({ imports: map });
  writeFileSync(join(CLIENT, "_importmap.json"), importMap);
  const shellPath = join(CLIENT, "_shell.html");
  const shell = readFileSync(shellPath, "utf8");
  if (!shell.includes("<head>")) throw new Error("stable-names: the shell has no <head> for the import map");
  writeFileSync(shellPath, shell.replace("<head>", `<head><script type="importmap">${importMap}</script>`));
  const moved = old.filter((f) => renamed[f] !== f).length;
  console.log(`stable-names: ${old.length} chunks named by content (${moved} renamed), import map ${importMap.length} bytes`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
