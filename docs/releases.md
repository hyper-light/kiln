# Releases

kiln builds and ships itself: `.kiln/release.yaml` builds every platform's binary from the commit
the server runs. The server starts it on its own after each deploy (below). It also runs from
**Settings → Updates → Build every platform** (or the Runners page, when a machine's platform has no
build yet), on a tag (`v*`), or by hand.

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
   recording the run and commit. No token is involved. A server that knows the commit it runs
   keeps only builds of that commit, so no platform runs ahead of the server or behind it.
3. Every runner can then be upgraded remotely, from the Runners page or Settings → Updates; the
   server also serves the binaries at `/dl/kiln/<target>`, so `install.sh` / `install.ps1` install
   from it on any machine, with no GitHub involved. A runner of the server's own platform gets the
   server's own build.

## Every platform in step with the server

A server built knowing its commit (`KILN_COMMIT`) keeps every platform in step with itself. Every
5 minutes it compares the other platforms' binaries against that commit. When any isn't built from
it, the server runs the release pipeline on that commit. When the run succeeds, every machine's
runner can be upgraded to exactly what the server runs, so platforms don't drift apart.

- The commit must be in the repository kiln builds from (`release_repo`). A commit deployed
  before it was pushed fails to start; it's tried again after 30 minutes, at most 3 times.
- An uploaded binary isn't built from any commit, so the next build replaces it.
- `[updates] in_step = false` turns this off. Settings → Updates shows the commit, and which
  platforms are behind it.

The server learns its commit when it's built:
- `cargo build` takes it from git, unless tracked files have changes.
- The Docker build can't see `.git`, so pass it in:

```sh
docker build --build-arg KILN_COMMIT=$(git rev-parse HEAD) -t kiln:dev .
```

`deploy/cluster/bootstrap.sh` does this for you. An image built without it still works, but its
server can't keep other platforms in step.

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
