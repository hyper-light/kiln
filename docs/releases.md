# Releases

kiln builds and ships itself: `.kiln/release.yaml` runs on a tag (`v*`) or by hand.

1. One Linux runner builds the UI, then every platform's binary with `ci/release/build.sh`:
   - Linux x86_64 and aarch64: static musl.
   - macOS Apple Silicon and Intel: minimum macOS 11, Apple Silicon ad-hoc signed.
   - Windows x86_64.
   zig is the only cross toolchain. macOS links against `ci/release/macos-stubs`: text stubs
   listing the symbols kiln imports from CoreFoundation, IOKit and libobjc. macOS binds them to
   the real libraries at load time, so no Apple SDK or Mac is needed. If a dependency starts
   importing something new, `ci/release/regen-macos-stubs.sh` rebuilds the stubs.
2. `ci/release/publish.sh` uploads each binary to the server (`POST /api/updates/binaries`). The
   server then serves it at `/dl/kiln/<target>`, and `install.sh` / `install.ps1` install from it
   on any machine, with no GitHub involved.

The pipeline needs `KILN_RELEASE_URL` (the server to publish to) and `KILN_RELEASE_TOKEN` (its
admin token, until scoped tokens exist) in the server's `[secrets]`.

By hand, from a checkout on Linux:

```sh
npm ci --prefix ui && npm run --prefix ui build
ci/release/build.sh                                  # or a subset: ci/release/build.sh aarch64-apple-darwin
KILN_RELEASE_URL=http://server:7070 KILN_RELEASE_TOKEN=$(kiln token) ci/release/publish.sh
```

Adding machines then needs nothing but the server:

```sh
curl -fsSL http://server:7070/install.sh | sh -s -- --join          # Linux, macOS
```

Windows (PowerShell): download `http://server:7070/dl/kiln/x86_64-pc-windows-msvc` to `kiln.exe`
and run `kiln.exe join --server http://server:7070`.
