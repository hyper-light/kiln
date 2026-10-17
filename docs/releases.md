# Releases

kiln builds and ships itself: `.kiln/release.yaml` runs from **Settings → Updates → Build every
platform** (or the Runners page, when a machine's platform has no build yet), on a tag (`v*`), or
by hand.

1. One Linux runner builds the UI, then every platform's binary with `ci/release/build.sh`:
   - Linux x86_64 and aarch64: static musl.
   - macOS Apple Silicon and Intel: minimum macOS 11, Apple Silicon ad-hoc signed.
   - Windows x86_64.
   zig is the only cross toolchain. macOS links against `ci/release/macos-stubs`: text stubs
   listing the symbols kiln imports from CoreFoundation, IOKit and libobjc. macOS binds them to
   the real libraries at load time, so no Apple SDK or Mac is needed. If a dependency starts
   importing something new, `ci/release/regen-macos-stubs.sh` rebuilds the stubs.
2. The job leaves them as its artifacts (`dist/kiln-<target>`). When it succeeds, the server keeps
   each one that isn't its own platform as that platform's binary (`[updates]` names the pipeline:
   `release_repo = "hyper-light/kiln"`, `release_pipeline = "release"`, `release_branch = "main"`),
   recording the run and commit. No token is involved.
3. Every runner can then be upgraded remotely, from the Runners page or Settings → Updates; the
   server also serves the binaries at `/dl/kiln/<target>`, so `install.sh` / `install.ps1` install
   from it on any machine, with no GitHub involved. A runner of the server's own platform gets the
   server's own build.

By hand, from a checkout on Linux:

```sh
npm ci --prefix ui && npm run --prefix ui build
ci/release/build.sh                                  # or a subset: ci/release/build.sh aarch64-apple-darwin
KILN_RELEASE_URL=http://server:7070 KILN_RELEASE_TOKEN=$(kiln token) ci/release/publish.sh   # another server
```

Adding machines then needs nothing but the server:

```sh
curl -fsSL http://server:7070/install.sh | sh -s -- --join          # Linux, macOS
```

Windows (PowerShell): download `http://server:7070/dl/kiln/x86_64-pc-windows-msvc` to `kiln.exe`
and run `kiln.exe join --server http://server:7070`.
