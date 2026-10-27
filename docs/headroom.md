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
between the highest level held calm and the lowest level trouble began at.

**Holding.** A session's next turn, a turn kiln continues after a restart, or a goal starts only
when its learned demand fits the budgets and free memory: highest response ratio first, with
conservative backfilling, so short work goes first and nothing starves. It waits in kiln's queue
and goes the moment the engine lets it (held sessions are woken; nothing polls). A message a
person sends "now" never waits.

**Pacing.** When running work wants more than a budget, the budget is water-filled class by class,
max-min fair within a class, and each group runs that share of every period (`cgroup.freeze`):
only for the resource that's short. Groups are each agent session's cgroup and, with `escapes`
on, the user's transient units (`systemd-run --user`: agents' builds). Units the desktop starts
for a person (`app-…`, `dbus-…`, `xdg-…`) are never paced.

On macOS and Windows a group is each agent session's processes as kiln holds them (followed fork
by fork on macOS, its job object on Windows), metered process by process (CPU time, bytes
written, resident memory: `sysinfo`) and stopped process by process: SIGSTOP and SIGCONT on
macOS, every thread suspended and resumed on Windows. A process an agent starts while its group
is stopped runs until the next stop. Neither system says what its disks write in all, so
`delivered_bps` is unknown there: budgets are measured on kiln's own work either way.

**Failing safe.** What's stopped is noted in `<state>/headroom/paced.json` (a cgroup's folder; a
process id on macOS and Windows) and started again on the next start; what was learned is kept in `<state>/headroom/learned.json`. Headroom runs on its own
threads, from settings it kept, with the server away. `max_sessions` never turns away sessions a
restart continues.

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
  "escapes": true,
  "max_sessions": null
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
| A new regime | a fall below the calm the lower CUSUM detects with the same calibration, or an alarm with kiln quiet that outlasts the longest a cut of kiln's ever took to show |
| The probe interval | (tolerance - longest measured lag) / (expected samples to alarm + 1), never under the probe's own measured cost, and for a served target never under its service time / load share |
| The write and CPU budgets | each, for kiln's own work: bisection between the highest level held calm and the lowest level trouble began at; doubling while there is no unsafe level; halving the gap is the only step. Trouble cuts both (either may be the cause); each is raised on its own where it limits |
| How long a level must hold | the slowest target's expected detection time plus the longest measured lag |
| A run's demand | its peak unslowed write rate, CPU cores and memory, and its duration (a session's turn, a build's unit), recorded when it ends; the mean per kind of work, with the mean over everything as the prior |
| Who starts | class, then response ratio ((wait + run) / run); every waiting item a reservation on the time profile of when running work is expected to free room (conservative backfilling); with nothing learned, one at a time |
| Each running group's share | each budget water-filled class by class, max-min fair within a class; a group runs at the smaller of its two shares, so work is slowed only for the resource that's short |
| When shares change | only when the budget or the set of running work does |

Representation limits are not tuning: estimates are exact counts, sums and extremes, so none has a
resolution or a window.

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
