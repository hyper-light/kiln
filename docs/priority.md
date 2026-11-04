# Priority: the server first

On a machine where kiln's server runs beside its own work, the server comes first: it runs at the
most CPU weight, and CI legs and agents below it, each in a group of its own at the least weight
(Linux), under background policy (macOS) or below normal (Windows); headroom slows them when the
server's own delay rises; and the server can be given cores of its own. This is plan 01a1a59c2b29
(goal 8) and the CPU-priority part of plan 01a19617's step 10. It
joins `docs/baremetal.md` (the bare-metal design, on its own branch until it lands) as that lands:
its §5 writes the server's services, and this is how they rank.

## Why

Production's server had no CPU limit and was never throttled, yet it lost to CI: it competed by
weight with the CI pods beside it, and its threads waited for a CPU 3-4% of the time at moderate
load (research 01a1a59c2b2a, f1). `nice` can't fix that: it ranks a thread only against threads in
its own cgroup (f3). Weights rank cgroups against their siblings and give way when a sibling is idle
(f4), and an idle cgroup (`cpu.idle`, SCHED_IDLE for a group) gets the CPU only when nothing else
wants it (f5). Google's Borg and Heracles and Meta's fbtax2 run the latency-critical service first
and batch work as filler, by weight rather than by caps (f13, f15, f17); caps throttle a service's
tail (f6). On Linux the lab chose weights over idle cgroups for legs and agents ("The lab", below).

## Linux

**The server.** `kiln install` writes the server's unit into `kiln.slice`, a slice of its own with
`CPUWeight=1000` and `IOWeight=1000`, and the unit takes the same weights. A system install's slice
is `/etc/systemd/system/kiln.slice`, a top-level slice beside `user.slice` and `system.slice`, where
CI and agents run: under full contention it gets about 1000 of every 1,200 parts of the CPU against
two slices at the kernel's default of 100 (f12). Weights rank a cgroup only against its siblings, so
they go on the slice; on the service, its one child, they'd rank nothing. The name has no dash:
systemd reads `a-b.slice` as a child of `a.slice`. A user's install writes the user manager's
`kiln.slice`, which ranks the server against the user's other units (`app.slice`, where their
runner runs), not against `system.slice`: only root can place a slice at the top.

**Legs and agents.** Each agent session (`kiln-agents/<session>`), each reach run an agent asked for
(`kiln-agents/run-<id>`), each CI job (`kiln-jobs-<pid>/<job>`) and each leg's microVM
(`kiln-jobs-<pid>/vm-<id>`) runs in a cgroup of its own with the least CPU weight, `cpu.weight` 1:
below kiln's own processes and a person's runs in the runner's unit, and paced whole by headroom. A
person's own reach run (`kiln run` from their terminal) runs as they'd run it. The server comes
first where it meets them: `kiln.slice`'s weight against the slice the runner's unit is in.

Not idle, which was this goal's first design. `cpu.idle` puts a group below its siblings alone, while
the scheduler takes a CPU that runs nothing but idle work as free for any thread that wakes, the
server's too (`sched_idle_cpu`). The server's threads went to the legs' CPUs, and whether a waking
thread preempts the one running there is decided where the two meet in the cgroup tree
(`find_matching_se`), where the legs' unit isn't idle: so they waited there for the leg. In the lab,
legs in idle cgroups made the API slower than the same legs left at their unit's weight, and running
their processes SCHED_IDLE as well didn't help (Linux 6.19's `kernel/sched/fair.c`, research f30 to
f37; "The lab").

cgroup v2 lets a cgroup share a controller among its children only while it holds no process
itself, and systemd owns a unit's cgroups unless it delegates them. So:

- `kiln install` writes every unit with `Delegate=yes`, and with `DelegateSubgroup=kiln` where
  systemd has it (254 and later): systemd starts kiln's supervisor in the unit's leaf, `kiln`.
- The machine's installed runner (and a server's own runner) arranges its unit as it starts: where
  systemd says the unit is delegated to it, it moves the unit's processes into the leaf (on a
  systemd without `DelegateSubgroup=`), enables `+cpu` for the unit's children and for its folders
  of groups, and gives each group the least weight as it makes it. The folders stay where they
  were, under the unit, so sessions that outlive a runner process are found by the next.
- A runner whose unit isn't delegated (one installed before this), one in a container, or one a
  test or a person started runs legs and agents at the default weight, and says why: the runner's
  facts (`executors.host.priority`) and its log at start. Reinstalling or upgrading the runner
  (`kiln install --agent --upgrade`) writes the delegated unit.

**Docker legs.** A leg's job container, its services, its Docker actions, its egress relay and the
containers its steps make through its Docker socket get the least weight Docker gives a container,
`--cpu-shares 2` (`cpu.weight` 1), as a host leg's group has. A leg's own `options:` win.

**Firecracker legs.** Firecracker's jailer needs root, so kiln confines each VMM in a jail of its
own (`kiln vmm-jail`, docs/ci.md W13-R05). A leg's VMM, jailed or not, joins a cgroup the runner
made beforehand at a leg's weight, the way the jailer places a VMM given a parent cgroup and no
settings of its own under cgroup v2 (f22). A microVM deployment's VMM runs as the service it is.

## macOS

The server's launchd job (a LaunchDaemon installed system-wide, else a LaunchAgent) runs with
`ProcessType` `Interactive`: no limits for its purpose, as an app has none. Left unset, launchd
throttles a job's CPU and I/O (f25). Each agent's harness and each CI step starts under background
policy, `setpriority(PRIO_DARWIN_PROCESS, 0, PRIO_DARWIN_BG)` between fork and exec, as `taskpolicy
-b` runs a program: lower CPU priority and throttled disk and network I/O, inherited by everything
it starts (f26). A step on a pty is put there as it starts.

## Windows

The server's process runs `ABOVE_NORMAL_PRIORITY_CLASS`; it sets that itself as it starts, whatever
started it. An agent's job object and each CI step's run every process in them, what they start
included, at `BELOW_NORMAL_PRIORITY_CLASS` (the job's `JOB_OBJECT_LIMIT_PRIORITY_CLASS`), at the least
share of weight-based CPU rate control, 1 of 9 (f23). Background mode,
`PROCESS_MODE_BACKGROUND_BEGIN`, lowers a process's I/O and memory priority too, which a CPU
priority alone doesn't (f24), but only a process can put itself in it. So kiln gives the process it
puts in each job what background mode would: very low memory priority and very low I/O priority,
before it runs anything. A step on a ConPTY, which starts outside any job, runs below normal with
the same.

## headroom listens to the server

Where the server runs on the runner's own machine (its address is loopback or one this machine
binds), headroom adds a target of its own, `kiln server delay`, read from `GET /healthz/delay`: the
server's own figures from the probes behind `kiln_shard_delay_seconds`
([runtime-lab.md](runtime-lab.md)), as JSON. `delay_s` is its worst shard's now, for a person;
`late_s` and `probes` are every probe's lateness added up and how many ran; `held_s` is a hold under
way past the probes' period; `period_s` is that period. A server without them (older), or with its
probes off, is no reading, never a breach.

- **What headroom reads:** how late the server's ready work ran on average since headroom last
  looked, every probe of every shard counted (the change in `late_s` over the change in
  `probes`). Not one probe of one shard at an instant: in the lab that missed nearly everything
  between two reads. A shard held by one long request (opening a long conversation) counts once
  its probe runs, its lateness in the total; the hold itself is the answer only when no probe ran
  at all since the last look. In the lab such holds reached 10 to 38 ms calm, the server's own
  work, which slowing kiln's can't shorten.
- **Its tolerance:** the shards' own clock, their probes' period (10 ms, `KILN_SHARD_PROBE_MS`):
  ready work a whole period late on average is a server behind its own work. A server probed at
  another period is measured against its own.
- **Its window:** trouble is caught within the platform's time (1 s), as every other target's is, so
  its probes keep that pace, and a probe still out is harm only once it's out that long (what it
  answers is what the server measured, not how long asking took).

It's detected as every target is ([headroom.md](headroom.md)): a shift of the delay from calm toward
the tolerance cuts kiln's CPU and write budgets, and the pacer slows what runs. CI jobs' cgroups are
paced, as agents' sessions are, so the legs and agents beside the server are what's slowed first. A
job a person paused is the pause's alone: the pacer neither stops it nor lets it go on.

## Cores of its own

`[server] cores` (a CPU list, `8-15`) gives the server cores of its own. Off by default:

- `kiln install` writes them into `kiln.slice` (`AllowedCPUs=`). Installed system-wide, the server's
  unit also makes the slice a cpuset partition as it starts (`cpuset.cpus.partition` `root`, as
  root): nothing outside it runs on those cores (f21). A user's install can confine the server
  there, not keep others off.
- The server fixes its shards to them, one each, keeping the first core for its threads that
  aren't shards, as hyper-rt's placement does (f11).

In the lab, cores of its own held every page and API number within noise of the server alone, which
weights didn't ("The lab", below). It stays off all the same: cores held for the server sit idle
whenever it does, which weights never leave idle, and how many it needs is its machine's and its
load's to say, not a constant's. A person turns it on where the server's medians must hold too.

## The lab

`kiln-lab` ([runtime-lab.md](runtime-lab.md)), its `realistic` profile, on fedora: an Intel Core
i9-14900HX (32 threads), Fedora's Linux 6.19.10 with lazy preemption at 1000 Hz, systemd 259. The
server measured is the build main's baseline measured (main at `2a2d93f6df5` with goal 1's metrics,
in release), the lab and everything it starts held to the 16 efficient cores (`taskset -c 16-31`) as
there. Beside it, legs: as many busy loops as the lab's CPUs (16), on the same CPUs, in a scope
delegated to them and arranged as kiln's runner arranges its unit (`scripts/priority_lab/legs.sh`):
kiln's own process in the leaf, `+cpu` for the unit's children and the jobs' folder, the loops in one
job's cgroup. The lab's unit stands in for `kiln.slice`, beside the legs' scope where the slice meets
the one a runner runs in. Configurations took turns round by round
(`scripts/priority_lab/driver.sh`); each cell is the median of the rounds and its noise (95%), as
`kiln-lab table` gives them, and a difference is beyond noise only as `kiln-lab compare` calls it,
over all 43 numbers at once. Other agents' builds and labs loaded the machine throughout (a load of
25 to 50 before each window), so calm is noisier here than in main's baseline; the turns spread
that over every configuration alike. The rounds, as `kiln-lab` wrote them:
`docs/priority/2026-11-04/`; the findings, research 01a1a59c2b2a f30 to f43.

Block 4, four rounds each, the server's unit at `CPUWeight=1000` but for even weights:

- **even weights:** the server and the legs both at the default weight 100: bare metal before this;
- **kiln.slice's weight:** the legs at their unit's weight: what lands here, where a leg's least
  weight inside its unit ranks it against kiln's own processes there alone (idle in the lab);
- **and idle groups:** each leg's job cgroup at `cpu.idle` 1 inside its unit, this goal's first
  design;
- **and an idle unit:** the legs' whole unit at `cpu.idle` 1 (`CPUWeight=idle`), idle where it
  meets the server.

| number | calm | even weights | kiln.slice's weight | and idle groups | and an idle unit |
| --- | --- | --- | --- | --- | --- |
| page p50 ms | 0.139 (0.130 to 0.150) | 0.864 (0.145 to 0.917) | 0.151 (0.143 to 0.157) | 0.931 (0.835 to 1.20) | 0.917 (0.729 to 1.02) |
| page p99 ms | 30.4 (0.271 to 68.0) | 79.3 (1.97 to 109) | 1.98 (1.92 to 2.26) | 2.58 (2.08 to 43.2) | 2.39 (2.04 to 55.9) |
| page p99.9 ms | 61.7 (0.524 to 126) | 211 (3.51 to 263) | 4.15 (2.95 to 26.9) | 5.93 (2.95 to 70.8) | 4.11 (3.01 to 117) |
| api_read p50 ms | 0.366 (0.337 to 0.403) | 1.55 (1.20 to 2.04) | 1.49 (1.27 to 1.70) | 2.36 (2.02 to 3.67) | 2.07 (2.07 to 3.17) |
| api_read p99 ms | 126 (109 to 162) | 162 (137 to 195) | 143 (112 to 171) | 186 (177 to 354) | 173 (115 to 252) |
| api_read p99.9 ms | 1575 (681 to 2785) | 1147 (609 to 1564) | 910 (573 to 1598) | 855 (686 to 1102) | 745 (596 to 960) |
| api_write p50 ms | 0.853 (0.827 to 0.885) | 1.79 (1.24 to 1.88) | 1.42 (1.19 to 1.61) | 1.93 (1.75 to 1.93) | 1.93 (1.92 to 1.93) |
| api_write p99 ms | 1042 (459 to 2880) | 725 (542 to 1071) | 525 (374 to 574) | 671 (609 to 902) | 670 (546 to 803) |
| websocket p50 ms | 0.638 (0.622 to 0.732) | 3.25 (2.61 to 3.51) | 2.54 (2.40 to 2.69) | 3.48 (3.25 to 4.53) | 3.39 (3.24 to 4.05) |
| websocket p99 ms | 904 (823 to 984) | 1229 (1114 to 1492) | 1025 (861 to 1250) | 1094 (1051 to 1283) | 1064 (745 to 1092) |
| websocket p99.9 ms | 1143 (1056 to 1251) | 1756 (1594 to 2431) | 1363 (1291 to 1522) | 1371 (1242 to 1588) | 1291 (1024 to 1318) |
| attach p50 ms | 1063 (1003 to 1165) | 1331 (1283 to 1550) | 1292 (1267 to 1320) | 1241 (1217 to 1311) | 1225 (1150 to 1356) |
| server: api_read took p99 ms | 111 (97.8 to 168) | 158 (122 to 180) | 116 (96.8 to 148) | 184 (131 to 286) | 182 (109 to 234) |
| server: websocket waited p99 ms | 885 (778 to 953) | 1755 (1162 to 1892) | 1156 (891 to 1763) | 1355 (1165 to 1899) | 1305 (733 to 1361) |
| shards' delay p99 ms | 0.955 (0.864 to 1.51) | 7.41 (4.99 to 9.19) | 3.41 (3.18 to 3.95) | 3.38 (2.37 to 4.96) | 3.26 (2.34 to 4.66) |
| shards' delay p99.9 ms | 23.5 (19.1 to 32.5) | 60.4 (44.6 to 80.5) | 45.0 (42.6 to 48.1) | 44.0 (19.9 to 57.9) | 39.4 (30.1 to 45.9) |

Before it, the same lab with fewer configurations: block 1 (four rounds each: calm, even weights,
idle groups), block 2 (six: calm, then idle groups whose loops ran SCHED_IDLE too, with the server
at 1000 and at 100) and block 3 (five: the lab alone on 8 of the efficient cores, and the same with 8
legs at normal priority on the other 8, as cores of its own would hold them).

What it says:

- **kiln.slice's weight is what puts the server first.** Against even weights it was better beyond
  noise in 8 numbers (the shards' delay at p99 from 7.41 ms to 3.41, the live feed's p99.9 from 1.76
  s to 1.36) and worse in none.
- **Idle groups made the server slower.** Against kiln.slice's weight alone they were worse in 9
  numbers and better in none: pages took 0.931 ms at the median against 0.151, API reads 186 ms at
  p99 against 143, writes 671 against 525. Block 1 said the same against even weights (pages 0.956
  ms at the median against 0.176), and block 2 that running the loops SCHED_IDLE as well didn't
  help (pages 1.15 ms at the median against 0.133 calm, API reads 316 ms at p99 against 73.5).
  Linux 6.19 takes a CPU that runs only idle work as free when a thread wakes, the server's too,
  and decides whether that thread preempts the running one where their cgroups meet, where the
  legs' unit isn't idle; under lazy preemption even a preemption waits for the tick (research
  01a1a59c2b2a, f30 to f37).
- **An idle unit didn't do better than weights either.** Idle where it meets the server, it held the
  tails as kiln.slice's weight did, but left pages, API reads and writes and the live feed slower at
  the median (5 numbers worse, 1 better): its CPUs still drew the server's threads, which under lazy
  preemption waited for the tick to take them. It would need the runner's unit in a slice of its
  own beside `kiln.slice`, so a runner and a server installed alike (both system-wide or both a
  user's); it isn't built.
- **Cores of its own held the API.** On cores the legs never ran on, every page and API number stayed
  within noise of the lab alone; only the live feed's p99.9 (1.19 s to 1.38) and attaches' tail (1.32
  s to 1.51) were worse.

Step 7 asked for the API's p99 with CPU hogs held within noise of calm, legs and agents at idle
priority. Idle, it wasn't: against calm, API reads' p99 (126 ms to 186) and the live feed's (0.90 s
to 1.09) were worse beyond noise. With what lands, kiln.slice's weight and legs at the least weight,
the p99s of pages, API reads and writes and the live feed were within noise of calm; its medians
weren't (API reads 0.366 ms to 1.49, the live feed 0.638 to 2.54), nor the shards' delay (0.955 ms
to 3.41 at p99), and calm's own page tail was wide on this busy machine: in quieter blocks pages
took 0.33 to 0.46 ms at p99 calm, against 2 ms with legs. Cores of its own held pages and the API
whole.

## Needs root, or a person

- A system install (root, once) for the top-level `kiln.slice`, the partition, and the server's
  memory protection the bare-metal design's §5 adds.
- Runners installed before this run legs and agents at the default weight until they're reinstalled
  or upgraded with a delegated unit. Upgrading a runner is a person's call: it ends the agents it
  runs.
- The user manager must give its units the `cpu` controller: systemd's own `user@.service`
  delegates `pids memory cpu`, and a runner's unit's `cgroup.controllers` says what it got.

## Tests

- `os::contain::tests::g8_s2_a_delegated_units_groups_run_at_the_least_weight` (Linux): makes a
  scope of the user's own delegated to it, arranges it as a runner does (its process into the leaf,
  `+cpu` for its children), and reads `cpu.weight` 1 on an agent's group with what it starts inside
  it, never `cpu.idle`, and the default on a person's run's.
  `g8_s2_the_unit_whose_leaf_kiln_runs_in_is_its_home` reads the home.
- `install::tests::g8_units_are_delegated_and_the_server_comes_first` and
  `g8_systemd_reads_the_units_kiln_writes`: the runner's and the server's units, `kiln.slice` with
  its weights and cores, `ProcessType` `Interactive` for the server's launchd job, and
  `systemd-analyze verify` finding nothing against them.
- `os::contain::tests::g8_s3_an_agents_processes_run_under_background_policy` (macOS): an agent's
  harness and what it starts under background policy, their scheduling priority (`ps`) below a
  plain process's; a person's run not; a running process put there.
- `os::contain::tests::g8_s4_an_agents_job_runs_below_normal_at_the_least_weight` and
  `g8_s4_the_server_runs_above_normal` (Windows): an agent's job's priority class and CPU rate
  control read back, cmd and its child below normal, the first process's memory and I/O priority;
  a person's job without a priority class; the server above normal.
- `api::tests::g8_s5_the_delay_is_the_worst_shards_now`: the server's answer, its worst shard's
  now, its probes' lateness added up and counted, the hold and the period.
- `headroom::probes::tests::g8_s5_a_servers_delay_is_how_late_its_work_ran_since_the_last_read`: the
  probe answers with the mean since its last read, or the hold, and gives no answer from a first
  read or a server without the figures.
- kiln-ci's `headroom::tests::g8_s5_the_servers_delay_rising_slows_legs_and_agents`, with the lab's
  numbers: calm near 0.43 ms with a stall every two minutes, then legs and agents raising it to
  1.9 to 4.5 ms: the engine alarms within seconds, cuts kiln's CPU, paces a leg and an agent alike,
  sees it back to calm, and probes at the window's pace, not the tolerance's.
  `g8_s5_a_windowed_targets_probe_out_past_its_tolerance_is_no_harm_until_its_window`: a rise the
  weights held (1.4 to 1.8 times calm) is no trouble, and a probe out is harm only at the window.
- kiln-lab's `standin::tests::g8_s5_a_hog_turn_burns_for_its_time_and_no_longer`: the stand-in's CPU
  hog turn, the lab's agents.
- `rt::tests::g8_s6_shards_are_fixed_to_cores_of_the_servers_own` and
  `os::tests::g8_s6_a_cpu_list_reads_as_linux_writes_one`: cores of the server's own.
