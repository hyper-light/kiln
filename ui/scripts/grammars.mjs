#!/usr/bin/env node
/**
 * Builds the tree-sitter grammars kiln highlights code with (ui/src/lib/syntax): every grammar
 * Vorpal compiles in (https://github.com/hyper-light/vorpal, `grammars/`), from Vorpal at one
 * commit, compiled to WebAssembly the way `tree-sitter build --wasm` does (wasi-sdk's clang, then
 * binaryen's wasm-opt, both pinned here and checked against their SHA-256s), for web-tree-sitter to
 * load. Writes, in ui/src/lib/syntax/grammars/:
 *
 *   <id>.wasm.gz            each grammar, gzipped (the browser inflates it: DecompressionStream)
 *   queries/<repo>/…        the grammars' own queries kiln reads (highlights, injections), as Vorpal has them
 *   licenses/<repo>/…       each grammar's license
 *   manifest.json           where each came from, and the SHA-256 of each grammar's wasm
 *
 * Every grammar is checked as it's built: it may only import what web-tree-sitter's runtime
 * exports (or exports itself), and it must load and parse there.
 *
 *   node scripts/grammars.mjs                    build them all
 *   node scripts/grammars.mjs --only rust,go     build some (the rest stay as they are)
 *   node scripts/grammars.mjs --check            build them all apart and compare with what's here
 *   --from <vorpal checkout>                     Vorpal's source from here (at the pinned commit)
 *   --jobs <n>                                   how many to build at once (default 4)
 *
 * Tools and Vorpal's source are downloaded once per machine, into KILN_GRAMMARS_CACHE (default
 * ~/.cache/kiln/grammars). No dependencies beyond Node, git and tar.
 */
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { cpus, homedir, setPriority } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { gunzipSync, gzipSync } from "node:zlib";

const UI = join(dirname(fileURLToPath(import.meta.url)), "..");
const OUT = join(UI, "src", "lib", "syntax", "grammars");

/** The Vorpal kiln takes its grammars from. */
export const VORPAL = { repository: "https://github.com/hyper-light/vorpal", commit: "4ec68042122fa7915461211c3c82f1c8bd95d0d9" };

/** The tools, as tree-sitter's CLI pins them (crates/loader/wasi-sdk-version, binaryen-version). */
const TOOLS = {
  "wasi-sdk": {
    version: "34.0",
    url: "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-34/wasi-sdk-34.0-{platform}.tar.gz",
    platforms: {
      "x86_64-linux": "b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4",
      "arm64-linux": "f7e243dff54d60bcc576e94d6166b69f410f2500ae4a9ceef34315be10e77971",
      "x86_64-macos": "87d27fa8adc68dee59bfbf2e22a6d34ef717c34d6bf1d8af2a56fc929d9ce0eb",
      "arm64-macos": "9c59398106b417f8f14913380fdf0097a8cc0ff4af9eb3ce0065a859e88d49e9",
    },
  },
  binaryen: {
    version: "133",
    url: "https://github.com/WebAssembly/binaryen/releases/download/version_133/binaryen-version_133-{platform}.tar.gz",
    platforms: {
      "x86_64-linux": "2dc9c7813f5375db93d96ead4b78222fcc3e2677bbb832297af4797782a37489",
      "aarch64-linux": "89c07ea56faf38d0fbecf36ca8ec0721756716185f265b568e133d427f299bf8",
      "x86_64-macos": "13a9b90be775c6389ce3d1f879cb8627bea56708ba8c122983941d53a8199b95",
      "arm64-macos": "ad66da82ac13f163e424b1643f16c6dfcccc98b5966296b43e52d3cab04f84a8",
    },
  },
};

/**
 * Each grammar: its folder in Vorpal's `grammars/`, its sources there, the function that returns
 * it, and the queries kiln reads (its own; ui/src/lib/syntax/languages.ts says which each
 * language uses). One row per Vorpal language (crates/language/src/lib.rs), plus the other
 * grammars of the same repositories kiln reads with them: Markdown's inline grammar (Markdown's
 * inline text), PHP with its HTML (a .php file; Vorpal's PHP is PHP alone) and OCaml's interfaces.
 */
export const GRAMMARS = [
  { id: "astro", dir: "tree-sitter-astro-next", src: "src", symbol: "tree_sitter_astro", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "bash", dir: "tree-sitter-bash", src: "src", symbol: "tree_sitter_bash", queries: ["queries/highlights.scm"] },
  { id: "c", dir: "tree-sitter-c", src: "src", symbol: "tree_sitter_c", queries: ["queries/highlights.scm"] },
  { id: "c_sharp", dir: "tree-sitter-c-sharp", src: "src", symbol: "tree_sitter_c_sharp", queries: ["queries/highlights.scm"] },
  { id: "cmake", dir: "tree-sitter-cmake", src: "src", symbol: "tree_sitter_cmake", queries: ["queries/highlights.scm"] },
  { id: "cpp", dir: "tree-sitter-cpp", src: "src", symbol: "tree_sitter_cpp", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "css", dir: "tree-sitter-css", src: "src", symbol: "tree_sitter_css", queries: ["queries/highlights.scm"] },
  { id: "dart", dir: "tree-sitter-dart", src: "src", symbol: "tree_sitter_dart", queries: ["queries/highlights.scm"] },
  { id: "dockerfile", dir: "tree-sitter-dockerfile", src: "src", symbol: "tree_sitter_dockerfile", queries: ["queries/highlights.scm"] },
  { id: "elixir", dir: "tree-sitter-elixir", src: "src", symbol: "tree_sitter_elixir", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "erlang", dir: "tree-sitter-erlang", src: "src", symbol: "tree_sitter_erlang", queries: ["queries/highlights.scm"] },
  { id: "go", dir: "tree-sitter-go", src: "src", symbol: "tree_sitter_go", queries: ["queries/highlights.scm"] },
  { id: "graphql", dir: "tree-sitter-graphql", src: "src", symbol: "tree_sitter_graphql", queries: [] },
  { id: "haskell", dir: "tree-sitter-haskell", src: "src", symbol: "tree_sitter_haskell", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "hcl", dir: "tree-sitter-hcl", src: "src", symbol: "tree_sitter_hcl", queries: [] },
  { id: "html", dir: "tree-sitter-html", src: "src", symbol: "tree_sitter_html", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "ini", dir: "tree-sitter-ini", src: "src", symbol: "tree_sitter_ini", queries: ["queries/highlights.scm"] },
  { id: "java", dir: "tree-sitter-java", src: "src", symbol: "tree_sitter_java", queries: ["queries/highlights.scm"] },
  {
    id: "javascript",
    dir: "tree-sitter-javascript",
    src: "src",
    symbol: "tree_sitter_javascript",
    queries: ["queries/highlights.scm", "queries/highlights-jsx.scm", "queries/highlights-params.scm", "queries/injections.scm"],
  },
  { id: "jsdoc", dir: "tree-sitter-jsdoc", src: "src", symbol: "tree_sitter_jsdoc", queries: ["queries/highlights.scm"] },
  { id: "json", dir: "tree-sitter-json", src: "src", symbol: "tree_sitter_json", queries: ["queries/highlights.scm"] },
  { id: "julia", dir: "tree-sitter-julia", src: "src", symbol: "tree_sitter_julia", queries: ["queries/highlights.scm"] },
  { id: "kotlin", dir: "tree-sitter-kotlin-sg", src: "src", symbol: "tree_sitter_kotlin", queries: ["queries/highlights.scm"] },
  { id: "lua", dir: "tree-sitter-lua", src: "src", symbol: "tree_sitter_lua", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "make", dir: "tree-sitter-make", src: "src", symbol: "tree_sitter_make", queries: ["queries/highlights.scm"] },
  {
    id: "markdown",
    dir: "tree-sitter-md",
    src: "tree-sitter-markdown/src",
    symbol: "tree_sitter_markdown",
    queries: ["tree-sitter-markdown/queries/highlights.scm", "tree-sitter-markdown/queries/injections.scm"],
  },
  {
    id: "markdown_inline",
    dir: "tree-sitter-md",
    src: "tree-sitter-markdown-inline/src",
    symbol: "tree_sitter_markdown_inline",
    queries: ["tree-sitter-markdown-inline/queries/highlights.scm", "tree-sitter-markdown-inline/queries/injections.scm"],
  },
  { id: "nix", dir: "tree-sitter-nix", src: "src", symbol: "tree_sitter_nix", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "objc", dir: "tree-sitter-objc", src: "src", symbol: "tree_sitter_objc", queries: ["queries/highlights.scm"] },
  { id: "ocaml", dir: "tree-sitter-ocaml", src: "grammars/ocaml/src", symbol: "tree_sitter_ocaml", queries: ["queries/highlights.scm"] },
  { id: "ocaml_interface", dir: "tree-sitter-ocaml", src: "grammars/interface/src", symbol: "tree_sitter_ocaml_interface", queries: ["queries/highlights.scm"] },
  { id: "perl", dir: "tree-sitter-perl", src: "src", symbol: "tree_sitter_perl", queries: [] },
  { id: "php", dir: "tree-sitter-php", src: "php/src", symbol: "tree_sitter_php", queries: ["queries/highlights.scm", "queries/injections.scm", "queries/injections-text.scm"] },
  { id: "php_only", dir: "tree-sitter-php", src: "php_only/src", symbol: "tree_sitter_php_only", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "powershell", dir: "tree-sitter-powershell", src: "src", symbol: "tree_sitter_powershell", queries: ["queries/highlights.scm"] },
  { id: "proto", dir: "tree-sitter-proto", src: "src", symbol: "tree_sitter_proto", queries: ["queries/highlights.scm"] },
  { id: "python", dir: "tree-sitter-python", src: "src", symbol: "tree_sitter_python", queries: ["queries/highlights.scm"] },
  { id: "r", dir: "tree-sitter-r", src: "src", symbol: "tree_sitter_r", queries: ["queries/highlights.scm"] },
  { id: "ruby", dir: "tree-sitter-ruby", src: "src", symbol: "tree_sitter_ruby", queries: ["queries/highlights.scm"] },
  { id: "rust", dir: "tree-sitter-rust", src: "src", symbol: "tree_sitter_rust", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "scala", dir: "tree-sitter-scala", src: "src", symbol: "tree_sitter_scala", queries: ["queries/highlights.scm"] },
  { id: "solidity", dir: "tree-sitter-solidity", src: "src", symbol: "tree_sitter_solidity", queries: ["queries/highlights.scm"] },
  { id: "sql", dir: "tree-sitter-sequel", src: "src", symbol: "tree_sitter_sql", queries: ["queries/highlights.scm"] },
  { id: "svelte", dir: "tree-sitter-svelte-ng", src: "src", symbol: "tree_sitter_svelte", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "swift", dir: "tree-sitter-swift", src: "src", symbol: "tree_sitter_swift", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "toml", dir: "tree-sitter-toml-ng", src: "src", symbol: "tree_sitter_toml", queries: ["queries/highlights.scm"] },
  { id: "tsx", dir: "tree-sitter-typescript", src: "tsx/src", symbol: "tree_sitter_tsx", queries: ["queries/highlights.scm"] },
  { id: "typescript", dir: "tree-sitter-typescript", src: "typescript/src", symbol: "tree_sitter_typescript", queries: ["queries/highlights.scm"] },
  { id: "vue", dir: "tree-sitter-vue", src: "src", symbol: "tree_sitter_vue", queries: ["queries/highlights.scm", "queries/injections.scm"] },
  { id: "xml", dir: "tree-sitter-xml", src: "xml/src", symbol: "tree_sitter_xml", queries: ["queries/xml/highlights.scm"] },
  { id: "yaml", dir: "tree-sitter-yaml", src: "src", symbol: "tree_sitter_yaml", queries: ["queries/highlights.scm"] },
  { id: "zig", dir: "tree-sitter-zig", src: "src", symbol: "tree_sitter_zig", queries: ["queries/highlights.scm", "queries/injections.scm"] },
];

/**
 * What a scanner may call that web-tree-sitter's runtime doesn't export: a failed assert() and
 * abort() trap, so the parse fails (and kiln shows the code plain) instead of the page.
 */
const SHIM_C = `#include <assert.h>
#include <stdlib.h>
void __assert_fail(const char *expr, const char *file, int line, const char *func) {
  (void)expr, (void)file, (void)line, (void)func;
  __builtin_trap();
}
void abort(void) { __builtin_trap(); }
`;

/**
 * What a C++ scanner needs besides (tree-sitter-vue's): operator new and delete on the runtime's
 * malloc and free, std::string whole (libc++ keeps some of it out of its headers), wmemchr, static
 * destructors registered and never run (a grammar lives as long as the page), and libc++'s abort
 * hook (a trap: there are no exceptions to throw).
 */
const SHIM_CXX = `#include <stddef.h>
#include <stdlib.h>
#include <wchar.h>
#include <string>
template class std::basic_string<char>;
void *operator new(size_t n) { return malloc(n); }
void *operator new[](size_t n) { return malloc(n); }
void operator delete(void *p) noexcept { free(p); }
void operator delete[](void *p) noexcept { free(p); }
void operator delete(void *p, size_t) noexcept { free(p); }
void operator delete[](void *p, size_t) noexcept { free(p); }
extern "C" int __cxa_atexit(void (*)(void *), void *, void *) { return 0; }
extern "C" wchar_t *wmemchr(const wchar_t *s, wchar_t c, size_t n) {
  for (; n; n--, s++)
    if (*s == c) return (wchar_t *)s;
  return 0;
}
namespace std {
inline namespace _LIBCPP_ABI_NAMESPACE {
[[noreturn]] void __libcpp_verbose_abort(char const *, ...) noexcept { __builtin_trap(); }
}
}
`;

const say = (m) => console.log(`grammars: ${m}`);
const sha256 = (buf) => createHash("sha256").update(buf).digest("hex");

function cacheDir() {
  if (process.env.KILN_GRAMMARS_CACHE) return process.env.KILN_GRAMMARS_CACHE;
  return join(process.env.XDG_CACHE_HOME || join(homedir(), ".cache"), "kiln", "grammars");
}

function args() {
  const a = process.argv.slice(2);
  const o = { check: false, only: null, from: null, jobs: Math.max(1, Math.min(4, cpus().length)) };
  for (let i = 0; i < a.length; i++) {
    if (a[i] === "--check") o.check = true;
    else if (a[i] === "--only") o.only = new Set((a[++i] ?? "").split(",").filter(Boolean));
    else if (a[i] === "--from") o.from = a[++i];
    else if (a[i] === "--jobs") o.jobs = Math.max(1, Number(a[++i]) || 1);
    else throw new Error(`unknown argument ${a[i]}`);
  }
  if (o.only) for (const id of o.only) if (!GRAMMARS.some((g) => g.id === id)) throw new Error(`no grammar ${id}`);
  return o;
}

/** Runs a program to its end (at a lower priority: building grammars is never urgent), or throws with what it said. */
function run(cmd, argv, opts = {}) {
  return new Promise((resolve, reject) => {
    const p = spawn(cmd, argv, { ...opts, stdio: ["ignore", "pipe", "pipe"] });
    try {
      if (p.pid) setPriority(p.pid, 10);
    } catch {
      // Not everywhere lets a process lower another's priority; it runs as it is.
    }
    let out = "";
    p.stdout.on("data", (d) => (out += d));
    p.stderr.on("data", (d) => (out += d));
    p.on("error", reject);
    p.on("close", (code) => (code === 0 ? resolve(out) : reject(new Error(`${cmd} ${argv.join(" ")} exited ${code}\n${out.slice(-4000)}`))));
  });
}

function hostPlatform(tool) {
  const os = { linux: "linux", darwin: "macos" }[process.platform];
  const arch = { x64: "x86_64", arm64: tool === "binaryen" && process.platform === "linux" ? "aarch64" : "arm64" }[process.arch];
  if (!os || !arch) throw new Error(`no ${tool} for ${process.platform}/${process.arch}: build the grammars on Linux or macOS`);
  return `${arch}-${os}`;
}

/** A tool's folder, downloaded and checked against its pinned SHA-256 the first time. */
async function tool(name) {
  const t = TOOLS[name];
  const platform = hostPlatform(name);
  const want = t.platforms[platform];
  if (!want) throw new Error(`no pinned ${name} for ${platform}`);
  const dir = join(cacheDir(), "tools", `${name}-${t.version}-${platform}`);
  if (existsSync(join(dir, ".ok"))) return dir;
  const url = t.url.replace("{platform}", platform);
  say(`downloading ${url}`);
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`);
  const data = Buffer.from(await r.arrayBuffer());
  const got = sha256(data);
  if (got !== want) throw new Error(`${url} has SHA-256 ${got}, not the pinned ${want}`);
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  const archive = join(dir, "archive.tar.gz");
  writeFileSync(archive, data);
  await run("tar", ["-xzf", archive, "-C", dir, "--strip-components=1"]);
  rmSync(archive);
  writeFileSync(join(dir, ".ok"), `${url}\n${want}\n`);
  return dir;
}

/** Vorpal's source at the pinned commit: a checkout named by --from, or a shallow fetch of the commit, once. */
async function vorpal(from) {
  if (from) {
    const head = (await run("git", ["-C", from, "rev-parse", "HEAD"])).trim();
    if (head !== VORPAL.commit) throw new Error(`${from} is at ${head}, not the pinned ${VORPAL.commit}`);
    return from;
  }
  const dir = join(cacheDir(), `vorpal-${VORPAL.commit.slice(0, 12)}`);
  if (existsSync(join(dir, ".ok"))) return dir;
  say(`fetching Vorpal ${VORPAL.commit.slice(0, 12)}`);
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  await run("git", ["init", "-q", dir]);
  await run("git", ["-C", dir, "fetch", "-q", "--depth", "1", VORPAL.repository, VORPAL.commit]);
  await run("git", ["-C", dir, "checkout", "-q", "--detach", "FETCH_HEAD"]);
  writeFileSync(join(dir, ".ok"), `${VORPAL.commit}\n`);
  return dir;
}

/** What web-tree-sitter's runtime exports to the grammars it loads. */
function runtimeExports() {
  const wasm = readFileSync(join(UI, "node_modules", "web-tree-sitter", "web-tree-sitter.wasm"));
  return new Set(WebAssembly.Module.exports(new WebAssembly.Module(wasm)).map((e) => e.name));
}

/** The imports a grammar has that nothing would give it: none, or it isn't built. */
function unresolved(wasm, given) {
  const m = new WebAssembly.Module(wasm);
  const own = new Set(WebAssembly.Module.exports(m).map((e) => e.name));
  const linker = new Set(["memory", "__indirect_function_table", "__stack_pointer", "__memory_base", "__table_base"]);
  return WebAssembly.Module.imports(m)
    .filter((i) => !(i.module === "env" && (linker.has(i.name) || (i.kind === "function" && (given.has(i.name) || own.has(i.name))))) && !i.module.startsWith("GOT."))
    .map((i) => `${i.module}.${i.name}`);
}

async function build(g, src, tools, work, given) {
  const dir = join(src, "grammars", g.dir, g.src);
  const clang = join(tools.wasi, "bin", "clang");
  const files = readdirSync(dir);
  const cxx = files.includes("scanner.cc");
  const out = join(work, g.id);
  rmSync(out, { recursive: true, force: true });
  mkdirSync(out, { recursive: true });
  writeFileSync(join(out, "shim.c"), SHIM_C);
  const common = ["--target=wasm32-wasip1", "-fPIC", "-Os", "-fvisibility=hidden", "-fno-exceptions"];
  const objects = [];
  const cc = async (file, lang, cwd) => {
    const obj = join(out, `${file.replace(/[^A-Za-z0-9]/g, "_")}.o`);
    const flags = lang === "c++" ? ["-xc++", "-std=c++17", "-fno-rtti"] : ["-xc"];
    await run(lang === "c++" ? `${clang}++` : clang, [...common, ...flags, "-I", ".", "-c", file, "-o", obj], { cwd });
    objects.push(obj);
  };
  await cc("parser.c", "c", dir);
  if (files.includes("scanner.c")) await cc("scanner.c", "c", dir);
  if (cxx) {
    await cc("scanner.cc", "c++", dir);
    writeFileSync(join(out, "shim.cc"), SHIM_CXX);
    await cc("shim.cc", "c++", out);
  }
  await cc("shim.c", "c", out);
  const raw = join(out, "raw.wasm");
  const linker = cxx ? `${clang}++` : clang;
  // Its static constructors run as it loads (a C++ scanner's tables of tag names): web-tree-sitter
  // calls __wasm_call_ctors when a grammar exports it.
  await run(linker, [
    "--target=wasm32-wasip1",
    "-shared",
    "-fPIC",
    "-nostdlib",
    "--no-wasm-opt",
    `-Wl,--export=${g.symbol}`,
    "-Wl,--export-if-defined=__wasm_call_ctors",
    "-Wl,--allow-undefined",
    "-Wl,--no-entry",
    "-o",
    raw,
    ...objects,
  ]);
  const wasm = join(out, `${g.id}.wasm`);
  await run(join(tools.binaryen, "bin", "wasm-opt"), [raw, "-Os", "--strip-debug", "--strip-producers", "-o", wasm]);
  const bytes = readFileSync(wasm);
  const missing = unresolved(bytes, given);
  if (missing.length) throw new Error(`${g.id} imports what web-tree-sitter doesn't give it: ${missing.join(", ")}`);
  return bytes;
}

/** Each grammar loads in web-tree-sitter and parses. */
async function smoke(built) {
  const { Parser, Language } = await import(pathToFileURL(join(UI, "node_modules", "web-tree-sitter", "web-tree-sitter.js")).href);
  await Parser.init();
  const parser = new Parser();
  for (const [id, bytes] of built) {
    const lang = await Language.load(bytes);
    if (lang.abiVersion < 13 || lang.abiVersion > 15) throw new Error(`${id}: ABI ${lang.abiVersion}, which web-tree-sitter can't read`);
    parser.setLanguage(lang);
    const tree = parser.parse("a = 1\n");
    if (!tree) throw new Error(`${id} didn't parse`);
    tree.delete();
  }
  parser.delete();
}

const LICENSE = /^(licen[cs]e|copying)(\.(md|txt))?$/i;

/** Everything a build writes, by its path under grammars/: the wasm (gzipped), queries, licenses and manifest. */
async function produce(o) {
  const src = await vorpal(o.from);
  const tools = { wasi: await tool("wasi-sdk"), binaryen: await tool("binaryen") };
  const given = runtimeExports();
  const work = join(cacheDir(), `work-${process.pid}`);
  const wanted = GRAMMARS.filter((g) => !o.only || o.only.has(g.id));
  const built = new Map();
  try {
    let next = 0;
    const worker = async () => {
      while (next < wanted.length) {
        const g = wanted[next++];
        built.set(g.id, await build(g, src, tools, work, given));
        say(`${g.id}: ${(built.get(g.id).length / 1024).toFixed(0)} KiB`);
      }
    };
    await Promise.all(Array.from({ length: o.jobs }, worker));
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
  await smoke(wanted.map((g) => [g.id, built.get(g.id)]));
  const files = new Map();
  const grammars = {};
  const provenance = JSON.parse(readFileSync(join(src, "grammars", "PROVENANCE.json"), "utf8"));
  for (const g of wanted) {
    const bytes = built.get(g.id);
    files.set(`${g.id}.wasm.gz`, gzipSync(bytes, { level: 9 }));
    for (const q of g.queries) files.set(`queries/${g.dir}/${q}`, readFileSync(join(src, "grammars", g.dir, q)));
    const license = readdirSync(join(src, "grammars", g.dir)).find((f) => LICENSE.test(f));
    if (!license) throw new Error(`${g.dir} has no license file`);
    files.set(`licenses/${g.dir}/${license}`, readFileSync(join(src, "grammars", g.dir, license)));
    const p = provenance[g.dir] ?? {};
    grammars[g.id] = { grammar: `${g.dir}/${g.src}`, repository: p.repository, commit: p.commit, version: p.version, license: p.license, sha256: sha256(bytes), bytes: bytes.length };
  }
  return { files, grammars };
}

function walk(dir, base = dir) {
  if (!existsSync(dir)) return [];
  return readdirSync(dir).flatMap((n) => {
    const p = join(dir, n);
    return statSync(p).isDirectory() ? walk(p, base) : [relative(base, p).split("\\").join("/")];
  });
}

function writeAtomic(path, data) {
  mkdirSync(dirname(path), { recursive: true });
  const tmp = `${path}.${process.pid}.tmp`;
  writeFileSync(tmp, data);
  renameSync(tmp, path);
}

function manifestOf(grammars) {
  const toolchain = Object.fromEntries(Object.entries(TOOLS).map(([k, t]) => [k, t.version]));
  const runtime = JSON.parse(readFileSync(join(UI, "node_modules", "web-tree-sitter", "package.json"), "utf8")).version;
  const sorted = Object.fromEntries(Object.entries(grammars).sort(([a], [b]) => a.localeCompare(b)));
  return `${JSON.stringify({ vorpal: VORPAL, toolchain, "web-tree-sitter": runtime, grammars: sorted }, null, 2)}\n`;
}

async function main() {
  const o = args();
  if (o.check && o.only) throw new Error("--check checks them all");
  const { files, grammars } = await produce(o);
  const manifestPath = join(OUT, "manifest.json");
  if (o.check) {
    const bad = [];
    const here = new Set(walk(OUT).filter((f) => f !== "manifest.json"));
    for (const [path, data] of files) {
      here.delete(path);
      const at = join(OUT, path);
      if (!existsSync(at)) bad.push(`${path} is missing`);
      else if (path.endsWith(".gz") ? sha256(gunzipSync(readFileSync(at))) !== sha256(gunzipSync(data)) : sha256(readFileSync(at)) !== sha256(data)) bad.push(`${path} differs`);
    }
    for (const path of here) bad.push(`${path} isn't one this build makes`);
    if (!existsSync(manifestPath) || readFileSync(manifestPath, "utf8") !== manifestOf(grammars)) bad.push("manifest.json differs");
    if (bad.length) {
      for (const b of bad) console.error(`grammars: ${b}`);
      process.exit(1);
    }
    say(`all ${GRAMMARS.length} grammars are what Vorpal ${VORPAL.commit.slice(0, 12)} builds`);
    return;
  }
  const before = existsSync(manifestPath) ? JSON.parse(readFileSync(manifestPath, "utf8")).grammars : {};
  const merged = o.only ? { ...before, ...grammars } : grammars;
  for (const [path, data] of files) writeAtomic(join(OUT, path), data);
  if (!o.only) {
    // What no grammar makes any more goes.
    for (const path of walk(OUT)) if (path !== "manifest.json" && !files.has(path)) rmSync(join(OUT, path));
  }
  writeAtomic(manifestPath, manifestOf(merged));
  const total = [...files].filter(([p]) => p.endsWith(".wasm.gz")).reduce((n, [, d]) => n + d.length, 0);
  say(`wrote ${Object.keys(grammars).length} grammars (${(total / 1e6).toFixed(1)} MB gzipped) to ${relative(process.cwd(), OUT) || "."}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    await main();
  } catch (e) {
    console.error(`grammars: ${e.message}`);
    process.exit(1);
  }
}
