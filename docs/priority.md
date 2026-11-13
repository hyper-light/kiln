# Priority: the server first

On a machine where kiln's server runs beside its own work, the server comes first: it runs at the
most CPU weight, and CI legs and agents below it, each in a group of its own at the least weight
(Linux), below normal (macOS: nice `PRIO_MAX`, the lowest priority there is, on every core; Windows); headroom slows them when the
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

**Memory.** A delegated unit that declares a floor for kiln's own processes (`MemoryMin=`,
`MemoryLow=`: production's server's unit) has it kept for them alone. As it arranges its unit, the
server (or runner) enables `+memory` for the unit's children, writes the unit's `memory.min` and
`memory.low` on its leaf, and gives `kiln-agents` a share of its own: what the unit may have (the
least of its `MemoryMax=`, a slice's above it and the machine's memory) less that floor as its
`memory.max`, and, where the unit or a slice above it is slowed (`MemoryHigh=`), as far short of
that as its `memory.high`. Every number is the unit's, a slice's or the machine's. What agents run
(builds of 8-10 GB each) is then slowed, reclaimed and killed inside their folder: it never takes
kiln's floor, slows the unit (its server's threads with it) or fills it, where an OOM kill could take
the server. The agents' sessions are counted together, in their folder. A unit that declares no
floor has nothing to keep and is left as it was; one that leaves the agents nothing past the floor
keeps the floor and says why the agents have no share. Both are said in the log at start (`kiln's own
processes keep their unit's memory floor`, `agents share their unit's memory`) and in how agents'
processes are held together. It's worked out as kiln starts: a unit's memory changed while it runs
(`systemctl set-property`) is taken up at its next start. Before this, production's agents ran in
the server's own cgroup, which hit its `memory.high` 15.9 million times.

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
throttles a job's CPU and I/O (f25). Each agent's harness and each CI step starts below normal, at
nice 20, `PRIO_MAX` in `<sys/resource.h>`, the lowest priority the scale has
(`setpriority(PRIO_PROCESS, 0, PRIO_MAX)` between fork and exec), inherited by everything it starts:
below everything the runner starts plainly, a person's run included, whatever the runner's own nice.
Raising a nice value needs no privilege, so this holds under a runner started under `nice` too. The
value is the scale's own bound, not a step of kiln's choosing: a runner already at `PRIO_MAX` leaves
its agents level with what it starts plainly, as nothing runs lower. A step on a pty is put there as
it starts.

Not under Darwin background policy (`PRIO_DARWIN_BG`, `taskpolicy -b`, f26), as they did until
2026-11-05: Apple silicon runs background work on its efficiency cores alone and throttles its disk
and network I/O, so an agent's builds and measurements had a fraction of the machine. Agents moved
their work where that didn't reach: a Windows VM and its measurements as launchd jobs of their own,
builds in Docker's VM. Neither was anything kiln knew of the agent when that Mac's memory ran out
and it panicked (no response from userspace for 93 s). Below normal, agents' work has every core the
server and a person's work aren't using; headroom slows it when the server's delay, a message's
delivery or memory says so.

What an agent starts through launchd (`launchctl bootstrap`, `load`, `submit`, `start` or
`kickstart` run by one of its processes) is the agent's too: the runner reads each `launchctl` an
agent's process runs and the label of each property list it names, follows the job's process as the
agent's whenever launchd runs one (headroom paces it with the agent's, at the agent's priority), and
boots the job out when the agent ends.

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
- **Its tolerance:** the shards' own clock, their probes' period (17.07 ms; 10 ms before goal 9 step 5
  derived it from hyper-rt's watchdog's limit, docs/runtime-lab.md):
  ready work a whole period late on average is a server behind its own work. A server probed at
  another period (another build's) is measured against its own.
- **Its window:** trouble is caught within the platform's time (1 s), as every other target's is, so
  its probes keep that pace, and a probe still out is harm only once it's out that long (what it
  answers is what the server measured, not how long asking took).

It comes first ([headroom.md](headroom.md), "Detection"): besides a shift toward the tolerance, a
rise past calm's noise is trouble, read by a second CUSUM whose reference is half calm's standard
deviation above calm's mean. Trouble cuts kiln's CPU (whatever the disks deliver: this delay is the
CPU's) and write budgets, and the pacer slows what runs. CI jobs' cgroups are paced, as agents'
sessions are, so the legs and agents beside the server are what's slowed first. A job a person
paused is the pause's alone: the pacer neither stops it nor lets it go on.

- **The cut holds** until the delay is back within calm's noise: the mirror of that CUSUM (what
  samples fell short of its reference, added up) passing the same threshold, counted from samples
  measured after the cut could show. At calm that takes as many samples as a rise past calm's
  noise takes to alarm (on a calm like fedora's, about 45 reads, 22 s). A cut that leaves the delay rising
  past calm's noise to the threshold again, once it could show, is cut again.
- **Calm stays calm:** what's measured while any share under 1 is in force, or under an alarm that
  cut kiln's work, neither joins calm nor calibrates the thresholds; an excursion learned as it
  came is taken back out once it turns out not to be calm.
- **Raised only as it holds:** a level counts as held only once it stayed calm as long as a rise
  past calm's noise takes to be seen, plus the longest a cut took to show; and the level is what
  kiln's work used, never more than its budget let it (CPU paced groups give up, unpaced ones take,
  so what kiln uses barely moves while the pacer holds it to less).

## Cores of its own

`[server] cores` (a CPU list, `8-15`) gives the server cores of its own. Off by default:

- `kiln install` writes them into `kiln.slice` (`AllowedCPUs=`). Installed system-wide, the server's
  unit also makes the slice a cpuset partition as it starts (`cpuset.cpus.partition` `root`, as
  root): nothing outside it runs on those cores (f21). A system install without them makes the slice
  a member again as the server starts, and `kiln uninstall` does too: a partition outlives the unit
  that made it, its cores held from everything else until then. A user's install can confine the
  server there (where root gave their service manager the cpuset controller), not keep others off
  ("What kiln install grants", below). The server says whether its cores are a partition's as it
  starts (its log) and in `/healthz/server` (`priority.cores`: its cores, its shards', and the
  partition with its state as the kernel says it, `/kiln.slice: root`, or why it's invalid): the
  unit's write can fail without failing the unit.
- The server fixes its shards to them, one each, keeping the first core for its threads that
  aren't shards, as hyper-rt's placement does (f11), and keeps its other threads off the shards'
  cores (goal 9, plan `01a1a87b2cec` step 4): a thread or process starts with the CPUs of the thread
  that started it, so the threads it starts after fixing them start on the rest, a thread a task on
  a shard starts is moved off within the shards' probe period (`crate::rt::keep_apart`), and a child
  a shard starts is moved as it starts. Linux alone: macOS and Windows have no cores of the server's
  own (`[server] cores` is refused there), so nothing changes on them.
- kiln's own legs and agents keep off them (step 7; "Legs and agents keep off the server's cores",
  below): those the server's own runner starts, and those of every runner whose config declares them.
  Where the server's slice holds its own runner on its cores (a partition), they run on those cores
  but off its shards'.

In the lab, cores of its own held every page and API number within noise of the server alone, which
weights didn't ("The lab", below). It stays off all the same: cores held for the server sit idle
whenever it does, which weights never leave idle, and how many it needs is its machine's and its
load's to say, not a constant's. A person turns it on where the server's medians must hold too.

## The kernel's preemption, and the server's tail

How soon a thread that wakes gets a CPU another thread holds is the kernel's preemption mode. A kernel
built with `CONFIG_PREEMPT_DYNAMIC` picks one at boot (`preempt=` on its command line; root reads the
mode in force in `/sys/kernel/debug/sched/preempt`):

| mode | a normal thread waking where another is busy in user space | where the other runs kernel code | a real-time thread waking |
| --- | --- | --- | --- |
| `none` | the CPU is interrupted at once and the waker runs as the other returns to user space: microseconds | not until that code blocks or returns | at once |
| `voluntary` | as `none` | at the kernel's preemption points | at once |
| `full` | as `none` | at once, outside sections that hold preemption off | at once |
| `lazy` | no interrupt: a lazy reschedule, taken as the other next returns from the kernel (a system call, an interrupt) or at the CPU's next tick, 1 ms at 1000 Hz | as for user space | at once: a real-time wake's preemption is never lazy |
| `rt` (`PREEMPT_RT`) | as `full`, interrupts handled in threads (fair threads may be preempted lazily where the kernel has lazy preemption) | at once | at once |

So under `lazy` a shard of the normal class that wakes where something is busy in user space waits up to a
tick, and its weight doesn't shorten the wait: the waker is marked, not run (research `01a1a87b2cee` f17;
`01a1a59c2b2a` f36). What the lab's machine measured, a thread woken as a shard is, with hogs at `cpu.weight` 1
beside it at `CPUWeight=1000` (`scripts/priority_lab/wakelab.sh`, fedora's Linux 6.19 with lazy preemption at
1000 Hz):

- **wake-look1:** woken by a socket on its own CPU beside spinning hogs, p99.9 812 µs, at most 1.05 ms; beside
  hogs that enter the kernel every 20 µs, p99.9 31.6 µs; with no hogs of the lab's, p99.9 400 µs (its
  neighbours'). Asking EEVDF for a 250 µs slice made the spinning hogs' wakes worse (p99 617 µs against 19).
- **wake-2** (2026-11-05, 06:40 to 07:04 UTC, three rounds of 10 s a cell, a load of 37 to 89 from other
  sessions): woken from another CPU onto a busy one, **p99 1.00 ms with every kind of hog, and with none of the
  lab's** (spinning 1001 µs, compilers 1009, entering the kernel 951, none 1003): other sessions' work kept the
  CPUs busy, and a lazy remote wake waits for the busy CPU's tick. On its own CPU a socket's wake waited a tick
  only behind hogs that never enter the kernel (spinning: p99.9 895 µs; pinned one to a CPU and entering the
  kernel every 20 µs: 15 µs). A 250 µs slice didn't move it (1000.4 µs at p99 against 1001.3).

Production (adacachy) runs `full`: a remote wake interrupts the busy CPU at once, so its tail beside legs is the
scheduler's choice of who runs, not the tick. fedora's tail with hogs is its kernel's, not production's, and
without root there (`preempt=full` at boot, or a cpuset partition) the lab can't show production's.

What it means for the server:

- **Weights alone can't hold a shard under a tick on a lazy kernel** beside anything busy in user space on its
  CPU (block 5 above: pages' p99 0.30 ms calm, 2.34 ms beside legs).
- **Cores of its own help as far as nothing else runs on them.** kiln keeps its own legs and agents off them
  (below), but on a lazy kernel anything else there (another user's work, other services) still holds a waking
  shard for a tick, as wake-2's neighbours did. **The installer's cpuset partition is what makes the server's
  cores its own on a lazy kernel:** a system install (root, once) makes `kiln.slice` a partition, and nothing
  outside it runs on its cores.
- **The real-time class works whatever else runs** ("A real-time class for the shards", below): a real-time
  wake preempts at once in every mode, lazy included.
- `preempt=full` at boot gives every normal wake its interrupt at once: the machine owner's call.

## Legs and agents keep off the server's cores

With `[server] cores`, kiln keeps its own legs and agents off the server's cores on Linux (goal 8, plan
`01a1a59c2b29` step 7):

- **Who.** The server's own runner (`[local_agent]`), and every runner whose config file declares
  `[server] cores` (a server and a runner one person installed share the machine's config, `KILN_CONFIG`). Each
  works out its work CPUs once as it starts, the CPUs it may run on less the server's
  (`crate::os::keep_work_off`), and says what it did and why: in its log, its facts
  (`executors.host.priority`), and for the server's own runner `/healthz/server` (`priority.work`).
- **How.** By CPU affinity, from each process's first instruction, which everything it starts inherits:
  - an agent's harness and the runs an agent asks for take the work CPUs between fork and exec
    (`crate::os::onto_work_cpus`): what of the CPUs it would start on are work CPUs, or the work CPUs whole
    where it starts from a thread fixed to a shard's core;
  - a CI leg's steps take them through the runner's ledger of CPUs (`crate::runner::cpus`, docs/ci.md
    W05-R03), which lends and shares only work CPUs and starts every step on them or on its leg's share of
    them; a step on a pty is moved onto them as it starts; a leg's job container and services get its CPUs as
    `--cpuset-cpus`, and its egress relay, its Docker actions and the containers its steps make through its
    Docker socket the work CPUs (a container that names CPUs of its own keeps them); a leg's microVM's VMM,
    its vCPUs with it, takes them as a harness does.
  - Affinity rather than a cpuset on kiln's folders of groups: a user's service manager doesn't delegate the
    `cpuset` controller (systemd's `user@.service` delegates `pids memory cpu`), and where a system install
    could, its partition already keeps everything outside `kiln.slice` off the server's cores. A process can
    widen its own affinity again (`taskset`); only a cpuset holds it there.
- **What it can't do.**
  - It keeps kiln's own work off, nothing else's: on a lazy kernel anything else on the server's cores still
    holds a waking shard for a tick (wake-2, above). The partition makes them the server's alone.
  - The server's own runner shares the server's cores wherever its slice holds the server there
    (`AllowedCPUs=` applied: a system install, or a user's manager given the `cpuset` controller), since
    everything the server starts is in its slice. Its legs and agents then run on those of them its shards
    aren't fixed to (the first, which its other threads keep, and any past the shards'; goal 9, plan
    `01a1a87b2cec` step 4), and only share a shard's where every core is a shard's. It says so; a runner of
    its own beside the server keeps them off all of them (`[local_agent] enabled = false`, and
    `kiln install --agent`).
- **macOS and Windows:** nothing changes, as a server has no cores of its own there. macOS has no CPU affinity
  for a process, and its legs and agents run below normal, so its scheduler runs the server first on any
  core; on Windows they run below normal, and its scheduler preempts them at once for the server.

## A real-time class for the shards

`[server] realtime = true` (off until declared, and on bare metal `kiln install` declares it wherever its unit
grants it: "The bare-metal default", below; `KILN_RT_REALTIME` overrides it) puts each of the server's
shards in Linux's round-robin real-time class, `SCHED_RR`, at the lowest real-time priority, 1 (goal 8, plan
`01a1a59c2b29` step 7; goal 9, plan `01a1a87b2cec` step 4; `hyper_rt::compute::realtime_current_thread`):

- A real-time thread waking where a normal one runs preempts it at once in every preemption mode, lazy
  included: whatever runs on the shard's CPU, kiln's or anyone's.
- Each shard asks as it starts, with the reset-on-fork flag, so a thread or process a shard starts runs at the
  normal policy (sched(7)); the server's other threads, its pools and helpers, stay as they were. Priority 1 is
  above every normal thread and below every other real-time one, the kernel's threaded interrupt handlers
  (50) among them.
- **What the installer's unit grants.** An unprivileged thread may take `SCHED_RR` up to its `RLIMIT_RTPRIO`
  (sched(7)). With `[server] realtime`, `kiln install` gives the server's unit `LimitRTPRIO=1`, that priority
  and no more. A system install's server runs as root and needs no grant. A user's service manager gives what
  its own limit allows (systemd sets the nearest it can, never failing the unit), which is 0 unless the
  machine's owner raised it for the user (a `limits.d` entry, `rtprio 1`). Not `CPUSchedulingPolicy=`: it would
  put every thread of the server, and everything it starts, in the class. On 2026-11-05 fedora (the lab) gave its
  user 0, in a shell, in a user scope and to the user's service manager; adacachy gives its user 99, soft and hard
  (its `limits.d` entries for the `audio` group), so there a user's install needs nothing the installer can't give.
  **Production's server isn't a user's install:** it runs from a hand-written system unit with `User=adalundhe`
  (the bare-metal design's phase 0), and its process's `RLIMIT_RTPRIO` was 0, soft and hard, on 2026-11-05
  (`/proc/<pid>/limits`): `limits.d` applies to PAM sessions, a user's service manager among them, never to a
  system unit, which gets the room only from its own `LimitRTPRIO=` (the system's manager grants it whatever
  `limits.d` says). Its unit needs `LimitRTPRIO=1` and its config `[server] realtime = true`: a deploy, Ada's call.
- **The room is the shards' alone.** Once every shard has answered, the server sets its own `RLIMIT_RTPRIO` to 0,
  soft and hard (`crate::os::drop_realtime_room`): nothing it starts from then on, its own runner's legs and
  agents among them, inherits room to take the class beside the shards, which keep it (the limit bounds asking,
  not having). The shards ask as the runtime starts, before the server serves anything.
- **Refused,** a shard runs as it would without it, with a slice of its step, and the server says so: in its
  log once every shard has answered, and in `/healthz/server` (`priority.realtime`: asked, shards, took,
  refused, why). It never fails for it. A kernel with real-time group scheduling (`CONFIG_RT_GROUP_SCHED`)
  refuses it too where the server's cgroup has no real-time runtime (fedora's and adacachy's kernels have none).
- **Each shard on a CPU of its own, the server's other threads kept off them** (goal 9, plan `01a1a87b2cec` step
  4, 2026-11-06; `crate::rt::realtime`). A real-time thread hands a CPU it shares over only as its class allows,
  and on two counts that's past the 1 ms the shards' delay is held to. Beside another shard of its priority, only at
  the class's round-robin quantum, `sched_rr_timeslice_ms` (100 ms), where the normal class shares the CPU by the
  shards' slices. Beside a thread of the server's own, only once that thread is moved: a real-time thread's
  `sched_yield` lets only real-time threads of its priority run, and a shard waits for other threads' progress by
  yielding, as concurrent-queue (under async-channel and kiln's own queues) waits for a push under way with a
  `yield_now` loop; a normal thread preempted mid-push on the shard's CPU would wait there, the shard spinning,
  until the load balancer moved it or the fair server's share came round. So before the runtime starts, the server
  asks a thread of its own whether the class is granted (the kernel's answer, the thread then ending), and where it
  is, fixes the shards first:
  - on its declared cores (`[server] cores`, "Cores of its own"), where they give each shard one and keep a core
    for its other threads;
  - else on CPUs the machine's topology gives (`crate::rt::class_cpus`, from `crate::os::cpu_topology`): its
    fastest first (a hybrid machine's performance cores where its kernel lists the core types, whatever a favoured
    core's capacity says; else the kernel's `cpu_capacity`), the next class only where the fastest has too few, one
    thread of each physical core (SMT siblings share one core's pipeline), in the last-level cache holding most of
    them, then the next, the kernel's order within each, the first kept for its other threads, which keep every CPU but the shards'
    (`keep_threads_off`, `keep_apart`, `keep_child_off_shards`, as for declared cores). On production's machine,
    one thread each of cores 1-4 (CPUs 1-4), CPU 0 kept; for the lab's server held to fedora's efficiency cores,
    17-20, 16 kept.
  - Where neither gives each shard a CPU of its own (fewer such cores than a shard each and one more, or declared
    cores that put two shards on one CPU, unless root set the quantum within the bar), no shard asks: they run at
    the normal class with their slice, and the server says why at once, in its log and in `priority.realtime.held`;
    it gives the room up then, as no shard will. Placed, `priority.realtime.place` says where, and the line said
    once every shard has answered names the CPUs. Where the class isn't granted at all, each shard asks as before
    and says why it was refused. macOS and Windows have no such class to place: unchanged.
- **Its cost.** A real-time shard keeps its CPU from normal threads for as long as it runs, a long poll (goal
  9's slow polls) too: it belongs with shards that give up their CPU on time, as kiln's do at their step, and
  that await other threads' work rather than spin on it. Normal threads keep a share of a CPU real-time threads
  hold: from Linux 6.12 the fair server gives them 50 ms of every second on each CPU, and before it real-time
  throttling (`sched_rt_runtime_us`, 950 ms of each second by default; adacachy sets it to the whole second,
  leaving the fair server). No `LimitRTTIME=`: a shard busy with ready work makes no blocking call for as long
  as the work lasts, and the limit would kill the server for it.
- **macOS and Windows** refuse it, saying why: macOS's real-time policy for user threads (time constraint) is
  for periodic work of a known computation, as audio's is, and demotes a thread that overruns it; Windows
  preempts a lower priority level at once already, and its real-time priority class would put the server above
  the system's own work.

## What kiln install grants

The server's unit, as `kiln install` writes it on Linux with systemd, for each kind of install (goal 9, plan
`01a1a87b2cec` step 4). It reads what the machine lets it have first (`crate::install::room_here`: the root
cgroup's controllers for a system install; for a user's, their service manager's own `RLIMIT_RTPRIO` and its
cgroup's controllers) and says each line as it installs (`priority`, `realtime`, `cores`):

| | system install (`--system`, as root) | a user's install (`systemd --user`) |
| --- | --- | --- |
| CPU and I/O weight 1000 | on `/etc/systemd/system/kiln.slice`, beside `user.slice` and `system.slice`, where CI and agents run | on the user manager's `kiln.slice`, beside the user's other units (`app.slice`, where their runner runs); never against `system.slice`, as only root places a slice at the top |
| `Delegate=yes` (`DelegateSubgroup=kiln` from systemd 254) | yes | yes |
| the shards' real-time class (`LimitRTPRIO=1` with `[server] realtime`) | granted: the server runs as root (`CAP_SYS_NICE`) | granted only as far as the user manager's own hard `RLIMIT_RTPRIO` (systemd gives a unit the nearest it can), which only root raises: `<user> - rtprio 1` in `/etc/security/limits.d`, then a new service manager (logging out and in, or a reboot, which restarts the runner) |
| `AllowedCPUs=` (`[server] cores`) | applies (the root cgroup has the cpuset controller on cgroup v2) | applies only where root delegated the cpuset controller to the user's manager (a drop-in for `user@.service`, `Delegate=pids memory cpu cpuset`; systemd's own delegates `pids memory cpu`): elsewhere the shards are still fixed to the cores, by affinity, but the server isn't held there |
| a cpuset partition of the cores | made as the server starts (`ExecStartPre=+-`, as root), made a member again without cores, and by `kiln uninstall` | never: `cpuset.cpus.partition` is the parent cgroup's to set and makes a partition only of a partition's child (the root cgroup is one); a user's slice sits inside their service manager's, under cgroups root owns, and a remote partition needs `cpuset.cpus.exclusive` written by root on `user.slice`, `user-<uid>.slice` and `user@<uid>.service` |

So without root a user's unit can't get: a slice ahead of the system's; the real-time class past their service
manager's limit; `AllowedCPUs=` without the cpuset controller root delegates; a partition; `preempt=full` (a boot
parameter). The server says what it got as it starts (its log, `/healthz/server`'s `priority`), whatever the unit
asked for.

## The bare-metal default

What `kiln install` declares for a new server on Linux with systemd (goal 9, plan `01a1a87b2cec` step 4: "the
winner is the bare-metal default where it needs no more than the installer can grant"):

- **The shards in the real-time class, wherever the unit grants it:** every system install, and a user's install
  whose service manager allows `RLIMIT_RTPRIO` 1 or more. `kiln install` writes `[server] realtime = true` into
  the new config with a line saying why, and the unit's `LimitRTPRIO=1`; a config it keeps, it leaves, saying
  whether this install could grant the class and how to turn it on. Where the unit can't grant it, it stays off,
  and the install says what it would take. `[server] realtime = false` turns it off. `Config::default()` (tests,
  `kiln up`, containers, Kubernetes) keeps it off. Granted, the shards take it each on a CPU of its own, the
  server's other threads kept off them ("A real-time class for the shards"), or not at all, said why.
- **Cores of its own stay declared** (`[server] cores`): how many the server needs, and which, is its machine's
  and its load's to say, and cores held for it sit idle whenever it does. Declared on a system install, they're
  a partition.
- **macOS and Windows keep what they have:** the server's launchd job at `ProcessType` `Interactive`, its legs and
  agents below normal (nice `PRIO_MAX`); on Windows the server above normal, legs and agents below normal in jobs of
  their own. Neither has cores of the server's own or the real-time class, and `kiln install` says so.

Its basis:

- **Weights alone can't make the bar.** Goal 9's bar is shard delay p99.9 under 1 ms with hogs. In step 7's lab
  ("Step 7's lab", below; docs/runtime-lab.md), weights alone gave 3.28 ms with hogs (1.12 calm), cores of its own
  by affinity 1.96, kiln's own placement of a runner's hogs 1.97: on a kernel that preempts lazily a normal-class
  wake onto a busy CPU waits for its tick (wake-2: p99 1.00 ms, with any hog or none of the lab's), and on cores
  shared with anything else a shard still waits there.
- **The real-time class is the one way that holds whatever runs on the shard's CPU,** under every preemption
  mode: a real-time wake preempts a normal thread at once, lazy kernels included ("The kernel's preemption"
  above; research `01a1a87b2cee` f17). Cores of their own hold only as far as nothing else runs on them, which
  takes a partition (root) and cores held idle; research f9 and f12 (Caladan, Heracles) weigh against static
  partitions.
- **It needs nothing the installer can't grant** where it's on: root's `CAP_SYS_NICE`, or room within a user's
  service manager's limit; refused, the server runs as without it and says why.
- **Its risk is bounded:** the lowest real-time priority, below the kernel's own real-time threads; reset on
  fork; the room given up once the shards asked; shards that give their CPU back at their step and park when
  idle; normal threads keep the fair server's share; and each shard on a CPU no other shard and no other thread of
  the server's runs on, so neither waits on a real-time shard's turn (2026-11-06: before, a shard yielding while it
  waited on a thread of the server's own preempted on its CPU could hold that thread there until the load balancer
  moved it; `rt::tests::g9_s4_a_yielding_real_time_thread_holds_a_normal_one_on_its_cpu` shows it on the kernel).

It rests on the kernel's rules and step 7's lab, not yet on a lab A/B of the class itself: no lab machine
granted it (fedora's user manager has `RLIMIT_RTPRIO` 0). The A/B that decides it is in docs/runtime-lab.md
("Goal 8 step 7 and goal 9 step 4", "The A/B to run"): weights alone, cores, and the class beside hogs and on
cores. If the class doesn't beat weights alone there, the default goes back to off (`realtime_default` in
`crate::install`).

**Ada's call** (each needs root, or production):

- **The A/B on fedora:** one root command gives the lab's units the room (docs/runtime-lab.md), or a `limits.d`
  entry, which takes a new user manager there (a restart of fedora's runner).
- **Production:** its unit (`kiln-server.service`, a system unit with `User=`) needs `LimitRTPRIO=1` (a drop-in,
  `kiln-server.service.d/realtime.conf`) and its config `[server] realtime = true`, with a binary that places the
  shards as above (docs/runtime-lab.md, "What production grants the shards, and what's missing": today it grants
  the weights alone, no class and no pinning); `[server] cores` there would also need a partition, which its
  hand-written unit doesn't make (the installer's does), and a choice of cores.
- **A partition on fedora's lab,** `preempt=full` at boot there, and `isolated` partitions (no load balancing
  on the shards' cores) anywhere: root's.

## A nudge for the shards

`[server] nudge` is what a shard has in the real-time class's place where that's refused, needing no privilege
(goal 8, plan `01a1a59c2b29` step 7; goal 6, plan `01a1a59c2abe` step 3; `hyper_rt::compute::nudge_current_thread`,
`crate::nudge`). It's off until declared (docs/ci.md §0.3, "Opt-in automation"): `"off"` is the default;
`"auto"` is the mode a config declares, and `"on"` the other. `KILN_RT_NUDGE` overrides it (`auto`, `on`, `off`,
or a flag's word, `1` or `0`; any other is refused), and `KILN_RT_NUDGE_US` sets its period for a lab's A/B.

- **What a nudge is.** A nudged shard's waits in its driver end by its period, the step quantum less the timers'
  tick (166.7 µs; goal 9, plan `01a1a87b2cec` step 4: `crate::rt::nudge_period_ns`), so with its timer slack (that
  tick) its CPU takes an interrupt of the shard's own at least every quantum, 266.7 µs, while it waits: the host's
  share of a ready task's delay in the quantum's derivation. Under `lazy` a wake marks the busy thread and sends no
  interrupt, and the CPU switches as that thread next returns to user space from the kernel: the shard's own timer
  is such a return. A shard woken behind a thread spinning in user space then runs within about a quantum, instead
  of at the kernel's next tick (up to 1 ms at 1000 Hz). Nothing a normal thread may set makes the wake itself
  preempt at once (docs/runtime-lab.md, "Goal 8 step 7 and goal 9 step 4", reads the kernel for why).
- **Its period, and what wake-2 measured.** A held wake waits at most the period and the lateness of the shard's
  own timer. wake-2 measured that lateness behind each kind of hog: a thread's timer woke it within 57.8 µs of
  due at p99 behind spinning hogs (its slack 50 µs), and within 80 to 129 µs behind compilers and hogs that enter
  the kernel, where it asked for a slice of its own as kiln's shards do (without one, 394 to 641 µs). So at the
  derived period a held wake waits about 225 to 296 µs at the most, about the quantum the delay bar's derivation
  leaves the host, against up to the 1 ms tick unnudged (wake-2: p99 1.00 ms with every kind of hog); at the
  timers' tick, the period until goal 9 step 4, about 160 to 230 µs, the floor every ready timer of a shard pays
  by design (two ticks, `DELAY_FLOOR_NS`), for a third again as many wakes. A longer period buys fewer wakes at a
  later bound; a shorter one, a bound under a floor no ready work of the shard is held to anyway. The lab's A/B of
  the two decides (docs/runtime-lab.md, "The A/B of the wake path": `nudge-on-hogs` against `nudge100-hogs`).
- **`on`'s cost.** A waiting shard wakes once a period, a park and a turn of its loop, CPU it spends and the
  threads beside it lose; and while a shard waits its CPU can't reach the deeper idle states. A calm server
  pays that for nothing.
- **What the lab found of `on`** (docs/runtime-lab.md, "The nudge's A/B"; 2026-11-05, at the timers' tick): with
  hogs it gave back most of what a lazy wake costs a request (API reads' wait for their shard at p99.9 from 1.53 to
  0.60 ms, their median from 0.63 to 0.24 ms), but not goal 9 step 4's bar (the shards' delay p99.9 2.98 to
  2.19 ms, the same within noise), at a busy share 27% higher. So `on` isn't the bare-metal default: it needs no
  grant, but it doesn't make the bar, it costs what goal 9 step 5 cuts, and production's kernel preempts fully,
  where it would only cost.
- **`auto`: only while the host holds the shard's wakes.** Once a period of its health probe (17.07 ms), on its own
  thread, each shard reads the kernel's count of how long its thread waited, runnable, for a CPU (Linux's run
  delay, `/proc/thread-self/schedstat`, a file kept open: one read), and its parks by what ended them. The
  period's wait, less what a calm CPU makes a wake by the shard's own timer wait (0.7 µs, wake-2's most with no
  hog), over the wakes another thread or its I/O made, is how long its host held each of them. A period at
  17.2 µs or more was held: the geometric mean of the most a run whose wake nothing held waited in wake-2 (4.4 µs,
  compilers beside a socket's wake on its own CPU; 1.2 µs with no hog) and the least a held one did (67.4 µs,
  hogs entering the kernel, a wake from another CPU; spinning hogs 368 to 436 µs, compilers 294 to 352, other
  sessions' work alone 200 to 223), a factor of 3.9 from each. A shard nudges while a held period was within the
  last tolerance (1 s, the interval `crate::overload` judges a queue over), so one held now and then nudges
  between; it stops a tolerance after its host lets it run, and a shard nothing but its own timers woke for a
  tolerance is idle and stops too. Nudging, its held wakes still read held (they wait about 130 µs on average,
  half the nudge's bound at its period, and its own timers' wakes on a busy CPU wait past 0.7 µs each), so it goes
  on as long as its host holds it.
- **A calm server pays nothing for it** but the reading, a file read each period on each shard: a calm shard's
  wakes waited 1.2 µs each in wake-2, well under 17.2 µs, as do those of a kernel that preempts fully
  (production's), where a wake interrupts the busy CPU at once and a nudge would buy nothing. On fedora, where
  other sessions' work runs on the lab's CPUs, a "calm" shard may well be held (wake-2: 200 to 223 µs a run with
  none of the lab's hogs), and nudges then.
- A shard that took the real-time class doesn't nudge: its wake preempts at once. The others ask for their
  slice, then the nudge (where it's `auto`, leave to nudge on their own).
- **It says what it did.** The log says how the shards took it once every shard has answered, and each shard's
  first nudge, with what its wakes waited; `/healthz/server` too (`priority.nudge`: asked, mode, period, shards,
  nudged, those in the real-time class, refused, why, `armed`: the shards nudging now, `starts`: the times they
  began); `/metrics` each shard's time nudged (`kiln_shard_nudged_seconds_total`), its starts
  (`kiln_shard_nudge_starts_total`), whether it nudges now (`kiln_shard_nudging`), its thread's wait for a CPU
  (`kiln_shard_run_wait_seconds_total`) and its last period's held wait a wake (`kiln_shard_held_wake_seconds`).
- **macOS and Windows** refuse it, saying why: they have no lazy preemption to bound. A Linux kernel that keeps no
  thread's wait for a CPU (no `CONFIG_SCHED_INFO`) refuses `auto`, saying why; `on` works there. `auto` judges
  at its shards' probes, which every server runs.
- **Off until declared.** What kiln decides on its own is off until declared (docs/ci.md §0.3, "Opt-in
  automation"), so `Config::default`'s `nudge` is `off` and `auto` is the mode a config declares. The lab's A/B
  (docs/runtime-lab.md, "Goal 6 step 3 and goal 8 step 7: the nudge on its own") says what `auto` costs a calm
  server and buys beside hogs, for whoever declares it.

## Critical's own path

What a person's page, sign-in or action and a health check wait for besides their shard's turn (goal 6, plan
`01a1a59c2abe` step 3), and what each does now:

- **Their shard's wakes.** A request on a kept-alive connection (a browser's, the lab's pages) wakes its
  connection's shard as its bytes arrive; a new connection wakes the shard its listener's task runs on, then the
  one it's placed on (`crate::rt::spawn_connection`, round-robin as goal 7's A/B chose). On a lazy kernel beside
  a busy thread each such wake waits up to a tick; nudging (`auto`, above), each waits at most the nudge's bound.
  Placing a new connection on the shard that accepts it would take one of the two wakes off it, but changes goal
  7's placement, and is for its A/B to decide.
- **The answer's write.** A response is written in its connection's task as its handler returns (HTTP/1.1 and
  HTTP/2 alike, round-robin placement), no wake between; a page's bytes come from memory (`crate::ui`).
- **Health.** `/readyz` answers from what the server knows (`crate::pool_health::store_writer_stuck`,
  `Store::writer_running`), no longer a `SELECT 1` through kiln.db's one writer, which waited behind every write
  before it (the lab's writer line: 0.7 s at p99; on 2026-10-30 production's readiness failed on it under the
  host's load and a deploy was rolled back). The store is unready only once its writer's thread has stopped, or
  while calls wait for it and it has finished none for 30 s: as long as the longest any prober kiln configures
  waits for `/readyz` (KilnCD's checks, 30 s where they name no timeout; the Kubernetes probe kiln installs, 3 s),
  so no prober sees it unready where the `SELECT 1` would have answered within its wait.
- **The pools' lines.** A job a Critical task submits to the blocking pool (a sign-in's password file, a
  person's session and rights read from kiln.db on the `store` share) waits ahead of the rest of its share's
  line, as the shard's run queue takes that class first, its share's places bounding it as they bound the rest
  (`hyper_rt::blocking`). Before, a person's read waited behind every read before it: custody's held the `store`
  share's line at about 14 jobs, 80 ms each (the census, train 8).
- **Not yet:** the compute pool's line is first come, first served (a sign-in's Argon2 check waited there with
  the rest: 37 ms at p99 with hogs in g9p7); and a sign-in, like a person's action that writes, waits for
  kiln.db's writer, whose line is the store's group commit's to shorten.

## A spin before the park

`KILN_RT_SPIN=1` (the lab's to ask for, off otherwise; goal 9, plan `01a1a87b2cec` step 4; `hyper_rt::park_spin`)
has each of the server's shards spin before each park as long as its own idle gaps make cheapest in CPU, up to
16 µs; `KILN_RT_SPIN_US` has it spin a fixed time whatever it costs, the lab's measure of what a longer spin buys:

- A shard still spinning when its work comes is never woken: its senders find it running and skip their kicks, and
  the work runs at once, where a park's wake would wait, on a kernel that preempts lazily, for its CPU's next
  interrupt behind a thread busy there (up to a tick).
- Each window of 385 idle moments, the spin becomes the one of 0, 1, 2, 4, 8 and 16 µs that would have spent the
  least of the shard's CPU on them: a gap the spin catches costs the spin to it, one it misses the whole spin and
  the park after it, whose CPU the shard learns from its own parks (its task clock, no system call) and its
  senders' kicks. A gap a kick ended runs to the kick's stamp, not to the park's return, which adds the wake's own
  latency. So it spins only where, on the last window's gaps, spinning cost less CPU than parking at once: the busy
  share goal 9 step 5 holds the shards to can only fall by it.
- Its spins, their hits, the time they spun and the spin in force are on `/metrics` (`kiln_shard_spins_total`,
  `kiln_shard_spin_hits_total`, `kiln_shard_spin_seconds_total`, `kiln_shard_spin_window_seconds`), and what was
  asked in `/healthz/server` (`priority.spin`); the log says it once every shard has taken it.
- **What it can't do.** It catches only gaps under 16 µs: work a shard waits on longer (a request from another
  CPU, most jobs on the pools) still wakes it, and that wait is the nudge's to bound. At the lab's rate of parks a
  16 µs spin that missed every gap would cost 1.9% of a CPU a shard, three times goal 9 step 5's bar with hogs on
  its own (docs/runtime-lab.md, "The shard's own wake path"), so only the sized spin is a candidate default, and
  only where the lab's A/B finds it buys a number for no busy share. macOS and Windows take it as Linux does; only
  Linux's shards learn a park's CPU on a task clock, so elsewhere a sized spin stays at none.

## Step 7's lab

`scripts/priority_lab/s7c.sh` runs the A/B of the three ways to keep the host off the shards, in production's
layout (the server alone in a scope of its own at `CPUWeight=1000`, hogs at `cpu.weight` 1), the
configurations taking turns round by round, eight rounds each (goal 9, plan `01a1a87b2cec` step 4: shard delay
p99.9 under 1 ms with hogs):

- `calm`: the server on `SERVER_CPUS` (16-31), no hogs; `hogs`, `hogs-cc`: weights alone, with as many spinning
  hogs or C compilers as those CPUs;
- `cores-calm`, `cores`, `cores-cc`: the server on cores of its own (`CORES`, 16-23: its scope's `AllowedCPUs=`
  and `[server] cores`), the hogs on the rest (24-31), where the work CPUs put kiln's own;
- `fence`: as `cores`, but the hogs are a kiln runner's agents (the stand-in's `lab:hog`), started by a runner
  held to `SERVER_CPUS` whose config declares the server's cores, so kiln itself puts them on 24-31; each
  round notes the CPUs each hog thread may run on;
- `rt`, `rt-cc`: weights alone with the shards in the real-time class (`KILN_RT_REALTIME=1`, the scope given
  `LimitRTPRIO=1` as the installer's unit is); run only where the machine grants it, else skipped and said so.
- `nudge`, `nudge-cc`, `calm-nudge`: weights alone with the shards nudging their CPUs (`KILN_RT_NUDGE=1`, "A
  nudge for the shards"), beside the hogs or the compilers, and with none: what the nudge buys and costs.

Each round notes `/healthz/server` (the real-time class as the shards took it, the work CPUs) and how busy
other processes kept the server's cores (`foreign.py`): on a lazy kernel, work that isn't the lab's on those
cores holds a waking shard for a tick, and only a partition keeps it out.

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

**Block 5: the same lab on current main.** Block 4 measured main before kiln's own runtime
(`crates/kiln-rt`) and its compute pool. Block 5 is the same lab on main at `82ca3c74d03`, which has
them: block 4's five configurations, four rounds each, on 2026-11-04 from 17:51 to 18:24 UTC. Other
agents' kiln builds (and this goal's own, on every CPU) loaded the machine throughout: a load of 20
to 54 before each window, CPU pressure 2% calm and 22-31% with legs. The rounds:
`docs/priority/2026-11-04/b5-*.json`.

| number | calm | even weights | kiln.slice's weight | and idle groups | and an idle unit |
| --- | --- | --- | --- | --- | --- |
| page p50 ms | 0.152 (0.127 to 0.178) | 0.594 (0.201 to 0.678) | 0.197 (0.156 to 0.567) | 0.966 (0.776 to 1.23) | 1.07 (0.939 to 1.19) |
| page p99 ms | 0.303 (0.250 to 0.333) | 61.2 (3.02 to 84.6) | 2.34 (1.96 to 57.3) | 2.35 (2.10 to 2.47) | 11.4 (2.05 to 71.3) |
| page p99.9 ms | 0.649 (0.309 to 1.21) | 142 (20.8 to 226) | 3.65 (2.52 to 137) | 4.30 (3.03 to 13.5) | 23.9 (2.90 to 122) |
| api_read p50 ms | 0.417 (0.353 to 0.514) | 1.64 (1.56 to 1.75) | 1.82 (1.67 to 1.93) | 3.16 (2.53 to 3.80) | 3.15 (2.61 to 3.85) |
| api_read p99 ms | 66.7 (56.7 to 100) | 158 (125 to 179) | 133 (103 to 176) | 238 (209 to 272) | 237 (201 to 273) |
| api_read p99.9 ms | 678 (305 to 775) | 1074 (563 to 2608) | 811 (708 to 1066) | 1167 (944 to 4377) | 1053 (995 to 2192) |
| api_write p50 ms | 0.931 (0.785 to 1.05) | 1.76 (1.57 to 2.24) | 1.89 (1.54 to 1.93) | 1.94 (1.93 to 1.99) | 1.94 (1.94 to 2.00) |
| api_write p99 ms | 415 (97.5 to 536) | 701 (366 to 1248) | 591 (514 to 668) | 921 (795 to 3386) | 742 (697 to 879) |
| websocket p50 ms | 0.693 (0.587 to 0.819) | 2.77 (2.55 to 3.18) | 2.68 (2.63 to 2.75) | 3.80 (3.30 to 4.40) | 3.77 (3.27 to 4.28) |
| websocket p99 ms | 883 (812 to 1147) | 1294 (1141 to 1426) | 1097 (695 to 1309) | 1062 (1012 to 1140) | 1102 (978 to 1191) |
| websocket p99.9 ms | 1191 (965 to 1564) | 1808 (1550 to 1982) | 1502 (1282 to 1757) | 1347 (1218 to 1388) | 1327 (1261 to 1411) |
| attach p50 ms | 1075 (965 to 1188) | 1350 (1232 to 1552) | 1398 (1222 to 1449) | 1312 (1102 to 1466) | 1270 (1174 to 1360) |
| server: api_read took p99 ms | 46.6 (45.7 to 56.8) | 95.7 (87.0 to 186) | 89.5 (63.3 to 96.8) | 204 (197 to 221) | 204 (165 to 220) |
| server: websocket waited p99 ms | 873 (805 to 1485) | 1664 (1513 to 1935) | 1299 (752 to 1839) | 1287 (995 to 1644) | 1309 (966 to 1638) |
| shards' delay p99 ms | 0.748 (0.618 to 0.911) | 3.33 (2.10 to 4.72) | 2.76 (1.77 to 3.55) | 2.60 (2.29 to 3.06) | 2.33 (2.27 to 3.55) |
| shards' delay p99.9 ms | 23.9 (14.1 to 26.8) | 23.5 (21.9 to 29.7) | 24.5 (18.9 to 28.2) | 10.5 (2.49 to 23.4) | 11.8 (2.50 to 22.1) |

On current main:

- **Idle stays turned down.** Against kiln.slice's weight, idle groups were worse in 11 numbers and
  better in 2 (pages 0.197 ms at the median against 0.966, API reads 133 ms at p99 against 238,
  writes 591 against 921, the server's own API reads 89.5 ms against 204), and an idle unit worse in
  9 and better in none.
- **kiln.slice's weight and even weights didn't differ beyond noise** in any of the 43 numbers (block
  4: 8 better). Both left the machine's CPUs as contended (22-23% CPU pressure).
- **Against calm, kiln.slice's weight was worse in 17 numbers, better in none,** API reads' p99
  among them (66.7 ms to 133) and pages' (0.303 ms to 2.34), with every median (API reads 0.417 ms to
  1.82, the live feed 0.693 to 2.68) and the shards' delay (0.748 ms to 2.76 at p99). The live
  feed's and API writes' p99s stayed within noise.

Step 7 asked for the API's p99 with CPU hogs held within noise of calm, legs and agents at idle
priority. Idle, it wasn't: against calm, API reads' p99 (126 ms to 186) and the live feed's (0.90 s
to 1.09) were worse beyond noise. With what lands, kiln.slice's weight and legs at the least weight,
the p99s of pages, API reads and writes and the live feed were within noise of calm; its medians
weren't (API reads 0.366 ms to 1.49, the live feed 0.638 to 2.54), nor the shards' delay (0.955 ms
to 3.41 at p99), and calm's own page tail was wide on this busy machine: in quieter blocks pages
took 0.33 to 0.46 ms at p99 calm, against 2 ms with legs. Cores of its own held pages and the API
whole. On current main (block 5), with a quieter calm, weights didn't hold the API's p99 either:
API reads' and pages' p99s were worse beyond noise (66.7 ms to 133, 0.303 to 2.34), only the live
feed's and writes' held. So weights put the server first, but only cores of its own (block 3, not run
again) kept the API whole; headroom ("headroom's lab", below) is what slows the legs when the
server's delay rises toward harm.

### headroom's lab (step 5)

`scripts/priority_lab/headroom.sh` runs one long round of the `realistic` profile on fedora's CPUs
0-15 (other agents' labs held 16-31), the lab's unit standing in for `kiln.slice` at
`CPUWeight=1000`. Beside it, a runner of the same build with headroom on, its settings' defaults
(`escapes` off), in a delegated scope of its own at 100. Its legs and agents are CPU hogs it started:
CI jobs of a pipeline run by hand (`legs.yaml`, four jobs of four busy loops) and agents' turns of
the lab's stand-in (`lab:hog:<threads>:<secs>`), each in a cgroup of its own that headroom paces.
`hogs.sh` starts and raises them, moves the server's process into the runner's cgroup (`join`), and
turns headroom off and on. `record.py` notes `/healthz/delay` four times a second, the shards'
histogram, headroom's status as the server has it, and each group's freezer and CPU. `timeline.py`
reads that back. Other agents' kiln builds loaded fedora throughout (a load of 15 to 80), and it
reset three times (16:48, 18:45 and 18:58 UTC); the lab's raw recordings stay in its folder on that
machine, not here.

**What main (`82ca3c74d03`) did.** Research 01a1a59c2b2a, f44 to f47:

- **Weights held the server.** With 16, then 32 hog threads at kiln.slice's weight, the server's
  delay rose in its body, not toward harm: `/healthz/delay`'s median from 0.21 ms to 0.3-0.8, the
  shards' p99 from 0.5-0.9 ms to 2-5, the shards' mean over 2 s from 0.23 ms to 0.31-0.85. At even
  weights, with 48 more threads, the same. headroom's delay target never moved: its CUSUM stayed at
  0.
- **Sharing the server's cgroup, they didn't, and headroom didn't see it.** With 96 hog threads in
  the server's own cgroup (as a server's own runner holds its agents where the server isn't a
  delegated unit), its shards ran ready work 40 to 100 times later than calm for four minutes
  (median 8 to 28 ms, the shards' p99 25 to 97 ms, their mean over 2 s 2.8 ms). The delay target
  never alarmed: its CUSUM peaked at 6.5 against a threshold of 12.2, while its calm rose with the
  hogs from 1.3 ms to 4.6.
- **It caught harm, not trouble.** It alarmed only when probes took a second: a machine-wide stall
  under the other agents' compiles (the shards' probes every 2 s fell from about 775 to 347), where
  headroom cut kiln's CPU from 10.9 cores to 5.5 and froze its 16 groups 50-75% of the time. That
  stall wasn't kiln's, and slowing kiln didn't end it.

Why: it read one probe of the worst shard at an instant, which calm's own spikes (30 to 223 ms at
most in each 30 s, from other load and long requests) dwarf and which misses everything between two
reads; and against the platform's 1 s, the CUSUM counts a reading only past about 40 ms (half way
from calm to 1 s, in its logarithm). Below that every reading pulled the CUSUM down and joined calm.
What headroom reads now ("headroom listens to the server", above) is every probe of every shard
between two reads, against the shards' 10 ms period: calm's mean stayed within 0.21-0.44 ms over 2 s,
kiln.slice's weight's 0.31-0.85, and the shared cgroup's reached 2.8 (4.5 at p90).

**With the fix (`e4219270345`, server and runner).** Research 01a1a59c2b2a, f50 and f51:

- **Where the weights hold the server, it holds still.** With 32 hog threads at kiln.slice's
  weight, the server's mean delay between reads stayed at 0.23-0.27 ms (calm 0.22), and the target's
  CUSUM at 0 (18:35 to 18:39 UTC). Its one cut came from the runner-disk target (a slow fsync), and
  was raised back within seconds, as on main.
- **Where they don't, it slows the legs before harm.** With the server sharing the runner's cgroup,
  32 hog threads raised the mean to 0.5-0.8 ms (calm 0.2-0.3), short of trouble. 64 more raised it to
  4.6 ms, and 23 s after they started headroom alarmed on the server's delay ("typical 0.4 ms, now
  4.6 ms (CUSUM 1.49 past 1.17)") and cut kiln's CPU from 15.3 cores to 7.6, pacing the legs and
  agents (18:52:39 UTC). On main the same load went four minutes with no alarm.

What the lab couldn't show is the delay coming back down with the legs paced: within a minute of that
cut, other agents' work restarting after a reset took fedora from 57 GB available to 1 GB (a load of
280), every reading stalled, and the lab was stopped so as not to add to it. Twice before, fedora
had reset under the same pressure (16:48 and 18:45 UTC), and once more after (18:58). In run 5, the
same machine-wide stall came minutes after the server joined the runner's cgroup: headroom cut
kiln's CPU to nothing and froze its groups 94-100% of the time, and the readings stayed at seconds,
since the stall wasn't kiln's. The engine's test with the lab's numbers
(`headroom::tests::g8_s5_the_servers_delay_rising_slows_legs_and_agents`) shows the rest: the cut
paces the leg and the agent alike, and the delay back down clears the alarm, the cut held until the
level holds.

**item2: main let go (`0c11fb03a06`, 2026-11-05 01:27-01:39 UTC, CPUs 16-31).** Research
01a1a59c2b2a, f52 to f55. The server joined the runner's cgroup at 90 s, the legs (32 threads)
started at 120 s and the agents (64) at 210 s. In 10 s bins of `timeline.py`:

| t s | delay med ms | mean since read med ms | shards p99 ms | calm ms | CUSUM | threshold | CPU budget | paced | least share |
|---|---|---|---|---|---|---|---|---|---|
| 110 | 0.21 | 0.20 | 0.60 | 0.234 | 0.00 | 0.21 | - | 0 | |
| 120 | 1.00 | 0.55 | 7.76 | 0.245 | 0.00 | 1.79 | 14.3 | 8 | 52 |
| 130 | 0.76 | 0.51 | 5.99 | 0.260 | 0.00 | 1.77 | 28.6 | 0 | |
| 220 | 5.75 | 1.38 | 21.84 | 0.407 | 0.00 | 5.25 | 29.6 | 0 | |
| 340 | 0.21 | 0.25 | 13.89 | 0.607 | 0.00 | 3.89 | 7.5 | 5 | 24 |
| 460 | 10.04 | 2.05 | 33.15 | 0.757 | 0.00 | 3.40 | 30.1 | 0 | |
| 630 | 3.97 | 1.05 | 19.41 | 0.914 | 0.00 | 3.40 | 28.4 | 0 | |

headroom alarmed at 126 s ("typical 0.2 ms, now 3.5 ms (CUSUM 0.82 past 0.23)") and cut kiln's CPU
from 14.3 cores to 7.1, was back to calm a second later, and had the budget at 28.6 cores by 137 s.
The agents' alarm at 216 s cut nothing and cleared at 221 s. From then to the end the delay sat at
4 to 6 times calm with nothing paced, but for a runner-disk alarm at 346 s whose cut (the agents at
24-30%) brought it back to calm until it was let go at 358 s. Read back from its own status, the
causes: every sample not under an alarm joined calm, and the legs' 2.5 and the agents' 4 to 6.4
times calm sat below the CUSUM's reference (half way to the 10 ms tolerance, 6.6 times calm), so
calm drifted from 0.238 ms to 0.919 and the CUSUM never rose again; an alarm cleared the moment its
CUSUM touched zero; the engine read only the writes' lag, unknown all run since only the CPU was cut,
so a level counted as held after a second, and every excursion a cut ended was learned as noise (the
threshold from 0.23 to 5.6); the CPU knee took kiln's measured cores, 12.9-14.4 under a 7.1-core
budget (what paced groups gave up, unpaced ones took), as the level held, and so disproved the bound
it had cut at; and the alarm at 216 s found the disks "full" against a capacity read during the
kiln-server alarm at 204 s, so cut no CPU. headroom.md's "Simulation" runs this as a trace, and goal
8 step 5's engine holds it there; item2b below is the lab's word.

## Needs root, or a person

- A system install (root, once) for the top-level `kiln.slice`, the partition, and the server's
  memory protection the bare-metal design's §5 adds.
- Runners installed before this run legs and agents at the default weight until they're reinstalled
  or upgraded with a delegated unit. Upgrading a runner is a person's call: it ends the agents it
  runs.
- The user manager must give its units the `cpu` controller: systemd's own `user@.service`
  delegates `pids memory cpu`, and a runner's unit's `cgroup.controllers` says what it got.
- On a lazy kernel, the server's cores are its own only with the partition (a system install): kiln
  keeps its own legs and agents off them, nothing else. `AllowedCPUs=` on a user's install needs the
  cpuset controller delegated to their service manager (root's drop-in for `user@.service`).
- The real-time class for a user's install: the machine's owner raises the user's `RLIMIT_RTPRIO` to 1
  (a `limits.d` entry, then a new service manager), or installs the server system-wide. `preempt=full` at
  boot, on a lazy kernel.
- Production (a hand-written system unit with `User=`): `LimitRTPRIO=1` in its unit and `[server] realtime
  = true` in its config, a deploy of a binary that places the shards ("The bare-metal default", above; the exact
  drop-in, what shows it took and how to take it back: docs/runtime-lab.md, "Ada's call").
- Production's agents in cgroups of their own, apart from its server's memory ("Memory", above): its unit
  needs `Delegate=yes` and `DelegateSubgroup=kiln`, and a restart, which ends every agent session the
  server hosts. Its `MemoryHigh=` and `MemoryMax=` then hold the server and its agents together.
- The lab: its machine's kernel is lazy and it has no root there, so its A/Bs of the partition and of the
  real-time class wait on the owner (research `01a1a87b2cee` f17, f18: rtkit refuses this user a
  real-time class there). The real-time A/B needs one root command there (docs/runtime-lab.md, "Goal 8
  step 7 and goal 9 step 4: the server's cores").

## Tests

- `os::contain::tests::g8_s2_a_delegated_units_groups_run_at_the_least_weight` (Linux): makes a
  scope of the user's own delegated to it, arranges it as a runner does (its process into the leaf,
  `+cpu` for its children), and reads `cpu.weight` 1 on an agent's group with what it starts inside
  it, never `cpu.idle`, and the default on a person's run's.
  `g8_s2_the_unit_whose_leaf_kiln_runs_in_is_its_home` reads the home.
- `os::contain::tests::agents_memory_is_held_in_their_folder_and_never_takes_kilns_floor` (Linux): a
  delegated scope of the user's own with a floor, a point it's slowed at and a most (`MemoryMin=`,
  `MemoryLow=`, `MemoryHigh=`, `MemoryMax=`, no swap), arranged as the server does: the floor on its
  leaf alone, the agents' share on their folder, and an agent's work twice that share killed in the
  folder while the unit itself is never slowed, filled or made to kill and its own process runs on.
  `agents_share_what_their_unit_may_have_less_kilns_own_floor`: the share of production's unit as it
  was written, without its limits, under a slice's, and none where the floor leaves nothing.
- `install::tests::g8_units_are_delegated_and_the_server_comes_first` and
  `g8_systemd_reads_the_units_kiln_writes`: the runner's and the server's units, `kiln.slice` with
  its weights and cores, `ProcessType` `Interactive` for the server's launchd job, and
  `systemd-analyze verify` finding nothing against them.
- `os::contain::tests::g8_s3_an_agents_processes_run_under_background_policy` (macOS): an agent's
  harness and what it starts run at nice `PRIO_MAX` (`ps`), strictly below a plain process's under a
  runner below `PRIO_MAX` and level with it under one at it; a person's run doesn't; a step on a pty
  is put there as it runs.
- `os::contain::tests::what_an_agent_starts_through_launchd_is_followed_as_its` (macOS): a launchd
  job an agent's script bootstraps from a relative property list is followed as the agent's, below
  normal, and ending the agent boots it out.
- `os::contain::tests::what_launchctl_starts_is_read_from_its_arguments` (macOS): `bootstrap`,
  `load`, `submit`, `start` and `kickstart`.
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
  `g8_s5_a_windowed_targets_probe_out_past_its_tolerance_is_no_harm_until_its_window`: for a
  windowed target that doesn't come first, a rise the weights held (1.4 to 1.8 times calm) is no
  trouble, and a probe out is harm only at the window.
- kiln-ci's `headroom::sim::g8_s5_the_11_05_lab_rising_delay_paces_the_legs_and_holds_it_down_until_the_load_ends`:
  item2 as a trace (headroom.md, "Simulation"). On main's engine every loaded stretch was past
  calm's noise and nothing was paced 5 s after the first cut; now the legs are paced within 30 s,
  the delay is held within calm's noise while the load stays, something stays paced until it ends,
  it's calm after, and calm stays what it was.
- kiln-lab's `standin::tests::g8_s5_a_hog_turn_burns_for_its_time_and_no_longer`: the stand-in's CPU
  hog turn, the lab's agents.
- `rt::tests::g8_s6_shards_are_fixed_to_cores_of_the_servers_own` and
  `os::tests::g8_s6_a_cpu_list_reads_as_linux_writes_one`: cores of the server's own.
- `os::tests::g8_s7_legs_and_agents_keep_off_the_servers_cores` and
  `g8_s7_cpus_are_written_as_linux_writes_a_list`: the work CPUs, this process's less the server's; where
  they'd run anyway beside a partition; shared, and said so, when the server's are all a process has.
  `os::contain::tests::g8_s7_a_process_keeps_what_of_its_cpus_are_work_cpus` and
  `g8_s7_a_leg_or_agent_starts_on_the_work_cpus` (Linux): a process started there runs on them from its
  first instruction, what it starts too, from a thread that may run anywhere and from one fixed to a
  shard's core, and a leg's share stays its share.
- `runner::cpus::tests::g8_s7_legs_keep_off_the_servers_cores`: a runner beside a server with cores of its
  own makes its legs' shares and leases of its work CPUs alone and runs a step that holds nothing on them;
  `runner::docker_sock::tests::g8_s7_a_steps_containers_keep_to_the_work_cpus`: a container a step makes
  keeps to them unless it names its own.
- kiln-rt's `compute::tests::g9_s4_the_real_time_class_is_taken_whole_or_refused` (Linux): a shard asking
  gets the class whole, a thread it starts at the normal policy, or is refused and runs as it did;
  `g9_s4_a_real_time_class_ranks_above_every_other_policy`. `rt::tests::g9_s4_the_real_time_class_is_said_as_the_shards_took_it`:
  said once every shard answered, a refusal in a person's words. `install::tests::g8_units_are_delegated_and_the_server_comes_first`:
  `LimitRTPRIO=1` for a server asking for the class, and no policy for all of it.
- Goal 9, plan `01a1a87b2cec` step 4, what `kiln install` grants and the bare-metal default:
  `install::tests::g9_s4_the_real_time_class_is_granted_by_a_system_install_and_within_a_users_manager_limit`
  (root grants it; a user's manager at 99, 1 or unlimited grants it, at 0 says what root must do, unread
  grants nothing); `g9_s4_cores_get_what_each_install_may` (a system install's partition made, given back
  without cores, left alone on a user's; `AllowedCPUs=` only where the cpuset controller is; the server's own
  runner off the shards' cores, said so; the unit's `ExecStartPre=` lines); `g9_s4_kiln_install_says_how_the_shards_run`;
  `g9_s4_a_new_servers_config_declares_the_class_where_its_unit_grants_it` (and `Config::default()` keeps it
  off); `g9_s4_a_service_managers_limits_and_cgroup_read_as_linux_writes_them` and
  `g9_s4_this_machines_service_manager_is_read`; `g8_systemd_reads_the_units_kiln_writes` with the unit that
  gives a partition's cores back. `os::tests::g9_s4_legs_and_agents_in_a_partition_keep_off_the_shards_cores`,
  `os::tests::g9_s4_the_real_time_room_is_given_up_whole` (Linux: a process's `RLIMIT_RTPRIO` to 0, soft and
  hard, read back from `/proc`), `os::cgroup::tests::g9_s4_the_partition_a_server_runs_in`.
- Goal 9, plan `01a1a87b2cec` step 4, the class's placement (2026-11-06): `rt::tests::g9_s4_shards_in_the_class_take_cpus_of_their_own`
  (production's machine gives 1-4, 0 kept, every other CPU the server's other threads'; the lab's 16-31 gives 17-20;
  fedora whole its performance cores, one thread of each; the cache holding most cores first; two cores for two
  shards and the rest, or none read, is no place); `g9_s4_the_class_is_held_back_where_a_shard_would_share_its_cpu`
  (two shards on a CPU at a 100 ms quantum, or the server's other threads beside them, held back and said why; a
  quantum within the bar allowed; a CPU each fixed and said where); `g9_s4_whether_the_class_is_granted_is_asked_of_the_kernel`
  (granted with `RLIMIT_RTPRIO` room, refused without it naming the grant, the asking thread's class its own);
  `g9_s4_a_yielding_real_time_thread_holds_a_normal_one_on_its_cpu` (where granted: a real-time thread yielding
  for 1 ms lets a normal thread on its CPU make no progress, where at the normal class it runs at once);
  `g9_s4_the_real_time_class_is_said_as_the_shards_took_it` (where they took it, and a class held back, said);
  `os::tests::g9_s4_a_cpus_place_reads_as_linux_writes_it`, `g9_s4_the_round_robin_quantum_reads_as_linux_writes_it`,
  `g9_s4_this_machines_cpus_are_placed`.
- Goal 9, plan `01a1a87b2cec` step 4, the shard's own wake path: kiln-rt's `park_spin::tests` (the cap a park
  bound past the cheapest wake, the window's share to five points; long gaps or free parks never spin; the spin the
  cheapest in CPU on the window's gaps, worked by hand; the chosen spin never costing more than parking, over a
  spread of gaps and park costs; a sized spin sizing itself from hits and parks, a kicked park's gap to its stamp;
  stale stamps, withdrawn parks and an unmeasured park cost; fixed and off spins, a new ask; only a shard may ask)
  and `shard_loop::park_spin_loop` (in the loop: no spin asked, none made; a fixed spin running out and the park
  after it, or finding a wake at once with no park; a sized spin counting each park's gap and learning a park's
  CPU where the shard has a task clock). `rt::tests::g9_s4_the_nudge_s_period_is_the_quantum_less_the_timers_slack`,
  `g9_s4_the_spin_before_a_park_is_the_labs_to_ask_for`, `g9_s4_the_spin_is_said_as_the_shards_took_it`;
  `shard_health::tests::g9_s4_each_shards_spins_before_its_parks_are_on_metrics`; kiln-lab's
  `lab::tests::g9_s4_the_shards_spins_before_their_parks_are_reported_per_request`.
