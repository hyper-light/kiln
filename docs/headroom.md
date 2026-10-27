# Headroom

A machine never gets so busy that what matters on it stops working: on any hardware, without
root, whatever kiln deploys there or runs there.

## Why

On 2026-10-26 adacachy, which runs production kiln on minikube and a runner hosting up to 64 agent
sessions, hung twice. The disks were 16–60% busy: what ran out was the time a sync takes. About
five agents' gates ran `cargo test --workspace` at once, each linking about 100 test binaries of
1.3–2 GB. When Docker tore a container down, the overlay unmount synced the whole host btrfs
(`ovl_sync_fs` → `sync_inodes_sb`) and waited behind those writes for minutes. Docker hung,
production's node went NotReady, and the host locked up. After the reboot, the runner resumed all
47 sessions at once and their gates did it again.

The kernel's own pressure numbers didn't tell this apart from noise either: the same evening,
`/proc/pressure/io` read 60% "full" on an idle machine, because a terminal waiting on io_uring
counts as IO wait.

## How it works

kiln protects outcomes, not resource levels. It measures what matters directly, learns how much
of its own work the machine takes before that suffers, and only ever slows down what it started
itself, which needs no privileges. Headroom is on by default (the exception in `docs/ci.md` §0.3),
and every decision it makes says why.

**Targets.** What must stay healthy, each with a tolerance: how slow it may get before it's
harmed. A target that declares none takes the platform's default, Kubernetes' 1 s probe timeout.
- `fsync`: a few bytes written and made durable in a folder. A database commit or a container's
  teardown waits for the same thing on that file system.
- `http`: `GET` an `http://` URL. A 5xx or a refused connection means the target is down for its
  own reasons, not slow, and isn't counted.
- `tcp`: connect to `host:port`.

kiln adds its own unless `automatic` is off: a sync in the runner's data folder and the health of
the server the runner reports to. Each target is probed on a thread of its own, at an interval the
engine derives; a probe still out counts while it hangs.

**Detection.** A CUSUM on each target's log-latency against its calm, looking for a shift toward
its tolerance, calibrated on the machine's own record against the false-alarm budget. A sample at
the tolerance is harm, at once.

**Budgets.** For kiln's own work, a budget of writes a second and one of CPU cores, each bisected
between the highest level held calm and the lowest level trouble began at. Where the system says
what its disks write (Linux), what they delivered while trouble last showed is all they take, and
the write budget is never raised past it less what others write.

**Holding.** Writes and CPU aren't held for: the pacer shares the budgets among what runs, from
the first moment it runs, so holding work it could slow would protect nothing and only delay it.
Memory can't be slowed: a session's next turn, a turn kiln continues after a restart, or a goal
starts when its learned peak memory fits what's free, highest response ratio first, with
conservative backfilling, so short work goes first and nothing starves. It waits in kiln's queue
and goes the moment the engine lets it (held sessions are woken; nothing polls). A message a
person sends "now" never waits.

**Pacing.** When running work wants more than a budget, the budget is water-filled class by class,
max-min fair within a class, and each group runs that share of every period (`cgroup.freeze`):
only for the resource that's short. Groups are each agent session's cgroup and, with `escapes`
on, the user's transient units (`systemd-run --user`: agents' builds), paced by one kiln a
machine (the one holding `$XDG_RUNTIME_DIR/kiln-headroom-escapes.lock`) and never one that kiln
itself runs in. Units the desktop starts
for a person (`app-…`, `dbus-…`, `xdg-…`) are never paced.

On macOS and Windows a group is each agent session's processes as kiln holds them (followed fork
by fork on macOS, its job object on Windows), metered process by process (CPU time, bytes
written, resident memory: `sysinfo`) and stopped process by process: SIGSTOP and SIGCONT on
macOS, every thread suspended and resumed on Windows. A process an agent starts while its group
is stopped runs until the next stop. Neither system says what its disks write in all, so
`delivered_bps` is unknown there: budgets are measured on kiln's own work either way.

**Failing safe.** What's stopped is noted in `<state>/headroom/paced.json` (a cgroup's folder; a
process id on macOS and Windows) and started again on the next start; what was learned is kept in `<state>/headroom/learned.json`. Headroom runs on its own
threads, from settings it kept, with the server away. How many agent sessions a machine has at
work at once is the agent limit's (`docs/agents.md`), not headroom's.

## Settings

Per runner, in the UI (Runners → Headroom) or the API:
- `GET /api/headroom`: every runner that runs headroom.
- `GET /api/runners/:name/headroom`: its settings, whether it applied them, and what it reports.
- `PUT /api/runners/:name/headroom`: new settings (checked, kept, sent to the runner at once).

The server keeps them (`kv` key `headroom/<runner>`) and sends them when the runner connects and
when they change (`ServerMsg::Headroom`, to runners announcing `CAP_HEADROOM`). The runner keeps
them in `<state>/headroom/settings.json` and applies them at start, before any session resumes.

```json
{
  "enabled": true,
  "automatic": true,
  "targets": [{ "name": "production", "probe": { "kind": "http", "url": "http://192.168.49.2:30070/healthz" }, "tolerance_ms": 3000 }],
  "false_alarms_per_day": 1,
  "probe_load_share": 0.1,
  "escapes": true
}
```

## Where every number comes from (v2)

Headroom v2's decisions live in `crates/kiln-ci/src/headroom` as pure code (`docs/ci.md` §0.3):
the runner passes in what it measured and when, and carries out what comes back. No number in it
is a tuning choice. Each is a measurement, follows from one, or is one of the two things a person
decides.

**What a person decides** (settings, with defaults from the platform where it has one):
- the false-alarm budget: how many times a day headroom may hold or slow work when nothing would
  have gone wrong;
- each target's tolerance: how slow it may get before it's harmed. A target that declares one
  (a Kubernetes probe's timeout, a systemd watchdog) brings its own;
- the probe-load share: how much of a served target's time its probes may take.

**Everything else:**

| Number | Comes from |
|---|---|
| A target's calm latency | the exact mean of its log-latency in the current regime |
| CUSUM's reference | half the distance from the calm mean to the tolerance (the shift that matters) |
| The alarm threshold | the higher of: the (allowed + 1)-th largest harmless excursion recorded, where allowed = budget x days recorded; and the height the record's own exponential tail (count and sum of its excursions, CUSUM's near-exponential excursions per Siegmund) puts at the budget's rate |
| Harm | a sample, or a probe still out, at the tolerance |
| A new regime | a fall below the calm the lower CUSUM detects with the same calibration, or trouble that lasts with kiln's own work quiet (never begun, or stopped by its cuts) longer than any cut of kiln's took to show (the tolerance before one has): kiln isn't the cause, and its work runs again |
| The probe interval | (tolerance - longest measured lag) / (expected samples to alarm + 1), never under the probe's own measured cost, and for a served target never under its service time / load share |
| The write and CPU budgets | each, for kiln's own work: bisection between the highest level held calm and the lowest level trouble began at; doubling while there is no unsafe level; halving the gap is the only step. Trouble cuts both (either may be the cause) unless the disks are delivering all they were measured to take, when only writes are cut; each is raised on its own where it limits (what runs wants more, or work waiting would take it past) |
| A level held | the mean kiln's work used over the hold (its integral over time), not its peak: a burst a buffer took isn't a rate the machine sustains |
| A bound tried | once the bisection's gap is finer than the pacer applies (a share is whole percent: what's wanted / 100), the bound itself; held calm there, it was wrong and the next bound up (any higher level trouble began at) takes its place |
| Cutting again | only on evidence: a probe of the same target sent after the first answer since the cut, itself rising (past CUSUM's reference), and no faster. Probes already out at a cut can't show it |
| The disks' capacity | the most the machine delivered while trouble showed (a queue built up), on systems that say what they write: the trouble under way's while it lasts, else the last |
| A cut, with the capacity known | to what drains what's queued within the tolerance: capacity x (1 - latency / tolerance) - what others write (Little: what's queued is the latency times the capacity) |
| Calm to calibrate on | an excursion that ended with no alarm, with kiln quiet, or within the longest a cut takes to show (so it ended on its own); one kiln's cut may have ended isn't noise |
| A probe still out | at least as slow as it has been out: it raises the alarm once that alone would take the CUSUM past its threshold |
| How long a level must hold | the slowest target's expected detection time plus the longest a cut took to show (the first probe after it that answered faster) |
| A run's demand | its peak unslowed write rate, CPU cores and memory, and its duration (a session's turn, a build's unit), recorded when it ends; the mean per kind of work, with the mean over everything as the prior |
| Who starts | when its peak memory fits (writes and CPU are paced, not held for); in order of class, then response ratio ((wait + run) / run); every waiting item a reservation on the time profile of when running work is expected to free memory (conservative backfilling); with nothing learned, one at a time |
| Each running group's share | each budget water-filled class by class, max-min fair within a class; a group runs at the smaller of its two shares, so work is slowed only for the resource that's short |
| When shares change | only when the budget or the set of running work does |

Representation limits are not tuning: estimates are exact counts, sums and extremes, so none has a
resolution or a window.

## Simulation

`crates/kiln-ci/src/headroom/sim.rs` runs v2's engine, AIMD with first-come-first-served
admission (a window of concurrent jobs, +1 for each calm probe while it limits, halved when a probe
passes the tolerance), and no control over the same traces, deterministically (virtual time, a
seeded generator). The machine is a fluid model of the 10-26 mechanism: work writes into the page
cache, the disk drains it at its capacity, writers are throttled only once the dirty backlog
reaches its limit (10 GB), and a sync waits behind the whole backlog. It is adacachy-like (32
cores, 128 GB, a 1 GB/s disk); gates write 400 MB/s for 150 s on 4 cores with 8 GB, turns 5 MB/s
for 60 s. None of that is given to the engine. Traces: the 10-26 crash (five gates, then 47
resumed at once at 600 s), Poisson arrivals at 80% of the disk for two hours, four bursts of 8
gates and 10 turns, and the steady trace with the disk halving at 30 minutes. `cargo test -p
kiln-ci headroom::sim::compare -- --nocapture` prints the table (seed 1) and asserts, on every
trace and seeds 1-3: v2 does as much work as AIMD by the time the first of them finishes, with a
lower mean wait, less time over the tolerance and a lower peak, and none at all once it has
detected trouble.

| trace | control | done | mean wait s | p99 wait s | mean flow s | makespan s | peak sync s | s over 1 s |
|---|---|---|---|---|---|---|---|---|
|---|---|---|---|---|---|---|---|---|
| 10-26 crash | none | 52/52 | 0 | 0 | 2568 | 3410 | 10.00 | 3108 |
| 10-26 crash | AIMD+FCFS | 52/52 | 1148 | 2709 | 1424 | 3459 | 10.00 | 2958 |
| 10-26 crash | v2 | 52/52 | 849 | 1920 | 1729 | 3420 | 0.20 | 0 |
| steady 80% | none | 350/350 | 0 | 0 | 115 | 7299 | 10.00 | 3776 |
| steady 80% | AIMD+FCFS | 350/350 | 42 | 260 | 129 | 7324 | 10.00 | 3313 |
| steady 80% | v2 | 350/350 | 0 | 0 | 104 | 7304 | 0.15 | 0 |
| bursts | none | 72/72 | 0 | 0 | 313 | 4073 | 10.00 | 1918 |
| bursts | AIMD+FCFS | 72/72 | 248 | 492 | 472 | 4152 | 10.00 | 1906 |
| bursts | v2 | 72/72 | 1 | 4 | 250 | 4083 | 0.20 | 0 |
| disk halves at 30 min | none | 350/350 | 0 | 0 | 1401 | 10238 | 20.00 | 9204 |
| disk halves at 30 min | AIMD+FCFS | 350/350 | 1238 | 3073 | 1355 | 10360 | 20.00 | 8898 |
| disk halves at 30 min | v2 | 350/350 | 205 | 2017 | 628 | 10324 | 0.75 | 0 |

Over seeds 1-3:

- **The tolerance.** v2 keeps every sync under 1 s on every trace (peak 0.15-0.75 s; 0 s over),
  the disk halving without warning included. Without control and under AIMD, syncs sit at the
  dirty limit (10-20 s) for most of every trace: what hung Docker on 10-26.
- **Work done** by the time the first of v2 and AIMD finished, v2's over AIMD's: 1.0000-1.0882;
  v2 finishes first on every trace but steady seed 2 (the same second).
- **Waits.** Mean wait to start, v2 against AIMD: the crash 849 against 1,148 s, steady 0 against
  42-89 s, bursts 1 against 248 s, the disk halving 205-369 against 1,238-1,811 s.
- **Time to finish.** v2's mean is lower on every trace but the crash (1,729 against 1,424 s):
  there every job is the same size, where first come first served is best on that measure and
  sharing (v2 holds work only for memory) is not; with jobs of different sizes, as kiln's are,
  sharing's mean doesn't grow with how much they vary, and v2's is lower (steady 104-126 against
  129-186 s, bursts 250 against 472 s).

The simulation found, and these changes fixed, faults in the engine the unit tests hadn't:

- cuts made by probes already out when a cut was made, ratcheting the budget to nothing;
- trouble with nothing of kiln's in use setting a bound at zero, which nothing could raise again;
- false bounds kept for good, and higher bounds forgotten when a lower one was shown wrong;
- work larger than the budget never starting, and work held that the pacer could have shared;
- the CPU budget never raised while work waited on it, and cut when the disks were the cause;
- a moment's burst counted as a level held;
- a cut's lag measured to the end of the drain rather than to when it showed, and never measured
  when a cut drained before a slow answer came;
- cuts halving blindly where the queue says how much drains it;
- excursions kiln's own cuts ended counted as noise, so each intervention taught the detector to
  alarm later;
- work started in a step running unpaced until the next;
- a probe still out not counted as evidence until it answered.

## Debug builds

kiln's dev and test builds keep line tables only (`[profile.dev] debug = "line-tables-only"`):
backtraces still give file and line, and each gate writes several times less.

## Next

- Pausing containers, microVMs, VMs and Kubernetes pods through their own controls, with OS
  limits where available.
- Targets from KilnCD's records of what it deployed on each machine.
- Learned costs per command and leases on the window; merging identical gates; moving work to
  calmer machines; the server pacing what it sends a busy runner, and itself.
- Work on a separate file system where one is writable without root.
