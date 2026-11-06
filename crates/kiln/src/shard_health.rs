//! Each shard's health on `/metrics` (goal 1, plan 01a1a59c2a68 step 1): how late ready work runs on
//! it, how much of the time it's busy, and its slow polls by cause.
//!
//! A probe pinned to each shard ([`crate::rt::spawn_on`]) sleeps a period on the shard's timers and
//! notes, as it wakes, how late it ran: work that was ready waited that long behind whatever held the
//! shard (Node's `monitorEventLoopDelay` measures its loop the same way, sampling every 10 ms by
//! default: research 01a1a59c2a6a, f8). Each wake it also copies, on the shard itself, what hyper-rt
//! counts there (`hyper_rt::futures::shard_counters`: polls, polls that held the shard past twice its
//! step quantum and who held them) and the CPU time of the shard's thread, into atomics of its own.
//! `/metrics` reads only those atomics, from whatever thread serves it: reading never touches a shard,
//! takes no lock and asks nothing of the shard's loop.
//!
//! - **Delay** (`kiln_shard_delay_seconds`): how late each probe ran against when its timer fell
//!   due, in buckets fine enough to read to tens of microseconds below a millisecond ([`DELAY_BUCKETS`]).
//!   It includes up to one tick of the timer wheel (100 µs, `crate::rt::runtime_config`) and the
//!   shard thread's timer slack (its tick again) by design. A shard held for good stops probing:
//!   `kiln_shard_stalled_seconds` says how long since its probe last ran, before any late wake can be
//!   noted.
//! - **Busy** (`kiln_shard_busy_seconds_total`): the CPU time the shard's thread has run, so its rate
//!   is the share of the time the shard was on the CPU (Linux and macOS; Windows keeps a thread's
//!   times at the scheduler's tick, so no series there). Time the host held a ready shard off the
//!   CPU isn't busy: it shows in the delay, and as `host` slow polls. Read from hyper-rt's task clock
//!   with no system call where the shard has one (Linux), else the OS's clock for the thread
//!   ([`CpuClock`]; goal 9, plan 01a1a87b2cec step 5).
//! - **Slow polls** (`kiln_shard_slow_polls_total`, by `cause`): hyper-rt's long polls, each one that
//!   held its shard past twice the step quantum (`crate::rt::STEP_BUDGET_NS`, 266.7 µs, so past 533
//!   µs), attributed as the runtime does (`hyper_rt`'s `attribution`): `task` (it ran on the CPU),
//!   `blocking` (it waited inside a call), `host` (it was runnable and off the CPU: preempted),
//!   `unknown` (where the OS can't tell: off the CPU on macOS, every one on Windows).
//! - **Quantum** (`kiln_shard_quantum_seconds`): the step quantum each shard runs with, so what reads
//!   the slow polls and what held the shards can hold them to it (goal 9, plan 01a1a87b2cec step 3:
//!   no poll past the quantum).
//! - **Tasks** (`kiln_shard_tasks`, `kiln_shard_tasks_most`): the tasks each shard holds as its probe
//!   wakes (admitted and not yet ended, by hyper-rt's counts), and the most it has held so: what a shard's
//!   task slots, which it makes as it starts, are sized from (`crate::rt::Profile`; goal 11, plan
//!   01a1a87b2e56 step 3).
//! - **What held a shard** (`kiln_shard_held_*`, by `site` and `label`): every poll past 100 µs
//!   (`hyper_rt::slow::SLOW_POLL_NS`, goal 9, plan 01a1a87b2cec step 2), counted against the spawn
//!   site of its task and the label its code set (a request's route, `crate::route_latency::label_of`),
//!   as the shard keeps them (`hyper_rt::slow`): how many, their holds added up, and the longest; and of
//!   them, those the host held (preempted, as hyper-rt finds out for a poll past twice its step quantum),
//!   with their time: what no code held (steps 3 and 4).
//! - **What its parks, wakes and calls cost it** ([`Wakes`], [`Calls`]; goal 9, plan 01a1a87b2cec step 5, which
//!   the lab reports per request): its steps, its parks by what ended them (`kiln_shard_wakeups_total{by}`) and
//!   those withdrawn, the wakes it took in and those its thread made (`kiln_shard_wakes_sent_total{to}`), the kicks
//!   it sent and was saved, the threads it unparked; its readiness registrations, and its tasks' TCP accepts,
//!   reads and writes, with those that moved nothing (`kiln_shard_socket_calls_idle_total{call}`). Copied at the
//!   probe's first wake and every [`COUNTS_EVERY`]th after: these families move every 100 ms, the rest every wake.

use std::fmt::Write as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::metrics::Histogram;

/// How often each probe wakes, unless `KILN_SHARD_PROBE_MS` says (`0`: no probes): Node's default
/// sampling of its loop's delay (research 01a1a59c2a6a, f8).
pub const PERIOD: Duration = Duration::from_millis(10);

/// Every how many of its wakes a probe copies what its shard's parks, wakes and calls cost it ([`Wakes`], [`Calls`],
/// [`ParkTimes`]: 34 counts) to `/metrics`, its first wake among them (goal 9, plan 01a1a87b2cec step 5): every 10th,
/// so those families move every 100 ms at [`PERIOD`]. The lab reads them as differences across its window (kiln-lab's
/// `window_s`: 60 s, 10 s at the shortest), so a copy up to 100 ms old moves a window's count by 2 × 0.1 s / 10 s = 2%
/// at the most, 0.3% over the default. The delay, how late, the most late, the last wake, the CPU time and the tasks
/// stay each wake's own: goal 9's step 1 reads those at every wake.
pub const COUNTS_EVERY: u64 = 10;

/// How late a probe ran, in seconds (goal 9, plan 01a1a87b2cec step 1): a shard's ready work is to run within
/// a millisecond at p99.9 (Ada, 2026-11-05), so the buckets resolve that millisecond: 25 µs apart to 250 µs,
/// where a calm probe lands (the wheel's tick and the timer slack, up to 100 µs each by design), then 50 µs
/// apart to 1 ms, so a quantile read off them (linearly within a bucket, as `histogram_quantile` does) is
/// within 25 µs of the probe's own below 1 ms, a fortieth of the bar; then 250 µs apart to 2 ms and the
/// steps of the stall buckets (`crate::metrics::STALL_BUCKETS`) to a minute, so a stall of seconds still lands
/// in a bucket of its own.
pub const DELAY_BUCKETS: &[f64] = &[
    0.00001, 0.000025, 0.00005, 0.000075, 0.0001, 0.000125, 0.00015, 0.000175, 0.0002, 0.000225, 0.00025, 0.0003, 0.00035, 0.0004, 0.00045, 0.0005, 0.00055, 0.0006, 0.00065, 0.0007,
    0.00075, 0.0008, 0.00085, 0.0009, 0.00095, 0.001, 0.00125, 0.0015, 0.00175, 0.002, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 25.0, 60.0,
];

/// One shard's figures, written by its probe alone, read by `/metrics`. On a cache line of its own
/// (the largest we target, Apple silicon's 128 bytes, as hyper-rt's own pulse is): each probe writes
/// only its own.
#[repr(align(128))]
pub struct Shard {
    delay: Histogram,
    /// The latest probe: when it ran (nanoseconds since [`base`]), how late, and the most late yet.
    last_ns: AtomicU64,
    late_ns: AtomicU64,
    most_late_ns: AtomicU64,
    /// The shard thread's CPU time, nanoseconds (`u64::MAX`: not known on this OS).
    cpu_ns: AtomicU64,
    polls: AtomicU64,
    slow_task: AtomicU64,
    slow_blocking: AtomicU64,
    slow_host: AtomicU64,
    slow_unknown: AtomicU64,
    longest_poll_ns: AtomicU64,
    overrun_ns: AtomicU64,
    tasks: AtomicU64,
    most_tasks: AtomicU64,
    /// The shard's step quantum, nanoseconds, as its probe last read it (0: not yet).
    quantum_ns: AtomicU64,
    /// What its parks and wakes cost it ([`Wakes`]), as the probe last copied them.
    wakes: WakeCells,
    /// The system calls its thread made beside them ([`Calls`]), as the probe last copied them.
    calls: CallCells,
    /// How long its parks lasted ([`ParkTimes`]), as the probe last copied them.
    parks: ParkCells,
    /// What its spin before a park did ([`Spins`]), as the probe last copied it.
    spins: SpinCells,
}

/// What a shard's parks and wakes cost it, as hyper-rt counts them on the shard (goal 9, plan 01a1a87b2cec step
/// 5: the CPU a request costs the shards, per request in the lab's report). Each park that waits is a system call
/// and a sleep, and each kick a system call on the thread that sends it; a park withdrawn costs neither.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wakes {
    /// Loop steps.
    pub steps: u64,
    /// Parks that waited in the driver.
    pub parks: u64,
    /// Of those, the ones a kick ended (another thread's wake or message), I/O ended (a socket's readiness), and
    /// their deadline ended (a timer's).
    pub by_kick: u64,
    pub by_io: u64,
    pub by_timer: u64,
    /// Parks withdrawn, work already waiting.
    pub withdrawn: u64,
    /// Wakes from other threads the shard took in.
    pub taken: u64,
    /// Wakes its thread made through tasks' wakers: to its own tasks, and to other shards'.
    pub sent_own: u64,
    pub sent_other: u64,
    /// Kicks its thread made to parked shards, and kicks the shard's senders were saved (it was running).
    pub kicks_sent: u64,
    pub kicks_saved: u64,
    /// Threads its thread unparked: compute pool workers woken for its jobs, threads waiting on what it published.
    pub threads_woken: u64,
}

impl Wakes {
    fn of(c: &hyper_rt::shard_loop::Counters) -> Self {
        Self {
            steps: c.steps,
            parks: c.waits,
            by_kick: c.woken_by_kick,
            by_io: c.woken_by_io,
            by_timer: c.woken_by_timer,
            withdrawn: c.parks_withdrawn,
            taken: c.wakes_foreign,
            sent_own: c.wakes_sent_local,
            sent_other: c.wakes_sent_foreign,
            kicks_sent: c.kicks_sent,
            kicks_saved: c.kicks_saved,
            threads_woken: c.threads_woken,
        }
    }

    /// Its counts in a row, in [`WakeCells`]' order.
    fn row(&self) -> [u64; 12] {
        [self.steps, self.parks, self.by_kick, self.by_io, self.by_timer, self.withdrawn, self.taken, self.sent_own, self.sent_other, self.kicks_sent, self.kicks_saved, self.threads_woken]
    }

    /// [`Wakes::row`]'s inverse.
    fn of_row(row: [u64; 12]) -> Self {
        let [steps, parks, by_kick, by_io, by_timer, withdrawn, taken, sent_own, sent_other, kicks_sent, kicks_saved, threads_woken] = row;
        Self { steps, parks, by_kick, by_io, by_timer, withdrawn, taken, sent_own, sent_other, kicks_sent, kicks_saved, threads_woken }
    }
}

/// [`Wakes`] as the probe stores them: an atomic each, in [`Wakes::row`]'s order.
#[derive(Default)]
struct WakeCells([AtomicU64; 12]);

impl WakeCells {
    fn store(&self, wakes: &Wakes) {
        for (cell, n) in self.0.iter().zip(wakes.row()) {
            cell.store(n, Ordering::Relaxed);
        }
    }

    fn load(&self) -> Wakes {
        Wakes::of_row(self.0.each_ref().map(|cell| cell.load(Ordering::Relaxed)))
    }
}

/// The system calls a shard's thread makes beside its parks, kicks and unparks (goal 9, plan 01a1a87b2cec step 5:
/// what a request costs a shard in calls into the kernel, per request in the lab's report, where batched writes
/// show): its readiness registrations with its driver, and its tasks' TCP accepts, reads and writes, with the
/// reads that found nothing ready and the writes that found the send buffer full (each a call that moved nothing,
/// a wait after it).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Calls {
    /// The driver's non-blocking waits a busy shard made, once a quantum, for what its sockets have ready.
    pub harvests: u64,
    pub arms: u64,
    pub accepts: u64,
    pub reads: u64,
    pub reads_empty: u64,
    pub writes: u64,
    pub writes_full: u64,
}

impl Calls {
    fn of(c: &hyper_rt::shard_loop::Counters) -> Self {
        Self { harvests: c.harvests, arms: c.arms, accepts: c.socket_accepts, reads: c.socket_reads, reads_empty: c.socket_reads_empty, writes: c.socket_writes, writes_full: c.socket_writes_full }
    }
}

/// [`Calls`] as the probe stores them: an atomic each, in [`Calls`]' order.
#[derive(Default)]
struct CallCells([AtomicU64; 7]);

impl CallCells {
    fn store(&self, calls: &Calls) {
        let row = [calls.harvests, calls.arms, calls.accepts, calls.reads, calls.reads_empty, calls.writes, calls.writes_full];
        for (cell, n) in self.0.iter().zip(row) {
            cell.store(n, Ordering::Relaxed);
        }
    }

    fn load(&self) -> Calls {
        let [harvests, arms, accepts, reads, reads_empty, writes, writes_full] = self.0.each_ref().map(|cell| cell.load(Ordering::Relaxed));
        Calls { harvests, arms, accepts, reads, reads_empty, writes, writes_full }
    }
}

/// How long a shard's parks lasted (goal 9, plan 01a1a87b2cec step 5: whether a short spin before parking would catch
/// their work for less than a park and its wake cost): their time summed, and the parks under each of hyper-rt's
/// bounds (`hyper_rt::shard_loop::PARK_BOUNDS_NS`, doubling from 1 µs to 8,192 µs), a histogram's cumulative buckets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParkTimes {
    pub parked_ns: u64,
    pub under: [u64; hyper_rt::shard_loop::PARK_BOUNDS_NS.len()],
}

impl ParkTimes {
    fn of(c: &hyper_rt::shard_loop::Counters) -> Self {
        let under = [
            c.parks_under_1us,
            c.parks_under_2us,
            c.parks_under_4us,
            c.parks_under_8us,
            c.parks_under_16us,
            c.parks_under_32us,
            c.parks_under_64us,
            c.parks_under_128us,
            c.parks_under_256us,
            c.parks_under_512us,
            c.parks_under_1024us,
            c.parks_under_2048us,
            c.parks_under_4096us,
            c.parks_under_8192us,
        ];
        Self { parked_ns: c.parked_ns, under }
    }
}

/// [`ParkTimes`] as the probe stores them: the time, then each bucket.
#[derive(Default)]
struct ParkCells {
    parked_ns: AtomicU64,
    under: [AtomicU64; hyper_rt::shard_loop::PARK_BOUNDS_NS.len()],
}

impl ParkCells {
    fn store(&self, parks: &ParkTimes) {
        self.parked_ns.store(parks.parked_ns, Ordering::Relaxed);
        for (cell, n) in self.under.iter().zip(parks.under) {
            cell.store(n, Ordering::Relaxed);
        }
    }

    fn load(&self) -> ParkTimes {
        ParkTimes { parked_ns: self.parked_ns.load(Ordering::Relaxed), under: self.under.each_ref().map(|cell| cell.load(Ordering::Relaxed)) }
    }
}

/// What a shard's spin before a park did, as hyper-rt counts it on the shard (goal 9, plan 01a1a87b2cec step 4;
/// `hyper_rt::park_spin`): its spins, those that found work before they ran out (each a park, its wake and its
/// senders' kick saved), the time they spun (CPU its busy share counts), and the spin in force. All 0 where none was
/// asked for (`KILN_RT_SPIN`, `crate::rt`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spins {
    pub spins: u64,
    pub hits: u64,
    pub spun_ns: u64,
    pub now_ns: u64,
}

impl Spins {
    fn of(c: &hyper_rt::shard_loop::Counters) -> Self {
        Self { spins: c.park_spins, hits: c.park_spin_hits, spun_ns: c.park_spin_ns, now_ns: c.park_spin_now_ns }
    }
}

/// [`Spins`] as the probe stores them: an atomic each, in [`Spins`]' order.
#[derive(Default)]
struct SpinCells([AtomicU64; 4]);

impl SpinCells {
    fn store(&self, spins: &Spins) {
        for (cell, n) in self.0.iter().zip([spins.spins, spins.hits, spins.spun_ns, spins.now_ns]) {
            cell.store(n, Ordering::Relaxed);
        }
    }

    fn load(&self) -> Spins {
        let [spins, hits, spun_ns, now_ns] = self.0.each_ref().map(|cell| cell.load(Ordering::Relaxed));
        Spins { spins, hits, spun_ns, now_ns }
    }
}

impl Shard {
    fn new() -> Self {
        Self {
            delay: Histogram::new(DELAY_BUCKETS),
            last_ns: AtomicU64::new(0),
            late_ns: AtomicU64::new(0),
            most_late_ns: AtomicU64::new(0),
            cpu_ns: AtomicU64::new(u64::MAX),
            polls: AtomicU64::new(0),
            slow_task: AtomicU64::new(0),
            slow_blocking: AtomicU64::new(0),
            slow_host: AtomicU64::new(0),
            slow_unknown: AtomicU64::new(0),
            longest_poll_ns: AtomicU64::new(0),
            overrun_ns: AtomicU64::new(0),
            tasks: AtomicU64::new(0),
            most_tasks: AtomicU64::new(0),
            quantum_ns: AtomicU64::new(0),
            wakes: WakeCells::default(),
            calls: CallCells::default(),
            parks: ParkCells::default(),
            spins: SpinCells::default(),
        }
    }

    /// A probe's wake, `late` after its timer fell due, at `now`, with the shard's counts as the wake read them
    /// (`counters`) and its thread's CPU time (`cpu`, [`CpuClock`]); on the shard, so what it reads of the shard is
    /// the shard's own.
    fn note(&self, late: Duration, now: Instant, counters: Option<&hyper_rt::shard_loop::Counters>, cpu: Option<u64>) {
        let late_ns = nanos(late);
        self.delay.observe(late.as_secs_f64());
        self.late_ns.store(late_ns, Ordering::Relaxed);
        self.most_late_ns.fetch_max(late_ns, Ordering::Relaxed);
        self.last_ns.store(nanos(now.saturating_duration_since(base())), Ordering::Relaxed);
        if let Some(c) = counters {
            self.polls.store(c.polls, Ordering::Relaxed);
            self.slow_task.store(c.long_steps.saturating_sub(c.blocked_steps), Ordering::Relaxed);
            self.slow_blocking.store(c.blocked_steps, Ordering::Relaxed);
            self.slow_host.store(c.preempted_steps, Ordering::Relaxed);
            self.slow_unknown.store(c.unattributed_steps, Ordering::Relaxed);
            self.longest_poll_ns.store(c.longest_step_ns, Ordering::Relaxed);
            self.overrun_ns.store(c.scheduler_overrun_ns, Ordering::Relaxed);
            let tasks = held_tasks(c);
            self.tasks.store(tasks, Ordering::Relaxed);
            self.most_tasks.fetch_max(tasks, Ordering::Relaxed);
            // At its first wake and every [`COUNTS_EVERY`]th after (this wake's number is its delay's count).
            if self.delay.count().saturating_sub(1).is_multiple_of(COUNTS_EVERY) {
                self.wakes.store(&Wakes::of(c));
                self.calls.store(&Calls::of(c));
                self.parks.store(&ParkTimes::of(c));
                self.spins.store(&Spins::of(c));
            }
        }
        if let Some(q) = hyper_rt::futures::step_budget_ns() {
            self.quantum_ns.store(q, Ordering::Relaxed);
        }
        if let Some(cpu) = cpu {
            self.cpu_ns.store(cpu, Ordering::Relaxed);
        }
    }

    /// How late its probes ran, each noted.
    pub fn delay(&self) -> &Histogram {
        &self.delay
    }

    /// What its parks and wakes have cost it, as its probe last copied them (goal 9, plan 01a1a87b2cec step 5).
    pub fn wakes(&self) -> Wakes {
        self.wakes.load()
    }

    /// The system calls its thread has made beside them, as its probe last copied them ([`Calls`]).
    pub fn calls(&self) -> Calls {
        self.calls.load()
    }

    /// How long its parks have lasted, as its probe last copied them ([`ParkTimes`]).
    pub fn parks(&self) -> ParkTimes {
        self.parks.load()
    }

    /// What its spin before a park did, as its probe last copied it ([`Spins`]).
    pub fn spins(&self) -> Spins {
        self.spins.load()
    }

    /// What the shard's probe has noted, as it stands.
    pub fn snapshot(&self) -> Snapshot {
        let last = self.last_ns.load(Ordering::Relaxed);
        Snapshot {
            probes: self.delay.count(),
            late_sum: Duration::from_micros(self.delay.sum_us()),
            late: Duration::from_nanos(self.late_ns.load(Ordering::Relaxed)),
            most_late: Duration::from_nanos(self.most_late_ns.load(Ordering::Relaxed)),
            since_probe: (last > 0).then(|| Duration::from_nanos(nanos(base().elapsed()).saturating_sub(last))),
            busy: Some(self.cpu_ns.load(Ordering::Relaxed)).filter(|n| *n != u64::MAX).map(Duration::from_nanos),
            polls: self.polls.load(Ordering::Relaxed),
            slow: Slow {
                task: self.slow_task.load(Ordering::Relaxed),
                blocking: self.slow_blocking.load(Ordering::Relaxed),
                host: self.slow_host.load(Ordering::Relaxed),
                unknown: self.slow_unknown.load(Ordering::Relaxed),
            },
            longest_poll: Duration::from_nanos(self.longest_poll_ns.load(Ordering::Relaxed)),
            overrun: Duration::from_nanos(self.overrun_ns.load(Ordering::Relaxed)),
            tasks: self.tasks.load(Ordering::Relaxed),
            most_tasks: self.most_tasks.load(Ordering::Relaxed),
        }
    }
}

/// The tasks a shard holds by its counts: those admitted, less those whose future returned, was dropped or
/// unwound.
fn held_tasks(c: &hyper_rt::shard_loop::Counters) -> u64 {
    c.spawns.saturating_sub(c.completed).saturating_sub(c.cancelled).saturating_sub(c.unwound)
}

/// Slow polls by cause (see the module's notes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slow {
    pub task: u64,
    pub blocking: u64,
    pub host: u64,
    pub unknown: u64,
}

impl Slow {
    pub fn total(&self) -> u64 {
        self.task.saturating_add(self.blocking).saturating_add(self.host).saturating_add(self.unknown)
    }
}

/// One shard's figures as read.
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    /// Probes noted.
    pub probes: u64,
    /// How late they ran, added up: with `probes`, how late its ready work ran on average between
    /// two reads (what headroom watches: docs/priority.md).
    pub late_sum: Duration,
    /// How late the latest probe ran, and the most late of any.
    pub late: Duration,
    pub most_late: Duration,
    /// How long since the latest probe ran (none ran yet: `None`).
    pub since_probe: Option<Duration>,
    /// The CPU time the shard's thread has run, where the OS keeps it per thread.
    pub busy: Option<Duration>,
    pub polls: u64,
    pub slow: Slow,
    pub longest_poll: Duration,
    pub overrun: Duration,
    /// The tasks it held at its probe's latest wake, and the most it held at any.
    pub tasks: u64,
    pub most_tasks: u64,
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The instant probes' times are counted from.
fn base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

static SHARDS: OnceLock<Box<[Shard]>> = OnceLock::new();

/// The probes' period, nanoseconds, once [`start`] has run (0 before).
static PERIOD_NS: AtomicU64 = AtomicU64::new(0);

/// Shard `i`'s delay now, for placing work on the less delayed of two shards (goal 7, plan 01a1a59c2aea step 1):
/// how late its latest probe ran, or, when its next probe is overdue, how long overdue it is, so a shard held
/// right now shows it before its probe runs again. `None` with no probe noted (none started, or none ran yet).
/// Reads two atomics and the clock, no shard.
pub fn delay_now(i: usize) -> Option<Duration> {
    let s = shards().get(i)?;
    let last = s.last_ns.load(Ordering::Relaxed);
    if last == 0 {
        return None;
    }
    let late = s.late_ns.load(Ordering::Relaxed);
    let due = last.saturating_add(PERIOD_NS.load(Ordering::Relaxed));
    let overdue = nanos(base().elapsed()).saturating_sub(due);
    Some(Duration::from_nanos(late.max(overdue)))
}

/// Every shard's figures, in the runtime's order (empty before [`start`]).
pub fn shards() -> &'static [Shard] {
    SHARDS.get().map_or(&[], |s| &s[..])
}

/// The probes' period from `KILN_SHARD_PROBE_MS` (`0`: none), else [`PERIOD`].
pub fn period_from_env() -> Option<Duration> {
    match std::env::var("KILN_SHARD_PROBE_MS").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(PERIOD),
    }
}

/// Starts a probe on every shard, waking every `period`: once a process; later calls do nothing.
/// How many shards took theirs.
pub fn start(period: Duration) -> usize {
    let mut fresh = false;
    let shards = SHARDS.get_or_init(|| {
        fresh = true;
        (0..crate::rt::shard_count()).map(|_| Shard::new()).collect()
    });
    if !fresh {
        return 0;
    }
    let _ = base();
    let period = period.max(Duration::from_millis(1));
    PERIOD_NS.store(nanos(period), Ordering::Relaxed);
    let mut started = 0;
    for i in 0..shards.len() {
        if crate::rt::spawn_on(i, probe(i, period)) {
            started += 1;
        } else {
            tracing::warn!(shard = i, "a shard refused its health probe: /metrics won't show it");
        }
    }
    started
}

/// Shard `i`'s probe: wakes every `period` on its shard's timers and notes how late it ran.
async fn probe(i: usize, period: Duration) {
    let Some(me) = shards().get(i) else { return };
    wake_every(me, i, period, u64::MAX).await;
}

/// `me`'s probe on shard `i`, for `wakes` wakes: each a `period` after the last one ran, noted with how late it ran
/// however idle the shard (goal 9, plan 01a1a87b2cec step 1's samples: none skipped).
async fn wake_every(me: &Shard, i: usize, period: Duration, wakes: u64) {
    let mut due = Instant::now() + period;
    let mut clock = CpuClock::Unchosen;
    for _ in 0..wakes {
        crate::rt::sleep_until(due).await;
        let now = Instant::now();
        let late = now.saturating_duration_since(due);
        // The shard's counts, read once a wake: noted for `/metrics` and handed to what it sheds, which read a copy
        // of its own (goal 9, plan 01a1a87b2cec step 5).
        let counters = hyper_rt::futures::shard_counters();
        let cpu = clock.read();
        me.note(late, now, counters.as_ref(), cpu);
        // What the shard sheds, from its delay and how much of the time it ran (goal 6, plan 01a1a59c2abe step 4).
        crate::overload::tick(i, late, cpu, counters.as_ref());
        // Whether its host holds its wakes, and so whether it nudges its CPU (goal 8, plan 01a1a59c2b29 step 7).
        crate::nudge::tick(i, now, counters.as_ref());
        // The next one a period from now: a shard that was held doesn't then run a burst of probes
        // to catch up, each noting the same hold again.
        due = now + period;
    }
}

/// Where a probe reads its shard thread's CPU time (goal 9, plan 01a1a87b2cec step 5): hyper-rt's task clock, read with
/// no call (`hyper_rt::futures::thread_run_ns`), where the shard has one (Linux, where the OS opened the event); else the
/// OS's clock for the thread ([`thread_cpu_ns`]: `clock_gettime`, a call at every wake of every shard, 155 ns on the
/// lab's machine by hyper-rt's `task_clock` notes). Chosen at the probe's first reading and kept: the two count from
/// different starts (the task clock from its opening, as the shard's thread began), so the busy counter never mixes
/// them. Both run while the thread runs, in its system calls too (`tests`), so the busy share means what it meant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CpuClock {
    Unchosen,
    Task,
    Os,
}

impl CpuClock {
    /// The shard thread's CPU time now, nanoseconds, by the clock chosen (at the first reading: the task clock where the
    /// shard has one). None where neither keeps it, or where the task clock's page stopped offering it.
    fn read(&mut self) -> Option<u64> {
        match self {
            CpuClock::Task => hyper_rt::futures::thread_run_ns(),
            CpuClock::Os => thread_cpu_ns(),
            CpuClock::Unchosen => match hyper_rt::futures::thread_run_ns() {
                Some(ns) => {
                    *self = CpuClock::Task;
                    Some(ns)
                }
                None => {
                    *self = CpuClock::Os;
                    thread_cpu_ns()
                }
            },
        }
    }
}

/// The CPU time the calling thread has run, nanoseconds (Linux and macOS; elsewhere the OS keeps a
/// thread's times at its scheduler's tick, or not at all).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn thread_cpu_ns() -> Option<u64> {
    let t = nix::time::clock_gettime(nix::time::ClockId::CLOCK_THREAD_CPUTIME_ID).ok()?;
    u64::try_from(t.tv_sec()).ok()?.checked_mul(1_000_000_000)?.checked_add(u64::try_from(t.tv_nsec()).ok()?)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn thread_cpu_ns() -> Option<u64> {
    None
}

#[allow(clippy::cast_precision_loss)]
fn secs(d: Duration) -> f64 {
    d.as_nanos() as f64 / 1e9
}

/// The `kiln_shard_*` families, in the exposition.
pub fn render(o: &mut String) {
    let all = shards();
    let help = |o: &mut String, name: &str, kind: &str, h: &str| {
        let _ = writeln!(o, "# HELP {name} {h}\n# TYPE {name} {kind}");
    };
    let snaps: Vec<Snapshot> = all.iter().map(Shard::snapshot).collect();
    help(o, "kiln_shard_delay_seconds", "histogram", "How late ready work ran on each shard: a probe's timer, due every period, against when the probe ran (up to a timer tick and the thread's timer slack, 100 us each, by design).");
    for (i, s) in all.iter().enumerate() {
        s.delay.render(o, "kiln_shard_delay_seconds", &format!("shard=\"{i}\""));
    }
    help(o, "kiln_shard_stalled_seconds", "gauge", "How long since each shard's probe last ran: a shard held in one poll shows here before its delay can.");
    for (i, s) in snaps.iter().enumerate() {
        if let Some(d) = s.since_probe {
            let _ = writeln!(o, "kiln_shard_stalled_seconds{{shard=\"{i}\"}} {}", secs(d));
        }
    }
    help(o, "kiln_shard_busy_seconds_total", "counter", "CPU time each shard's thread has run (Linux, macOS): its rate is the share of the time the shard was busy on the CPU.");
    for (i, s) in snaps.iter().enumerate() {
        if let Some(d) = s.busy {
            let _ = writeln!(o, "kiln_shard_busy_seconds_total{{shard=\"{i}\"}} {}", secs(d));
        }
    }
    help(o, "kiln_shard_polls_total", "counter", "Task polls each shard ran.");
    for (i, s) in snaps.iter().enumerate() {
        let _ = writeln!(o, "kiln_shard_polls_total{{shard=\"{i}\"}} {}", s.polls);
    }
    help(
        o,
        "kiln_shard_slow_polls_total",
        "counter",
        "Polls that held a shard past twice its step quantum, by cause: task (on the CPU), blocking (waiting inside a call), host (runnable, off the CPU), unknown (the OS can't tell).",
    );
    for (i, s) in snaps.iter().enumerate() {
        for (cause, n) in [("task", s.slow.task), ("blocking", s.slow.blocking), ("host", s.slow.host), ("unknown", s.slow.unknown)] {
            let _ = writeln!(o, "kiln_shard_slow_polls_total{{shard=\"{i}\",cause=\"{cause}\"}} {n}");
        }
    }
    help(o, "kiln_shard_longest_poll_seconds", "gauge", "The longest single poll each shard has run.");
    for (i, s) in snaps.iter().enumerate() {
        let _ = writeln!(o, "kiln_shard_longest_poll_seconds{{shard=\"{i}\"}} {}", secs(s.longest_poll));
    }
    help(o, "kiln_shard_scheduler_overrun_seconds", "gauge", "How late each shard's steps ran after its waits, as the shard measured it (the host descheduling it), decaying as it recovers.");
    for (i, s) in snaps.iter().enumerate() {
        let _ = writeln!(o, "kiln_shard_scheduler_overrun_seconds{{shard=\"{i}\"}} {}", secs(s.overrun));
    }
    help(o, "kiln_shard_tasks", "gauge", "Tasks each shard held (admitted, not yet ended) as its probe last woke.");
    for (i, s) in snaps.iter().enumerate() {
        let _ = writeln!(o, "kiln_shard_tasks{{shard=\"{i}\"}} {}", s.tasks);
    }
    help(o, "kiln_shard_tasks_most", "gauge", "The most tasks each shard has held as its probe woke: what its task slots are sized from.");
    for (i, s) in snaps.iter().enumerate() {
        let _ = writeln!(o, "kiln_shard_tasks_most{{shard=\"{i}\"}} {}", s.most_tasks);
    }
    help(o, "kiln_shard_quantum_seconds", "gauge", "The step quantum each shard runs with: the longest a poll is to hold it, and half the hold its slow polls are counted past.");
    render_quantum(o, all);
    render_wakes(o, &all.iter().map(Shard::wakes).collect::<Vec<_>>(), &help);
    render_calls(o, &all.iter().map(Shard::calls).collect::<Vec<_>>(), &help);
    render_parks(o, &all.iter().map(|s| (s.parks(), s.wakes().parks)).collect::<Vec<_>>(), &help);
    render_spins(o, &all.iter().map(Shard::spins).collect::<Vec<_>>(), &help);
    render_held(o, &help);
}

/// The families of what each shard's spin before a park did ([`Spins`]; goal 9, plan 01a1a87b2cec step 4), from `all`,
/// each shard's in the runtime's order: its spins and their hits, the time they spun, and the spin in force.
fn render_spins(o: &mut String, all: &[Spins], help: &dyn Fn(&mut String, &str, &str, &str)) {
    type Value = fn(&Spins) -> String;
    let families: [(&str, &str, &str, Value); 4] = [
        ("kiln_shard_spins_total", "counter", "Idle moments each shard spun before it parked (KILN_RT_SPIN; none where it wasn't asked for).", |s: &Spins| s.spins.to_string()),
        ("kiln_shard_spin_hits_total", "counter", "Of those, the spins that found work before they ran out: each a park, its wake and its senders' kick saved.", |s: &Spins| s.hits.to_string()),
        ("kiln_shard_spin_seconds_total", "counter", "The time each shard's spins before its parks spun: CPU its busy share counts.", |s: &Spins| secs(Duration::from_nanos(s.spun_ns)).to_string()),
        ("kiln_shard_spin_window_seconds", "gauge", "The spin before a park each shard makes now: the one its own gaps make cheapest in CPU where it sizes itself, the asked one where fixed, 0 for none.", |s: &Spins| secs(Duration::from_nanos(s.now_ns)).to_string()),
    ];
    for (name, kind, h, value) in families {
        help(o, name, kind, h);
        for (i, s) in all.iter().enumerate() {
            let _ = writeln!(o, "{name}{{shard=\"{i}\"}} {}", value(s));
        }
    }
}

/// `kiln_shard_park_seconds`: how long each shard's parks lasted ([`ParkTimes`]; goal 9, plan 01a1a87b2cec step 5), a
/// histogram of hyper-rt's bounds, from `all`: each shard's times and its parks' count, in the runtime's order.
fn render_parks(o: &mut String, all: &[(ParkTimes, u64)], help: &dyn Fn(&mut String, &str, &str, &str)) {
    let name = "kiln_shard_park_seconds";
    help(o, name, "histogram", "How long each shard's parks lasted, from its driver wait's call to its return: under 16 us, a spin as long would have served its work sooner than its wake.");
    for (i, (parks, count)) in all.iter().enumerate() {
        for (bound, n) in hyper_rt::shard_loop::PARK_BOUNDS_NS.iter().zip(parks.under) {
            let _ = writeln!(o, "{name}_bucket{{shard=\"{i}\",le=\"{}\"}} {n}", secs(Duration::from_nanos(*bound)));
        }
        let _ = writeln!(o, "{name}_bucket{{shard=\"{i}\",le=\"+Inf\"}} {count}");
        let _ = writeln!(o, "{name}_sum{{shard=\"{i}\"}} {}", secs(Duration::from_nanos(parks.parked_ns)));
        let _ = writeln!(o, "{name}_count{{shard=\"{i}\"}} {count}");
    }
}

/// The families of the system calls each shard's thread made beside its parks and wakes ([`Calls`]; goal 9, plan
/// 01a1a87b2cec step 5), from `all`, each shard's in the runtime's order.
fn render_calls(o: &mut String, all: &[Calls], help: &dyn Fn(&mut String, &str, &str, &str)) {
    type Count = fn(&Calls) -> u64;
    let families: [(&str, &str, &[(&str, Count)]); 4] = [
        ("kiln_shard_harvests_total", "Non-blocking waits each shard made in its driver while busy, once a quantum, for what its sockets had ready: each a system call.", &[("", |c: &Calls| c.harvests)]),
        ("kiln_shard_arms_total", "Readiness registrations each shard made with its driver: each a system call (epoll_ctl, kevent; an AFD poll on Windows).", &[("", |c: &Calls| c.arms)]),
        (
            "kiln_shard_socket_calls_total",
            "TCP accepts, reads and writes each shard's tasks made: each a system call.",
            &[(",call=\"accept\"", |c: &Calls| c.accepts), (",call=\"read\"", |c: &Calls| c.reads), (",call=\"write\"", |c: &Calls| c.writes)],
        ),
        (
            "kiln_shard_socket_calls_idle_total",
            "Of those, the reads that found nothing ready and the writes that found the send buffer full: calls that moved nothing, each with a wait after.",
            &[(",call=\"read\"", |c: &Calls| c.reads_empty), (",call=\"write\"", |c: &Calls| c.writes_full)],
        ),
    ];
    for (name, h, labels) in families {
        help(o, name, "counter", h);
        for (i, c) in all.iter().enumerate() {
            for (label, n) in labels {
                let _ = writeln!(o, "{name}{{shard=\"{i}\"{label}}} {}", n(c));
            }
        }
    }
}

/// Each shard's step quantum, as its probe last read it (goal 9, plan 01a1a87b2cec step 3): what the lab holds what
/// held the shards to. A shard whose probe hasn't run yet says none.
fn render_quantum(o: &mut String, all: &[Shard]) {
    for (i, s) in all.iter().enumerate() {
        let q = s.quantum_ns.load(Ordering::Relaxed);
        if q > 0 {
            let _ = writeln!(o, "kiln_shard_quantum_seconds{{shard=\"{i}\"}} {}", secs(Duration::from_nanos(q)));
        }
    }
}

/// The families of what each shard's parks and wakes cost it ([`Wakes`]; goal 9, plan 01a1a87b2cec step 5), from
/// `all`, each shard's in the runtime's order.
fn render_wakes(o: &mut String, all: &[Wakes], help: &dyn Fn(&mut String, &str, &str, &str)) {
    type Count = fn(&Wakes) -> u64;
    let families: [(&str, &str, &[(&str, Count)]); 9] = [
        ("kiln_shard_steps_total", "Loop steps each shard ran.", &[("", |w: &Wakes| w.steps)]),
        ("kiln_shard_parks_total", "Parks each shard waited in its driver: each a system call and a sleep.", &[("", |w: &Wakes| w.parks)]),
        (
            "kiln_shard_wakeups_total",
            "Each shard's parks by what ended them: kick (another thread's wake or message), io (a socket's readiness), timer (their deadline).",
            &[(",by=\"kick\"", |w: &Wakes| w.by_kick), (",by=\"io\"", |w: &Wakes| w.by_io), (",by=\"timer\"", |w: &Wakes| w.by_timer)],
        ),
        ("kiln_shard_parks_withdrawn_total", "Parks each shard withdrew with work already waiting: no system call.", &[("", |w: &Wakes| w.withdrawn)]),
        ("kiln_shard_wakes_taken_total", "Wakes from other threads each shard took in.", &[("", |w: &Wakes| w.taken)]),
        (
            "kiln_shard_wakes_sent_total",
            "Wakes each shard's thread made through tasks' wakers (a channel's send, a broadcast's subscribers): to its own tasks, and to other shards'.",
            &[(",to=\"own\"", |w: &Wakes| w.sent_own), (",to=\"other\"", |w: &Wakes| w.sent_other)],
        ),
        ("kiln_shard_kicks_sent_total", "Kicks each shard's thread made to parked shards: each a system call on it.", &[("", |w: &Wakes| w.kicks_sent)]),
        ("kiln_shard_kicks_saved_total", "Wakes and messages sent to each shard that found it running or its park's kick claimed: kicks saved.", &[("", |w: &Wakes| w.kicks_saved)]),
        ("kiln_shard_threads_woken_total", "Threads each shard's thread unparked (compute pool workers for its jobs, threads waiting on what it published): each a system call on it.", &[("", |w: &Wakes| w.threads_woken)]),
    ];
    for (name, h, labels) in families {
        help(o, name, "counter", h);
        for (i, w) in all.iter().enumerate() {
            for (label, n) in labels {
                let _ = writeln!(o, "{name}{{shard=\"{i}\"{label}}} {}", n(w));
            }
        }
    }
}

/// The `kiln_shard_held_*` families: what held each shard past 100 µs, by its task's spawn site and the
/// label its code set (`hyper_rt::slow`; goal 9, plan 01a1a87b2cec step 2). Read from each shard's own table,
/// which only the shard writes: no lock, nothing asked of the shard.
fn render_held(o: &mut String, help: &dyn Fn(&mut String, &str, &str, &str)) {
    // No probes, no runtime asked for (rendering must not start one).
    if shards().is_empty() {
        return;
    }
    // Each shard's entries by what the exposition names them (two labels of one name are one series): polls,
    // their time, the longest, those of them the host held with their time, and those whose thread said its time on
    // the CPU with their holds, that time and the longest of it.
    #[derive(Default)]
    struct Row {
        polls: u64,
        ns: u64,
        longest: u64,
        host_polls: u64,
        host_ns: u64,
        cpu_polls: u64,
        cpu_held_ns: u64,
        cpu_ns: u64,
        longest_cpu: u64,
    }
    let mut rows: std::collections::BTreeMap<(usize, String, &'static str), Row> = std::collections::BTreeMap::new();
    for (i, holder) in crate::rt::shard_holders().into_iter().take(shards().len()).enumerate() {
        for p in hyper_rt::slow::slow_polls(holder) {
            let site = p.site.map_or_else(String::new, |l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
            let row = rows.entry((i, site, p.label.map_or("", |l| l.name))).or_default();
            row.polls = row.polls.saturating_add(p.polls);
            row.ns = row.ns.saturating_add(p.total_ns);
            row.longest = row.longest.max(p.longest_ns);
            row.host_polls = row.host_polls.saturating_add(p.host_polls);
            row.host_ns = row.host_ns.saturating_add(p.host_ns);
            row.cpu_polls = row.cpu_polls.saturating_add(p.cpu_polls);
            row.cpu_held_ns = row.cpu_held_ns.saturating_add(p.cpu_held_ns);
            row.cpu_ns = row.cpu_ns.saturating_add(p.cpu_ns);
            row.longest_cpu = row.longest_cpu.max(p.longest_cpu_ns);
        }
    }
    let series = |o: &mut String, name: &str, value: &dyn Fn(&Row) -> String| {
        for ((i, site, label), row) in &rows {
            let _ = writeln!(o, "{name}{{shard=\"{i}\",site=\"{}\",label=\"{}\"}} {}", escape(site), escape(label), value(row));
        }
    };
    help(o, "kiln_shard_held_polls_total", "counter", "Polls that held a shard past 100 us, by the spawn site of their task and the label its code set (a request's route).");
    series(o, "kiln_shard_held_polls_total", &|r| r.polls.to_string());
    help(o, "kiln_shard_held_seconds_total", "counter", "How long those polls held their shard, added up.");
    series(o, "kiln_shard_held_seconds_total", &|r| secs(Duration::from_nanos(r.ns)).to_string());
    help(o, "kiln_shard_held_longest_seconds", "gauge", "The longest of those polls.");
    series(o, "kiln_shard_held_longest_seconds", &|r| secs(Duration::from_nanos(r.longest)).to_string());
    // Goal 9, plan 01a1a87b2cec steps 3 and 4: what of it no code held.
    help(o, "kiln_shard_held_host_polls_total", "counter", "Of those polls, the ones the host held: the shard's thread runnable and off the CPU (preempted), as found for polls past twice the step quantum.");
    series(o, "kiln_shard_held_host_polls_total", &|r| r.host_polls.to_string());
    help(o, "kiln_shard_held_host_seconds_total", "counter", "How long the polls the host held held their shard, added up.");
    series(o, "kiln_shard_held_host_seconds_total", &|r| secs(Duration::from_nanos(r.host_ns)).to_string());
    // Goal 9, plan 01a1a87b2cec step 3: each poll's time on the CPU (`hyper_rt`'s task clock: Linux, where the OS
    // lets the shard's thread open its perf event), what of a hold kiln's code ran; the rest was off the CPU.
    help(o, "kiln_shard_held_cpu_polls_total", "counter", "Of those polls, the ones whose shard's thread said its time on the CPU (Linux).");
    series(o, "kiln_shard_held_cpu_polls_total", &|r| r.cpu_polls.to_string());
    help(o, "kiln_shard_held_cpu_held_seconds_total", "counter", "How long those polls held their shard, added up.");
    series(o, "kiln_shard_held_cpu_held_seconds_total", &|r| secs(Duration::from_nanos(r.cpu_held_ns)).to_string());
    help(o, "kiln_shard_held_cpu_seconds_total", "counter", "Their time on the CPU, added up: the rest of their holds was off it (the host's, or a call that blocked).");
    series(o, "kiln_shard_held_cpu_seconds_total", &|r| secs(Duration::from_nanos(r.cpu_ns)).to_string());
    help(o, "kiln_shard_held_longest_cpu_seconds", "gauge", "The longest any of them ran on the CPU.");
    series(o, "kiln_shard_held_longest_cpu_seconds", &|r| secs(Duration::from_nanos(r.longest_cpu)).to_string());
}

/// A label's value as the exposition writes it: its backslashes, quotes and line ends escaped.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9 (plan 01a1a87b2cec step 3): each shard's step quantum on `/metrics`, as its probe read it, so the lab
    /// holds what held the shards to the quantum they ran with; a shard whose probe hasn't run yet says none.
    #[test]
    fn g9_s3_each_shards_quantum_is_on_metrics() {
        let all = [Shard::new(), Shard::new()];
        if let Some(s) = all.first() {
            s.quantum_ns.store(crate::rt::STEP_BUDGET_NS, Ordering::Relaxed);
        }
        let mut o = String::new();
        render_quantum(&mut o, &all);
        assert_eq!(o, "kiln_shard_quantum_seconds{shard=\"0\"} 0.000266666\n");
    }

    /// Goal 9, plan 01a1a87b2cec step 5: what hyper-rt counts of a shard's parks and wakes becomes its [`Wakes`],
    /// each count in its place, and survives the probe's atomics as it was.
    #[test]
    fn g9_s5_a_shards_counts_become_its_wakes() {
        let c = hyper_rt::shard_loop::Counters {
            steps: 1,
            waits: 2,
            woken_by_kick: 3,
            woken_by_io: 4,
            woken_by_timer: 5,
            parks_withdrawn: 6,
            wakes_foreign: 7,
            wakes_sent_local: 8,
            wakes_sent_foreign: 9,
            kicks_sent: 10,
            kicks_saved: 11,
            threads_woken: 12,
            ..Default::default()
        };
        let w = Wakes::of(&c);
        assert_eq!(w, Wakes { steps: 1, parks: 2, by_kick: 3, by_io: 4, by_timer: 5, withdrawn: 6, taken: 7, sent_own: 8, sent_other: 9, kicks_sent: 10, kicks_saved: 11, threads_woken: 12 });
        let cells = WakeCells::default();
        cells.store(&w);
        assert_eq!(cells.load(), w);
    }

    /// Goal 9, plan 01a1a87b2cec step 5: each shard's parks and wakes are on `/metrics`, a series each, a park by
    /// what ended it and a wake by whom it went to, as the lab reads them per request.
    #[test]
    fn g9_s5_each_shards_parks_and_wakes_are_on_metrics() {
        let w = Wakes { steps: 1, parks: 2, by_kick: 3, by_io: 4, by_timer: 5, withdrawn: 6, taken: 7, sent_own: 8, sent_other: 9, kicks_sent: 10, kicks_saved: 11, threads_woken: 12 };
        let mut o = String::new();
        render_wakes(&mut o, &[Wakes::default(), w], &|o: &mut String, name: &str, kind: &str, _: &str| {
            let _ = writeln!(o, "# TYPE {name} {kind}");
        });
        for line in [
            "kiln_shard_steps_total{shard=\"1\"} 1",
            "kiln_shard_parks_total{shard=\"1\"} 2",
            "kiln_shard_wakeups_total{shard=\"1\",by=\"kick\"} 3",
            "kiln_shard_wakeups_total{shard=\"1\",by=\"io\"} 4",
            "kiln_shard_wakeups_total{shard=\"1\",by=\"timer\"} 5",
            "kiln_shard_parks_withdrawn_total{shard=\"1\"} 6",
            "kiln_shard_wakes_taken_total{shard=\"1\"} 7",
            "kiln_shard_wakes_sent_total{shard=\"1\",to=\"own\"} 8",
            "kiln_shard_wakes_sent_total{shard=\"1\",to=\"other\"} 9",
            "kiln_shard_kicks_sent_total{shard=\"1\"} 10",
            "kiln_shard_kicks_saved_total{shard=\"1\"} 11",
            "kiln_shard_threads_woken_total{shard=\"1\"} 12",
            "kiln_shard_parks_total{shard=\"0\"} 0",
            "# TYPE kiln_shard_wakeups_total counter",
        ] {
            assert!(o.lines().any(|l| l == line), "{line:?} not in:\n{o}");
        }
        assert_eq!(o.lines().filter(|l| l.starts_with("# TYPE")).count(), 9, "nine families:\n{o}");
    }

    /// Goal 9, plan 01a1a87b2cec step 5: how long a shard's parks lasted is on `/metrics` as a histogram of hyper-rt's
    /// bounds, its count the parks, its sum their time, and survives the probe's atomics as it was.
    #[test]
    fn g9_s5_each_shards_park_times_are_a_histogram_on_metrics() {
        let c = hyper_rt::shard_loop::Counters {
            parked_ns: 3_000_000,
            parks_under_1us: 1,
            parks_under_2us: 1,
            parks_under_4us: 1,
            parks_under_8us: 2,
            parks_under_16us: 2,
            parks_under_32us: 2,
            parks_under_64us: 2,
            parks_under_128us: 3,
            parks_under_256us: 3,
            parks_under_512us: 3,
            parks_under_1024us: 3,
            parks_under_2048us: 3,
            parks_under_4096us: 3,
            parks_under_8192us: 3,
            ..Default::default()
        };
        let parks = ParkTimes::of(&c);
        assert_eq!(parks.under, [1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3]);
        let cells = ParkCells::default();
        cells.store(&parks);
        assert_eq!(cells.load(), parks);
        let mut o = String::new();
        render_parks(&mut o, &[(parks, 4)], &|o: &mut String, name: &str, kind: &str, _: &str| {
            let _ = writeln!(o, "# TYPE {name} {kind}");
        });
        for line in [
            "# TYPE kiln_shard_park_seconds histogram",
            "kiln_shard_park_seconds_bucket{shard=\"0\",le=\"0.000001\"} 1",
            "kiln_shard_park_seconds_bucket{shard=\"0\",le=\"0.000016\"} 2",
            "kiln_shard_park_seconds_bucket{shard=\"0\",le=\"0.008192\"} 3",
            "kiln_shard_park_seconds_bucket{shard=\"0\",le=\"+Inf\"} 4",
            "kiln_shard_park_seconds_sum{shard=\"0\"} 0.003",
            "kiln_shard_park_seconds_count{shard=\"0\"} 4",
        ] {
            assert!(o.lines().any(|l| l == line), "{line:?} not in:\n{o}");
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 4: what a shard's spin before a park did is on `/metrics`, each count in its
    /// place, its time and the spin in force in seconds, and survives the probe's atomics as it was; a shard asked for
    /// none says zeros.
    #[test]
    fn g9_s4_each_shards_spins_before_its_parks_are_on_metrics() {
        let c = hyper_rt::shard_loop::Counters { park_spins: 10, park_spin_hits: 4, park_spin_ns: 120_000, park_spin_now_ns: 16_000, ..Default::default() };
        let spins = Spins::of(&c);
        assert_eq!(spins, Spins { spins: 10, hits: 4, spun_ns: 120_000, now_ns: 16_000 });
        let cells = SpinCells::default();
        cells.store(&spins);
        assert_eq!(cells.load(), spins);
        let mut o = String::new();
        render_spins(&mut o, &[spins, Spins::default()], &|o: &mut String, name: &str, kind: &str, _: &str| {
            let _ = writeln!(o, "# TYPE {name} {kind}");
        });
        for line in [
            "# TYPE kiln_shard_spins_total counter",
            "kiln_shard_spins_total{shard=\"0\"} 10",
            "kiln_shard_spin_hits_total{shard=\"0\"} 4",
            "kiln_shard_spin_seconds_total{shard=\"0\"} 0.00012",
            "# TYPE kiln_shard_spin_window_seconds gauge",
            "kiln_shard_spin_window_seconds{shard=\"0\"} 0.000016",
            "kiln_shard_spins_total{shard=\"1\"} 0",
            "kiln_shard_spin_window_seconds{shard=\"1\"} 0",
        ] {
            assert!(o.lines().any(|l| l == line), "{line:?} not in:\n{o}");
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 5, the probe's own cost cut with its samples as step 1 has them: a probe on a
    /// shard wakes every period, each wake noted with how late it ran however idle the shard (none skipped), and the
    /// shard's counts as that wake read them (the one copy it hands on to what the shard sheds).
    #[test]
    fn g9_s5_the_probe_wakes_every_period_and_notes_every_wake() -> anyhow::Result<()> {
        let _ = base();
        crate::rt::block_on(async {
            let me = Shard::new();
            let wakes = 10_u64;
            let start = Instant::now();
            // No overload watch for this shard (its index is none): the shard's samples alone.
            wake_every(&me, usize::MAX, PERIOD, wakes).await;
            let took = start.elapsed();
            let s = me.snapshot();
            anyhow::ensure!(s.probes == wakes, "{} of {wakes} wakes noted", s.probes);
            anyhow::ensure!(took >= PERIOD * u32::try_from(wakes)?, "{wakes} wakes a period apart took {took:?}");
            anyhow::ensure!(me.delay().sum_us() >= s.late.as_micros().try_into()?, "each wake's delay noted: {s:?}");
            anyhow::ensure!(s.polls > 0 && s.tasks > 0 && s.since_probe.is_some(), "the shard's counts as its wakes read them: {s:?}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a probe copies what its shard's parks, wakes and calls cost it at its first
    /// wake and every [`COUNTS_EVERY`]th after (those families move every 100 ms), while each wake's own samples (its
    /// delay, the shard's polls) are noted at every wake as before.
    #[test]
    fn g9_s5_a_probe_copies_parks_wakes_and_calls_every_tenth_wake() -> anyhow::Result<()> {
        let _ = base();
        crate::rt::block_on(async {
            let me = Shard::new();
            wake_every(&me, usize::MAX, PERIOD, 1).await;
            let first = (me.wakes(), me.calls(), me.parks());
            anyhow::ensure!(first.0.steps > 0, "copied at the first wake: {first:?}");
            wake_every(&me, usize::MAX, PERIOD, COUNTS_EVERY - 1).await;
            let polls = me.snapshot().polls;
            anyhow::ensure!((me.wakes(), me.calls(), me.parks()) == first, "copied again before its {COUNTS_EVERY}th wake after");
            wake_every(&me, usize::MAX, PERIOD, 1).await;
            anyhow::ensure!(me.wakes().steps > first.0.steps, "not copied again at its {COUNTS_EVERY}th wake after");
            let s = me.snapshot();
            anyhow::ensure!(s.probes == COUNTS_EVERY + 1 && s.polls > polls, "each wake noted all the same: {s:?}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: a probe reads its shard thread's CPU time from hyper-rt's task clock, with no
    /// call, where the shard has one, else from the OS's clock for the thread, and keeps to the one it chose. Over the
    /// same stretch of spinning and system calls the task clock runs as the OS's does, the calls' time in the kernel
    /// with the rest, so the busy share means what it meant; and the probe's samples are as they were.
    #[test]
    fn g9_s5_a_probe_reads_its_threads_cpu_time_from_the_task_clock() -> anyhow::Result<()> {
        let _ = base();
        crate::rt::block_on(async {
            let mut clock = CpuClock::Unchosen;
            let first = clock.read();
            let task = hyper_rt::futures::thread_run_ns().is_some();
            anyhow::ensure!(clock == if task { CpuClock::Task } else { CpuClock::Os }, "{clock:?} chosen, the task clock there: {task}");
            if let (true, Some(os0), Some(t0)) = (task, thread_cpu_ns(), clock.read()) {
                // A file's metadata asked again and again (system calls, their time in the kernel) until the thread has
                // run 20 ms on the CPU, or for 2 s on a loaded machine.
                let began = Instant::now();
                let mut calls = 0_u64;
                while thread_cpu_ns().unwrap_or(os0).saturating_sub(os0) < 20_000_000 && began.elapsed() < Duration::from_secs(2) {
                    calls += u64::from(std::fs::metadata("/").is_ok());
                }
                let (os, t) = (thread_cpu_ns().unwrap_or(os0).saturating_sub(os0), clock.read().unwrap_or(t0).saturating_sub(t0));
                anyhow::ensure!(os.abs_diff(t) <= (os / 10).max(2_000_000), "over {calls} calls the OS's clock ran {os} ns, the task clock {t} ns");
            }
            anyhow::ensure!(clock.read() >= first, "the chosen clock only moves on");
            let me = Shard::new();
            wake_every(&me, usize::MAX, PERIOD, 3).await;
            let s = me.snapshot();
            anyhow::ensure!(s.probes == 3 && (s.busy.is_some() || first.is_none()), "three wakes noted, with the thread's CPU time: {s:?}");
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 5: what hyper-rt counts of a shard's arms and socket calls becomes its
    /// [`Calls`], survives the probe's atomics, and is on `/metrics` a series a call, the idle ones apart.
    #[test]
    fn g9_s5_each_shards_system_calls_are_on_metrics() {
        let c = hyper_rt::shard_loop::Counters { arms: 1, socket_accepts: 2, socket_reads: 3, socket_reads_empty: 4, socket_writes: 5, socket_writes_full: 6, harvests: 7, ..Default::default() };
        let calls = Calls::of(&c);
        assert_eq!(calls, Calls { harvests: 7, arms: 1, accepts: 2, reads: 3, reads_empty: 4, writes: 5, writes_full: 6 });
        let cells = CallCells::default();
        cells.store(&calls);
        assert_eq!(cells.load(), calls);
        let mut o = String::new();
        render_calls(&mut o, &[calls], &|o: &mut String, name: &str, kind: &str, _: &str| {
            let _ = writeln!(o, "# TYPE {name} {kind}");
        });
        for line in [
            "kiln_shard_harvests_total{shard=\"0\"} 7",
            "kiln_shard_arms_total{shard=\"0\"} 1",
            "kiln_shard_socket_calls_total{shard=\"0\",call=\"accept\"} 2",
            "kiln_shard_socket_calls_total{shard=\"0\",call=\"read\"} 3",
            "kiln_shard_socket_calls_total{shard=\"0\",call=\"write\"} 5",
            "kiln_shard_socket_calls_idle_total{shard=\"0\",call=\"read\"} 4",
            "kiln_shard_socket_calls_idle_total{shard=\"0\",call=\"write\"} 6",
        ] {
            assert!(o.lines().any(|l| l == line), "{line:?} not in:\n{o}");
        }
    }
}
