import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";
import { gunzipSync } from "node:zlib";
import { createHash } from "node:crypto";
import { MAX_CHARS } from "../src/lib/syntax/engine.ts";
import { luaRegex } from "../src/lib/syntax/engine.ts";
import { parseQuery } from "../src/lib/syntax/fit.ts";
import { grammarsOf, LANGUAGES, languageById, languageByName, languageOfFence, languageOfFile, languageOfPath, languageOfShebang } from "../src/lib/syntax/languages.ts";
import { classOf } from "../src/lib/syntax/theme.ts";
import { SAMPLES } from "./syntax-samples.ts";
import { classOfWord, highlighted, highlighter } from "./syntax-node.ts";

const SYNTAX = new URL("../src/lib/syntax/", import.meta.url);

/** Every language Vorpal parses (crates/language/src/lib.rs at the commit grammars/manifest.json names), and kiln's name for it. */
const VORPAL: Record<string, string> = {
  Bash: "bash",
  C: "c",
  CMake: "cmake",
  Erlang: "erlang",
  Cpp: "cpp",
  CSharp: "csharp",
  Css: "css",
  Dart: "dart",
  Dockerfile: "dockerfile",
  Elixir: "elixir",
  Go: "go",
  GraphQL: "graphql",
  Haskell: "haskell",
  Hcl: "hcl",
  Html: "html",
  Ini: "ini",
  Java: "java",
  JavaScript: "javascript",
  Julia: "julia",
  JsDoc: "jsdoc",
  Json: "json",
  Kotlin: "kotlin",
  Lua: "lua",
  Make: "make",
  Markdown: "markdown",
  Astro: "astro",
  ObjectiveC: "objc",
  OCaml: "ocaml",
  Nix: "nix",
  Php: "php",
  Proto: "proto",
  Perl: "perl",
  PowerShell: "powershell",
  Python: "python",
  R: "r",
  Ruby: "ruby",
  Rust: "rust",
  Scala: "scala",
  Svelte: "svelte",
  Sql: "sql",
  Solidity: "solidity",
  Swift: "swift",
  Toml: "toml",
  Tsx: "tsx",
  TypeScript: "typescript",
  Xml: "xml",
  Zig: "zig",
  Vue: "vue",
  Yaml: "yaml",
};

/**
 * What fitting the grammars' own queries to the grammars kiln reads them with takes out (fit.ts):
 * this and nothing else, so a new grammar or query that fits worse is seen.
 */
const DROPPED: Record<string, string[]> = {
  // OCaml's query names the `#!` line only OCaml files have.
  "ocaml_interface/ocaml_interface": ["(shebang)"],
  // PHP's HTML is the PHP grammar's: PHP alone has none.
  "php/php_only": ['((text) @injection.content (#set! injection.language "html") (#set! injection.combined))'],
  // An anchor where today's tree-sitter takes none.
  "solidity/solidity": ["(struct_expression type: ((expression(identifier)) @type .))"],
  // JavaScript's parameters, as a JavaScript newer than tree-sitter-typescript's has them.
  "tsx/tsx": ["(formal_parameters [ (identifier) @variable.parameter (array_pattern (identifier) @variable.parameter) (object_pattern ["],
  "typescript/typescript": ["(formal_parameters [ (identifier) @variable.parameter (array_pattern (identifier) @variable.parameter) (object_pattern ["],
};

test("every language Vorpal parses is one kiln highlights", () => {
  assert.equal(Object.keys(VORPAL).length, 49);
  for (const [vorpal, id] of Object.entries(VORPAL)) assert.ok(languageById(id), `${vorpal} (${id})`);
});

test("every grammar a language parses with is built, and every built grammar is one a language parses with", () => {
  const manifest = JSON.parse(readFileSync(new URL("grammars/manifest.json", SYNTAX), "utf8")) as { grammars: Record<string, { sha256: string }> };
  const wanted = new Set(LANGUAGES.flatMap(grammarsOf));
  assert.deepEqual([...wanted].sort(), Object.keys(manifest.grammars).sort());
  const files = readdirSync(new URL("grammars/", SYNTAX)).filter((f) => f.endsWith(".wasm.gz"));
  assert.deepEqual(files.map((f) => f.replace(".wasm.gz", "")).sort(), [...wanted].sort());
  // Each is the wasm the manifest says it is.
  for (const f of files) {
    const wasm = gunzipSync(readFileSync(new URL(`grammars/${f}`, SYNTAX)));
    assert.equal(createHash("sha256").update(wasm).digest("hex"), manifest.grammars[f.replace(".wasm.gz", "")]?.sha256, f);
  }
});

test("every language loads, and its queries fit its grammars losing only what's known", async () => {
  for (const l of LANGUAGES) assert.ok(await highlighter.load(l.id), l.id);
  const dropped = Object.fromEntries([...highlighter.dropped].filter(([, v]) => v.length));
  assert.deepEqual(dropped, DROPPED);
});

for (const [lang, s] of Object.entries(SAMPLES)) {
  test(`${lang} highlights as its queries say`, async () => {
    for (const [word, cls] of s.expect) assert.equal(await classOfWord(lang, s.code, word), cls, `${lang}: ${JSON.stringify(word)}`);
  });
}

test("every language has a sample", () => {
  for (const l of LANGUAGES) assert.ok(SAMPLES[l.id], l.id);
});

test("what's written inside another language waits for that language, then reads in its own colours", async () => {
  const md = "Some text.\n\n```python\nimport os\n```\n";
  await highlighter.load("markdown");
  const h = highlighter.highlight("markdown", md);
  // Its fence's language isn't known to have loaded: the highlight says so.
  if (!highlighter.ready("python")) assert.deepEqual(h?.wants, ["python"]);
  assert.equal(await classOfWord("markdown", md, "import"), "hl-kw");
  // A fence naming no language kiln knows reads plain.
  assert.equal(await classOfWord("markdown", "```nosuchlang\nimport os\n```\n", "import"), "");
});

test("PHP without <?php is PHP alone", async () => {
  assert.equal(await classOfWord("php", "$n = strlen('a');", "strlen"), "hl-fn");
  assert.equal(await classOfWord("php", "<?php $n = strlen('a'); ?>\n<b>x</b>", "b"), "hl-tag");
});

test("tokens are counted in UTF-16 code units and split at each line", async () => {
  const h = await highlighted("rust", 'let s = "héllo 🌍";\n/* a\nb */ let t = 1;');
  const [first, second, third] = h.lines;
  assert.deepEqual(
    first?.find(([, , c]) => c === "hl-str"),
    [8, 18, "hl-str"],
  );
  // A block comment over lines is each line's part of it.
  assert.deepEqual(second, [[0, 4, "hl-com"]]);
  assert.deepEqual(third?.[0], [0, 4, "hl-com"]);
});

test("nothing to highlight, or too much, reads plain", async () => {
  assert.deepEqual((await highlighted("rust", "")).lines, [[]]);
  assert.equal(highlighter.highlight("rust", "x".repeat(MAX_CHARS + 1)), null);
  assert.equal(highlighter.highlight("nosuchlang", "x"), null);
});

test("a language is found by its path, its fence and its first line", () => {
  const by = (f: (s: string) => { id: string } | undefined) => (s: string) => f(s)?.id;
  const path = by(languageOfPath);
  assert.equal(path("src/main.rs"), "rust");
  assert.equal(path("ui/src/App.tsx"), "tsx");
  assert.equal(path("x.d.ts"), "typescript");
  assert.equal(path("Dockerfile"), "dockerfile");
  assert.equal(path("deploy/Dockerfile.dev"), "dockerfile");
  assert.equal(path("prod.Dockerfile"), "dockerfile");
  assert.equal(path("Agentfile"), "dockerfile");
  assert.equal(path("Makefile"), "make");
  assert.equal(path("CMakeLists.txt"), "cmake");
  assert.equal(path("Cargo.lock"), "toml");
  assert.equal(path(".kiln/ci.yaml"), "yaml");
  assert.equal(path("analysis.R"), "r");
  assert.equal(path(".bashrc"), "bash");
  assert.equal(path("main.tf"), "hcl");
  assert.equal(path("Gemfile"), "ruby");
  assert.equal(path("notes.txt"), undefined);
  assert.equal(path("LICENSE"), undefined);
  const fence = by(languageOfFence);
  assert.equal(fence("ts"), "typescript");
  assert.equal(fence("rust,ignore"), "rust");
  assert.equal(fence('python title="x.py"'), "python");
  assert.equal(fence("{.sh}"), "bash");
  assert.equal(fence("C#"), "csharp");
  assert.equal(fence("Dockerfile"), "dockerfile");
  assert.equal(fence("src/lib.rs"), "rust");
  assert.equal(fence("console"), undefined);
  assert.equal(fence(""), undefined);
  const shebang = by(languageOfShebang);
  assert.equal(shebang("#!/bin/sh\necho"), "bash");
  assert.equal(shebang("#!/usr/bin/env python3\n"), "python");
  assert.equal(shebang("#!/usr/bin/env -S node --no-warnings\n"), "javascript");
  assert.equal(shebang("echo"), undefined);
  assert.equal(languageOfFile("bin/run", "#!/usr/bin/env ruby\n")?.id, "ruby");
  assert.equal(languageByName("YML")?.id, "yaml");
});

test("each extension and file name is its own language's", () => {
  for (const l of LANGUAGES) {
    for (const e of l.extensions) assert.equal(languageOfPath(`x.${e}`)?.id, l.id, `.${e}`);
    for (const f of l.filenames ?? []) assert.equal(languageOfPath(`a/${f}`)?.id, l.id, f);
  }
});

test("capture names take the class of their longest known part", () => {
  assert.equal(classOf("function.method.call"), "hl-fn");
  assert.equal(classOf("function.macro"), "hl-meta");
  assert.equal(classOf("string.special.key"), "hl-prop");
  assert.equal(classOf("keyword.directive.define"), "hl-meta");
  assert.equal(classOf("variable"), "");
  assert.equal(classOf("none"), "");
  assert.equal(classOf("spell"), null);
  assert.equal(classOf("_name"), null);
  assert.equal(classOf("injection.content"), null);
  assert.equal(classOf("nosuchthing"), null);
});

test("Lua patterns read as regular expressions", () => {
  assert.ok(luaRegex("^[%u@][%u%d_]+$")?.test("CMAKE_C"));
  assert.ok(!luaRegex("^[%u@][%u%d_]+$")?.test("cmake"));
  assert.ok(luaRegex("^//!")?.test("//! docs"));
  assert.ok(luaRegex("^%a+%.%d$")?.test("v.1"));
  assert.equal(luaRegex("%bxy"), null);
});

test("a query reads as its patterns, predicates skipped", () => {
  const ps = parseQuery(`; a comment
(call_expression function: (identifier) @function (#eq? @function "x)"))
["if" "else"] @keyword
((identifier) @constant (#match? @constant "^[A-Z]"))`);
  assert.equal(ps.length, 3);
  assert.deepEqual(
    ps.map((p) => p.kind),
    ["list", "alt", "list"],
  );
  assert.equal(ps[1]?.items.map((i) => i.text).join(","), "if,else");
});
