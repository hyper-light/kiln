# Vendoring

kiln builds from its own copies of the projects it's made of: Turso and Bun today, and the
hyper-light products (Amphora, Quiver, Hex, Veil, Reliquary, Mantle, and what they take from Focal,
Shards, Slates and hyper-raft) as the critical path brings them in (its master plan §5, CP-M1). Each
copy is its upstream at one revision plus kiln's recorded changes, and one command takes a new
revision.

## The rules

- **The manifest.** `vendor/MANIFEST.toml` names each project: where it comes from, the revision
  kiln takes, the folder it lives in, what of upstream's tree is taken, and which files in the folder
  are kiln's own (its `UPSTREAM.md`, notes).
- **Recorded changes.** kiln's edits to a project are patches in `vendor/patches/<project>/`, applied
  in name order, each a row of the project's `UPSTREAM.md` (file, change, why, the patch). An edit no
  change records fails `scripts/vendor.py check`; record it with `scripts/vendor.py record`. kiln's
  CI checks every git project on every run (`check --kind git`, one shallow fetch apiece). Bun's
  release is checked against its sources by every re-vendor and by `check bun`, and its hashes by
  every release build's pack, but not in CI: its image comes from Docker Hub, whose pulls are
  rate-limited. Each change is offered upstream as a branch, and a hyper-light product's agent is
  told of it.
- **One copy of each.** kiln's tree holds one copy of each project. A project kiln vendors itself
  lives at `vendor/<project>`, and every hyper-light product is one: a product that vendors another
  takes it at kiln's revision with kiln's recorded changes, and kiln's copy of the product leaves
  the product's own copy out (`exclude`), its dependency on it pointed at kiln's copy by `[patch]`.
  Third-party code only one product vendors stays inside that product's copy
  (`vendor/<product>/vendor/<project>`): that copy is kiln's one. The re-vendor checks all of
  this against the product's own `vendor/MANIFEST.toml` and `vendor/patches/`.
- **One revision linked.** `crates/kiln/tests/vendored.rs` fails the gate when `Cargo.lock` links a
  crate a vendored project provides more than once, or from crates.io or git instead of its folder.
- **The person's word.** Pushing to any hyper-light repository takes the person's go-ahead, and so
  does creating one.

## Commands

```sh
scripts/vendor.py <project> <revision>        # take a revision (a full commit; Bun: a release)
scripts/vendor.py <project> <revision> --from ~/Projects/amphora   # a commit not pushed yet
scripts/vendor.py check [<project> ...]       # each folder is upstream plus its changes, byte for byte
scripts/vendor.py record <project> "<title>" --why "<why>"         # record an edit as the next change
scripts/vendor.py list                        # projects, revisions, changes
python3 scripts/test_vendor.py                # the tool's own tests
```

A re-vendor:

1. checks the folder is what the manifest says (an edit no change records would be lost: it stops
   and names the files);
2. takes upstream's tree at the new revision;
3. re-applies each recorded change by three-way merge (the pins are checked first). A change that
   conflicts stops it, naming the change and the files, with kiln's tree as it was (as with git's
   merges, an upstream edit to the line beside a change's counts); a change upstream now makes
   itself is dropped, and it says so;
4. refreshes each change against the new tree, writes the folder, and moves the manifest's
   revision (and the revision named in `UPSTREAM.md`);
5. checks the pins: every project the new revision's own `vendor/MANIFEST.toml` takes must be one
   kiln takes, at the same revision;
6. runs the gate: kiln's (`gate` at the manifest's top: the tests, clippy, the macOS and Windows
   cross-checks, tsc, eslint and the UI's build), then the project's own commands. The
   cross-checks are clippy through zig, as the release builds those platforms, so the machine needs
   zig, cargo-zigbuild and the two rustup targets (`aarch64-apple-darwin`,
   `x86_64-pc-windows-gnu`). `--no-gate` skips it, and a failure leaves the new tree in place to
   fix, or to put back with `git checkout`.

Upstream sources are cached in `~/.cache/kiln/vendor` (`KILN_VENDOR_CACHE`): git projects as bare
repositories, image layers by digest.

### When a change conflicts

The re-vendor keeps git's merge in `~/.cache/kiln/vendor/conflict/<project>`, a git repository
whose working tree is the new revision with the changes before the conflicting one applied, and the
conflicting one merged with git's markers. Rework the change there, onto upstream's new code (for
Turso's `Cargo.toml`: upstream's new root manifest, cut down to the crates kiln takes again), then:

```sh
scripts/vendor.py <project> --continue     # the change as resolved, the rest re-applied, then the gate
scripts/vendor.py <project> --abort        # or drop the stopped re-vendor
```

`--continue` takes the resolution as the change's new form (its patch file keeps its name and its
Subject and Why), carries the rest, and stops again at the next conflict, if any. kiln's tree is
written only once every change applies; while one waits, a new re-vendor of that project is
refused, and so is `--continue` while git's markers remain or once kiln's folder has changed.

## Adding a project

Write its `[projects.<name>]` (the table below), then take it: `scripts/vendor.py <name>
<commit>`, with `--from <a clone>` for a commit not pushed yet. Amphora, as it comes in:

```toml
[projects.amphora]
kind = "git"
repository = "https://github.com/hyper-light/amphora"
commit = "<the commit kiln takes>"
path = "vendor/amphora"
```

```sh
scripts/vendor.py amphora <commit> --from ~/Projects/amphora
```

Amphora's engine isn't a vendored Turso: since 2026-10-20 it's Amphora's own code. The four Turso
crates (from upstream 3e09a82) are Amphora's `crates/engine*`, named `amphora-engine`,
`amphora-engine-parser`, `amphora-engine-macros` and `amphora-engine-ext`, and
`crates/engine/ORIGIN.md` records where they came from and every change; Amphora's manifest has no
`[projects.turso]`. When kiln vendors Amphora, the engine comes as part of it, under its own names,
so it can't collide with kiln's `turso_core`. kiln's `vendor/turso` stays kiln's own copy, for
kiln's conversation store, until that store moves onto Amphora (T1); then its entry, folder and
patches go.

## The manifest

Every project has `kind`, `path` (a folder under `vendor/`), and `own` (files in the folder that
are kiln's, kept as they are).

| Kind | Keys | Its revision |
|---|---|---|
| `git` | `repository`; `commit`; `take` (paths of upstream's tree kiln takes, at the same paths; everything when left out); `exclude` (paths left out of those) | a full commit id |
| `bun` | `version`; `image`, `image_tag` and `image_digest` (Oven's Alpine image, by its index digest); `licenses` (each license file's source); `musl.<arch>` (each runtime file's path in the image's platform) | a release version |

Either kind can add `gate`, commands run after the project's re-vendor, before kiln's own.

## Products and `[patch]`

A hyper-light product depends on another by name and version, as if from crates.io
(`amphora-api = "0.1"`, every product crate `publish = false`), and each repository's root
`Cargo.toml` points the name at its own vendored copy:

```toml
[patch.crates-io]
amphora-api = { path = "vendor/amphora/crates/amphora-api" }
```

kiln's root `Cargo.toml` patches the same names to kiln's copies, so a product kiln vendors builds
against kiln's copy of every other: one copy, one revision. A crates.io patch, not a git one: cargo
takes a patch of a name crates.io doesn't have, and builds offline from the lock after; a patch of a
git URL fetches that repository whenever cargo resolves anew, which fails for one not yet pushed.

No product package takes the name of a crate kiln already links from elsewhere, or
`crates/kiln/tests/vendored.rs` sees two: crates.io's `hex` (hex encoding) is one, so Hex's binary
is in the `hex-cli` package.

## The products' repositories

Each new product repository (hyper-light/amphora, quiver, hex, veil, reliquary) carries the
critical path's bar from its first commit: a Cargo workspace whose lints deny panics (clippy's
`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `arithmetic_side_effects` and the rest;
`scripts/check-production.sh` forbids them in production code, so no `#[allow]` or `#[expect]` gets
around them), its `CLAUDE.md`, `STATUS.md`,
`docs/design/`, its requirement registry (`docs/requirements.toml`, from its track's plan), kiln's
`scripts/vendor.py` with a `vendor/MANIFEST.toml` of its own, and its gate (`scripts/gates.sh`: fmt,
clippy for Linux, macOS and Windows, the production check, tests, the vendor check), which kiln CI
runs from its `.kiln/ci.yaml` on every push and pull request.
