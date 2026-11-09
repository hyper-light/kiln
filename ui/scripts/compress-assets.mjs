#!/usr/bin/env node
/** W21-R02: compressed immutable build artifacts, outside the public client directory. */
import { createHash } from "node:crypto";
import { mkdir, readdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { brotliCompressSync, constants, gzipSync } from "node:zlib";

const UI = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const TYPES = { ".js": "text/javascript; charset=utf-8", ".css": "text/css; charset=utf-8", ".svg": "image/svg+xml", ".json": "application/json" };
const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");

export async function compressAssets(client = join(UI, "dist", "client"), encoded = join(UI, "dist", "encoded")) {
  const names = (await readdir(join(client, "assets"))).sort();
  if (names.length > 4096) throw new Error("UI asset count exceeds the packaging budget");
  await mkdir(encoded, { recursive: true });
  const assets = {};
  for (const name of names) {
    if (!/^[A-Za-z0-9_-][A-Za-z0-9_.-]*$/.test(name)) throw new Error("Unsupported UI asset filename: " + name);
    const type = TYPES[extname(name)];
    if (!type) continue;
    const raw = await readFile(join(client, "assets", name));
    const sha256 = digest(raw);
    const entry = { type, bytes: raw.length, sha256 };
    if (raw.length > 1024) {
      const variants = {
        gzip: gzipSync(raw, { level: 9 }),
        br: brotliCompressSync(raw, { params: { [constants.BROTLI_PARAM_QUALITY]: 6 } }),
      };
      for (const [encoding, bytes] of Object.entries(variants)) {
        if (bytes.length >= raw.length) continue;
        const file = `${name}.${sha256}.${encoding}`;
        await writeFile(join(encoded, file), bytes);
        entry[encoding] = { file, bytes: bytes.length, sha256: digest(bytes) };
      }
    }
    assets[`/assets/${name}`] = entry;
  }
  const manifest = { version: 1, assets };
  const temporary = join(encoded, `manifest.${process.pid}.json`);
  await writeFile(temporary, JSON.stringify(manifest));
  await rename(temporary, join(encoded, "manifest.json"));
  return manifest;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const manifest = await compressAssets();
  console.log(`compress-assets: ${Object.keys(manifest.assets).length} immutable assets`);
}
