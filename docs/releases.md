# Releases

kiln builds and ships itself: every push to the release branch (`main`) runs `.kiln/release.yaml`,
which builds every platform's binary from that commit, all at once, each on a machine of its own
platform where kiln has one. By the time that commit is deployed, its binaries are usually ready,
and every runner upgrades as soon as the server runs it. The server also starts the pipeline on
its own commit when a platform's binary is missing (below), and it runs from **Settings → Updates →
Build every platform** (or the Runners page, when a machine's platform has no build yet), on a tag
(`v*`), or by hand. A newer push replaces a build still under way.

1. Three jobs, at once, each building the UI first, then its binaries with `ci/release/build.sh`:
   - `linux`, on the server's own Linux runner: x86_64 and aarch64, static musl.
   - `windows`, on the same runner: x86_64, cross-built with zig.
   - `macos`, on a Mac: Apple Silicon and Intel, built natively with the Mac's own toolchain and
     SDK, minimum macOS 11. On Linux, `build.sh` can cross-build them too (zig, linking against
     `ci/release/macos-stubs`: text stubs listing the symbols kiln imports from CoreFoundation,
     IOKit and libobjc, which macOS binds to the real libraries at load time). If a dependency
     starts importing something new, `ci/release/regen-macos-stubs.sh` rebuilds the stubs.
2. Each job leaves its binaries as artifacts (`dist/kiln-<target>`). As each job succeeds, the
   server keeps each one that isn't its own platform as that platform's binary (`[updates]` names
   the pipeline: `release_repo = "hyper-light/kiln"`, `release_pipeline = "release"`,
   `release_branch = "main"`), recording the run and commit. No token is involved. A server that
   knows the commit it runs keeps only builds of that commit, so no platform runs ahead of the
   server or behind it: a push's binaries wait in its run until the server runs that commit, and
   are kept then.
3. Every runner can then be upgraded remotely, from the Runners page or Settings → Updates; the
   server also serves the binaries at `/dl/kiln/<target>`, so `install.sh` / `install.ps1` install
   from it on any machine, with no GitHub involved. A runner of the server's own platform gets the
   server's own build.

A runner with no binary of the server's commit yet doesn't wait on another machine: it waits only
while its own platform's job is building, and otherwise a Mac or Linux machine with a kiln checkout
builds the commit itself (a Mac's build is kept for the other Macs).

## Every platform in step with the server

A server built knowing its commit (`KILN_COMMIT`) keeps every platform in step with itself. Every
5 minutes it compares the other platforms' binaries against that commit. When any isn't built from
it, the server first keeps what the push's build of that commit made, then, if a platform is still
missing, runs the release pipeline on that commit. As its jobs succeed, every machine's runner can
be upgraded to exactly what the server runs, so platforms don't drift apart.

- The commit must be in the repository kiln builds from (`release_repo`). A commit deployed
  before it was pushed can't start. The server tries again as soon as kiln sees a push to the
  release branch, and every 10 minutes until then. A build that fails is tried again after 30
  minutes, at most 3 times.
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

By hand, from a checkout on Linux (on a Mac, `build.sh` builds the macOS targets natively):

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
