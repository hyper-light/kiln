# Step 2: Devin's bundles through kiln's one unpacker

Kiln plan `01a1ce705e4d7008993f5c7291ef4476` (step 25 of `01a1cd8e862e7169a074db3d5fa33f3d`), step 2 of 6;
research `01a1ce705e4d70089c04cee721525c8f` (f6, f26). Implementer: agent `01a1ceb7914e702aa9059e6dfdd3c79a`,
under step 25's coordinator `01a1ce6f55c3749ca87f3ef3393ec8b0` and ROOT `01a148884a4770fdaf0111f3f792a248`.
Branch `s25-devin-tree-pin`, from kiln main `3a4b09005`.

Policy read: Shards origin/main `66113379c`, where `AGENTS.md` (sha256 `f66d304c…`), `docs/development-rules.md`
(`df2acf47…`) and `docs/development-evidence.md` (`61613a24…`) are as they were at this agent's first readback
(their last change is `cbd392849`); R01–R16 bind. Kiln origin/main `edba77bdb`: `AGENTS.md` sha256 `16b05ef8…`, as
read; `docs/ci.md` is `be867453…`, which changed after that read only in W05's facts (`virt.microvm`) and in
notifications' delivery, not in §0 or §0.3.

## What this step does now

ROOT dropped this step's own unpacker (`s25-unpack`): the one unpacker is steps 26 and 27's, on main
(`crates/kiln/src/agentfiles/unpack.rs`). Its pin form takes a tree (`strip`, `unpacked_bytes`, `entries`) or a
member, and the fetcher reads every file's `unpack` before fetching anything. Devin's pin still named each bundle
`{"format": "tar.gz"}` alone, which the unpacker refuses as neither (`Why::Neither`): on main,
`kiln agentfiles context devin` made no context. So this step:

- pins each bundle as a tree: `strip` 0 (the bundle's `bin/` and `share/` are the harness directory's), `entries`
  (its directories and regular files, as `tar --list` lists them) and `unpacked_bytes` (their sizes, as
  `tar --list --verbose` gives them). pin.sh counts both while it checks each entry, and the unpacker counts them
  again as it unpacks;
- tests Devin's shape through the fetcher, over HTTP from kiln's own server, with kiln's own Agentfile and
  harness.json for Devin.

## Files

| File | What |
|---|---|
| `.kiln/images/harness-devin/pin.sh` | Writes each bundle's tree. |
| `.kiln/images/harness-devin/pin-tests.sh` | Holds every fixture's pin, and `verify` a fresh download, to the totals Python's `tarfile` counts. Still catches a pin.sh that writes `unpack` as a string. |
| `.kiln/images/harness-devin/pin.json` | 3000.12.43 again, the same two bundles, written by `pin.sh 3000.12.43` (sha256 `772cb68f…`). current/ names 3000.12.57 now, which its `verified` says. |
| `crates/kiln/src/agentfiles/builtin/devin_tests.rs` | Devin's pin must read, as the fetcher reads it, as the unpacker's tree, stripped of nothing, with both totals. A pin without `entries`, or with `strip` 1, is refused. |
| `crates/kiln/src/agentfiles/context/unpack_tests.rs` | The three `step25_` tests below. |
| [pin-tests.txt](pin-tests.txt), [pin-run.txt](pin-run.txt), [kiln-tests.txt](kiln-tests.txt), [live.txt](live.txt) | The records. [job.sh](job.sh) and [live.sh](live.sh) ran them. |

The three tests:

- `step25_a_devin_shaped_context_holds_bin_devin_share_and_harness_json_alone`: a context for
  linux/amd64,linux/arm64 holds the Agentfile, `devin-amd64/` and `devin-arm64/`. Each holds exactly
  `bin/devin` (its arch's bytes, 0755), `share/` as the bundle holds it, and harness.json.
- `step25_a_devin_bundle_of_another_digest_or_size_is_never_opened`: arm64's bundle holds a symlink first, which
  the unpacker refuses when the bundle is pinned as it is (the control). Pinned with another SHA-256, or a size
  larger or smaller, the fetcher refuses it by digest or size instead, so the archive is never opened.
- `step25_a_devin_bundle_the_unpacker_refuses_leaves_nothing`: at Devin's strip 0, each of these comes back as the
  unpacker's typed error naming the URL and the entry: a symlink, a hard link, a character or block device, a
  FIFO, an absolute path, a `..` path (two forms), harness.json at the bundle's root, and an entry or a byte fewer
  than pinned.

Each refusal leaves nothing in the context's directory.

## Results (adacachy, Linux 7.2.9-2-cachyos x86_64; main `3a4b09005` with this change)

| Run | Result |
|---|---|
| `pin-tests.sh` | 81 passed, 0 failed |
| `pin.sh 3000.12.43` | Exit 0. Both bundles' SHA-256 and size are unchanged. linux/amd64: 250 entries (16 directories, 234 regular files) of 210206170 bytes. linux/arm64: 250 of 192507154. |
| `pin-tests.sh verify pin.json` | 2 of 2: each bundle downloaded again. SHA-256, size and tree match, the tree counted by `tarfile` from the download. |
| `cargo test -p kiln --lib --locked agentfiles` | 90 passed, 0 failed |
| `cargo clippy -p kiln --all-targets --locked -- -D warnings` | Exit 0 |
| live: `kiln agentfiles context devin DIR --platform linux/amd64,linux/arm64` | Exit 0, recipe `8211452f…`. DIR holds the Agentfile (kiln's), `devin-amd64/` and `devin-arm64/`. Each per-arch directory holds `bin` (only `devin`), `harness.json` (kiln's) and `share`: 251 paths, exactly its bundle's 250 entries and harness.json. Every file's bytes are the bundle's. `bin/devin` is 0755, with the bundle's size and SHA-256 (amd64: 209662952 bytes, `1e5e2753…`; arm64: 191963936 bytes, `b6028024…`). Its 16 directories are 0755, and its other 233 files and harness.json are 0644. No set-ID or sticky bit. |

The merge with main is tested again before landing; its commit says what ran.

## Found beside it, for step 20

Codex's `linux/arm64` tree pin carries `"skip": ["codex-path/rg"]`. Main's unpacker has no such field, and its
`Fields` refuses unknown fields. So `kiln agentfiles context codex --platform linux/arm64` refuses before fetching,
as Devin's did. This step leaves it alone: it isn't Devin's.

## Owed

- **pin.sh and its tests:** need Linux with GNU tar. They ran on x86_64 Linux only.
- **kiln's tests:** ran on x86_64 Linux only. CI's macOS and Windows jobs build kiln with clippy.
- **ROOT's:** building Devin's image and its smoke on a microVM host.
