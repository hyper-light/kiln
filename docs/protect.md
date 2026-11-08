# Protect: what kiln's heaviest work may not break

kiln's own work (agents' builds, CI legs, the gate) runs beside services that matter more: a kiln
server, the machine's daemons, a person's shell. This is how kiln keeps them served, and where every
number it does that with comes from. It is LB05 of the load-balancing program (plan
`01a19b3f080775379a337525ad2905ce`, research `01a19b3f08187580b93f41bd08617e43`), steps 1 to 3, and
audit item G11 (the gate's guard). Headroom ([headroom.md](headroom.md)) paces kiln's work by what it
measures; the guard (`kiln protect`, below) stops the gate; both decide in pure code
(`kiln_ci::headroom`, `kiln_ci::protect`, docs/ci.md §0.3).

The rules: no number is picked. Each is measured, derived from a cited source, the platform's own,
or a person's setting whose default says where it comes from. Nothing polls: what can tell kiln of
a change does, and a clock is armed only for a real deadline.

## Objectives (LB05 step 1)

**Who waits.** A kiln server already classes every request by who waits on it (`overload::class_of`,
`kiln_ci::protect::objective::Useful`): **Critical** (health, sign-in, a person's pages and actions,
a cancel among them), **Normal** (the API: agents, tools, the CLI) and **Background** (polling,
catch-ups, sync, telemetry). These are the useful-request classes.

**Deadlines.** A request is served when it's answered by its deadline. A person's is their flow of
thought: "1.0 second is about the limit for the user's flow of thought to stay uninterrupted"
(J. Nielsen, "Response Times: The 3 Important Limits", from Usability Engineering, 1993; after
R. B. Miller, AFIPS FJCC 1968, and S. K. Card, G. G. Robertson and J. D. Mackinlay, CHI 1991):
`objective::FLOW_US`, 1 s. It is the default of every deadline a person sets (Headroom's target
tolerance, the guard's `--deadline-ms`), and the tolerance of Headroom's own targets, each of which
a person waits on (a sync in the runner's folder, the server's health, a message reaching an agent).
It was Kubernetes' probe timeout, borrowed: the same 1 s, now for the outcome it protects.

**Failures.** A request that failed served no one, however fast: a fast 503 is failed service,
never a fast answer, and so is a 429 (RFC 6585 §4) or no answer (`Outcome::met`). Headroom's HTTP
probe now answers `Answer::Failed` for them, harm at once (it was "down" and counted for nothing, so
a server shedding at once looked healthy: research f6).

**Quantile and error objectives.** The share of a class's requests served by its deadline is its
objective. What's watched is that share's two ways of failing: latency rising toward the deadline
(a CUSUM on log-latency toward it, `headroom::detect`), and failures (harm at once).

**Ambient foreground demand.** The service's own load, as its calm: each detector's calm is learned
from the service's own answers in its current regime; the server's overload watches learn each
queue's calm from periods it didn't stand (`overload::Standing`). kiln's work is what's added to it.

**Reserved control and cancel capacity.** On the server, a class, not a share: Critical is never
shed and runs first (`overload`), and the server's slice outweighs legs and agents 1000 to 1
(docs/priority.md). Where a share must be kept instead, an M/M/1 server's response is exponential
with rate μ − λ (Wikipedia, "M/M/1 queue", after L. Kleinrock, Queueing Systems vol. 1, 1975), so a
control request of service time s is answered by D but for a share ε while its load ρ ≤ 1 −
s·ln(1/ε)/D: the share to keep free is s·ln(1/ε)/D, s measured, D and ε the objective's. The
guard's own control path, `systemctl --user stop`, is measured each time it runs (`act_us`).

**Allowed burst risk.** A person's: how many times a day kiln may act when nothing would have gone
wrong (Headroom's `false_alarms_per_day`, the guard's `--false-alarms-per-day`, 1 by default as
Headroom's has been). Thresholds are calibrated on the machine's own record against it; harm (a
deadline missed) never waits on a record.

**The reaction budget.** From trouble's start, protection has the deadline less the calm to act,
and a reaction takes four parts, each measured (`protect::budget::Reaction`): sensing (a watcher
seeing it), deciding, acting (until what was stopped stops) and draining what was already issued
(Little's law: the wait behind a queue L draining at λ is L/λ; a sync waits behind every dirty page
of its file system, 2026-10-26). Below zero, reacting can't keep the deadline; only not starting
what would break it can.

| Part | The guard's | Headroom's |
|---|---|---|
| Sensing | an unprivileged PSI trigger is looked at every 2 s and a tick (`PSI_FREQ`, psi.c v6.12), a privileged one ten times a window; a server's line, as it happens | its probe interval and the samples its CUSUM takes to alarm (`detect::interval_us`) |
| Deciding | microseconds: one pure step | one pure step |
| Acting | `systemctl --user stop`, measured each time (`Learned::act_us`) | a group's next switch, due at once (the switch's lag measured: `pace::cycle_us`) |
| Draining | the work's own dirty pages over the disks' rate, said with every stop for I/O | the longest a cut took to show (`lag_us`), learned |

## The model, and what falsifies it (LB05 step 2)

kiln lays a slowdown on its own work only with evidence its work could be the cause
(`protect::cause`). Each resource, what's valid input, and the safe fallback:

| Resource | Evidence it's kiln's | Without it | Unknown |
|---|---|---|---|
| CPU contention | kiln's work ran on the CPUs over the span; for Headroom, the CPU short (no core idle to the pacer's step) or a target that measures scheduling itself (the server's delay) | not kiln's: cutting its CPU can't speed the target | kiln's (protects the service) |
| Memory and reclaim | kiln's work holds memory | not kiln's | kiln's |
| Disk queueing | kiln's work did I/O, or has dirty pages of its own still to write back (cgroup writeback attributes them to it on ext2, ext4, btrfs, f2fs and xfs: cgroup-v2.rst "Writeback") | not kiln's | kiln's (a user's unit has no io controller by default to count its I/O: systemd's user@.service delegates pids, memory and cpu) |
| Async writeback | as disk queueing: the work's `file_dirty` and `file_writeback` | | |
| Cache and memory bandwidth | not observable without privileges (perf's uncore counters): it shows only as a CPU-bound target's latency rising with cores idle and no stall, which is why the server's delay is cut for regardless of idle cores | | |

**What the disks take.** Headroom took what the machine delivered during any alarm for the disks'
capacity, and capped kiln's writes there for good (research f9). Now it learns it only when every
disk written over the span was busy throughout (`cause::at_capacity`): a disk idle part of the span
wasn't the bottleneck. The busy counter counts ticks since Linux 5.0 and can count less than there
was, never more (iostats.rst, v6.12), so idle under two of Linux's coarsest ticks (20 ms: HZ_100,
kernel/Kconfig.hz) isn't taken as idle; the counter's misses keep a capacity unlearned, the safe
way (no capacity learned caps nothing). Busy throughout is what fullness needs, not proof of it on
a disk that serves requests in parallel.

**The counterexamples** (tests that fail on the model before this):
- CPU only: an fsync slow while the CPU is short and the disk idle 60% of a second learns no disk
  capacity, and cuts kiln's CPU with its writes
  (`headroom::tests::lb05_a_slow_fsync_with_the_disk_idle_learns_no_disk_capacity`);
- disk only: the disk busy throughout with 20 of 32 cores idle learns capacity and cuts no CPU
  (`lb05_a_full_disk_with_cores_idle_learns_capacity_and_cuts_no_cpu`);
- the guard's: an I/O stall while kiln's work only computed, or a CPU stall while it only wrote,
  stops nothing (`protect::guard::tests::lb05_g11_cpu_only_and_disk_only_counterexamples_stop_nothing`).

**Transient or persistent.** A burst whose excursion ends on its own is calm's: the record of such
excursions calibrates each threshold, so one only alarms past what the machine's own bursts reach.
Harm (a deadline missed) acts at once. Correlated noise is why the calibration is empirical and not
a Gaussian formula (`detect::Peaks`: the (allowed + 1)-th largest harmless excursion, held above the
record's exponential tail).

**Changed demand under a stable id.** Shares are decided again when a group running at full speed
changes what it wants (an agent's turn that starts a build). A paced group's demand is read through
its share, and the CPU it gives up goes to others (2026-11-05), so it moves with the pacing itself:
deciding again on it would chase that, as the 11-05 lab's simulation shows (it fails otherwise).

## Admission, reclaim and recovery (LB05 step 3)

- **Reused:** Headroom's engine, probes and pacer, as sound: its detectors, its bisection of the
  budgets, its pacing by class.
- **The tolerance:** the deadline of what waits ([Objectives](#objectives-lb05-step-1)).
- **The probe share:** derived. A probe taking δ of a target's time raises an M/M/1 target's latency
  by (1 − ρ)/(1 − ρ − δ); probes may move it no more than the target's own detector's reference (the
  shift it looks for), so probing can't make the trouble it looks for: δ ≤ (1 − ρ)(1 − e^−k), its
  idle share 1 − ρ its fastest answer over its calm one (`Detector::probe_share`). Until that's
  known, probing faster wins no lag back. A person's `probe_load_share` caps it (1 by default: no
  cap; it was 0.1, Kubernetes' period over its timeout).
- **Overshoot prevented:** the guard keeps the gate from starting beside a service strained already
  (a server's queue standing, its worst shard a period late, a service the kernel averages at
  harm's rate); Headroom starts work paced from its first moment at what its kind was learned to want.
- **Recovery** ramps from what was measured: a budget rises only after a level held calm for the
  slowest target's detection delay plus the longest a cut took to show, and a target that comes
  first holds its alarm until it's back within calm's noise (the mirror CUSUM): hysteresis from the
  same calibration (headroom.md).
- **Replayable:** the guard's inputs and decisions go to its log, and `kiln protect --replay <log>`
  makes the same decisions again (`guard::replay`, tested from JSON).
- **Admitted, observed, enforced**, kept apart:

| | Admitted (asked) | Enforced (applied) | Observed |
|---|---|---|---|
| Headroom | each group's share (`Pace.share`) | the pacer's whole percent (`step_of`) | the share of time it was let run (`Group.share`, `ran_share`) |
| The gate's I/O | `IOWeight=1` | a weight is enforced by io.cost or BFQ, in a cgroup with the io controller: the gate says which, disk by disk | its I/O (`io.stat`) where counted |
| The guard | a stop | `systemctl --user stop`, timed | the work's exit |

## The guard (G11)

`ci/gate.sh` runs the gate as a systemd user unit through `kiln protect` (`crates/kiln/src/protect.rs`,
the thin side; `kiln_ci::protect::guard`, the decisions). It stops the unit when the machine's own
services suffer and the gate can be the cause, and keeps it from starting beside a strained one.

**What it watches, by event:**
- **The kernel.** A PSI trigger on each of `/proc/pressure/{cpu,memory,io}` ("some <stall µs>
  <window µs>": Documentation/accounting/psi.rst) wakes it once a window of the machine's "some"
  stall reaches what any service's stall at the reference would force (below), and it reads then
  each protected service's "full" stall from its cgroup
  (`/sys/fs/cgroup/<slice>/<unit>/<resource>.pressure`, readable by anyone). A service's "full"
  forces the machine's "some" to at least the trigger, so no window a service was stalled past the
  reference goes unseen.
- **A kiln server's own word.** `GET /healthz/strain` streams one line at once, one each time what
  its shards or pools shed changes, and one whenever the guard's deadline passes without another
  (`beat`), never more often than its shards' probes measure (17.07 ms). Each line carries
  `/healthz/delay`'s figures, its queues' standing and the requests shed. Its ready work's lateness
  is read against its own period by headroom's detector; a pool's line standing (kiln.db's writer,
  the files' share) is I/O, the compute pool's is CPU; a shard's own queue standing while it ran most
  of the time is its own work, not kiln's.
- **The work.** Its exit, which a thread waits on; its cgroup's `cpu.stat`, `io.stat`, `memory.stat`
  and `memory.current`, read as the rest is.

**What it protects:** every cgroup a level below the root's (each slice's units; a top-level leaf
such as `init.scope` itself): `kiln.slice/kiln-server.service`, `system.slice/docker.service`,
another user's slice; never the gate's own nor the guard's (the person's own slice with them), nor
containers kiln made (their `kiln.runner-work` label: kiln's work, Headroom's to pace). With
`--guard-containers`, those containers alone.

**How it decides** (`kiln_ci::protect::stall`): harm is a whole deadline of "full" stall in the
kernel's window (every request of that service needing the resource waited a deadline in it, or
could have); a CUSUM on each window's stall, its reference halfway from calm to harm (Page,
Biometrika 1954, as headroom's), alarms sooner once the guard's own record holds harmless
excursions to calibrate on (headroom's calibration, against the person's budget, over the time
watched); with none recorded, only harm stops. The kernel's trigger on the whole machine sits
where a service's stall at the reference would put the machine's: the kernel averages each group's
per-CPU stall by that group's non-idle time ("For averaging, each CPU is weighted by its non-idle
time in the sampling period": psi.c's `collect_percpu_times`), so one service starved on one CPU of
N busy ones moves the machine's average by as little as a stall squared over N windows: the trigger
is reference² / (CPUs × window), 3.9 ms of 2 s on 32 CPUs (`stall::wake_us`; armed at the reference
itself, as first written, it slept through exactly that case, and a proptest of the averaging now
holds it). Windows it didn't wake for held every service short of the reference, so they only bring
the CUSUM down, and clamped falls add up as their sum does: the CUSUM misses nothing it would have
seen window by window (a proptest holds it to that too). On a machine that stalls at all, the kernel
wakes it about once a window (2 s): that's the price of watching root's services from a user's
seat, and it is the kernel's, not a clock of kiln's. A server
silent past the deadline plus RFC 6298's timeout on its lateness (α 1/8, β 1/4, K 4, a 1 s floor)
stopped answering; so did one whose stream ended: kiln can't tell why, and stopping its work is the
side that protects it.

**Checked on a real Linux** (Linux 7.3.3, 32 CPUs, an unprivileged user, a systemd 262 user manager;
2026-11-08):
- Triggers on `/proc/pressure/io` are taken for windows of 2 s and 4 s and refused (`EINVAL`) for
  1 s, 1.5 s and 12 s, for a threshold of 0 and one past the window. The files of `system.slice` and
  its units are root's (`EACCES` for the write that makes a trigger); a user unit's own are the
  user's, and a trigger on one is accepted. A file with no trigger polls ready at once
  (`POLLERR | POLLPRI`).
- An armed trigger woke on `POLLPRI` every 2 s and only while its threshold was reached, the
  averages work's cadence. 300 MB of direct, synchronous writes by one process, with all else the
  machine did in those 9 s, moved its "some" total by 73 ms on 32 CPUs: the dilution `wake_us` is
  there for. A trigger at the reference (0.5 s of a 2 s window) would have slept through it.
- `kiln protect` over a real `systemd-run --user` unit, its triggers armed, a stand-in server on
  loopback: it ran the unit to its end; stopped it (75, why said, the unit gone) when the server's
  `store_write` line stood while the unit wrote; left it alone when the same line stood after the
  unit had gone quiet for a deadline ("not kiln's"); stopped it when the server's stream ended and
  when the server went silent for its deadline and RFC 6298's second; refused to start it beside a
  server that refused or whose queue already stood; and read 53 cgroups' stalls, and the unit's own
  CPU, I/O, dirty pages and memory, at each wake. Every run replayed to the same decisions.
- The unit's `IOWeight=1` is written (`io.weight`: default 1) and not enforced there: the disks'
  scheduler is kyber and `io.cost.qos` is empty, as the gate's dry run says, disk by disk.

**What an unprivileged user can watch by event** (Linux 6.12; psi.c and cgroup.c read at that tag):
- PSI triggers on `/proc/pressure/*`: the files are 0666, and since Linux 6.4 (commit d82caa273565,
  "sched/psi: Allow unprivileged polling of N*2s period") a trigger is taken from any user whose
  window is a whole number of 2 s, evaluated by the averages work every 2 s; notifications are
  "rate-limited to one per tracking window". Before 6.4, a write needs `CAP_SYS_RESOURCE`: the guard
  then says stalls go unwatched. A file with no trigger polls ready at once (`EPOLLERR | EPOLLPRI`),
  so nothing can be waited on without one.
- PSI triggers on a cgroup's own files: they're the owner's to write (`S_IWUSR`; a cgroup and its
  files belong to whoever made it: cgroup.c's `cgroup_create`, `cgroup_add_file`). The gate's own
  unit is the user's; `system.slice` and `kiln.slice` are root's. So a service's own stall is read
  at the machine's wake, not watched by a trigger of its own.
- `memory.events`, `cgroup.events`, `pids.events`: a change "generates a file modified event"
  (cgroup-v2.rst), readable by anyone: memory limits hit, a cgroup emptied. Not stalls.
- systemd: `MemoryPressureWatch=` (254) and `CPUPressureWatch=` / `IOPressureWatch=` (261: "which
  enable services to get generic notifications on CPU or IO pressure events") make a service's
  pressure file "accessible for reading and writing by the service's user", per 2 s window.
- io.latency is enforced among peers ("The limits are only applied at the peer level"), so it
  protects a service from its siblings: `IODeviceLatencyTargetSec=` on a system unit is root's.
  io.cost's `io.cost.qos` and `io.cost.model` exist "only on the root cgroup", root's too; it's what
  makes `io.weight` proportional on a multi-queue disk, where BFQ isn't the scheduler.

**What needs the person (root, once):**
- for the guard to wake on the server's own stall rather than the machine's: `IOPressureWatch=yes`
  and `CPUPressureWatch=yes` (systemd 261 or later) on `kiln-server.service`, which hands its
  `io.pressure` and `cpu.pressure` to its user (on adacachy, the same user the gate runs as);
- for `IOWeight=1` to be enforced: the io controller delegated to the user manager
  (`Delegate=pids memory cpu io` in a drop-in for `user@.service`) and, on each disk, BFQ as its
  scheduler or `io.cost.qos` enabled;
- for windows under 2 s and sensing ten times a window: `CAP_SYS_RESOURCE` for the guard.

**The gate's guard before `kiln protect`.** A runner's kiln from before this branch has no
`kiln protect`: the gate then runs its loop of 2026-10-28 (G11's numbers, below), and says so, until
the machine's runner is upgraded. Upgrading runners is the person's.

## The gate's I/O (G11's caps)

The gate capped its reads at 400 MB/s and its writes at 150 MB/s on each disk its build is on. A
write cap limits how fast the gate's own dirty pages are written back, not how many it holds: the
kernel lets a cgroup's dirty pages grow to its share of the dirty limit ("Both system-wide and
per-cgroup dirty memory states are examined and the more restrictive of the two is enforced":
cgroup-v2.rst), and a whole file system's sync (an overlay's unmount, 2026-10-26) waits behind all
of them. Little's law: the wait is what's queued over the rate it drains, so a cap lengthens it.
A cap also holds the gate back with the disk idle. A weight gives way only where others want the
disk ("work conserving"), so the gate asks the least, `IOWeight=1`, and says, disk by disk, whether
it's enforced. Where a person wants caps, `--io-read` and `--io-write` set them.

## Every number

The guard's and the gate's:

| Number | Was | What it controls | Now | From |
|---|---|---|---|---|
| system.slice's "full" avg10 over **30** | the loop of 2026-10-28 | when a service is strained | harm: a deadline of "full" stall in a window (50% of a 2 s window for 1 s); sooner, the CUSUM past its calibrated threshold | the deadline (Nielsen); the kernel's window (psi.c); headroom's calibration |
| **three** checks running | the loop | how long strain must last | the CUSUM's accumulated evidence, calibrated on the record; harm at once | Page 1954; the person's false-alarm budget |
| **10 s** apart | the loop | how often it looked | nothing looks: the kernel's trigger wakes it, at most once a window | psi.rst: "rate-limited to one per tracking window" |
| `--guard-url` over **3 s** | the loop | when a URL was slow | the server's own word; silence past the deadline and RFC 6298's timeout | RFC 6298 §2 |
| curl's **10 s** `--max-time` (and its `echo 10`) | the loop | a URL that didn't answer | a stream that ends or doesn't open is failed service | LB05 step 1 |
| **400M** read, **150M** write | `IOReadBandwidthMax`, `IOWriteBandwidthMax` | the gate's disk bandwidth | `IOWeight=1`, the least; caps only where a person sets them | cgroup-v2.rst's io.weight and writeback; Little |
| 1 s deadline | | the guard's harm and heartbeat | a person's setting, `FLOW_US` by default | Nielsen 1993 |
| 1 false alarm a day | | the guard's thresholds | a person's setting, Headroom's default | the person |
| 2 s window | | the trigger's window | the shortest the kernel takes that holds the deadline: a whole number of 2 s, unprivileged; the deadline itself with `CAP_SYS_RESOURCE` (≤ 10 s) | psi.c v6.12 |
| halfway from calm to harm | | the CUSUM's reference | Page's reference for a shift from calm to harm | Page 1954; headroom.md |
| reference² / (CPUs × window) | | the machine's trigger | the least "some" a service's stall at the reference forces, under PSI's averaging | psi.c v6.12 `collect_percpu_times`, `test_states` |
| no threshold until an excursion is recorded | | the CUSUM's threshold | only harm stops until the record vouches for a level | headroom.md ("a short record can't vouch for rare events") |
| α 1/8, β 1/4, K 4, 1 s floor and start | | a server's silence | RFC 6298's estimator on each heartbeat's lateness | RFC 6298 §2.1–2.4 |
| 1 s connect timeout | | opening a server's stream | RFC 6298 §2.1's timeout before anything is measured | RFC 6298 |
| 20 ms idle | | when a disk was idle | two of Linux's coarsest ticks | Kconfig.hz HZ_100; iostats.rst |
| 2 s and a tick | | how soon an unprivileged trigger tells | the averages work's period | psi.c `PSI_FREQ` |
| 17.07 ms | | how often a server's line may come | its shards' probe period | docs/runtime-lab.md |
| 75 | | the guard's exit when it stopped or refused | sysexits' EX_TEMPFAIL | sysexits.h |

Headroom's (`crates/kiln-ci/src/headroom`, `crates/kiln/src/headroom`):

| Number | Value | What it controls | Now | From |
|---|---|---|---|---|
| a target's default tolerance | 1 s | harm, the probe window | `DEFAULT_TOLERANCE_MS`, a person's flow | Nielsen 1993; was Kubernetes' timeout |
| the probe share | 0.1 | how much of a target probes take | derived per target, under a person's cap (1 by default) | M/M/1 (Kleinrock 1975) and the detector's reference |
| the false-alarm budget | 1 a day | every threshold's calibration | a person's setting | the person |
| the server delay target's window | 1 s | how soon its trouble is caught | the default tolerance | as above |
| the server delay's tolerance | its probe period | harm for the server | its own clock | docs/priority.md |
| the CUSUM references | half the shift | detection | Page's half-shift | Page 1954; Lorden; Moustakides |
| the thresholds | calibrated | false alarms | (allowed + 1)-th largest harmless excursion, above the exponential tail | headroom.md; Siegmund |
| the probe interval | (window − lag)/(samples + 1) | sensing | unchanged; lag won back only where the target affords it | headroom.md |
| bisection's half, doubling | ½, 2 | the budgets' search | binary search for an unknown threshold, exponential search when unbounded | Bentley and Yao, "An almost optimal algorithm for unbounded searching", 1976 |
| two pacer steps | 2 × resolution | when the bound itself is tried | the pacer's resolution | the pacer |
| the pacer's step | whole percent (100) | a share's resolution | representation (a share is a whole percent) | the pacer |
| capacity | delivered during trouble | the write budget's cap | learned only with every disk written busy throughout | LB05 step 2 (above) |
| how far kiln must be cut before "no faster" says the trouble isn't kiln's | a quarter (4, chosen) | `Episode::cut_far` | below the midpoint of what the machine held calm before this trouble (`safe`: bisection's own next test of that range), or down to the least a cut can change | the knee's record, refit by every level held calm; bisection (Bentley and Yao 1976); the pacer's step |
| the least worth cutting for or to | 1 MB/s, 1 core (chosen) | a cut's floor | a pacer's step (a hundredth) of what the machine has: its cores (`least_cpu`, as `cpu_short` already counts them); the most its disks are known to take (`least_writes`: capacity learned under trouble, what they deliver now, or a page over its calm sync where the kernel doesn't say, as on macOS); else of what its work wants | the pacer's whole-percent resolution; each scale measured on the machine, refit as it's measured |
| the pacer's tick | 50 ms | how often the pacer looked | removed: the pacer wakes at each group's next switch, a due time of that group's alone (`Duty::phase`), or when told of new shares; a share is a rate being enforced, so its switches are due times computed from it | the shares themselves |
| `MIN_PERIOD` | 1 s | the shortest cycle, and the least span measured | as a cycle: a hundred of the actuator's measured lags (`cycle_us`: the least slice, a step, is a switch long, so a late switch costs at most a step, and switching costs at most 2 % of a group's time); as a span: a hundred ticks of the CPU counters (`measured_over`: each CPU's idle read to within a pacer's step), 1 s where they tick 100 times a second | RFC 6298's estimator on each switch's lateness; `sysconf(_SC_CLK_TCK)` (Linux, macOS); Windows' default clock interval, 15.625 ms (Microsoft, "Timers, Timer Resolution, and Development of Efficient Code", 2010) |
| `GUARD_WATCH` | 2 s | how often the pacer's guard looked for its pacer's end | removed: the guard waits on its pacer's end, the kernel's word (a pidfd, Linux 5.3; kqueue's `NOTE_EXIT`, macOS), as on its input's end; on Windows the pacer's end of that pipe is its alone (the standard library makes a parent's end uninheritable), so its input's end says it | `pidfd_open(2)`; kqueue(2) |
| the catch-up cap | 4 ticks | credit a group earned after a stall | removed: a group's place in its cycle is a function of time, so there's no credit to earn or catch up (a stall shows as a late switch, which the lag measures) | |
| a pacer's record left behind | looked for once a period | when a record whose pacer and guard both ended is released | each time a record appears or a pacer or guard one names ends (inotify and pidfds on Linux, kqueue on macOS); as a pacer starts, everywhere | inotify(7); `pidfd_open(2)`; kqueue(2) |
| the escapes walk | 3 levels | how deep the user manager's slices are read | the user manager's tree | systemd |
| parse bounds | 16 B, 4 KiB | an HTTP head, a delay answer | representation | |

Beyond this branch: CI placement's `target_use` (0.85) is derived on the rescued `lb/constants`
work from the knee of an M/M/m queue (Millsap 2010), to reuse when it lands; W24-R19's `disk_free`
(20 GiB) stays a person's setting whose default is theirs.

Outside Headroom:

| Number | Was | What it controls | Now | From |
|---|---|---|---|---|
| the server's overload tolerance | 1 s, Kubernetes' probe timeout, borrowed | how long a queue stands before the server sheds (`overload`), and a held period keeps a shard nudging (`nudge`) | a person's flow, `DEFAULT_TOLERANCE_MS`: what the server's queues hold up is a person waiting | Nielsen 1993; `objective::FLOW_US` |
| W24-R19's `io_full` default | 40 % (chosen) | PSI's `full avg10`, in percent, past which a heavy leg isn't started on a machine | 50 %: the knee of one queue (a disk's), the utilization past which response time grows faster than throughput; a person's setting, because how much pressure beside CI work is acceptable is theirs (`[ci.scheduler.admission] io_full`) | Millsap, "Thinking Clearly About Performance" (CACM 53(9), 2010): for M/M/1, R/ρ is least at ρ = 0.5; a machine's "full" share can't exceed its disk's busy share, so this says the disk is past the knee at least |
