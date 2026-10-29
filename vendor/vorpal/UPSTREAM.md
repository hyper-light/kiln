# vorpal, vendored

The search across a change (`docs/ci.md` W26-R07): a reviewer searches a change's files at a
revision by text or by structure, each hit with the definition it sits in. kiln reaches it through
kiln-seams' `Search` (`docs/seams.md`); `crates/kiln-search` is that seam on these crates.

- **Upstream:** https://github.com/hyper-light/vorpal, commit
  `4420a4dfd7b96854784431ff899bbdd35510e07e` (2026-09-17, its release v0.10.2), MIT (`LICENSE`);
  each grammar is its own authors', MIT, its `LICENSE` beside it.
- **What is here:** upstream's root manifest and, at upstream's paths, the crates vorpal's text and
  structural search are made of:
  - `crates/core` (vorpal-core): the pattern engine (ast-grep's, renamed), the regex literal analysis
    and literal prefilter, and the byte trigram;
  - `crates/language` (vorpal-language): the languages and their tree-sitter grammars;
  - `crates/outline` and `crates/config` (vorpal-outline, vorpal-config): each file's definitions,
    by the rules vorpal ships for every language;
  - `crates/kg` (vorpal-kg) and what it's built on, `crates/mem`, `crates/segment`, `crates/graph`
    and `crates/canonical`: kiln takes its trigram store (`vorpal_kg::trigramstore`), the text
    tier vorpal's own `text_search` and `code_search` prune with;
  - `vendor/tree-sitter`: the tree-sitter runtime vorpal carries (0.26.10);
  - `grammars/`: 15 of the 48 grammars vorpal carries, for Rust, TypeScript and TSX, JavaScript,
    Python, Go, Java, C, C++, shell, JSON, YAML, TOML, CSS, HTML and Markdown. A file in another
    language is searched by text, with no definition named; code search leaves it out.
- **What isn't:** vorpal's index (`crates/index`: the knowledge graph, its semantic tier and the GPU
  encoder it carries, `wgpu`), its ingest, its CLI and daemon (tokio), its bindings, and the other
  grammars. kiln builds a change's index itself (`crates/kiln-search`) from the pieces above:
  the files, each one's definitions and its trigrams, in vorpal's trigram store.
- **How kiln builds it:** kiln's root `Cargo.toml` points each crate's name here
  (`[patch.crates-io]`, as vorpal's own does), the tree-sitter runtime and the 15 grammars too;
  `crates/kiln-search` takes `vorpal-language` without its default features and with those 15
  grammars'. Nothing
  here pulls in tokio. kiln's workspace excludes `vendor/vorpal`, so the crates' `workspace = true`
  settings resolve against the root manifest here.

## Changes from upstream

None: kiln builds upstream's tree as it is.

## Taking a newer upstream

`python3 scripts/vendor.py vorpal <commit>` takes these paths at that commit; a grammar kiln adds
is a path in `vendor/MANIFEST.toml`'s `take`, a `[patch.crates-io]` line in kiln's root
`Cargo.toml` and a feature of `vorpal-language` in `crates/kiln-search/Cargo.toml`.
