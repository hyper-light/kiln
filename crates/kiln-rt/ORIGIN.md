# kiln-rt: where it comes from, and its rules

kiln's runtime: its own copy of hyper-raft's `hyper-rt`, under that package name, so that every crate
kiln builds that names `hyper-rt` builds on it: kiln's own (`crates/kiln`, `crates/kiln-http`) and the
vendored ones (Focal's bus, hyper-raft's `hyper-block` and `hyper-rt-transport`, Mantle's crates). It is
kiln's code: kiln changes it as kiln needs (how it schedules work, first), under the rules below, without
touching the copy Focal, Mantle and Quiver share.

## Where it comes from

- **Upstream:** https://github.com/hyper-light/hyper-raft, `crates/hyper-rt` at commit
  `0856e05df3d136f0870a98808113294a73399206` (2026-11-02), MIT (`vendor/hyper-raft/LICENSE`): the
  revision kiln vendors (`vendor/MANIFEST.toml`, `[projects.hyper-raft]`; `vendor/hyper-raft/UPSTREAM.md`).
- **Taken:** copied from `vendor/hyper-raft/crates/hyper-rt` at kiln's `2a2d93f6df5` (2026-11-03), which
  holds that commit's crate with no recorded change of kiln's (kiln's goal 2 of the API runtime program,
  plan `01a1a59c2a7a`).
- **Upstream's own history**, from slates' runtime on, is its ORIGIN.md, kept below as it was.
- Comments name upstream's documents and scripts by upstream's paths (`docs/runtime.md`,
  `docs/sim.md`, `CLAUDE.md`, `scripts/check-contracts.py`): they are hyper-raft's, at that commit, not
  kiln's.

## How kiln builds on it

- kiln's root `Cargo.toml` points the name `hyper-rt` here (`[patch.crates-io]`), for every crate that
  takes it by name.
- A crate that takes it by path can't be patched, and two path packages named `hyper-rt` 0.1.0 can't
  share a lock file, so kiln's recorded changes point those paths here: hyper-raft's `hyper-block` and
  `hyper-rt-transport` (`vendor/patches/hyper-raft/0002-*`) and Mantle's crates
  (`vendor/patches/mantle/0003-*`), each marked `Kiln-only:`, as no product's tree holds this crate
  (`docs/vendoring.md`, "Taken as kiln's own"); `bench/http` and `bench/kilndb` patch the name here too.
- `cargo tree -i hyper-rt` shows one `hyper-rt`, from `crates/kiln-rt`. `vendor/hyper-raft/crates/hyper-rt`
  stays upstream's, which no crate kiln builds takes: the base a diff against upstream starts from.

## What differs from upstream

Behavior: upstream's, and kiln's own additions: the compute pool and an observer of each pool's jobs (goal 3),
turns by time, a watchdog and task classes (goal 6, plan `01a1a59c2abe`, "Shards that protect themselves"), and what
held a shard past 100 µs by spawn site and label, and how much of it the host held, preempted (goal 9, plan `01a1a87b2cec`). `src/`,
`tests/` and `benches/` are upstream's but for the rows below; `diff -r vendor/hyper-raft/crates/hyper-rt
crates/kiln-rt` shows each row.

| File | Change | Why |
|---|---|---|
| `Cargo.toml` | hyper-raft's workspace values written out as inheriting them gave (version 0.1.0, edition 2024, rust-version 1.98.1, each dependency's version and features, the lint table); license and repository from kiln's workspace; `hyper-measure` by name, which kiln's `[patch.crates-io]` points at `vendor/hyper-raft/crates/hyper-measure` | a member of kiln's workspace inherits kiln's values: version 0.2.0, kiln's lint table, no `cfg(loom)` among the expected cfgs |
| `clippy.toml` | new: hyper-raft's root `clippy.toml`, less raft-proto's accessors and the no-`Arc` entry, with parking_lot's locks refused | clippy reads the `clippy.toml` nearest a crate's manifest, and hyper-raft's root isn't in kiln; the lockless rule below |
| `src/sync/mod.rs` | `Semaphore::try_acquire` and `Acquire::poll` call `AtomicU64::try_update` where upstream calls `fetch_update` | Rust 1.99 deprecates `fetch_update`, renamed `try_update`, and kiln's clippy runs `-D warnings` on stable (1.99.1); upstream pins 1.98.1, which has both names (Mantle builds kiln-rt on it). The same function under its new name: no behavior changes. Upstream's to take when it moves past 1.98 |
| `src/compute.rs`, `src/prioritysys.rs`, `tests/compute.rs`, `tests/compute_beside.rs`, `tests/compute_wait.rs`, `benches/compute_priority.rs`, `benches/compute_wait.rs` | new: the compute pool, a small work-stealing pool for CPU-heavy jobs (snapshots, serialization, compression, diffs, hashing), as many threads as the CPU the process is granted less its shards, below the shards at a rank that keeps a share of the CPU against other work in the cgroup (Linux `SCHED_BATCH` at nice +10, macOS utility QoS, Windows below normal), jobs in pieces of a configured time that give their thread to a waiting job, results by one-shot; and `compute::prefer_current_thread`, a shard's request for an EEVDF slice of its step (Linux 6.12+), so it preempts a pool thread it shares a CPU with. Each chosen by A/B in the benches: `SCHED_IDLE` and background QoS keep a shard calm but left jobs a request waits on waiting seconds beside other work; nice and batch alone left a shard waiting up to a slice. `prioritysys.rs` is the OS-interface file, every `unsafe` block with its `SAFETY` line | kiln's departure (goal 3 of the API runtime program, plan `01a1a59c2a8e`): heavy work off the shards, without taking the blocking pool's places. Stays here |
| `src/blocking.rs`, `src/compute.rs` (`observe`), `tests/pools_observed.rs` | each pool tells an observer its consumer sets once: the blocking pool, each job's share, its wait for one of the share's places and its run, as the job ends (reading the clock only when observed); the compute pool, each job's wait for a thread and each piece's time | kiln's departure (goal 3 of the API runtime program, plan `01a1a59c2a8e` steps 3 and 4): kiln shows on `/metrics` where its heavy work went and how long its pieces held a thread, and sizes the blocking pool's shares from what their calls do (Little's law). Stays here |
| `src/machine/facts.rs` | new `cpu_grant()`, with each platform's `allowed_count` | the compute pool's size: the cores the process may run on and its cgroup's CPU budget, without `Facts::query`'s reads of every core's class and caches. Stays here |
| `src/lib.rs` | `pub mod compute;`, `mod prioritysys;`, and a line of the module list | the compute pool |
| `Cargo.toml` (dependencies, benches) | `crossbeam-deque`; the `compute_priority` bench | the compute pool's queues: Chase and Lev's deque and a lock-free injector, no lock on any path, already in kiln's lock file through rayon |
| `ORIGIN.md` | this file, upstream's below | |
| `src/coop.rs` (new), `src/shard.rs` (`charge`, `wanted_untimed`, `coop_spent`, `coop_charged`), the leaves' polls (`futures.rs`'s `Sleep` and `Join`, `sync/mod.rs`'s receives, sends, notifications, watches and permits, `readiness.rs`'s `Ready`, `tcp/mod.rs`'s and `local.rs`'s accepts, reads and writes, `udp/mod.rs`'s send and receive, `signal.rs`'s `Recv`), `shard_loop.rs` (`coop_spent` and `coop_charged` cleared as each poll begins; `Counters::coop_charges`, `coop_yields`) | a task's turn by time with no hand-placed yields: each operation of a leaf charges the polling task, and past its share (the step quantum) with anything else wanting the shard the leaf answers `Pending`, its task woken, so the task yields at its next await; each poll's first charge goes on (each poll makes one operation), and a charge under another waker than the task's own counts but never refuses; a readiness wait is charged only as it goes on, once fired, never as it registers or waits (refused there, it woke its task at once and spent the poll's first charge on no operation, so the leaf after it was refused at every poll: the front door's pipe test spun a shard so, `tests/readiness_waits.rs`); `consume_budget` for a long computation. A simulated shard never yields by budget | kiln's goal 6, step 1: upstream yields only where code asks (`maybe_yield`), five places in kiln; Tokio's leaves yield on a budget (research `01a1a59c2abf`, f2), Seastar's tasks on a time quota (f7). `tests/coop_budget.rs` |
| `src/watch.rs` (new), `src/registry.rs` (`Entry::watch`, `board`, `movable`, `spawned_at`, `os_thread`; `push_movable`, `shard_runnable`, `location`), `src/runtime.rs` (the `Watchdog` a `Runtime` starts and stops; `submit_movable`), `src/thread_clock.rs` (`os_thread`, `runnable`), `src/shard_loop.rs` (`Watch::begin` and `end` around each poll; the movable spawns drained with control), `src/shard.rs`, `src/task.rs`, `src/futures.rs`, `src/sim.rs` (`#[track_caller]` at every spawn, the site recorded in the slot) | a watchdog thread per runtime: a shard held in one poll past `batch` × its quantum is marked (`watch::stalled`) and reported, rate-limited to 5 a minute a shard, with its task, its spawn site and whose the time was; movable spawns waiting for it go to another shard | kiln's goal 6, step 2: Seastar's stall detector and Go's sysmon (f8, f13); no backtrace, as `watch.rs` says why. `tests/watchdog.rs` |
| `src/queue.rs` (classes' lists, `listed_ns`), `src/shard.rs` (a spawn takes its spawner's class; listings stamped), `src/shard_loop.rs` (each poll's time charged to its class and its wait counted, `Counters::queue_ns_*`, `queue_polls_*`; inputs read once a budget while classes are kept; the batch takes its next task with the quantum as the lead), `src/coop.rs` (`Class`, `set_class`, `class`, `outranked`, `give_way`) | task classes: Critical, Normal (the default) and Background, each with work given an equal share of the shard's time and the most urgent first while it has run no more than the shard's quantum past the least-run class with work (`LocalQueue::pop_within`, the lead a poll's turn: its lag so bounded, the shares equal over time); a class that starts listing is raised to the shard's virtual time (no credit for idling, no debt for time run alone), and a poll is charged to its class at most the watchdog's limit; a shard whose tasks take no class runs Normal's lists alone, as before; a task that lowered its class within a poll gives way to a more urgent one waiting (`give_way`, `LocalQueue::waiting_above`: kiln's connections read each head as Critical). Each class's queueing delay counted | kiln's goal 6, steps 3 and 4: Seastar's scheduling groups and Google's criticalities (f10, f16). `tests/classes.rs`, `queue.rs`'s tests |
| `src/blocking.rs` (`Occupancy`, `occupancy`), `src/compute.rs` (`occupancy`), `tests/pools_observed.rs` | how full a share or the compute pool is now: its places (a share's limit, the pool's threads) and the jobs it holds, running or waiting, read from the counts each already keeps for its bound (one atomic read, no lock) | kiln's goal 6, step 4: kiln sheds requests when work waits in the pools past their calm, and an observer hears of a job's wait only as it starts or ends, so a pool whose jobs all wait tells nothing; this tells a pool with nothing waiting from one whose waits aren't told yet |
| `src/blocking.rs` (`Line`; `Message::Job`'s `critical`, read from `coop::class` as `run` submits) | a share's line keeps two kinds of job: those submitted from a Critical task wait ahead of the rest, each kind in the order given, both held to the share's places | kiln's goal 6, step 3: a person's read waited behind every job before it in its share's line, first come, first served whatever its class (kiln's census, train 8: custody's reads held the `store` share at about 14 waiting, 80 ms each); the run queue already takes Critical first. `blocking.rs`'s tests |
| `src/slow.rs` (new), `src/registry.rs` (`Entry::slow`), `src/shard.rs` (`label`), `src/shard_loop.rs` (`label` cleared as each poll begins; `note_slow` after a poll past `SLOW_POLL_NS`), `src/futures.rs` (`label`), `src/lib.rs` | what held a shard: each poll past 100 µs counted against its task's spawn site and the label its code set (a request's route), with the time it held the shard, in a table each shard keeps on its registry entry, which only the shard writes and any thread reads without a lock (`slow_polls`) | kiln's goal 9, plan `01a1a87b2cec` step 2: the watchdog names a task only past a step's worth of polls (16 ms on kiln's server), and the long-poll counters say whose time it was but not whose code; a shard's delay at its tail is made of the shorter holds |
| `src/slab.rs` (new), `src/lib.rs` (`mod slab;`), `src/shard.rs` (the task, timer and wait cells in `Slab`s, made by `make_task`, `make_timer` and `make_wait` as a free stack runs dry; `generation_base`), `src/shard_loop.rs` (`Core::tasks` grown to the slots made, by `slot_mut`, in `Shard::build` and where a slot is installed), `src/timer.rs` (the wheel's entries grown as timers are first taken), `src/cells.rs` (`CellRing` wraps at its cells made and grows as a `VecDeque` does, `CellStack` makes its cells as it first holds that many; `CellRing::each`), `src/queue.rs` (each slot's place made as the queue first sees it; its lists are `CellRing`s), `src/interests.rs` (the handles' table starts at two slots and doubles as it first holds more, within the size `bound` takes), `tests/desk_on_demand.rs`, `benches/desk.rs`, `Cargo.toml` (the `desk` bench) | a shard's desk made as it is used: room for the bound is allocated at the build and not written (`Slab`: the operating system gives pages as cells are made, at its high-water mark), so a runtime's memory follows the most it has held, not its bound, and the bound stays a bound. `slab.rs` is the one new `unsafe`, each block with its `SAFETY` line: a cell is written once, at the high-water mark no reference has reached, on the desk's own thread. The handles' table allocates at its growths (at most the log of its largest size, over a shard's life; none on a fire or a wake), a refusal of the allocator refused as `Capacity` | kiln's goal 11 (plan `01a1a87b2e56`, "A tenth of the memory"): a shard wrote its desk for its whole bound as it started (kiln's server: 8,192 tasks, 16,384 timers and waits a shard), whatever it held, and kiln's idle server held at most 68 tasks a shard (`kiln_shard_tasks_most`). `tests/desk_on_demand.rs` shows it (resident size at the start, after a burst and after the same burst again); `benches/desk.rs` measures a spawn on fresh and given-back slots and a yield, for the A/B against the build before (the cells' part, 2c766b6d99c: given-back slots 55.0 to 55.9 ns a spawn against 54.4 to 55.6 before; a fresh slot's first spawn about 100 ns against 77, its page made at first use rather than at the start). Stays here |
| `src/compute.rs` (`realtime_current_thread`, `REALTIME_LOWEST`, `Priority::Realtime`), `src/prioritysys.rs` (each platform's `realtime`; Linux's `of_current` reads a real-time class) | a shard's request for Linux's round-robin real-time class at the lowest real-time priority, with the reset-on-fork flag so nothing it starts inherits it, refused where the thread has no `CAP_SYS_NICE` or `RLIMIT_RTPRIO` room (and on macOS and Windows, which keep no class kiln asks for: each `realtime` says why); and a thread's priority read back as a real-time class, which ranks above every other policy | kiln's departure (goal 8 of the API runtime program, plan `01a1a59c2b29` step 7; goal 9, plan `01a1a87b2cec` step 4): under Linux's lazy preemption a normal thread waking beside one busy in user space waits for the next tick, and a real-time one doesn't (research `01a1a87b2cee` f17). Stays here |
| `src/task_clock.rs` (new), `src/lib.rs` (`mod task_clock;`), `src/shard_loop.rs` (`Core::task_clock`, opened as the shard is built on its thread; read as each poll starts and after a poll past `SLOW_POLL_NS`), `src/slow.rs` (`SlowTable::note_with_cpu`; `SlowPoll::cpu_polls`, `cpu_held_ns`, `cpu_ns`, `longest_cpu_ns`) | each slow poll's time on the CPU beside its hold: the shard thread's run time from a self-monitoring perf event's page (`PERF_COUNT_SW_TASK_CLOCK`, the page's enabled time and the CPU's counter scaled since the kernel wrote it, under its sequence count: no system call), Linux on x86-64 and arm64 only, none where the event can't be opened or the page offers no user time; `task_clock.rs` is the OS-interface file, every `unsafe` block with its `SAFETY` line | kiln's departure (goal 9, plan `01a1a87b2cec` step 3): a poll's wall time counts the host's preemptions with kiln's code, and the attribution's readings, a call each, are too dear for every poll, so on a shared machine they left 96% of the long polls unattributed; what ran on the CPU is the code's. Stays here |
| `src/tcpsys.rs` (`CongestionName`, `CongestionError`, `CongestionRefusal`, `TcpInfo`; each arm's `set_congestion`, `congestion`, `set_send_buffer`, `send_buffer`, `set_recv_buffer`, `recv_buffer`, `tcp_info`), `src/tcp/mod.rs` (`TcpStream`'s and `TcpListener`'s methods of those names), `tests/tcp_streams.rs` (`bp_g3_s1_*`) | per-socket congestion control (Linux `TCP_CONGESTION`, set and read back on a stream or a listener, whose accepted connections inherit it; macOS and Windows not offered), `SO_SNDBUF` and `SO_RCVBUF` on a stream (all three), and the connection's own reading: unsent and in-flight bytes, minimum and smoothed RTT, delivery rate, window, segment size, retransmissions and the algorithm in use (Linux `TCP_INFO` and `SIOCOUTQ`, macOS `TCP_CONNECTION_INFO`, Windows `SIO_TCP_INFO` version 0), each field `None` where the platform doesn't give it. A refused algorithm is an error naming it and why (`EPERM`: not in `tcp_allowed_congestion_control`; `ENOENT`: not loaded), and the socket keeps its own | kiln's departure (goal 3 of the browser protocol program, plan `01a1a832eb2f711f9c6f51d88effe89d` step 1): the protocol's socket sizes its low-water mark from the path it measures and asks for BBR where the host allows it. Stays here |
| `src/shard_loop.rs` (`Reading`, `Shard::step_after`, `park_owned_then`; `run`, `left_nothing`; `wait_ended`; `inputs_pending`, `inputs_pending_at`, `read_inputs`), `src/runtime.rs` (`LocalRuntime::block_on`'s loop), `src/timer.rs` (`Wheel::advance`, `next_event`, `scans`), `src/queue.rs` (`ready_classes`, `yielded_classes`), `src/registry.rs` (`Pulse::record_steps` and the other figures' recorders), `src/shard.rs` (`claim` records its spawn on the pulse), `src/shard_loop/turn_costs.rs` | what the loop did per turn that it needn't: a step records only its count on the pulse, each other figure recorded where it's counted (each step copied five, from two lines of counters a turn otherwise leaves alone); the run queue keeps two bits a class (a slot ready, a slot yielded), set from the class's lists after each change, so a pop with one class ready, a charge, a class's start, a step's release and every emptiness test read no other class's lists (with classes kept, each read all three, two in a heap block of their own); a wait's return is read once, for the park's length, the driver's retrieval age and the next step's clock, and a busy step's last poll end is the next step's clock where the shard carries no counter (loops that take their turns back to back only: `run`, `block_on`; `step` and `park` read afresh), so a park cycle reads the clock four times where it read seven; a wait's end reads the clock only for an attribution window; an idle turn of a shard with readiness waits reads no input none of which is pending (it set `harvest_owed` and read them all, examining its wheel and asking its driver twice); the wheel keeps its next tick with work between changes to a slot's occupancy and examines no tick without work, the target's included; in `run` and `block_on`, a step that did work and left nothing for the next to find (no task ready, no input pending as of its last reading) goes on as after an idle step, to the park, so a wake from a park takes one step where it took two (`block_on` checks its root's output first, so a root that ended in that step returns at once) | kiln's goal 9, plan `01a1a87b2cec` step 5 ("a tenth of the CPU"): in rt7's profile the loop itself was 12% of the shards' user time, about half its steps the idle one before each park (44.6 steps to 22.9 parks a request in g9p16); on x86-64 each clock read is a vDSO call. Stays here |
| `src/park_spin.rs` (new), `src/lib.rs` (`pub mod park_spin;`), `src/shard_loop.rs` (`Core::park_spin`; `spin_before_park` and `take_park_spin` in `run`'s idle path, after the activity spin; `park_announced` notes each park's gap and, for a spin that sizes itself, the park's CPU on the task clock; `Counters::park_spins`, `park_spin_hits`, `park_spin_ns`, `park_spin_now_ns`), `src/shard_loop/park_spin_loop.rs` (tests) | a spin before each park that a task on the shard asks for (`park_spin::ask_current_thread`, a thread-local as the nudge is): fixed, or sized each window of 385 idle moments to the one of 0 and the park bounds up to its cap that would have spent the least CPU on them, a caught gap costing the spin to it and a missed one the spin and a park, whose CPU the shard learns from its own parks and its senders' kicks; a kicked park's gap runs to the kick's stamp (`Parking::time_wakes`, turned on by a sizing spin), not to the park's return. Off unless asked, as before | kiln's goal 9, plan `01a1a87b2cec` step 4: under Linux's lazy preemption a shard woken from another CPU waits for that CPU's next interrupt behind a busy thread, and a shard still spinning when its work comes isn't woken at all; sized so the spin spends less CPU than parking wherever it spins (goal 9 step 5's busy share). Stays here |
| `src/timer.rs` (`Wheel::next_deadline_ns`, `Next`, `scan_next_event`) | the wheel's next deadline is its armed timers' earliest deadline, found in the same pass as its next tick with work: a coarser level's from its first occupied slot's entries, bounded by its next occupied slot's boundary; `advance` still moves through every boundary on its way, so a timer cascades at its boundary whenever the wheel next advances past it. Upstream's next deadline was that next tick with work, a slot's boundary among them | kiln's goal 9, plan `01a1a87b2cec` step 5 ("a tenth of the CPU"): a shard parks until the wheel's next deadline, so a timer of a coarser level woke its parked shard at its slot's boundary only to move it a level down, then again at its deadline: the server's shards' probe (17.07 ms, level 1 on the 100 µs tick) twice a period, a sleep of seconds two or three times (rt9's calm rounds: 7.4 timer wakes a request against the probes' 2.3). Stays here |
| `src/quiet.rs` (new), `src/quiet/tests.rs`, `src/lib.rs` (`pub mod quiet;`), `src/futures.rs` (`sleep_quiet`, `SleepQuiet`, `Came`), `src/shard.rs` (`ShardContext::quiet`, `last_wake`), `src/shard_loop.rs` (`refresh_due` takes a quiet sleep's due time; `fire_quiet` in `read_inputs`; `quiet_deadline` in `park_with_cleanup`, `awake_deadline` for `run`'s spins; `note_last_wake` after each park's wait, `Core::last_by` from `wait_in_driver`), `src/parking.rs` (`Parking::stamp_wakes`; `costed`: a timed kick counts its CPU only under `time_wakes`) | a quiet sleep (`sleep_quiet(span, quiet)`, one a shard): due `span` after its first poll; running when it falls due, it ends at the next step as a timer would; parked, the park doesn't end for it until `quiet` after the park began, so it ends at the step after the shard's next wake, or by its own deadline after `quiet` with no wake at all; it says how its turn came (`Came`: running, woken by work, or quiet), timed to its poll from its due time, from its wake's start where the shard knows it (a kick's sender's stamp, another timer's deadline), or from its own deadline | kiln's departure (goal 9, plan `01a1a87b2cec` step 5, Ada's word of 2026-11-06): kiln's shards' delay probe woke each parked shard every 17.07 ms on a timer to time its wake, 234 of the ~457 timer wakes a second of kiln's lab (g9p16), each a park and a wake its busy share counts; a shard woken by work within the period has a wake to time already. Stays here |
| `src/timer_sites.rs` (new), `src/lib.rs` (`pub mod timer_sites;`), `src/registry.rs` (`Entry::timer_sites`), `src/shard.rs` (`TimerCell::site`; `ShardContext::arm_timer_from`, `arm_timer` its caller's), `src/futures.rs` (`sleep` and `within` take their caller's place, `#[track_caller]`; `sleep_from` for a wrapper's), `src/quiet.rs` (a quiet sleep's site), `src/shard_loop.rs` (`note_timer_wake`: a park a timer ended counted against the first timer to fall due in the step after it, none where none had; a quiet sleep's wake against its own) | each shard's timer wakes by where the timer was asked for, in a table on its registry entry that only the shard writes and any thread reads without a lock (`timer_wakes`), as `slow`'s; and the kicks that found it parked by where the task each woke was spawned (`kicked_tasks`, written by the kicks' senders, `registry`'s `kick_parked`) | kiln's goal 9, plan `01a1a87b2cec` step 5: the shards' timer wakes were ~457 a second in kiln's lab (g9p16), 234 of them its delay probe's; to cut the rest, what armed them. Stays here |
| `src/parking.rs` (`KickFrom`, `kick_if_parked_from`, `kicks_from`; `kicks_skipped` and `kicks_claimed` a padded count per sender; `mark_thread`, `sender`), `src/registry.rs` (the registry's kicks by their sender; `kicks_from`), `src/compute.rs`, `src/blocking.rs` (each pool's threads say whose they are) | the kicks sent each shard, by who sent them (a thread running a shard, a compute or blocking pool's worker, any other thread) and whether each found it parked (claimed, a system call) or awake (saved), its total saved as before | kiln's goal 9, plan `01a1a87b2cec` step 5: the shards' ~1,750 kicks a second in kiln's lab (g9p16), to cut those not needed. Stays here |
| `src/futures.rs` (`spent`, `Spent`, `Spent::cost_ns`), `src/futures/spent_tests.rs` | what a piece of work spent on the thread that ran it: its wall time, its time on the CPU (the shard's task clock, else the OS's clock for the thread) and, on Linux, whether it waited inside a call (the thread's voluntary switches); its cost, its CPU time unless it waited or that isn't known | kiln's goal 9, plan 01a1a87b2cec step 5: kiln's estimates of where work runs learned wall times, so on a loaded host 37-55 µs reads of runners' frames went to the compute pool, their callers waiting 218-363 µs. Stays here |
| `src/futures.rs` (`poll_clock`, `PollClock`) | the poll a shard's thread is in, on the shard's clock: when it began (the loop's published `now_ns`, the same within a poll) and now, so a task's code weighs its next piece against what's left of the step's quantum, however it came to it | kiln's goal 9, plan 01a1a87b2cec step 3: kiln's HTTP connections wrote an answer in the poll its handler had spent, past the quantum (the agent sessions' reads, 0.94 ms on the CPU in g9p35). Stays here |
| `src/idle.rs` (`when_spare`, `spare`), `src/compute.rs` (`Pool::serve`) | a compute pool worker that finds no job takes one piece of the process's spare work, if it set one, before it rests, and looks for jobs again after it | kiln's consts-audit R3 (Ada, 2026-11-06: no arbitrary constants): runners' payloads looked at for fields this build doesn't know with spare capacity alone, no thread of their own sleeping 100 ms at a time and no wake for one. Stays here |

## The rules

kiln-rt keeps its upstream's lint wall (the lint table in `Cargo.toml`, `clippy.toml`) and kiln's rules
(`docs/ci.md` §0), and these:

1. **Lockless.** No `Mutex`, `RwLock` or `Condvar`, std's, parking_lot's or any crate's. State is
   reached in one of four ways: owned by one shard or task, and reached by a message to its owner;
   moved over a bounded channel or ring; an atomic, its orderings argued where it is used; or an
   immutable snapshot, published whole and replaced, never changed in place, so no reader waits.
   `Arc` is allowed where a task that moves between shards needs one, and nowhere else; `Rc` is not.
   - **The check:** `clippy.toml`'s `disallowed-types` refuses std's and parking_lot's locks (and `Rc`),
     denied by the lint table (`disallowed_types = "deny"`), in kiln's clippy on every platform
     (`ci/gate.sh`, `.kiln/ci.yaml`'s rust job). parking_lot's entries name nothing until it is a
     dependency (`allow-invalid`); a lock of another crate is a review failure. `Arc` isn't on the list,
     so that an `Arc` never needs an allow of the lint that refuses locks: an allow of
     `clippy::disallowed_types` is never right in shipped code. Tests keep upstream's one exception, a
     harness lock that serializes a file's tests (`tests/lifecycle.rs`, `tests/reclaim.rs`), allowed at
     its site, and loom's `Arc` in its models.
2. **No panics.** Shipped code returns typed errors (`RtError`, `SyncError`): `unwrap`, `expect`,
   `panic!`, `todo!`, `unimplemented!`, `unreachable!`, the assert family, indexing and slicing,
   unchecked arithmetic and narrowing casts are denied by the lint table. Tests opt out at their crate
   root.
3. **No picked constants.** Every constant is derived, from what the runtime measures (its calibration,
   upstream's `docs/runtime.md` §10: the step budget, the timer's tick, spinning by park cost) or from
   other constants, or cited where it is defined: a paper, an operating system's documentation, or a
   measurement recorded with it (its test or benchmark, the commit that set it). A value that can be
   neither is configuration (`RuntimeConfig`), its default derived. `clippy.toml`'s shape threshold only
   ratchets down.
4. **Fixes flow upstream.** hyper-raft's `hyper-rt` is kiln-rt's upstream, and Focal, Mantle and Quiver
   build on it.
   - A fix to code kiln-rt still shares with upstream (a race, a lost wake, a platform's quirk, a path
     that panics) is made here with its test, then offered to hyper-raft as a branch with that test, on
     the person's word (`docs/vendoring.md`, "The person's word"). Its row in the table above names the
     branch, and goes once a revision kiln takes has the fix.
   - A change for kiln's own departures (tasks that move between shards, yields by time, a watchdog)
     stays here, with its row and why.
   - Upstream's fixes come here by hand. A re-vendor of hyper-raft (`scripts/vendor.py hyper-raft`)
     moves `vendor/hyper-raft`, not this crate: diff upstream's `crates/hyper-rt` from the commit named
     above to the new one, port what applies, and name both commits here.

## Upstream's ORIGIN.md (hyper-raft at `0856e05`)

slates' runtime, taken at slates `6b9ce5c` (2026-10-05) and designed onward for all three consumers
(docs/runtime.md). slates was not changed; it moves onto this crate on its own schedule.

### What was taken

| Here | From slates | What changed in the port |
|---|---|---|
| `src/*.rs` (runtime, shard, registry, parking, task, queue, timer, driver, kqueue, epoll, iocp, afd, netsys, udp, tcp, readiness, futures, sim, attribution, thread_clock, waker, control, error) | `crates/rt/src` | Paths renamed (`slates_mem` → `crate::mem`, `slates_machine` → `crate::machine`). `uring.rs` not taken: its io_uring driver runs in readiness mode only, a second path to epoll (docs/runtime.md §3.7). `RuntimeConfig::from_profile` became `from_calibration` (below). `UdpSocket::dont_fragment` added, so the don't-fragment tests read the option through the crate instead of an `unsafe` call of their own. |
| `src/mem/{error,handle,loom_bounds,mpsc,ring,segmented,slab}.rs` | `crates/mem/src` | Only what the runtime uses. The arenas, the buddy allocator, budgets, locked and shared regions and pre-faulting are slates' storage, not a runtime, and are not taken: the runtime locks and pre-faults nothing. |
| `src/machine/{bench,clock,derived,error,facts,placement,stats,wake}.rs` | `crates/machine/src` | `serde` derives and the JSON profile removed; the BLAKE3 identity hash removed (a consumer keys its stored calibration as it chooses, docs/runtime.md §10.2). `Placement::of` takes the consumer's reserved cores in place of slates' fixed one for control and the OS (§10.3); the wake probe's waker runs on the first reserved core, or a sibling shard's when none is reserved. |
| `src/machine/probes.rs` | `crates/machine/src/probes.rs` | Only the system-call probe and thread pinning. The fault, memcpy, hash, codec, lock-capacity and core-matrix probes size slates' storage and are not taken. |
| `src/machine/calibration.rs` | new, in place of slates' `profile.rs` | The three measurements the runtime needs and the constants derived from them under the consumer's policy (docs/runtime.md §10). |
| `tests/*.rs` | `crates/rt/tests` | Paths renamed. `timer_allocations.rs` counts through hyper-measure's counting allocator; slates' own check that its allocator does not attribute another thread's allocations is hyper-measure's to test, and is not carried. The `rt_bench` example and the callgrind bench are not taken; docs/runtime.md §12's benchmarks replace them. |

### The suite at the port

`cargo test -p hyper-rt` on macOS arm64, Rust 1.98.0, before any of docs/runtime.md's departures:
91 unit tests, 70 integration tests in 22 files, 6 doctests, all passing. The departures are made
against this suite.

| `src/udp/linux.rs`, `src/udp/macos.rs`, `src/udp/batched.rs` | hyper-tokio `src/sys/{linux,macos}.rs`, `src/socket.rs`, `src/clock.rs` | The batched calls and kernel stamps, moved so hyper-tokio and hyper-rt share one layer (docs/runtime.md §5.1, §14). `Batched` awaits hyper-rt's readiness instead of tokio's reactor, so a receive always asks the kernel (no `Through::Reactor`). Stamps land on the shard's clock by their age; macOS reads the age on `mach_absolute_time` (`Clock::now_ticks`), the stamp, in the kernel's ticks, carried into the process's at the kernel's rate first (`machine::clock::MachTicks`: a process Rosetta 2 translates is shown a timebase of 1/1 against the stamp's 24 MHz, docs/runtime.md §5.1). Astray reports are a seam outcome; EIO from a segmented send resends unsegmented instead of dropping. |

### Mantle's runtime work, taken here (2026-10-12)

Made in mantle's vendored copy (by the owner's direction and by Codex) and taken here from mantle's
commits as they were (`cf6e37c`, `6f05bd8`, `3743b25`, `f797bd9`, `bfdf9c2`, `bb0779e`, `bd7c0fd`,
replayed onto `8cd3604`, the revision mantle took, and merged as mantle merged them at `be8d04d`), then
merged with `main`:
- cooperative service admission and cancellation ownership (`spawn_service`; a service's cancellation
  is a level it reads, its future kept until its cleanup returns), nonblocking manual-step and readiness
  progression, typed driver-loss cleanup, context checks before each synchronization receive poll,
  checked channel slot and ticket layouts before construction, and cold, runtime-owned, bounded native
  retirement groups (`prepare_retirement`, `prepare_retirement_with_original`); TCP serving metadata
  owned from the first shard claim, accept futures keeping their guards across refused admission and
  cancellation (`docs/runtime.md` §4, §8);
- the loop under load: one kick a park, a tracking shard spinning as long as blocking costs it in CPU,
  the cheap clock read once a poll, and channels that hand values over with no lock (`docs/runtime.md`
  §3.9, mantle's `docs/design/event-loop.md` D1–D4);
- a step's poll budget covering the wakes its own polls make (hyper-raft PR #16, `ca14726`) and a
  thread's CPU account read only while attribution is armed (#15).

In the merge with `main`: the one-shot keeps mantle's lock-free state, with `main`'s
`OneshotSender::is_closed` read over it; a poll that unwound still returns its end, as mantle's step reads
it, and tells a waiting `block_on` its root unwound (`RtError::RootUnwound`, `main`'s); the service
unwind test now holds that a root's unwind ends its call refused, its peer service cancelled, released
and joined inside the call, where it held the panic passing out of `block_on`.

Made here with it, for `tests/service_resources.rs`, red in mantle since `6f05bd8`: the shard's control
queue is a ring held in its registry entry (`control.rs`, `docs/runtime.md` §8), not std's
`sync_channel`, whose lock on macOS is a pthread mutex boxed at its first lock, so a shard losing its
driver allocated as it closed the queue, inside the measured driver-loss round (backtraces:
`OnceBox::initialize` under `close_control` from `lose_driver`). The queue has no cell: hyper-rt's
channel there, the first try, put a call cycle through the registry (the entry's free dropping its
sender, whose drop wakes through the registry) that `scripts/check-recursion.py` refused on every
target (CI run 38282201776). The test's harness moved to hyper-rt's channels (std's also grew a blocked
receiver's waiter list, `mpmc::waker::Entry`); with both, no hot round allocates. One observer thread
serves every driver-loss round, as one serves every reuse round: a thread started per round began inside
the hot window it was started for, and its first stack pages counted (4 of 10 runs red, 1–2 faults; a
64 KiB stack touch at its start made it 20 of 20, 3–4 faults a round; with one observer 30 of 30 green,
and 20 of 20 with that touch).

Also made here with it, each found on CI or by widening a window the scheduler opens on a busy host:
- A receiver's drop closes its ring, a mark in the tail, so a send claims its place before the close or
  is refused `Closed` (`docs/runtime.md` §8). mantle's channel read a state bit before the push, and a
  receiver's drop could free a full ring's room between the read and the push (`tests/sync.rs`).
- `readiness_waits`' wait dropped on another shard takes the slot back once a later step has polled the
  task, where it took one yield for a control drain: a step polls a task that yields again within its
  batch (PR #16).
- `service_driverless_receipts` counted hyper-measure's Linux probe as the runtime's: the probe read
  `/proc` through `format!` and `read_to_string`; it reads into a buffer on the stack now.
- `service_resources`' observer yields once before any window: its loop yields only when the shard has
  not parked yet, and its first yield, in a random measured round, ran kernel32!SwitchToThread for the
  first time in the process, which Windows counts as a page fault (8 runs in 40 on windows-2025, the
  page named by Windows' working-set watch and cdb; 0 in 80 with the yield).

### Made here: long polls judged by step (2026-10-12)

- mantle's ranges bench found the shard reading its thread's CPU clock once at the end of every poll past
  the quantum and once at every step's start: a seek's poll ran a little past the 2.1–4.0 µs quantum, so
  727,739 reads in a million seeks, 2.9% of the shard's time in `__thread_selfusage`. From a long poll
  judged a run on the CPU, a step's long polls are judged together at one reading at its end
  (`attribution::Tracker::by_step`, `judge_in_window`): an overrun past the window's off-CPU time was
  spent on the CPU, the per-poll rule's verdict, and a window that leaves one undecided judges one at a
  time again. Reads fell to about one a step (8–9 times fewer), 0.5% of the shard's time
  (`docs/benchmarks.md`, "hyper-rt: long polls judged by step"). Tests: `tests/attribution_by_step.rs`
  (the reads beyond one a step paid for by undecided verdicts; it fails on the per-poll reading, 41
  reads for 32 long polls), two unit tests of the window and the modes; `tests/wake_estimate.rs`'s
  attribution counts unchanged.
