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

**Each runner has an update policy, and nothing is forced.** Settings → Updates sets it for one
runner, any selection of them, or all:
- **Automatic**: upgraded as soon as there is a build of the newest kiln for its platform.
- **Notify** (the default): its row says a newer kiln is available and where the build comes from
  (this server's own build, a release run, a machine that built it); it's upgraded when you choose.
- **Off**: left alone. It isn't upgraded, on its own or with *Upgrade all*, until its policy
  changes.

A machine can also refuse upgrades itself, whatever the server says: `[updates] accept = false` in
its own kiln config (`kiln config set updates.accept false`). Its runner refuses every upgrade
from then on, and tells its server within a minute, which shows it as *off on the machine*; no
restart is needed. Every runner is also told when a newer kiln is available for it, and where
its build comes from: it logs it, and keeps it in `update-available.json` in its state directory.

From the command line:

```sh
kiln updates                                  # every runner: what it runs, its updates, what's next
kiln updates policy off --all                 # or: automatic | notify, for runners named
kiln updates policy automatic build-box-1 build-box-2
kiln updates upgrade build-box-1              # now (or --all; runners with updates off are left alone)
kiln updates build                            # build every platform of the server's commit
kiln config set updates.accept false          # this machine refuses upgrades (kiln config get/unset/show/path)
```

`kiln config` changes this machine's kiln config file in place, keeping its comments, and refuses
a change kiln couldn't read (an unknown key, a wrong type) before writing anything.

`PUT /api/updates/runners/{name}` with `{"policy": "auto" | "notify" | "off"}` sets one;
`PUT /api/updates/policy` with `{"policy": …, "runners": [...]}` (or `"all": true`) sets many.
`[updates] follow = false` still turns automatic upgrades off for every runner. The server exports
each runner's state as Prometheus gauges: `kiln_runner_up`, `kiln_runner_behind`,
`kiln_runner_update_policy{policy}` and `kiln_runner_version_info{version}`.

A runner with no binary of the server's commit yet doesn't wait on another machine: it waits only
while its own platform's job is building, and otherwise a Mac or Linux machine with a kiln checkout
builds the commit itself (a Mac's build is kept for the other Macs).

## How a runner is upgraded: blue/green

A runner under `kiln supervise` on Linux or macOS (how `kiln install --agent` runs one) is never
replaced while it runs. The new build comes up beside it, and takes over only once it works:

1. The runner downloads the new build, checks it, and keeps it in `<state>/versions/<build>/`.
2. Its supervisor starts the new build beside it, as another process of the same machine. The new
   build has 3 minutes to connect to the server and say it's ready. Until then it ranks as the
   machine's oldest process, so new work still goes to the runner it replaces.
3. Once it's ready, the runner hands it every agent session and terminal over a local socket.
   Their processes keep running: pipes and PTYs are passed across, not reopened. A reply that was
   streaming goes on, and background commands never stop. Whatever reaches the old runner
   meanwhile (a message for an agent, keystrokes, a new session) goes on to the new build.
4. The supervisor promotes the new build, and the server sends it new work from then on. The old
   runner finishes the jobs it's running (jobs aren't moved yet), then exits. The supervisor keeps
   the new build, and starts it again after a restart or reboot.

If the new build doesn't come up, the runner goes on with everything, as it was, and the machine
never goes offline. That covers a build that can't connect, crashes, or can't take over in time.
The new build is killed, and the upgrade's log says why it failed. A new build that ends soon after
it starts, three times in a row, is dropped: the supervisor goes back to the binary it was
installed with.

The upgrade's log (Settings → Updates, and the Runners page) follows each step: the download, the
new build starting and connecting, what was handed over and what couldn't be, and the old runner
retiring. While the old runner finishes its jobs, the machine lists both processes, and what still
keeps the old one running.

For tracing (OTel), each upgrade is a span under `kiln::upgrade` named `upgrade`. Its children are
`upgrade.start`, `upgrade.healthy`, one `upgrade.move` per session or terminal,
`upgrade.retire`, and `upgrade.rollback`. Each records `result` (`ok`, `failed`, `skipped`, or
`rolled_back` on the root) and `error`.

The machine's local sockets, for `kiln spawn` and `kiln run`, are the same for every runner process
of a machine. Shells opened before an upgrade keep using them.

**On a Mac, run `kiln signing setup` once.** macOS's local network privacy judges every program
by its code signature. kiln's builds are ad hoc signed, so to macOS each upgrade is a program it
has never seen, cut off from the local network, and so from the kiln server, until someone allows
it again. `kiln signing setup` makes kiln a signing identity on the Mac: a self-signed certificate
for code signing, in a keychain of kiln's own (`~/Library/Keychains/kiln-signing.keychain-db`).
macOS asks once to trust it for code signing. Then kiln signs every kiln binary installed there,
and from then on every binary it installs or downloads, all under one identifier
(`io.github.hyper-light.kiln`). The runner takes up its signed binary, macOS asks once more
whether kiln may use the local network, and upgrades keep that permission. `kiln signing` says
whether a machine is set up. A signed binary keeps the build id it was handed out as
(`<binary>.kiln-build` beside it), so the server knows it for that build; launchd's watcher writes it
too, and each runner brings the watcher up to date with its own kiln as it starts. An upgrade is
done once the runner reports the commit it was upgraded to, even on a binary signed again since.

Some runners still upgrade as before: those not under a supervisor (`kiln agent` run by hand), and
Windows runners. On Unix they upgrade in place: the process re-executes itself, keeping its
terminals and agent sessions. Otherwise they upgrade through `kiln install --agent --upgrade`.

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

## Upgrading a runner

A runner takes a new binary up once no job runs on it, and keeps its terminals; agent sessions
continue in the new binary. Before switching, it checks the new binary starts (`kiln --version`);
one that doesn't is never switched to.

- **Linux:** the runner re-executes itself in place (same process), and so does its supervisor.
- **macOS:** nothing re-executes itself. macOS's local network privacy stops recognising a
  process that replaced its own image, so after an in-place upgrade the runner's connections to
  the server failed with "No route to host" until kiln was started afresh. Instead, the runner
  hands its terminals to its supervisor (`kiln supervise`) and exits, and the supervisor starts the
  new binary as a fresh process that takes them over.
- **Windows:** the supervisor restarts the runner (and its terminals).

On macOS the launchd job (`~/Library/LaunchAgents/io.github.hyper-light.kiln-agent.plist`) runs a
launcher, `~/.local/share/kiln/launcher/kiln-agent`: a copy of kiln that upgrades never replace,
running the `kiln` they do (`~/.local/bin/kiln`) with `supervise --exe`. kiln's macOS builds are
ad hoc signed, so to macOS every build is a new program; a job that ran the new build itself would
be blocked from the local network after every upgrade until someone allowed it again. macOS asks
once ("kiln-agent would like to find devices on your local network") the first time the launcher
runs; allow it. `kiln install --agent` and `kiln join` make the launcher and keep one that works,
and an older runner, from before launchers, has launchd start it afresh from one at its next
upgrade. If the launchd job can't be loaded, they say why; a runner they start outside launchd
instead stops at logout.

A Mac where nobody can answer that question (a CI machine) can let every program reach the kiln
server's network instead (macOS 15.5 or later; restart afterwards):

```sh
sudo defaults write com.apple.network.local-network AllowedEthernetLocalNetworkAddresses -array "192.168.1.0/24"
sudo defaults write com.apple.network.local-network AllowedWiFiLocalNetworkAddresses -array "192.168.1.0/24"
```
