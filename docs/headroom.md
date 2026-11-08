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
harmed. A target that declares none takes a person's flow of thought, 1 s: what waits on each of
kiln's own is a person's action ([protect.md](protect.md), "Objectives").
- `fsync`: a few bytes written and made durable in a folder. A database commit or a container's
  teardown waits for the same thing on that file system.
- `http`: `GET` an `http://` URL. A 5xx or a 429 is failed service, however fast it came: harm, at
  once (LB05: a server shedding at once can't look healthy). A refused connection means the target
  is down for its own reasons, not slow, and isn't counted.
- `tcp`: connect to `host:port`.

kiln adds its own unless `automatic` is off: a sync in the runner's data folder and, where the
server the runner reports to runs on this machine, that server's health and how late its shards ran
ready work on average since the last look (`kiln server delay`, [priority.md](priority.md)): its
tolerance is the shards' own probe period, and, as it answers with a level it measured rather than
with how long the probe took, its window (how soon trouble must be caught) is a person's 1 s.
A runner on another machine paces nothing for the server: its work can't slow it. (It once did, and
on 2026-11-05 the Mac runners, reading production's latency while production's own machine was
loaded as harm their builds did, held every agent's run to about 1% of a CPU.)
Each target is probed on a thread of its own, at an interval the engine derives; a probe still out
counts while it hangs.

**Detection.** A CUSUM on each target's log-latency against its calm, looking for a shift toward
its tolerance, calibrated on the machine's own record against the false-alarm budget. A sample at
the tolerance is harm, at once. A target that comes first (`kiln server delay`,
[priority.md](priority.md)) is in trouble once it rises past what's calm: a second CUSUM, its
reference half calm's standard deviation above calm's mean, calibrated the same way. Its alarm
holds until it's back within calm's noise, and calm is learned only from what was measured with
nothing of kiln's paced and no alarm that cut it.

**Budgets.** For kiln's own work, a budget of writes a second and one of CPU cores, each bisected
between the highest level held calm and the lowest level trouble began at. Where the system says
what its disks write (Linux), what they delivered while trouble last showed is all they take, and
the write budget is never raised past it less what others write.

**Cutting only what's short.** kiln's CPU is cut for trouble only while the CPU is short: no core
of the machine idle to within the pacer's step (all processes counted), or a target in trouble
measures how late work is scheduled itself (the server's delay). With cores idle, cutting kiln's
CPU can't speed a disk's probe or a harness up. On 2026-11-05 a Mac's agents were held to 1 core of
18, with 8 idle, for an fsync that slowed.

**Agent messages.** Whether messages get through to the runner's agents is a target too, kiln's
own on every runner (`headroom/messages.rs`), and the same for every harness: Claude Code, Codex,
each ACP agent (Devin, Gemini, Goose, Amp…), Antigravity, and whatever comes next. It isn't probed.
After everything a harness says or is told, the shared harness layer (`harness::kinds::Adapter`)
reports what of each session waits to be delivered:
- a message written to its harness and not yet taken (its delivery `Sent`);
- one its adapter holds until the harness is ready (Codex's thread, an ACP agent's session);
- one in kiln's queue while the harness is starting or idle (not up yet, or held).

A message queued behind a running turn, or waiting on a person's answer, waits by design and isn't
counted, and an exited harness's waits are dropped unmeasured. Each wait is timed from when it began
to when it ended, and the longest still going counts as a probe out, so a harness that's stopped,
starved or never came up alarms while it's stuck, not after. Its tolerance is a person's 1 s, as
for any target that declares none. Its calm level and alarm threshold are learned like every
other target's.

Headroom never stands in a message's way. A session with anything waiting is a person's for as long
as it waits: never held at admission, never paced (the engine decides shares again when a group's
class changes), and the pacer is woken as it begins waiting, so a harness stopped mid-period goes
on at once. Before this (2026-11-05) a Mac's pacer held every agent's processes stopped, harnesses
included, and nothing sent to them arrived.

**Holding.** Writes and CPU aren't held for: the pacer shares the budgets among what runs, from
the first moment it runs, so holding work it could slow would protect nothing and only delay it.
Memory can't be slowed: a session's next turn, a turn kiln continues after a restart, or a goal
starts when its learned peak memory fits what's free, highest response ratio first, with
conservative backfilling, so short work goes first and nothing starves. It waits in kiln's queue
and goes the moment the engine lets it (held sessions are woken; nothing polls). A message a
person sends "now" never waits.

**Pacing.** When running work wants more than a budget, the budget is water-filled class by class,
max-min fair within a class, and each group runs that share of every period (`cgroup.freeze`):
only for the resource that's short. The pacer looks every 50 ms and a period is at least a second.
A group earns its share of the time that passes and spends the time it runs: it starts a slice once
it has earned one (its share of a period, and at least a tick) and runs until that is spent. So a
share above zero always runs and over time gets exactly its share (a 5 ms period, which a fast
disk's probe asks for, once gave every share under a fifth no time at all). A group's CPU and
writes are measured over at least a period too, and the engine is told the part of that time the
group was let run, not the share it was given. Groups are each agent session's cgroup, each CI
job's (a leg's microVM's among them; a job a person paused is the pause's alone, neither stopped nor
let go on by the pacer), and, with `escapes` on, the user's transient units (`systemd-run --user`:
agents' builds). Those units are paced only by the machine's installed runner: the one its supervisor started (its parent is
the process serving the supervisor's control socket it was given), holding `$XDG_RUNTIME_DIR/kiln-headroom-escapes.lock`
(tried again until held, so a runner that took over from another takes it once that one is gone).
A runner a test or a person started paces its own sessions and jobs only. Units kiln itself runs in, units
the desktop starts for a person (`app-…`, `dbus-…`, `xdg-…`) and kiln's guards
(`kiln-headroom-guard-…`) are never paced, and neither is anything someone else froze: a cgroup
already frozen when headroom would stop it is left as it is, and never thawed by headroom. Legs
and agents also run below the server where the machine lets kiln say so
([priority.md](priority.md)): the scheduler puts the server first before the pacer has to.

On macOS and Windows a group is each agent session's processes as kiln holds them (followed fork
by fork on macOS, its job object on Windows), metered process by process (CPU time, bytes
written, resident memory: `sysinfo`) and stopped process by process: SIGSTOP and SIGCONT on
macOS, every thread suspended and resumed on Windows. A process an agent starts while its group
is stopped runs until the next stop; one someone else stopped (macOS) is left alone, and on
Windows each suspension is undone by one resume, so only kiln's own are. Neither system says what
its disks write in all, so `delivered_bps` is unknown there: budgets are measured on kiln's own
work either way.

**Failing safe.** Nothing stays stopped once what stopped it is gone, however it goes (killed,
crashed, its unit restarted, replaced by a build without headroom). Every pacer has a guard, `kiln
headroom-guard`, a small process of its own: under a user manager the installed runner's guard
runs in a scope of its own (`kiln-headroom-guard-<pid>-<start>.scope`, which a restart of the
runner's unit leaves running); elsewhere it runs in a process group of its own, and on Windows
outside the runner's job where the job allows. The pacer stops nothing until its guard says it
runs, and tells it everything it may stop (a cgroup's folder; a process by its id and start on
macOS and Windows) before stopping any of it. The guard keeps the last list whole; once the
pacer's end of its input closes, which the system does however the pacer ends, the guard starts
it all again and goes. Each pacer also keeps its list in `<state>/headroom/paced/<pid>-<start>.json`:
a pacer releases every record whose pacer and guard have both ended, as it starts and once a
period after (and, as it starts, the single `paced.json` older builds kept). A guard that ends while its pacer runs takes nothing with it: the
pacer starts everything again itself and stops nothing until a guard runs again. What was
learned is kept in `<state>/headroom/learned.json`. Headroom runs on its own threads, from
settings it kept, with the server away. How many agent sessions a machine has at work at once is
the agent limit's (`docs/agents.md`), not headroom's.

**A person's pause.** A session a person paused (`Command::Pause`, `docs/agents.md`, "What a
person does with an agent's subagents") has its group held at share 0 on every pass, paced or not:
it never runs, and only while a guard runs, as any stop here. The engine neither counts it nor
learns from it. A runner where headroom doesn't run, or whose machine gives it no way to stop a
session's processes, refuses the pause and says why.

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
  "probe_load_share": 1,
  "escapes": true
}
```

## Where every number comes from (v2)

Headroom v2's decisions live in `crates/kiln-ci/src/headroom` as pure code (`docs/ci.md` §0.3):
the runner passes in what it measured and when, and carries out what comes back. No number in it
is a tuning choice. Each is a measurement, follows from one, or is one of the two things a person
decides.

**What a person decides** (settings, each default saying where it comes from):
- the false-alarm budget: how many times a day headroom may hold or slow work when nothing would
  have gone wrong;
- each target's tolerance: how slow it may get before it's harmed (a person's flow of thought, 1 s,
  by default). A target that declares one (a Kubernetes probe's timeout, a systemd watchdog) brings
  its own;
- the probe-load share: the most of each target's time its probes may take, a cap (none by default:
  below it, each target's own share is derived, [protect.md](protect.md) "Admission, reclaim and
  recovery").

**Everything else:**

| Number | Comes from |
|---|---|
| A target's calm latency | the exact mean of its log-latency in the current regime |
| CUSUM's reference | half the distance from the calm mean to the tolerance (the shift that matters) |
| Calm's noise (a target that comes first) | the exact standard deviation of calm's log-latency (its count, sum and sum of squares) |
| Its reference past calm's noise | half calm's standard deviation above calm's mean: the shift that matters is one of calm's own spread, and half a shift is the reference for detecting it, as above (Page 1954; Lorden; Moustakides) |
| Its threshold | as the alarm threshold, on a record of its own, over the time that record covers (time calm and unpaced) |
| Back within calm's noise (a cut held) | the mirror of that CUSUM, what samples fell short of its reference added up, passing the same threshold, from samples measured after the cut could show (its time plus the longest a cut took to show). At calm that takes threshold / reference samples: as long as a rise past calm's noise takes to alarm |
| The alarm threshold | the higher of: the (allowed + 1)-th largest harmless excursion recorded, where allowed = budget x days recorded; and the height the record's own exponential tail (count and sum of its excursions, CUSUM's near-exponential excursions per Siegmund) puts at the budget's rate |
| Harm | a sample at the tolerance, a failed answer (a 5xx, a 429), or a probe still out at the window (the tolerance, but for a target that answers with a level it measured: the server's delay's is a person's 1 s) |
| A new regime | a fall below the calm the lower CUSUM detects with the same calibration |
| The probe interval | (window - longest measured lag) / (expected samples to alarm + 1), leaving the lag out once it reaches the window (no probe rate wins that time back), and until what the target affords is known; never under the probe's own measured cost / the share it affords |
| The share a target affords its probes | no more than would move its latency by its detector's own reference (the shift it looks for), as an M/M/1 queue: (1 - ρ)(1 - e^-k), its idle share 1 - ρ its fastest answer over its calm; under the person's cap (LB05, [protect.md](protect.md)) |
| The write and CPU budgets | each, for kiln's own work: bisection between the highest level held calm and the lowest level trouble began at; doubling while there is no unsafe level; halving the gap is the only step. Trouble cuts both (either may be the cause) unless the disks are delivering all they were measured to take, when only writes are cut (not for a target that comes first: its latency is the CPU's); each is raised on its own where it limits (what runs wants more, or work waiting would take it past) |
| A level held | the mean kiln's work used over the hold (its integral over time), not its peak: a burst a buffer took isn't a rate the machine sustains. With none in use, no level (as trouble with none in use sets no bound) |
| The CPU's level | what kiln's work used, and no more than its CPU budget let it: CPU paced groups give up goes to unpaced ones, so what kiln uses can stay put while the pacer holds its work to less (11-05: 13-15 cores used under a 7.1-core budget) |
| A bound tried | once the bisection's gap is finer than the pacer applies (a share is whole percent: what's wanted / 100), the bound itself; held calm there, it was wrong and the next bound up (any higher level trouble began at) takes its place |
| Cutting again | only on evidence: a probe of the same target sent after the first answer since the cut, itself rising (past CUSUM's reference), and no faster, while kiln still writes. Probes already out at a cut can't show it. For a target that comes first, its CUSUM past calm's noise passing its threshold again from samples measured after the cut could show; and, where nothing of kiln's was in use when the trouble began, once it is |
| Undoing a cut | the same evidence with kiln's writes already stopped: what's queued isn't kiln's. Its budgets go back to what they were before the trouble began, nothing is learned from the cuts, and nothing more is cut until the trouble clears. The first answer after a cut is only the mark: that probe still waited behind what was queued before the cut |
| Not cutting | trouble that begins while kiln writes no more than it did when a cut was last undone, in bytes a second and as a share of what the disks deliver (forgotten in a new regime) |
| The CPU budget's ceiling | the machine's cores (std::thread::available_parallelism, which counts a container's CPU limit where it can read it) |
| Any demand's ceiling | the machine's cores for CPU, and the disks' capacity for writes: for each running group, and for each kind of work learned, what an earlier build learned included |
| The disks' capacity | the most the machine delivered while trouble showed (a queue built up) on a target that doesn't come first, on systems that say what they write, with every disk written busy throughout (idle under two of Linux's coarsest ticks: a disk idle part of the span wasn't the bottleneck, LB05): the trouble under way's while it lasts, else the last |
| A cut, with the capacity known | to what drains what's queued within the tolerance: capacity x (1 - latency / tolerance) - what others write (Little: what's queued is the latency times the capacity) |
| Calm to calibrate on | an excursion that ended with no alarm, with kiln quiet, or within the longest a cut takes to show (so it ended on its own); one kiln's cut may have ended isn't noise. For a target that comes first, also none with any share under 1 in force |
| Calm to learn from (a target that comes first) | the samples measured with no share under 1 in force and no alarm that cut kiln's work; an excursion past calm's noise is learned as it comes and taken back out once it turns out not to be calm |
| The lag | the longest a cut of kiln's took to show, of its writes or its CPU: to its first answer back within calm's noise, for a target that comes first |
| A probe still out | at least as slow as it has been out: it raises the alarm once that alone would take the CUSUM past its threshold |
| How long a level must hold | the slowest target's expected detection time (for a target that comes first, of a rise past calm's noise too) plus the longest a cut took to show (the first probe after it that answered faster) |
| A run's demand | its peak unslowed write rate, CPU cores and memory, and its duration (a session's turn, a build's unit), recorded when it ends; the mean per kind of work, with the mean over everything as the prior |
| Who starts | when its peak memory fits (writes and CPU are paced, not held for); in order of class, then response ratio ((wait + run) / run); every waiting item a reservation on the time profile of when running work is expected to free memory (conservative backfilling); with nothing learned, one at a time |
| Each running group's share | each budget water-filled class by class, max-min fair within a class; a group runs at the smaller of its two shares, so work is slowed only for the resource that's short |
| When shares change | only when the budget or the set of running work does, a group's demand first shows (a CI job's, before anything is learned of its kind), or a group running at full speed changes what it wants (LB05: a paced group's demand moves with its pacing, so it doesn't count) |

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

The 10-28 night is a trace of its own, since there the trouble isn't kiln's: 60 sessions all
night (0.3 cores and 0.2 MB/s each, 1.2% of what the disk takes) and two gates while the disk is
otherwise quiet, beside programs kiln can't pace that hold the disk near its capacity in 20-minute
stretches, 30 s at 104% of it and 30 s at 92%. `headroom::sim::the_night_of_10_28` asserts on
seeds 1-3 that v2 does at least 99% of the work no control does and stops no job longer than 2 s
at a time.

The 11-05 lab on fedora (goal 8's item2, [priority.md](priority.md)) is a trace of its own, on a
machine of CPUs rather than a disk: the kiln server and a runner's CI legs (32 busy threads) and
agents' turns (64 more), CPU hogs all, in one cgroup on 16 CPUs, the runner's headroom watching
the server's delay (calm 0.195 ms at the median, 12% of reads 2 to 8 times that). The server runs
ready work later as the hogs' runnable threads outnumber the CPUs, as the lab measured: 2.5 times
calm with the legs, 4 to 6.4 with the agents too. A paced group's threads run its share of the
time; CPU it gives up goes to the others, so what kiln uses barely moves. `headroom::sim::
g8_s5_the_11_05_lab_rising_delay_paces_the_legs_and_holds_it_down_until_the_load_ends` asserts on
seeds 1-3 that the legs are paced within 30 s of starting, every 30 s the load stays (past the
first 30 s of each rise) is within calm's noise (one standard deviation of calm's log-latency of its
geometric mean), something stays paced until the load ends, it's calm after, and calm stays what
it was. Main's engine (426b02ea011) fails it as item2 did: the legs at 2.4 to 2.6 times calm and
the agents at 6, every loaded stretch past calm's noise, nothing paced from 5 s after the first cut.
With the changes of goal 8 step 5 the delay holds at 0.224 to 0.258 ms against a calm of 0.226 to
0.239. A target that comes first lengthens every hold (a level counts as held only once a rise past
its calm's noise would have shown); `headroom::sim::g8_s5_the_night_of_10_28_beside_a_calm_server`
runs the 10-28 night with the server's delay a target too, calm all night, and v2 does the same
99.88% of the work, with no job stopped longer than 1.5 s. The other traces' rows are main's.

| trace | control | done | mean wait s | p99 wait s | mean flow s | makespan s | peak sync s | s over 1 s |
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
| disk halves at 30 min | v2 | 350/350 | 205 | 2018 | 628 | 10325 | 0.75 | 0 |
| 10-28 night | none | 62/62 | 0 | 0 | 6973 | 7200 | 1.56 | 1540 |
| 10-28 night | v2 | 62/62 | 7 | 15 | 6981 | 7216 | 1.56 | 1536 |

Over seeds 1-3:

- **The tolerance.** v2 keeps every sync under 1 s on every trace (peak 0.15-0.75 s; 0 s over),
  the disk halving without warning included. Without control and under AIMD, syncs sit at the
  dirty limit (10-20 s) for most of every trace: what hung Docker on 10-26.
- **Work done** by the time the first of v2 and AIMD finished, v2's over AIMD's: 1.0000-1.0882;
  v2 finishes first on every trace but steady seed 2 (the same second).
- **Waits.** Mean wait to start, v2 against AIMD: the crash 849 against 1,148 s, steady 0 against
  42-89 s, bursts 1 against 248 s, the disk halving 205-369 against 1,238-1,811 s.
- **The 10-28 night.** v2 does 99.88% of the work no control does and stops no job longer than
  1.5 s at a time. The disk is over the tolerance as long with v2 as without (1,536 against
  1,540 s): the trouble is the other programs', stopping kiln can't end it, so v2 lets kiln work.
  Where kiln's own gates write heavily while other programs hold the disk at its capacity,
  stopping them doesn't speed the next probe up either, and v2 lets them run too, as no control
  would.
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

The live run on adacachy on 10-28 found what the simulation then had no trace for: with programs
kiln can't pace filling the disk, every cut stopped kiln's writers and nothing let them go. The
check meant to (all of kiln's work idle for longer than the slowest recovery, 221 s that night)
never holds with hundreds of sessions, so about 290 sessions and every gate stayed stopped for two
hours, until the runner was rolled back. Now a cut that doesn't speed the probes up is undone and
not repeated while kiln writes no more, and the night is a trace.

That night's 221 s lag stayed in adacachy's learned state, and on 10-30 it showed what it did to
probing: with the lag past the 1 s tolerance, the time left to act came out at nothing, so every
probe ran as fast as its floor let it. Production's server was probed about 270 times a second,
and each runner's disk back to back, since a disk had no load share to hold it. Now a lag past the
tolerance leaves the interval as it is without one, and a disk is held to the load share like a
server.

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
