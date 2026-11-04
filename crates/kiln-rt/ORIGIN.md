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

Behavior: upstream's, and kiln's own additions: the compute pool (goal 3), and turns by time, a watchdog and
task classes (goal 6, plan `01a1a59c2abe`, "Shards that protect themselves"). `src/`, `tests/` and `benches/`
are upstream's but for the rows below; `diff -r vendor/hyper-raft/crates/hyper-rt crates/kiln-rt` shows each row.

| File | Change | Why |
|---|---|---|
| `Cargo.toml` | hyper-raft's workspace values written out as inheriting them gave (version 0.1.0, edition 2024, rust-version 1.98.1, each dependency's version and features, the lint table); license and repository from kiln's workspace; `hyper-measure` by name, which kiln's `[patch.crates-io]` points at `vendor/hyper-raft/crates/hyper-measure` | a member of kiln's workspace inherits kiln's values: version 0.2.0, kiln's lint table, no `cfg(loom)` among the expected cfgs |
| `clippy.toml` | new: hyper-raft's root `clippy.toml`, less raft-proto's accessors and the no-`Arc` entry, with parking_lot's locks refused | clippy reads the `clippy.toml` nearest a crate's manifest, and hyper-raft's root isn't in kiln; the lockless rule below |
| `src/sync/mod.rs` | `Semaphore::try_acquire` and `Acquire::poll` call `AtomicU64::try_update` where upstream calls `fetch_update` | Rust 1.99 deprecates `fetch_update`, renamed `try_update`, and kiln's clippy runs `-D warnings` on stable (1.99.1); upstream pins 1.98.1, which has both names (Mantle builds kiln-rt on it). The same function under its new name: no behavior changes. Upstream's to take when it moves past 1.98 |
| `src/compute.rs`, `src/prioritysys.rs`, `tests/compute.rs`, `tests/compute_beside.rs`, `tests/compute_wait.rs`, `benches/compute_priority.rs`, `benches/compute_wait.rs` | new: the compute pool, a small work-stealing pool for CPU-heavy jobs (snapshots, serialization, compression, diffs, hashing), as many threads as the CPU the process is granted less its shards, below the shards at a rank that keeps a share of the CPU against other work in the cgroup (Linux `SCHED_BATCH` at nice +10, macOS utility QoS, Windows below normal), jobs in pieces of a configured time that give their thread to a waiting job, results by one-shot; and `compute::prefer_current_thread`, a shard's request for an EEVDF slice of its step (Linux 6.12+), so it preempts a pool thread it shares a CPU with. Each chosen by A/B in the benches: `SCHED_IDLE` and background QoS keep a shard calm but left jobs a request waits on waiting seconds beside other work; nice and batch alone left a shard waiting up to a slice. `prioritysys.rs` is the OS-interface file, every `unsafe` block with its `SAFETY` line | kiln's departure (goal 3 of the API runtime program, plan `01a1a59c2a8e`): heavy work off the shards, without taking the blocking pool's places. Stays here |
| `src/machine/facts.rs` | new `cpu_grant()`, with each platform's `allowed_count` | the compute pool's size: the cores the process may run on and its cgroup's CPU budget, without `Facts::query`'s reads of every core's class and caches. Stays here |
| `src/lib.rs` | `pub mod compute;`, `mod prioritysys;`, and a line of the module list | the compute pool |
| `Cargo.toml` (dependencies, benches) | `crossbeam-deque`; the `compute_priority` bench | the compute pool's queues: Chase and Lev's deque and a lock-free injector, no lock on any path, already in kiln's lock file through rayon |
| `ORIGIN.md` | this file, upstream's below | |
| `src/coop.rs` (new), `src/shard.rs` (`charge`, `wanted_untimed`, `coop_spent`), the leaves' polls (`futures.rs`'s `Sleep` and `Join`, `sync/mod.rs`'s receives, sends, notifications, watches and permits, `readiness.rs`'s `Ready`, `tcp/mod.rs`'s and `local.rs`'s accepts, reads and writes, `udp/mod.rs`'s send and receive, `signal.rs`'s `Recv`), `shard_loop.rs` (`coop_spent` cleared as each poll begins; `Counters::coop_charges`, `coop_yields`) | a task's turn by time with no hand-placed yields: each operation of a leaf charges the polling task, and past its share (the step quantum) with anything else wanting the shard the leaf answers `Pending`, its task woken, so the task yields at its next await; `consume_budget` for a long computation. A simulated shard never yields by budget | kiln's goal 6, step 1: upstream yields only where code asks (`maybe_yield`), five places in kiln; Tokio's leaves yield on a budget (research `01a1a59c2abf`, f2), Seastar's tasks on a time quota (f7). `tests/coop_budget.rs` |
| `src/watch.rs` (new), `src/registry.rs` (`Entry::watch`, `board`, `movable`, `spawned_at`, `os_thread`; `push_movable`, `shard_runnable`, `location`), `src/runtime.rs` (the `Watchdog` a `Runtime` starts and stops; `submit_movable`), `src/thread_clock.rs` (`os_thread`, `runnable`), `src/shard_loop.rs` (`Watch::begin` and `end` around each poll; the movable spawns drained with control), `src/shard.rs`, `src/task.rs`, `src/futures.rs`, `src/sim.rs` (`#[track_caller]` at every spawn, the site recorded in the slot) | a watchdog thread per runtime: a shard held in one poll past `batch` × its quantum is marked (`watch::stalled`) and reported, rate-limited to 5 a minute a shard, with its task, its spawn site and whose the time was; movable spawns waiting for it go to another shard | kiln's goal 6, step 2: Seastar's stall detector and Go's sysmon (f8, f13); no backtrace, as `watch.rs` says why. `tests/watchdog.rs` |
| `src/queue.rs` (classes' lists, `listed_ns`), `src/shard.rs` (a spawn takes its spawner's class; listings stamped), `src/shard_loop.rs` (each poll's time charged to its class and its wait counted, `Counters::queue_ns_*`, `queue_polls_*`; inputs read once a budget while classes are kept), `src/coop.rs` (`Class`, `set_class`, `class`) | task classes: Critical, Normal (the default) and Background, each with work given an equal share of the shard's time and the most urgent first where they have run alike; a shard whose tasks take no class runs Normal's lists alone, as before. Each class's queueing delay counted | kiln's goal 6, steps 3 and 4: Seastar's scheduling groups and Google's criticalities (f10, f16). `tests/classes.rs`, `queue.rs`'s tests |

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
