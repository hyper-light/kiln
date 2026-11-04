# Priority: the server first

On a machine where kiln's server runs beside its own work, the server comes first. CI legs and
agents run at idle priority, each in a group of its own; headroom slows them when the server's own
delay rises; and the server can be given cores of its own. This is plan 01a1a59c2b29 (goal 8) and
the CPU-priority part of plan 01a19617's step 10. It joins `docs/baremetal.md` (the bare-metal
design, on its own branch until it lands) as that lands: its §5 writes the server's services, and
this is how they rank.

## Why

Production's server had no CPU limit and was never throttled, yet it lost to CI: it competed by
weight with the CI pods beside it, and its threads waited for a CPU 3-4% of the time at moderate
load (research 01a1a59c2b2a, f1). `nice` can't fix that: it ranks a thread only against threads in
its own cgroup (f3). Weights rank cgroups against their siblings and give way when a sibling is idle
(f4), and an idle cgroup (`cpu.idle`, SCHED_IDLE for a group) gets the CPU only when nothing else
wants it (f5). Google's Borg and Heracles and Meta's fbtax2 run the latency-critical service first
and batch work as filler, by weight rather than by caps (f13, f15, f17); caps throttle a service's
tail (f6).

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
(`kiln-jobs-<pid>/vm-<id>`) runs at idle priority twice over:

- **its processes run SCHED_IDLE,** set between fork and exec and inherited by everything they
  start (a step on a pty from the moment it's adopted). Any task that isn't idle preempts one as it
  wakes, whatever cgroup either is in, and a CPU running only idle tasks counts as free when the
  server's threads wake (`sched_idle_cpu`). This needs no controller and no delegation;
- **its cgroup has `cpu.idle` 1** (`cpu.weight` 1 on a kernel before 5.15, which has no
  `cpu.idle`), where the runner's unit is delegated to it: ranked against its siblings, it gets
  the CPU only when they don't want it, and headroom paces it whole.

The cgroup alone isn't enough: `cpu.idle` ranks a group only against its siblings, and the server's
threads are never a leg's siblings, so a leg's CPU still counted as free for the server's waking
threads, which then waited out the leg's slice there. In the lab that made the API slower than with
legs at normal priority ("The lab", below). A person's own reach run (`kiln run` from their
terminal) runs as they'd run it.

cgroup v2 lets a cgroup share a controller among its children only while it holds no process
itself, and systemd owns a unit's cgroups unless it delegates them. So:

- `kiln install` writes every unit with `Delegate=yes`, and with `DelegateSubgroup=kiln` where
  systemd has it (254 and later): systemd starts kiln's supervisor in the unit's leaf, `kiln`.
- The machine's installed runner (and a server's own runner) arranges its unit as it starts: where
  systemd says the unit is delegated to it, it moves the unit's processes into the leaf (on a
  systemd without `DelegateSubgroup=`), enables `+cpu` for the unit's children and for its folders
  of groups, and marks each group idle as it makes it. The folders stay where they were, under the
  unit, so sessions that outlive a runner process are found by the next.
- A runner whose unit isn't delegated (one installed before this), one in a container, or one a
  test or a person started still runs legs' and agents' processes SCHED_IDLE; their cgroups aren't
  marked idle, and it says why: the runner's facts (`executors.host.priority`) and its log at start.
  Reinstalling or upgrading the runner (`kiln install --agent --upgrade`) writes the delegated
  unit.

**Docker legs.** Docker's API has no `cpu.idle`: a leg's job container, its services, its Docker
actions, its egress relay and the containers its steps make through its Docker socket get the least
weight Docker gives a container, `--cpu-shares 2` (`cpu.weight` 1). A leg's own `options:` win. Their
processes are the engine's children, not kiln's, so they don't run SCHED_IDLE: a container leg ranks
below the server by weight alone, which is less (see the lab); host and microVM legs run fully idle.

**Firecracker legs.** Firecracker's jailer needs root, so kiln confines each VMM in a jail of its
own (`kiln vmm-jail`, docs/ci.md W13-R05). A leg's VMM, jailed or not, joins an idle cgroup the
runner made beforehand, the way the jailer places a VMM given a parent cgroup and no settings of
its own under cgroup v2 (f22), and runs SCHED_IDLE, its vCPUs' threads too. A microVM deployment's
VMM runs as the service it is.

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
binds), headroom adds a target of its own, `kiln server delay`: `GET /healthz/delay`, the server's
own measure of how late its shards run ready work, from the probes behind `kiln_shard_delay_seconds`
([runtime-lab.md](runtime-lab.md)): its worst shard's latest probe, or a hold under way past the
probes' period; or how long the probe took where that's longer. Its tolerance is the platform's (1 s) unless a person sets another, and
it's detected as every target is ([headroom.md](headroom.md)): a shift of its delay from calm toward
the tolerance cuts kiln's CPU and write budgets, and the pacer slows what runs. CI jobs' cgroups are
paced now, as agents' sessions are, so the legs and agents beside the server are what's slowed
first. A job a person paused is the pause's alone: the pacer neither stops it nor lets it go on.

## Cores of its own

`[server] cores` (a CPU list, `8-15`) gives the server cores of its own. Off by default:

- `kiln install` writes them into `kiln.slice` (`AllowedCPUs=`). Installed system-wide, the server's
  unit also makes the slice a cpuset partition as it starts (`cpuset.cpus.partition` `root`, as
  root): nothing outside it runs on those cores (f21). A user's install can confine the server
  there, not keep others off.
- The server fixes its shards to them, one each, keeping the first core for its threads that
  aren't shards, as hyper-rt's placement does (f11).

Cores held for the server are idle whenever it is, which weights never leave idle. That's why it's
off: the lab turns it on only if CPU hogs still hurt the server's tail with everything above in
place.

## Needs root, or a person

- A system install (root, once) for the top-level `kiln.slice`, the partition, and the server's
  memory protection the bare-metal design's §5 adds.
- Runners installed before this run legs and agents at normal priority until they're reinstalled
  or upgraded with a delegated unit. Upgrading a runner is a person's call: it ends the agents it
  runs.
- The user manager must give its units the `cpu` controller: systemd's own `user@.service`
  delegates `pids memory cpu`, and a runner's unit's `cgroup.controllers` says what it got.

## Tests

- `os::contain::tests::g8_s2_a_delegated_units_groups_run_at_idle_priority` (Linux): makes a scope
  of the user's own delegated to it, arranges it as a runner does (its process into the leaf, `+cpu`
  for its children), and reads `cpu.idle` 1 on an agent's group with what it starts inside it, not
  on a person's run's. `g8_s2_the_unit_whose_leaf_kiln_runs_in_is_its_home` reads the home.
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
- `api::tests::g8_s5_the_delay_is_the_worst_shards_now`: the server's answer, its worst shard's.
- `headroom::probes::tests::g8_s5_a_servers_delay_is_what_it_says_its_shards_wait` and
  kiln-ci's `headroom::tests::g8_s5_the_servers_delay_rising_slows_legs_and_agents`: the probe reads
  the delay a server says; the engine, the delay rising from calm, cuts kiln's CPU and paces a leg
  and an agent alike, then sees it back to calm.
- `rt::tests::g8_s6_shards_are_fixed_to_cores_of_the_servers_own` and
  `os::tests::g8_s6_a_cpu_list_reads_as_linux_writes_one`: cores of the server's own.

The lab (CPU hogs beside the server, the API's tail against calm) is goal 1's; its report goes here
once it has run.
