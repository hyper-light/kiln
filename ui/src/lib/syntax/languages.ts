/**
 * Every language kiln highlights: each language Vorpal parses (its table, crates/language/src/lib.rs
 * at the commit grammars/manifest.json names), by the same names, aliases, extensions and file
 * names, plus the names people write in a fence (```sh) and a few files of their own (Cargo.lock,
 * Gemfile). Each says which grammar it parses with and which queries colour it: the grammar's own
 * (grammars/queries, as Vorpal has them) or kiln's (queries/, where the grammar has none).
 *
 * Pure data and lookups, no loading: the browser and the tests read it alike.
 */

export interface Language {
  /** kiln's name for it, and its grammar's (grammars/<grammar>.wasm.gz). */
  id: string;
  /** What people call it. */
  name: string;
  /** Its grammar, when that isn't `id`. */
  grammar?: string;
  /** Another grammar for some text: PHP without `<?php` is PHP alone (Vorpal's PHP). */
  alt?: { grammar: string; when: (text: string) => boolean };
  /**
   * Its highlights query: these files (under src/lib/syntax), one after another. Where two patterns
   * capture the same node the later one wins (tree-sitter's highlighter and Neovim agree), unless
   * `precedence` says the query was written for the first to win (tree-sitter before 0.22).
   */
  highlights: string[];
  precedence?: "first" | "last";
  /** Its injections query: where other languages are written inside it. */
  injections?: string[];
  /** Names it goes by, lower case: in a fence, an injection, a `language` prop. */
  aliases: string[];
  /** File extensions, without the dot, lower case (none: it's only ever written inside another, as Markdown's inline text is). */
  extensions: string[];
  /** Whole file names. */
  filenames?: string[];
}

/** A query of the grammar's own (as Vorpal has it). */
const g = (dir: string, file = "queries/highlights.scm") => `grammars/queries/tree-sitter-${dir}/${file}`;
/** One of kiln's. */
const k = (name: string) => `queries/${name}.scm`;

const JS = [g("javascript"), g("javascript", "queries/highlights-jsx.scm"), g("javascript", "queries/highlights-params.scm")];
const PHP = { highlights: [g("php")], injections: [g("php", "queries/injections.scm")] };

export const LANGUAGES: readonly Language[] = [
  {
    id: "astro",
    name: "Astro",
    highlights: [g("html"), g("astro-next")],
    injections: [g("html", "queries/injections.scm"), g("astro-next", "queries/injections.scm")],
    aliases: ["astro"],
    extensions: ["astro"],
  },
  {
    id: "bash",
    name: "Bash",
    highlights: [g("bash")],
    aliases: ["bash", "sh", "shell", "shellscript", "zsh", "ksh", "dash", "ash", "env", "dotenv"],
    extensions: ["bash", "bats", "cgi", "command", "env", "fcgi", "ksh", "sh", "tmux", "tool", "zsh"],
    filenames: [".bashrc", ".bash_profile", ".bash_login", ".bash_logout", ".profile", ".zshrc", ".zprofile", ".zshenv", ".zlogin", ".envrc", ".env", "PKGBUILD", "APKBUILD"],
  },
  { id: "c", name: "C", highlights: [g("c")], aliases: ["c", "h"], extensions: ["c", "h"] },
  { id: "cmake", name: "CMake", highlights: [g("cmake")], aliases: ["cmake"], extensions: ["cmake"], filenames: ["CMakeLists.txt"] },
  {
    id: "cpp",
    name: "C++",
    highlights: [g("c"), g("cpp")],
    injections: [g("cpp", "queries/injections.scm")],
    aliases: ["cc", "c++", "cpp", "cxx", "hpp", "hh", "hxx", "cuda", "cu", "arduino", "ino"],
    extensions: ["cc", "hpp", "cpp", "c++", "hh", "cxx", "cu", "ino", "hxx", "h++", "ipp", "tpp", "cuh"],
  },
  { id: "csharp", name: "C#", grammar: "c_sharp", highlights: [g("c-sharp")], aliases: ["cs", "csharp", "c#", "c-sharp", "csx"], extensions: ["cs", "csx"] },
  { id: "css", name: "CSS", highlights: [g("css")], aliases: ["css", "scss"], extensions: ["css", "scss"] },
  { id: "dart", name: "Dart", highlights: [g("dart")], aliases: ["dart"], extensions: ["dart"] },
  {
    id: "dockerfile",
    name: "Dockerfile",
    highlights: [g("dockerfile")],
    injections: [k("dockerfile-injections")],
    aliases: ["dockerfile", "docker", "containerfile", "agentfile"],
    extensions: ["dockerfile", "containerfile"],
    // kiln's Agentfile reads as a Dockerfile (components/code/image-modes.ts).
    filenames: ["Dockerfile", "Containerfile", "Agentfile"],
  },
  {
    id: "elixir",
    name: "Elixir",
    highlights: [g("elixir")],
    injections: [g("elixir", "queries/injections.scm")],
    aliases: ["ex", "exs", "elixir"],
    extensions: ["ex", "exs"],
    filenames: ["mix.lock"],
  },
  { id: "erlang", name: "Erlang", highlights: [g("erlang")], aliases: ["erlang", "erl"], extensions: ["erl", "hrl"], filenames: ["rebar.config"] },
  { id: "go", name: "Go", highlights: [g("go")], precedence: "first", aliases: ["go", "golang"], extensions: ["go"] },
  { id: "graphql", name: "GraphQL", highlights: [k("graphql")], aliases: ["graphql", "gql"], extensions: ["graphql", "gql", "graphqls"] },
  {
    id: "haskell",
    name: "Haskell",
    highlights: [g("haskell")],
    injections: [g("haskell", "queries/injections.scm")],
    aliases: ["hs", "haskell"],
    extensions: ["hs"],
  },
  { id: "hcl", name: "HCL", highlights: [k("hcl")], aliases: ["hcl", "terraform", "tf", "tfvars", "nomad"], extensions: ["hcl", "nomad", "tf", "tfvars", "workflow"] },
  {
    id: "html",
    name: "HTML",
    highlights: [g("html")],
    injections: [g("html", "queries/injections.scm")],
    aliases: ["html", "htm", "xhtml"],
    extensions: ["html", "htm", "xhtml"],
  },
  {
    id: "ini",
    name: "INI",
    highlights: [g("ini")],
    aliases: ["ini", "properties", "cfg", "conf", "editorconfig", "gitconfig"],
    extensions: ["ini", "cfg", "properties"],
    filenames: [".editorconfig", ".gitconfig", ".gitmodules", ".npmrc", ".pypirc", ".flake8", ".pylintrc", ".coveragerc"],
  },
  { id: "java", name: "Java", highlights: [g("java")], aliases: ["java"], extensions: ["java"] },
  {
    id: "javascript",
    name: "JavaScript",
    highlights: JS,
    injections: [g("javascript", "queries/injections.scm")],
    aliases: ["javascript", "js", "jsx", "mjs", "cjs", "node"],
    extensions: ["cjs", "js", "mjs", "jsx"],
  },
  { id: "jsdoc", name: "JSDoc", highlights: [g("jsdoc")], aliases: ["jsdoc"], extensions: ["jsdoc"] },
  {
    id: "json",
    name: "JSON",
    highlights: [g("json")],
    precedence: "first",
    aliases: ["json", "jsonc", "jsonl", "ndjson", "geojson", "webmanifest"],
    extensions: ["json", "jsonc", "jsonl", "ndjson", "geojson", "webmanifest", "ipynb", "har", "avsc", "code-workspace"],
    filenames: [".babelrc", ".eslintrc", ".swcrc", "composer.lock", "Pipfile.lock", "flake.lock", "bun.lock"],
  },
  { id: "julia", name: "Julia", highlights: [g("julia")], aliases: ["julia", "jl"], extensions: ["jl"] },
  { id: "kotlin", name: "Kotlin", highlights: [g("kotlin-sg"), k("kotlin")], aliases: ["kotlin", "kt", "kts"], extensions: ["kt", "ktm", "kts"] },
  {
    id: "lua",
    name: "Lua",
    highlights: [g("lua")],
    injections: [g("lua", "queries/injections.scm")],
    aliases: ["lua"],
    extensions: ["lua", "rockspec"],
    filenames: [".luacheckrc"],
  },
  {
    id: "make",
    name: "Make",
    highlights: [g("make")],
    aliases: ["make", "makefile", "gnumake", "mk"],
    extensions: ["mk", "mak", "make"],
    filenames: ["Makefile", "makefile", "GNUmakefile", "BSDmakefile"],
  },
  {
    id: "markdown",
    name: "Markdown",
    highlights: [g("md", "tree-sitter-markdown/queries/highlights.scm")],
    injections: [g("md", "tree-sitter-markdown/queries/injections.scm")],
    aliases: ["markdown", "md", "mdx"],
    extensions: ["markdown", "md", "mdx", "mkd", "mdown"],
  },
  {
    id: "markdown_inline",
    name: "Markdown",
    highlights: [g("md", "tree-sitter-markdown-inline/queries/highlights.scm")],
    injections: [g("md", "tree-sitter-markdown-inline/queries/injections.scm")],
    aliases: ["markdown_inline"],
    extensions: [],
  },
  {
    id: "nix",
    name: "Nix",
    highlights: [g("nix")],
    injections: [g("nix", "queries/injections.scm")],
    aliases: ["nix"],
    extensions: ["nix"],
  },
  {
    id: "objc",
    name: "Objective-C",
    highlights: [g("c"), g("objc")],
    aliases: ["objc", "objective-c", "objectivec", "obj-c", "objcpp", "objective-c++"],
    extensions: ["m", "mm"],
  },
  { id: "ocaml", name: "OCaml", highlights: [g("ocaml")], aliases: ["ocaml", "ml"], extensions: ["ml"] },
  { id: "ocaml_interface", name: "OCaml interface", highlights: [g("ocaml")], aliases: ["ocaml_interface", "mli"], extensions: ["mli"] },
  {
    id: "php",
    name: "PHP",
    ...PHP,
    injections: [...PHP.injections, g("php", "queries/injections-text.scm")],
    alt: { grammar: "php_only", when: (text) => !/<\?(php|=|\s)/.test(text) },
    aliases: ["php"],
    extensions: ["php", "phtml"],
  },
  { id: "php_only", name: "PHP", ...PHP, aliases: ["php_only"], extensions: [] },
  { id: "proto", name: "Protocol Buffers", highlights: [g("proto")], aliases: ["proto", "protobuf"], extensions: ["proto"] },
  { id: "perl", name: "Perl", highlights: [k("perl")], aliases: ["perl", "pl", "pm"], extensions: ["pl", "pm", "t"] },
  { id: "powershell", name: "PowerShell", highlights: [g("powershell")], aliases: ["powershell", "pwsh", "ps1", "ps", "posh"], extensions: ["ps1", "psm1", "psd1"] },
  {
    id: "python",
    name: "Python",
    highlights: [g("python")],
    aliases: ["py", "python", "python3", "py3", "gyp", "bzl", "bazel", "starlark"],
    extensions: ["py", "py3", "pyi", "bzl", "bazel", "pyw", "gyp", "star"],
    filenames: ["BUILD", "WORKSPACE", "SConstruct", "SConscript", "Tiltfile"],
  },
  { id: "r", name: "R", highlights: [g("r")], aliases: ["r"], extensions: ["r"] },
  {
    id: "ruby",
    name: "Ruby",
    highlights: [g("ruby")],
    aliases: ["rb", "ruby", "gemspec", "rake"],
    extensions: ["rb", "rbw", "gemspec", "rake", "ru", "podspec", "jbuilder"],
    filenames: ["Gemfile", "Rakefile", "Vagrantfile", "Brewfile", "Podfile", "Fastfile", "Guardfile", "Capfile"],
  },
  {
    id: "rust",
    name: "Rust",
    highlights: [g("rust"), k("rust")],
    injections: [g("rust", "queries/injections.scm")],
    aliases: ["rs", "rust"],
    extensions: ["rs"],
  },
  { id: "scala", name: "Scala", highlights: [g("scala")], aliases: ["scala", "sc", "sbt"], extensions: ["scala", "sc", "sbt"] },
  {
    id: "svelte",
    name: "Svelte",
    highlights: [g("html"), g("svelte-ng")],
    injections: [g("html", "queries/injections.scm"), g("svelte-ng", "queries/injections.scm")],
    aliases: ["svelte"],
    extensions: ["svelte"],
  },
  { id: "sql", name: "SQL", highlights: [g("sequel"), k("sql")], aliases: ["sql", "psql", "postgres", "postgresql", "mysql", "sqlite", "plpgsql"], extensions: ["sql", "psql"] },
  { id: "solidity", name: "Solidity", highlights: [g("solidity")], aliases: ["sol", "solidity"], extensions: ["sol"] },
  {
    id: "swift",
    name: "Swift",
    highlights: [g("swift")],
    injections: [g("swift", "queries/injections.scm")],
    aliases: ["swift"],
    extensions: ["swift"],
  },
  {
    id: "toml",
    name: "TOML",
    highlights: [g("toml-ng"), k("toml")],
    aliases: ["toml"],
    extensions: ["toml"],
    filenames: ["Cargo.lock", "poetry.lock", "uv.lock", "pdm.lock", "Pipfile", "gopkg.lock"],
  },
  {
    id: "tsx",
    name: "TSX",
    highlights: [...JS, g("typescript")],
    injections: [g("javascript", "queries/injections.scm")],
    aliases: ["tsx"],
    extensions: ["tsx"],
  },
  {
    id: "typescript",
    name: "TypeScript",
    highlights: [g("javascript"), g("javascript", "queries/highlights-params.scm"), g("typescript")],
    injections: [g("javascript", "queries/injections.scm")],
    aliases: ["ts", "typescript", "mts", "cts"],
    extensions: ["ts", "cts", "mts"],
  },
  {
    id: "xml",
    name: "XML",
    highlights: [g("xml", "queries/xml/highlights.scm")],
    aliases: ["xml", "svg", "xsd", "xsl", "xslt", "plist", "rss", "atom", "xaml", "wsdl"],
    extensions: ["xml", "xsd", "xsl", "xslt", "svg", "rss", "atom", "plist", "xaml", "csproj", "props", "targets", "fsproj", "vbproj", "nuspec", "resx", "storyboard", "xib", "wsdl"],
  },
  { id: "zig", name: "Zig", highlights: [g("zig")], injections: [g("zig", "queries/injections.scm")], aliases: ["zig"], extensions: ["zig", "zon"] },
  {
    id: "vue",
    name: "Vue",
    highlights: [g("vue")],
    injections: [g("vue", "queries/injections.scm"), k("vue-injections")],
    aliases: ["vue"],
    extensions: ["vue"],
  },
  {
    id: "yaml",
    name: "YAML",
    highlights: [g("yaml")],
    aliases: ["yaml", "yml"],
    extensions: ["yaml", "yml"],
    filenames: [".clang-format", ".clang-tidy", ".yamllint"],
  },
];

const BY_ID = new Map(LANGUAGES.map((l) => [l.id, l]));
const BY_ALIAS = new Map<string, Language>();
const BY_EXT = new Map<string, Language>();
const BY_NAME = new Map<string, Language>();
for (const l of LANGUAGES) {
  BY_ALIAS.set(l.id, l);
  for (const a of l.aliases) if (!BY_ALIAS.has(a)) BY_ALIAS.set(a, l);
  for (const e of l.extensions) if (!BY_EXT.has(e)) BY_EXT.set(e, l);
  for (const f of l.filenames ?? []) BY_NAME.set(f, l);
}

export const languageById = (id: string): Language | undefined => BY_ID.get(id);

/** The grammars a language parses with: its own, and its other one. */
export const grammarsOf = (l: Language): string[] => [l.grammar ?? l.id, ...(l.alt ? [l.alt.grammar] : [])];

/**
 * The language a name says (a fence's ```ts, an injection's "javascript", a `language` prop), in
 * any case: an alias, else an extension (```py3), else a file name (```Dockerfile).
 */
export function languageByName(name: string | null | undefined): Language | undefined {
  const n = (name ?? "").trim().replace(/^\./, "");
  if (!n) return undefined;
  const lower = n.toLowerCase();
  return BY_ALIAS.get(lower) ?? BY_EXT.get(lower) ?? BY_NAME.get(n) ?? (n.includes("/") || n.includes(".") ? languageOfPath(n) : undefined);
}

/**
 * What a fence's info string says its code is: its first word (```rust,ignore, ```ts {1,3},
 * ```python title="x.py"), as a name or, failing that, a path (```src/main.rs).
 */
export function languageOfFence(info: string | null | undefined): Language | undefined {
  const word =
    (info ?? "")
      .trim()
      .replace(/^\{\s*\.?/, "")
      .split(/[\s,{}]/)[0] ?? "";
  return languageByName(word);
}

/** A path's language, by its file name, then its extension (`Dockerfile.dev`, `x.d.ts`, `CMakeLists.txt`). */
export function languageOfPath(path: string | null | undefined): Language | undefined {
  const name = (path ?? "").split(/[\\/]/).pop() ?? "";
  if (!name) return undefined;
  const exact = BY_NAME.get(name);
  if (exact) return exact;
  const lower = name.toLowerCase();
  // Dockerfile.dev, Dockerfile-prod, prod.Dockerfile, Makefile.inc
  if (/^(docker|container)file[.-]/.test(lower) || lower.endsWith(".dockerfile")) return BY_ID.get("dockerfile");
  if (/^(gnu)?makefile[.-]/.test(lower)) return BY_ID.get("make");
  const dot = lower.lastIndexOf(".");
  if (dot <= 0 && !lower.startsWith(".")) return undefined;
  return BY_EXT.get(lower.slice(dot + 1));
}

/** Interpreters a `#!` line names, and their languages. */
const SHEBANG: Record<string, string> = {
  sh: "bash",
  bash: "bash",
  zsh: "bash",
  ksh: "bash",
  dash: "bash",
  ash: "bash",
  python: "python",
  python2: "python",
  python3: "python",
  node: "javascript",
  nodejs: "javascript",
  deno: "typescript",
  bun: "typescript",
  tsx: "typescript",
  "ts-node": "typescript",
  ruby: "ruby",
  perl: "perl",
  php: "php",
  lua: "lua",
  rscript: "r",
  julia: "julia",
  pwsh: "powershell",
  make: "make",
  elixir: "elixir",
  escript: "erlang",
  runhaskell: "haskell",
  swift: "swift",
};

/** What a `#!` first line says the text is (`#!/usr/bin/env -S python3 -u`). */
export function languageOfShebang(text: string): Language | undefined {
  const m = /^#!\s*(\S+)(?:\s+(?:-S\s+)?(\S+))?/.exec(text);
  if (!m) return undefined;
  const prog = (m[1] ?? "").split("/").pop() ?? "";
  const name = prog === "env" ? (m[2] ?? "") : prog;
  const base = name.toLowerCase().replace(/[\d.]+$/, "");
  const id = SHEBANG[name.toLowerCase()] ?? SHEBANG[base];
  return id ? BY_ID.get(id) : undefined;
}

/** The language of a file: by its path, else what its first line says. */
export function languageOfFile(path: string | null | undefined, text?: string): Language | undefined {
  return languageOfPath(path) ?? (text ? languageOfShebang(text) : undefined);
}
