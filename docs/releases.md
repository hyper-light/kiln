# Releases

kiln builds and ships itself: every push to the release branch (`main`) runs `.kiln/release.yaml`,
which builds every platform's binary from that commit, all at once, each on a machine of its own
platform where kiln has one. By the time that commit is deployed, its binaries are usually ready,
and every runner upgrades as soon as the server runs it. The server also starts the pipeline on
its own commit when a platform's binary is missing (below), and it runs from **System → Builds →
Build every platform** (or the Runners page, when a machine's platform has no build yet), on a tag
(`v*`), or by hand. A newer push replaces a build still under way.

1. Three jobs, at once, each building the UI first, then its binaries with `ci/release/build.sh`:
   - `linux`, on the server's own Linux runner: x86_64 and aarch64, static musl.
   - `windows`, on the same runner: x86_64, cross-built with zig.
   - `macos`, on a Mac: Apple silicon only (kiln doesn't run on Intel Macs), built natively with
     the Mac's own toolchain and SDK, minimum macOS 11. It `requires` a Mac with cargo and node,
     so it waits for one rather than going to a Mac without developer tools. On Linux, `build.sh` can cross-build them too (zig, linking against
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
3. Every runner can then be upgraded remotely, from the Runners page or System → Updates; the
   server also serves the binaries at `/dl/kiln/<target>`, so `install.sh` / `install.ps1` install
   from it on any machine, with no GitHub involved. A runner of the server's own platform gets the
   server's own build.

**Each runner has an update policy, and nothing is forced.** System → Updates sets it for one
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
while its own platform's job is building, and otherwise the commit is built from the server's
source right away. No checkout of kiln is needed anywhere. A machine with the tools (cargo and
node, as its runner reports them) builds the commit itself; a Mac's build is kept, so the other
Macs download it. A Mac without them (no developer tools) gets its build over the wire: a Mac
that has them builds the commit natively, else a Linux machine cross-builds it
(`ci/release/build.sh`: zig, against the macOS stubs). The binary comes back to the server, and the
Mac downloads it in the same upgrade. One build of a commit serves every Mac: while one is under
way, the other Macs wait for it rather than start another.

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

The upgrade's log (System → Updates, and the Runners page) follows each step: the download, the
new build starting and connecting, what was handed over and what couldn't be, and the old runner
retiring. While the old runner finishes its jobs, the machine lists both processes, and what still
keeps the old one running.

For tracing (OTel), each upgrade is a span under `kiln::upgrade` named `upgrade`. Its children are
`upgrade.start`, `upgrade.healthy`, one `upgrade.move` per session or terminal,
`upgrade.retire`, and `upgrade.rollback`. Each records `result` (`ok`, `failed`, `skipped`, or
`rolled_back` on the root) and `error`.

The machine's local sockets, for `kiln spawn` and `kiln run`, are the same for every runner process
of a machine. Shells opened before an upgrade keep using them. The process that runs the machine's
sessions serves them: a new build takes them once it has taken over, and the old one takes them
back after a rollback. No other kiln process takes one a live process serves (a local `kiln
server`'s own agent given the runner's name and state folder, say), and a socket left with nothing
behind it (something took its file, then ended) is served again within seconds.

**On a Mac, run `kiln signing setup` once** (`kiln setup` offers it, and `install.sh` hands over
to `kiln setup` at a terminal). macOS's local network privacy judges every program
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

## Version skew

How far a runner's kiln may be behind the server's is counted in commits of the release branch
before the server's own (its first parents), by sha, under two tolerances:

- **Full** (`[updates] skew_full`, 100 by default): a runner on the server's commit or one of the
  `skew_full` before it does everything.
- **Upgrade** (`skew_upgrade`, 1000 by default, never less than `skew_full`): a runner on one of
  the commits past those, within `skew_upgrade`, connects so it can be brought up to date. Until
  it is, it takes no new work: no jobs or legs, no new agents, no new terminals (kiln's own, for
  upgrading it, still open).
- Further behind, a runner is **refused**: it's told how far behind it is and how to catch up
  (`kiln install --agent --upgrade` with a newer kiln, or a larger upgrade tolerance), and it
  tries again a minute later.

Main moves about 30 commits a day, so the defaults let a runner a few days behind do everything,
and one a month behind catch up. A runner ahead of the server, on a commit off the release branch
(a build on purpose) or of a local tree is left alone, as the fleet leaves it; so is one that says
no commit (a kiln from before it said one), which is upgraded as any runner behind is. Where the
server can't tell (it knows no commit of its own, or can't read its release repository), every
runner does everything: kiln never shuts a fleet out on what it can't see.

System → Updates sets both tolerances (kiln.toml's say until then; `GET`, `PUT` and `DELETE
/api/updates/skew`), lists the commits each takes, and shows each runner's standing.

## One runner a machine

A machine runs one kiln runner. `kiln join` (or `kiln install --agent`) on a machine whose runner
is running never starts a second one beside it, and never enrolls the machine again under it (the
runner's credentials would change under it):

- **Its runner works for that server already**, by whatever address it was given (the server
  vouches for the machine's own certificate, so a LAN address and a NodePort are one server): the
  new kiln takes it over. It starts beside the runner as the machine's newest process, with the
  runner's name, labels and slots, reaching the server at the address given. The old runner takes
  no new work, hands the new one its agent sessions as their turns end, and is retired (its service
  stopped and disabled) once it holds nothing.
- **It works for another server:** joining stops it first, once asked (or with `--yes`). Its agents
  stay with that server.
- **The server can't be asked:** nothing is joined. Try again once it answers.
- **It is the server's own machine, and the server runs its local agent:** that agent is the
  machine's runner, so joining under its name is refused (409) rather than enrolled as `<name>-2`.
  Turn the server's off (`[local_agent] enabled = false`) to run a separate runner there.

**Handing over.** While a machine has more than one runner process (a take-over, an upgrade beside
the running runner, or one left over from before), the server has each older process hand its
agent sessions to the newest. A session waits for its turn to end, and kiln's queue holds what is
sent meanwhile. Then the older process packs its timeline, state and queue and sends it to the
server, which passes it on to the newest process. The two share the machine's disk and home, so
the session's folder and its harness's record stay where they are: the newest resumes the
harness's own session from them, as after a restart, and the timeline says it was taken over. The
older process lets go only once the newest says it took the session over. If it couldn't, the
session goes on where it was, and the timeline says why. Should two processes ever both hold one
session (an answer lost as a link dropped), the newer keeps it and the older lets go. Terminals stay
with the process they run in until they close, and jobs finish where they run. Once an older
process holds nothing, the newest retires it. While a new build of the runner takes the machine
over (a blue/green upgrade), nothing is handed over: the newest process is then the one leaving,
and a session it took would end with it. Sessions wait for the new build. A process that is
handing everything over turns away a session that was already on its way to it, and that
session goes on where it was until it's asked for again.

**Nothing vanishes.** The server keeps which process of its machine holds each session, across
restarts (`holders.json` beside its agents' names). A process that says what it runs speaks for its
own sessions only: another process's, while that one isn't connected, are listed as away, never
closed.

Hand-overs take a server and runner processes that know them (`CAP_HANDOVER`, docs/wire.md). A
runner from before keeps what it runs until that ends, as it always did.

## When an upgrade fails

System → Updates says what each runner does next, and what went wrong when an upgrade failed (its
row reads *upgrade failed* or *build failed*, with the upgrade's output under it). From there:

- **Retry** tries the same way again now, without waiting for kiln's next automatic try (a failed
  automatic upgrade is tried again after 30 minutes, twice per commit).
- **Download instead** / **Build it instead**: the other way, when it's open. A download needs a
  binary of the newest kiln for the machine's platform. A build needs a machine with the tools to
  make it: the runner's own, or for a Mac without them, another Mac (or a Linux machine,
  cross-building) whose build it then downloads. A way that can't be taken now says why (no binary
  yet, a release run still building one, no machine with the tools for it, terminals turned off),
  so a runner kiln is waiting on can still be upgraded the other way.
- **Diagnose** (on every connected runner) looks at it, and lists in its row what's wrong, worst
  first, each with the repair that fixes it when kiln can make it (below).
- **Ask an agent**, under a diagnosis, fills Mission control's composer on that machine with a first message
  written for it: what kiln knows (the build it runs and the one it was going to, the error, the
  upgrade's last lines, what the diagnosis found) and where to look (the runner's log, a build's
  whole output in `~/.cache/kiln/self-build/build-<job>.log` on the machine that built it, disk,
  memory, toolchains). It asks the agent to fix the cause on the machine without stopping or
  reinstalling the runner, and to say what it changed. Read and change the message before starting
  it; the agent asks before commands and edits unless you choose otherwise.

`POST /api/updates/runners/{name}` with `{"way": "download" | "build"}` does the same as the
buttons (no body: the way kiln picks). `GET /api/updates` lists each runner's `ways`, each with `ok`
and `why`, and for a build another machine makes, `on` (that machine); an upgrade job made that way
carries `on` too.

### Diagnosing and repairing a runner

A diagnosis has two halves (`crate::doctor`):

- **What the server knows:** whether the runner is connected (and when it was last heard), the
  kiln it runs against the newest and what it does next, its last upgrade (failed, with its error
  and last lines; under way; or done), the Rust a failed self-build needed as cargo said it, and
  memory nearly full.
- **What the machine finds about itself**, asked over its link to the server (runners from this
  build on: an older one says so, and is diagnosed from what the server knows): room on the volumes
  where it works and keeps its state, what its self-builds keep (`~/.cache/kiln/self-build`), the
  newest self-build's log and whether it ended with errors, its clock against the server's, and on
  a Mac whether kiln is signed (docs above).

Each finding is `ok`, `info`, `warn` or `fail`, with a title, what was seen, and the repair that
fixes it when kiln can make it:

| Repair | What it does |
|---|---|
| `retry-upgrade` (with `way`) | upgrades it now, that way (the other way after a failed one, when it's open) |
| `clear-self-build-cache` | removes what self-builds compiled (the next one compiles from scratch); its logs stay. Refused while an upgrade of it is under way |
| `prune-versions` | removes older kiln builds the runner keeps; the one running and the newest few stay |
| `re-sign` | signs kiln again (a Mac whose signing is set up; setting it up is `kiln signing setup` at the Mac, which macOS asks about) |

No repair stops what the machine runs: none restarts its runner. A runner's own repairs run on
the machine itself, and come back with what they did.

| Method | Path | |
|---|---|---|
| GET | `/api/agents/{name}/diagnose` | `{runner, at, worst, findings: [{id, level, title, detail?, fix?: {action, label, way?}}], machine_checked, why, updates}`, worst first (`runners.read`) |
| POST | `/api/agents/{name}/repair` | `{action, way?}` → `{ok, action, message}` (or `{ok, action, job}` for an upgrade); 409 with why when it can't (`runners.manage`) |
| GET | `/api/updates/runners` | `{runners, jobs}`: the runners the caller may read against the newest kiln, and their latest upgrades (`runners.read` anywhere; a caller with it on some runners sees those) |

Agents do the same through kiln's operations (`runners.list`, `runners.diagnose`, `runners.repair`,
`updates.list`, `updates.retry`, `updates.policy`), with the rights of whoever they act for: a person
who can't manage runners can't have an agent repair one. `runners.list` (`GET /api/agents`) and
`updates.list` list the runners the caller may read: an agent attached to one runner sees that one.
When an upgrade goes through, the agent of an incident about that runner's upgrading (open, or
reported on) is told, so it checks the runner and resolves the incident rather than wait on it.

An upgrade kiln stopped waiting for can still finish: the runner switches once it's idle. When its
new build reports in later (and, upgrading blue/green, takes over), the job reads as done after all
and the runner as up to date.

### How long kiln waits

| | default | `[updates]` |
|---|---|---|
| for a runner to switch to its new build and report in, counted once it has no jobs running | 15 minutes | `upgrade_minutes` |
| while a runner finishes the jobs it is running (it switches only once they end) | 6 hours | `wait_for_jobs_minutes` |
| for a machine to build kiln itself, install included | 60 minutes | `build_minutes` |

System → Updates → *How long kiln waits* changes them for this server (kept in its database; *Use
kiln.toml's* goes back), and so does `PUT /api/updates/waits` with all three (`DELETE` goes back). A
change applies to upgrades under way too. A job that ran out of time says which wait it was, with a
link to change it.

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
- A run of the release pipeline at the commit still under way is the build: the server starts no
  other, and neither does *Build every platform* (System → Builds).
- With kiln's own workflows on the CI engine (`[ci] kiln_workflows`), the build is the engine's run
  of release.yaml. Each leg's `artifacts:` are an artifact of that leg (`artifacts-linux`,
  `artifacts-macos`, docs/ci.md W08-R08). As each leg succeeds, the binaries in its artifact are
  checked and kept, as a v1 job's are. A machine waiting on the build is told which run and leg.
- `[updates] in_step = false` turns this off. System → Builds shows the commit, and which
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

The script hands over to `kiln setup` once kiln is on the machine: at a terminal it asks first,
and on a Mac gives kiln its own signature; without one it asks nothing.

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
