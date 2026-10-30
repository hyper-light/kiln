# Train notes: headroom v2, kiln's own work yields to what runs beside it

Headroom watches each machine's own targets (its disk, production's server) and, when one gets slow
toward its tolerance, slows kiln's own work on that machine (agent sessions, CI legs, transient
units) and starts new sessions only when their memory fits. It is on by default, the exception
docs/ci.md §0.3 names. docs/headroom.md has the design and where every number comes from.

**Where it stands (2026-10-29, when written):**
- **Merged:** with this file, as one fast-forward of main to the crash-guard branch. Main had no
  headroom before this train.
- **Deployed:** not yet. Production first, then the runners take the release.
- **Checked, not fully gated** (2026-10-29, on adacachy):
  - On the landing tree (346c3568b7, main 9598c1e546 merged in): ci/gate.sh's clippy for the
    workspace with all targets, clean, and its build of every library's tests.
  - On the merge with main f7f2588be6 (065cb57f0f): clippy for kiln and kiln-ci (libraries and
    tests), clean; kiln-ci's headroom tests (34), kiln's proto, wire and headroom tests (122) and
    tests/headroom_guard.rs, all passing; the UI's typecheck, lint, 326 tests and build.
  - On the fixes (5c8e2b71bf): clippy for kiln and kiln-ci for Windows (libraries, as the gate
    checks them) and macOS (libraries and tests), clean. On Windows with tests, main's
    crates/kiln/src/ci/review_cli.rs:899 fails (`use super::*` unused there; Kiln PRs told).
  - Not run to its end: the workspace's whole test suite. The gate's guard stopped it twice
    (production's nodes strained, as most of that day), and Ada chose to land on these checks.
- **How it got here:**
  - 10-26: production went down when a runner resumed 47 sessions beside five gates and the disk
    stalled. A first version (v1) ran on adacachy's runner until the updater moved it to main's
    build on 10-27.
  - 10-28, 21:11 to 23:29: v2 ran on adacachy's runner. Programs kiln can't pace (Docker
    containers, a Kubernetes node thrashing under a CI job) filled the disk. v2 cut kiln's writes to
    zero 234 times and never let go, so about 290 sessions and every gate stayed frozen for two
    hours. The runner was rolled back, and v2's guard released every unit it had stopped.
  - Fixed since: a cut that doesn't speed the probes up is undone and not repeated while kiln
    writes no more (4074784dd9); the CPU budget and every demand are held to what the machine has
    (2986521273); the simulation replays that night (5c8e2b71bf).

## What it delivers

- **Detection.** Each target is probed on a schedule its tolerance sets. A CUSUM on its latency,
  calibrated on the machine's own calm, alarms before harm; a probe at its tolerance is harm at once.
- **Budgets.** Limits on kiln's own writes and CPU, found by bisection between what held calm and
  where trouble began. Trouble cuts them; a cut that doesn't speed the probes up while kiln's
  writes are stopped is undone, and isn't repeated while kiln writes no more than it did then.
- **Admission.** A session starts when its memory fits; writes and CPU are paced, not held for.
- **Pacing.** A group is slowed by stopping it part of the time: a cgroup freeze on Linux, SIGSTOP
  on macOS, suspended threads on Windows. Each pacer has a guard process that starts everything it
  stopped again as soon as the pacer ends, however it ends.
- **Settings and status.** Runners → Headroom in the UI and the API: on or off, automatic targets,
  outside units, false alarms a day, the probe's share, most sessions at once, and each target's
  tolerance; each target's latency, the budgets and every decision's reason.
- **Wire.** Capability bit 39 (CAP_HEADROOM), with AgentMsg::HeadroomStatus and ServerMsg::Headroom
  after main's Identity and Observe (docs/wire.md).

## Deploying it

1. Production first, through KilnCD. The server then announces CAP_HEADROOM and answers
   `/api/runners/<name>/headroom`. Nothing is paced until a runner on this build connects.
2. Then the runners, as they take the release. Each starts headroom from its settings (on by
   default) and logs `headroom: watching this machine` with how it paces and how many targets.

## Watching it

- A runner's log: `journalctl --user -u <its unit> -o cat | grep headroom`. Every cut, undo,
  skipped cut and raise is logged with its reason, as is every session held or let in.
- `GET /api/runners/<name>/headroom` (and Runners → Headroom): targets with their latency and
  tolerance, the budgets, what waits and what is slowed, each with its reason.
- Frozen work: a cgroup whose `cgroup.freeze` reads 1 while `systemctl --user show <unit> -p
  FreezerState` says `running` is headroom's. It should come and go within seconds, unless kiln's
  own work is what keeps the disk busy.
- Production: `/healthz`.

## Risks

- **kiln's work slowed while it is the cause.** That's the point; a long pause means kiln's own
  work keeps the disk busy, and the log says so.
- **A known limit.** When other programs hold the disk at its capacity and kiln's own gates add to
  it, stopping the gates doesn't speed the next probe up, so headroom lets them run, as no control
  would.
- **CPU trouble from others.** The undo rule watches writes; a CPU cut for trouble kiln didn't
  cause is raised again only as calm holds.
- **Version skew.** A runner on this build beside a server without CAP_HEADROOM paces from its own
  settings file, and the server shows no status for it.

## Rolling back

- **Turn it off:** Runners → Headroom → off, or the API. The runner lets everything it slows run
  at once, and holds no session back.
- **Roll the release back** through KilnCD; the runners follow. When a pacer ends, its guard
  starts everything it stopped again.

## Open

- macOS and Windows don't say what the disk delivers, so the disk's capacity isn't learned there.
- Each cgroup's own I/O pressure (io.pressure) could tell kiln's stalls from everyone else's better
  than the share of writes does.
- The known limit above.
