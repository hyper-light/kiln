#!/usr/bin/env node
/**
 * Packs kiln's UI server for the kiln binary to carry (vendor/bun/README.md): Bun for a platform,
 * pinned in vendor/bun/bun.json and checked against it, and the UI server's own files. Writes
 *
 *   dist/embed/runtime-<os>-<arch>.br   Bun (+ on Linux the musl runtime it needs) and its licenses
 *   dist/embed/app.br                   server/main.ts and dist/server/, the same for every platform
 *
 * each a brotli-compressed tar, which the kiln crate's build.rs embeds and the server unpacks once
 * (crates/kiln/src/uiserver/carried.rs). Bun is downloaded once per machine and version, into
 * KILN_BUN_CACHE (default ~/.cache/kiln/bun), and its pack is kept there too.
 *
 *   node scripts/pack-ui-server.mjs                       this machine's platform
 *   node scripts/pack-ui-server.mjs --target <triple>…    a Rust target's (repeatable)
 *   node scripts/pack-ui-server.mjs --platform linux-aarch64 …
 *   --require                                             fail when it can't (default: say so, go on)
 *
 *   KILN_UI_SERVER=off                 pack nothing
 *   KILN_UI_SERVER=require             as --require (the Docker build: `npm run build` must pack)
 *   KILN_UI_SERVER_PLATFORMS=a,b       the platforms when none are named on the command line
 *
 * No dependencies beyond Node: a build machine has it already, for the UI.
 */
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, relative, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { brotliCompressSync, constants, inflateRawSync } from "node:zlib";

const UI = join(dirname(fileURLToPath(import.meta.url)), "..");
const VENDOR = join(UI, "..", "vendor", "bun");
const OUT = join(UI, "dist", "embed");
/** Brotli 9: 33 MB for Bun in 8 s; 11 saves 3 MB for two minutes more. */
const QUALITY = 9;

const say = (m) => console.log(`pack-ui-server: ${m}`);

function args() {
  const a = process.argv.slice(2);
  const platforms = [];
  let strict = false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] === "--require") strict = true;
    else if (a[i] === "--platform") platforms.push(a[++i]);
    else if (a[i] === "--target") platforms.push(platformOf(a[++i]));
    else throw new Error(`unknown argument ${a[i]}`);
  }
  if (platforms.length === 0) {
    const env = (process.env.KILN_UI_SERVER_PLATFORMS ?? "").split(",").map((p) => p.trim()).filter(Boolean);
    platforms.push(...(env.length ? env : [hostPlatform()]));
  }
  return { platforms: [...new Set(platforms)], strict };
}

/** `<os>-<arch>` for a Rust target triple: the key build.rs looks for. */
export function platformOf(triple) {
  if (!triple) throw new Error("--target needs a triple");
  const os = /linux/.test(triple) ? "linux" : /apple-darwin|darwin/.test(triple) ? "macos" : /windows/.test(triple) ? "windows" : null;
  const arch = /^(x86_64|amd64)/.test(triple) ? "x86_64" : /^(aarch64|arm64)/.test(triple) ? "aarch64" : null;
  if (!os || !arch) throw new Error(`no Bun platform for target ${triple}`);
  return `${os}-${arch}`;
}

function hostPlatform() {
  const os = { linux: "linux", darwin: "macos", win32: "windows" }[process.platform];
  const arch = { x64: "x86_64", arm64: "aarch64" }[process.arch];
  if (!os || !arch) throw new Error(`no Bun platform for ${process.platform}/${process.arch}`);
  return `${os}-${arch}`;
}

function cacheDir() {
  if (process.env.KILN_BUN_CACHE) return process.env.KILN_BUN_CACHE;
  if (process.platform === "win32" && process.env.LOCALAPPDATA) return join(process.env.LOCALAPPDATA, "kiln", "bun");
  return join(process.env.XDG_CACHE_HOME || join(homedir(), ".cache"), "kiln", "bun");
}

const sha256 = (buf) => createHash("sha256").update(buf).digest("hex");

/** Writes `data` to `path` whole or not at all. */
function writeAtomic(path, data) {
  mkdirSync(dirname(path), { recursive: true });
  const tmp = `${path}.${process.pid}.tmp`;
  writeFileSync(tmp, data);
  renameSync(tmp, path);
}

/** The release file, from the cache or downloaded, checked against its pinned hash. */
async function release(manifest, p) {
  const want = p.sha256;
  const path = join(cacheDir(), manifest.version, p.file);
  if (existsSync(path)) {
    const got = readFileSync(path);
    if (sha256(got) === want) return got;
    rmSync(path, { force: true });
  }
  const url = manifest.url.replace("{version}", manifest.version).replace("{file}", p.file);
  say(`downloading ${url}`);
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`);
  const got = Buffer.from(await r.arrayBuffer());
  const hash = sha256(got);
  if (hash !== want) throw new Error(`${p.file} has SHA-256 ${hash}, not the pinned ${want}`);
  writeAtomic(path, got);
  return got;
}

/** One file out of a zip (deflated or stored; no zip64: Bun's releases don't need it). */
export function unzip(zip, wanted) {
  const eocd = zip.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  if (eocd < 0) throw new Error("not a zip");
  const count = zip.readUInt16LE(eocd + 10);
  let at = zip.readUInt32LE(eocd + 16);
  for (let i = 0; i < count; i++) {
    if (zip.readUInt32LE(at) !== 0x02014b50) throw new Error("bad zip central directory");
    const method = zip.readUInt16LE(at + 10);
    const size = zip.readUInt32LE(at + 20);
    const nameLen = zip.readUInt16LE(at + 28);
    const extraLen = zip.readUInt16LE(at + 30);
    const commentLen = zip.readUInt16LE(at + 32);
    const local = zip.readUInt32LE(at + 42);
    const name = zip.toString("utf8", at + 46, at + 46 + nameLen);
    at += 46 + nameLen + extraLen + commentLen;
    if (!wanted(name)) continue;
    if (size === 0xffffffff || local === 0xffffffff) throw new Error(`${name} needs zip64`);
    if (zip.readUInt32LE(local) !== 0x04034b50) throw new Error("bad zip entry");
    const start = local + 30 + zip.readUInt16LE(local + 26) + zip.readUInt16LE(local + 28);
    const body = zip.subarray(start, start + size);
    if (method === 0) return { name, data: Buffer.from(body) };
    if (method === 8) return { name, data: inflateRawSync(body) };
    throw new Error(`${name}: zip method ${method}`);
  }
  throw new Error("the file isn't in the zip");
}

/** A ustar archive of `files` ({ path, mode, data }), in the order given, with no owners or times. */
export function tar(files) {
  const blocks = [];
  for (const f of files) {
    const h = Buffer.alloc(512);
    let name = f.path;
    let prefix = "";
    if (Buffer.byteLength(name) > 100) {
      const cut = name.lastIndexOf("/", 155);
      if (cut <= 0 || Buffer.byteLength(name.slice(cut + 1)) > 100) throw new Error(`path too long for tar: ${name}`);
      prefix = name.slice(0, cut);
      name = name.slice(cut + 1);
    }
    const field = (s, at, len) => h.write(s, at, len, "utf8");
    const octal = (n, at, len) => field(`${n.toString(8).padStart(len - 1, "0")}\0`, at, len);
    field(name, 0, 100);
    octal(f.mode, 100, 8);
    octal(0, 108, 8);
    octal(0, 116, 8);
    octal(f.data.length, 124, 12);
    octal(0, 136, 12);
    h.fill(0x20, 148, 156);
    field("0", 156, 1);
    field("ustar\0", 257, 6);
    field("00", 263, 2);
    field(prefix, 345, 155);
    let sum = 0;
    for (const b of h) sum += b;
    field(`${sum.toString(8).padStart(6, "0")}\0 `, 148, 8);
    blocks.push(h, f.data, Buffer.alloc((512 - (f.data.length % 512)) % 512));
  }
  blocks.push(Buffer.alloc(1024));
  return Buffer.concat(blocks);
}

const brotli = (buf) =>
  brotliCompressSync(buf, {
    params: { [constants.BROTLI_PARAM_QUALITY]: QUALITY, [constants.BROTLI_PARAM_LGWIN]: 24, [constants.BROTLI_PARAM_SIZE_HINT]: buf.length },
  });

/** Every file under `dir`, sorted, as tar paths under `as`. */
function walk(dir, as) {
  const out = [];
  for (const name of readdirSync(dir).sort()) {
    const path = join(dir, name);
    const rel = `${as}/${name}`;
    if (statSync(path).isDirectory()) out.push(...walk(path, rel));
    else out.push({ path: rel, mode: 0o644, data: readFileSync(path) });
  }
  return out;
}

const licenses = () => [
  { path: "NOTICE", mode: 0o644, data: readFileSync(join(VENDOR, "NOTICE")) },
  ...walk(join(VENDOR, "licenses"), "licenses"),
];

/** The runtime pack for `platform`, from the cache when its inputs are the same. */
async function runtime(manifest, platform) {
  const p = manifest.platforms[platform];
  if (!p) throw new Error(`vendor/bun/bun.json has no Bun for ${platform}`);
  const files = [];
  const zip = await release(manifest, p);
  const exe = platform.startsWith("windows") ? "bun.exe" : "bun";
  files.push({ path: exe, mode: 0o755, data: unzip(zip, (n) => n === exe || n.endsWith(`/${exe}`)).data });
  if (p.musl) {
    for (const [name, want] of Object.entries(manifest.musl[p.musl])) {
      const data = readFileSync(join(VENDOR, "musl", p.musl, name));
      if (sha256(data) !== want) throw new Error(`vendor/bun/musl/${p.musl}/${name} doesn't match its pinned SHA-256`);
      files.push({ path: `lib/${name}`, mode: name.startsWith("ld-musl") ? 0o755 : 0o644, data });
    }
  }
  files.push({ path: "VERSION", mode: 0o644, data: Buffer.from(`bun ${manifest.version} (${p.file})\n`) }, ...licenses());
  const key = sha256(Buffer.concat(files.flatMap((f) => [Buffer.from(f.path), Buffer.from(sha256(f.data))]))).slice(0, 16);
  const cached = join(cacheDir(), manifest.version, `runtime-${platform}-${key}.br`);
  if (existsSync(cached)) return readFileSync(cached);
  say(`packing Bun ${manifest.version} for ${platform}`);
  const packed = brotli(tar(files));
  writeAtomic(cached, packed);
  return packed;
}

/** The UI server's own files: what ui/server/main.ts runs. */
function app() {
  const server = join(UI, "dist", "server");
  if (!existsSync(join(server, "server.js"))) throw new Error("dist/server is missing: build the UI first (vite build)");
  return brotli(tar([{ path: "server/main.ts", mode: 0o644, data: readFileSync(join(UI, "server", "main.ts")) }, ...walk(server, "dist/server")]));
}

async function main() {
  const mode = (process.env.KILN_UI_SERVER ?? "").toLowerCase();
  if (mode === "off") {
    say("KILN_UI_SERVER=off: packing nothing (kiln builds without its UI server)");
    return;
  }
  const { platforms, strict: asked } = args();
  const strict = asked || mode === "require";
  const manifest = JSON.parse(readFileSync(join(VENDOR, "bun.json"), "utf8"));
  let failed = false;
  try {
    const a = app();
    writeAtomic(join(OUT, "app.br"), a);
    say(`app.br: ${(a.length / 1e6).toFixed(1)} MB`);
  } catch (e) {
    failed = true;
    // Never leave an older build's files to be embedded as this one's.
    rmSync(join(OUT, "app.br"), { force: true });
    console.error(`pack-ui-server: the UI server's files: ${e.message}`);
  }
  for (const platform of platforms) {
    try {
      const r = await runtime(manifest, platform);
      writeAtomic(join(OUT, `runtime-${platform}.br`), r);
      say(`runtime-${platform}.br: Bun ${manifest.version}, ${(r.length / 1e6).toFixed(1)} MB`);
    } catch (e) {
      failed = true;
      rmSync(join(OUT, `runtime-${platform}.br`), { force: true });
      console.error(`pack-ui-server: ${platform}: ${e.message}`);
    }
  }
  if (failed) {
    if (strict) process.exit(1);
    console.error(`pack-ui-server: kiln will build without its UI server for what failed (${relative(process.cwd(), OUT) || "."}${sep})`);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) await main();
