# Bun, vendored

kiln's UI server is TanStack Start under Bun (`ui/server/main.ts`, run and watched by
`crates/kiln/src/uiserver.rs`). So that kiln stays one file, the binary carries Bun and the UI
server's files, and unpacks them once, into its data directory, when it first starts a server.

What's here:

- `bun.json`: the Bun release kiln carries, pinned. For each platform kiln ships for, the official
  release's file and its SHA-256 (from the release's `SHASUMS256.txt`). For Linux, the SHA-256 of
  each musl runtime file beside it.
- `musl/<arch>/`: what Bun's musl build needs on Linux (the musl loader, `libstdc++`, `libgcc_s`),
  taken from the `oven/bun:<version>-alpine` image Bun publishes. kiln starts Bun through that
  loader, so it runs on any Linux, glibc or musl, with nothing installed.
- `NOTICE` and `licenses/`: the licenses and where the sources are. They're packed with Bun, so
  they reach every machine that runs it.

Which Bun build each platform carries, and why:

| kiln built for | Bun release | Why |
|---|---|---|
| Linux x86-64 | `bun-linux-x64-musl-baseline` | kiln's Linux binary is static and runs anywhere; the musl build, with its runtime carried, does too. Baseline: no AVX2 needed |
| Linux ARM64 | `bun-linux-aarch64-musl` | as above |
| macOS x86-64 | `bun-darwin-x64-baseline` | signed by Oven; baseline for older Macs |
| macOS ARM64 | `bun-darwin-aarch64` | signed by Oven |
| Windows x86-64 | `bun-windows-x64-baseline` | baseline for older CPUs |

## How it gets in

1. `npm run build` in `ui/` builds the UI, then runs `ui/scripts/pack-ui-server.mjs`. That
   downloads this machine's Bun (once: it's cached, under `~/.cache/kiln/bun` or `KILN_BUN_CACHE`),
   checks it against `bun.json`, and writes two packs to `ui/dist/embed/`:
   - `runtime-<os>-<arch>.br`: Bun, the musl runtime on Linux, and the licenses;
   - `app.br`: the UI server's files (`server/main.ts` and `dist/server/`), the same for every
     platform.

   Other platforms: `node scripts/pack-ui-server.mjs --target <rust triple>`, or
   `KILN_UI_SERVER_PLATFORMS=linux-x86_64,macos-aarch64`. The release build
   (`ci/release/build.sh`) packs each target it builds. `KILN_UI_SERVER=off` packs nothing.
2. The kiln crate's `build.rs` embeds the packs for the platform it builds for. Without them, kiln
   builds all the same, without a UI server; a release build says so.
3. At runtime (`crates/kiln/src/uiserver/carried.rs`), a server unpacks them into
   `<data dir>/ui-server/<hash>/`. The folder is named after the packs' content, so a kiln with the
   same packs reuses it, and a new one never runs an old one's files. It unpacks into a temporary
   folder first, then renames it into place, so a half-unpacked folder is never used. Older folders
   are removed. `KILN_UI_SERVER_DIR` unpacks somewhere else, for a data directory whose filesystem
   doesn't allow running programs. `KILN_BUN` with `KILN_UI_DIR` still runs a Bun and files of your
   own.

## Updating Bun

1. In `bun.json`, set `version`, and each platform's `sha256` from the new release's
   `SHASUMS256.txt` (https://github.com/oven-sh/bun/releases/download/bun-v<version>/SHASUMS256.txt).
2. Refresh `musl/` from the matching image, for both architectures, and update their hashes and
   the Alpine versions (in `bun.json` and `NOTICE`):

   ```sh
   for arch in amd64 arm64; do
     a=$([ $arch = amd64 ] && echo x86_64 || echo aarch64)
     c=$(docker create --platform linux/$arch oven/bun:<version>-alpine)
     docker cp -L $c:/lib/ld-musl-$a.so.1 musl/$a/
     docker cp -L $c:/usr/lib/libstdc++.so.6 musl/$a/
     docker cp -L $c:/usr/lib/libgcc_s.so.1 musl/$a/
     docker cp $c:/lib/apk/db/installed /tmp/apk-$a   # the musl, libstdc++ and libgcc versions
     docker rm $c
   done
   sha256sum musl/*/*
   ```

3. Update `NOTICE`, and `licenses/bun-LICENSE.md` from the new tag, then run `npm run build` in
   `ui/` and start a server: its log says which Bun it runs.
