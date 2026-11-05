//! The watchdog (kiln's goal 6, plan 01a1a59c2abe step 2; ORIGIN.md, "What differs from upstream"): one thread
//! outside a runtime's shards that sees a shard held in one poll past its limit, says which task held it, where
//! that task was spawned and whose the time was, sends no new work there while it is held, and hands the work
//! that waits for that shard and may move to the runtime's other shards. It never adds a thread to do a held
//! shard's work: Tokio declines to, since threads added to a loaded scheduler can make it worse and the .NET
//! pool's hill climbing adapts in hundreds of milliseconds (research 01a1a59c2abf, f5).
//!
//! - **The limit** is a step's worth of polls that each use their share: `batch` polls of a quantum each. A step
//!   polls at most `batch` tasks (docs/runtime.md §3.3), and a task charged by its leaves (`crate::coop`) holds a
//!   wanted shard about a quantum, so work listed as a step begins waits at most about that long behind a shard
//!   whose tasks keep to their shares. One poll held longer delays the shard's work more than any step of such
//!   tasks could. Seastar keeps two marks: a task past twice its quota is counted (the loop's long poll here,
//!   §3.11, counted after the fact), and a reactor that makes no progress for 25 ms is reported as it happens
//!   (`blocked_reactor_notify_ms`, research f8). This is the second, derived from the shard's own quantum and
//!   batch rather than picked: kiln's server shards (a configured quantum of 250 µs, a batch of 64) are held
//!   16 ms before it.
//! - **Looks** come every half limit (never under the shard's timer tick, the finest wait the runtime keeps), so
//!   a poll held past the limit is seen within half a limit of passing it. Each look reads two words the shard
//!   writes as each poll begins and ends ([`Watch`]): which poll runs, and its task. A poll seen at two looks is
//!   one poll, held at least the time between them.
//! - **Whose the time was**: the shard thread's CPU clock read at the first look that saw the poll and at the
//!   look that finds it past the limit (`crate::thread_clock`, which any thread may read). More CPU than a
//!   quantum in between: the task ran on the CPU. Less: the thread was off the CPU, waiting inside a call (the
//!   task's) or runnable and not running (the host's): Linux tells them apart by the thread's state in
//!   `/proc`, macOS by its Mach run state, and elsewhere the report says off the CPU. Windows keeps a thread's
//!   CPU time at the scheduler's tick (about 15.6 ms), so its verdicts there are as coarse.
//! - **What it records**: every episode, counted ([`Stalls`]), and a report of each ([`StallReport`]: the
//!   task, where it was spawned, how long it had held the shard, the CPU it ran, whose), rate-limited as
//!   Seastar limits its stall reports, to 5 a minute a shard (`blocked_reactor_reports_per_minute`'s default);
//!   episodes past that are counted as suppressed.
//! - **No backtrace.** Seastar signals the stuck thread and unwinds its stack in the handler. Rust has no
//!   unwinder that is safe to run in a signal handler (`std::backtrace` allocates and takes locks, and the
//!   `backtrace` crate calls its handler-time walk unsynchronized and unsafe), release builds keep no frame
//!   pointers to walk instead, and Windows has no signals at all: its equivalent suspends the thread and walks
//!   it from outside. So a report names the task's spawn site instead, recorded at every spawn with
//!   `#[track_caller]`: the code whose future held the shard.
//! - **What moves**: a pinned task already admitted to a shard lives there (docs/runtime.md §3): it waits for
//!   the held shard. Work not yet admitted can go elsewhere: spawns submitted as movable
//!   ([`crate::runtime::submit_movable`], what kiln's spread placement and accepted connections use) wait in a
//!   queue of their own on the shard's entry, and the watchdog hands those that wait for a held shard to the
//!   runtime's next shard that is not held. Movable tasks (`crate::movable`, kiln's goal 7) queued on a held
//!   shard go to the runtime's shared queues, and parked siblings are kicked to take them: the held shard's queue
//!   detached from it, as Tokio hands a blocked worker's core to another thread. Placement asks [`stalled`] and
//!   sends no new work to a held shard. Spawns addressed to one shard ([`crate::runtime::submit_to_holder`], a
//!   shard's own spawns) stay.

use std::panic::Location;
#[cfg(loom)]
use loom::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, Ordering, fence};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, Ordering, fence};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::RtError;
use crate::mem::Encoded;
use crate::registry::{self, Entry, SlotHolder};
use crate::shard::TaskId;
use crate::task::SpawnRequest;
use crate::thread_clock::CpuReading;

/// Seastar's default cap on its stall detector's reports (`blocked_reactor_reports_per_minute`, 5; research
/// 01a1a59c2abf, f8 and its source s9).
pub const REPORTS_PER_MINUTE: u64 = 5;

/// Format: nanoseconds per minute.
const NS_PER_MINUTE: u64 = 60_000_000_000;

/// The two words a shard writes as each poll begins and ends, for the watchdog to read: on a line of its own
/// (the largest we target, Apple silicon's 128 bytes), which only the shard writes.
#[repr(align(128))]
#[derive(Debug, Default)]
pub(crate) struct Watch {
    /// Odd while a task's poll runs: one more as each poll begins, and one more as it ends.
    polls: AtomicU64,
    /// The word of the task being polled, written before `polls` turns odd.
    task: AtomicU64,
}

impl Watch {
    /// The shard begins polling the task `word`. Release on both: a reader that sees the count odd sees the
    /// word, and one that sees a later poll's word sees the count past this poll's.
    #[inline]
    pub(crate) fn begin(&self, word: u64) {
        self.task.store(word, Ordering::Release);
        let polls = self.polls.load(Ordering::Relaxed);
        self.polls.store(polls.wrapping_add(1), Ordering::Release);
    }

    /// The shard's poll ended (returned or unwound).
    #[inline]
    pub(crate) fn end(&self) {
        let polls = self.polls.load(Ordering::Relaxed);
        if polls & 1 == 1 {
            self.polls.store(polls.wrapping_add(1), Ordering::Release);
        }
    }

    /// The poll running now, its count and its task's word, read whole; `None` between polls. The count read
    /// again after the word: the same odd count both times means no poll began between, so the word is this
    /// poll's (`begin`'s order).
    fn running(&self) -> Option<(u64, u64)> {
        let polls = self.polls.load(Ordering::Acquire);
        if polls & 1 == 0 {
            return None;
        }
        let task = self.task.load(Ordering::Acquire);
        (self.polls.load(Ordering::Acquire) == polls).then_some((polls, task))
    }
}

/// Whose the time of a stall was, as the readings tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Whose {
    /// The task's: it ran on the CPU for more than a quantum of the time.
    Task = 1,
    /// The task's: the thread was off the CPU, waiting inside a call (blocked in the kernel).
    Blocked = 2,
    /// The host's: the thread was off the CPU while runnable (preempted, or its virtual CPU stolen).
    Host = 3,
    /// Off the CPU, and this platform cannot tell a call's wait from a preemption.
    OffCpu = 4,
    /// No reading of the thread's CPU clock.
    Unknown = 5,
}

impl Whose {
    fn from_u8(value: u8) -> Whose {
        match value {
            1 => Whose::Task,
            2 => Whose::Blocked,
            3 => Whose::Host,
            4 => Whose::OffCpu,
            _ => Whose::Unknown,
        }
    }

    /// The task's own (a bug of its code, as the bounded-work rule judges a long poll), rather than the host's
    /// or unknown.
    pub fn is_tasks(self) -> bool {
        matches!(self, Whose::Task | Whose::Blocked)
    }

    /// A word for it, as a metric's label or a log's field says it.
    pub fn as_str(self) -> &'static str {
        match self {
            Whose::Task => "task",
            Whose::Blocked => "blocking",
            Whose::Host => "host",
            Whose::OffCpu => "off_cpu",
            Whose::Unknown => "unknown",
        }
    }
}

/// What the watchdog found of one shard, written by it alone and read by anyone: on a line of its own.
#[repr(align(128))]
#[derive(Debug, Default)]
pub(crate) struct Board {
    /// Whether the shard is held in one poll past its limit now: placement sends no new work there.
    stalled: AtomicBool,
    /// Polls seen held past the limit.
    episodes: AtomicU64,
    /// The time those polls held the shard, as the watchdog saw them end, nanoseconds.
    held_ns: AtomicU64,
    /// Movable spawns that waited for the shard while it was held and went to another shard, and movable tasks
    /// queued on it that went to the shared queues (`crate::movable`).
    moved: AtomicU64,
    /// Episodes reported, and those past the reports' cap.
    reports: AtomicU64,
    suppressed: AtomicU64,
    /// The latest report, read whole under `seq` (odd while the watchdog writes it).
    seq: AtomicU64,
    task: AtomicU64,
    spawned_at: AtomicPtr<Location<'static>>,
    report_held_ns: AtomicU64,
    /// The CPU the thread ran in the report's window, nanoseconds (`u64::MAX`: no reading).
    cpu_ns: AtomicU64,
    whose: AtomicU8,
}

/// A shard's stalls as the watchdog counted them ([`stalls`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stalls {
    /// Whether the shard is held in one poll past its limit now.
    pub stalled: bool,
    /// Polls seen held past the limit.
    pub episodes: u64,
    /// The time those that ended held the shard, nanoseconds (seen to within a look).
    pub held_ns: u64,
    /// Movable spawns handed to another shard while this one was held, and movable tasks handed to the shared
    /// queues (`crate::movable`).
    pub moved: u64,
    /// Episodes reported ([`report`]), and those past the cap of [`REPORTS_PER_MINUTE`].
    pub reports: u64,
    /// Episodes not reported: past the cap.
    pub suppressed: u64,
}

/// The latest stall reported on a shard ([`report`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StallReport {
    /// Which report of the shard's this is, counting from one: a reader tells a new one by it.
    pub number: u64,
    /// The task whose poll held the shard.
    pub task: TaskId,
    /// Where that task was spawned (`#[track_caller]` at its spawn), when its slot recorded it.
    pub spawned_at: Option<&'static Location<'static>>,
    /// How long the poll had held the shard when it was reported, nanoseconds (at least this long).
    pub held_ns: u64,
    /// The CPU the shard's thread ran while the watchdog watched the poll, nanoseconds; `None` without a reading.
    pub cpu_ns: Option<u64>,
    /// Whose the time was.
    pub whose: Whose,
}

impl Board {
    fn stalls(&self) -> Stalls {
        Stalls {
            stalled: self.stalled.load(Ordering::Acquire),
            episodes: self.episodes.load(Ordering::Relaxed),
            held_ns: self.held_ns.load(Ordering::Relaxed),
            moved: self.moved.load(Ordering::Relaxed),
            reports: self.reports.load(Ordering::Relaxed),
            suppressed: self.suppressed.load(Ordering::Relaxed),
        }
    }

    /// Writes a report (the watchdog's alone): `seq` odd around the fields, so a reader that sees the same even
    /// count before and after read them whole.
    fn write(&self, report: &StallReport) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.task.store(report.task.0.word(), Ordering::Relaxed);
        let at = report
            .spawned_at
            .map_or(std::ptr::null_mut(), |at| std::ptr::from_ref(at).cast_mut());
        self.spawned_at.store(at, Ordering::Relaxed);
        self.report_held_ns.store(report.held_ns, Ordering::Relaxed);
        self.cpu_ns
            .store(report.cpu_ns.unwrap_or(u64::MAX), Ordering::Relaxed);
        self.whose.store(report.whose as u8, Ordering::Relaxed);
        self.reports.store(report.number, Ordering::Relaxed);
        self.seq.store(seq.wrapping_add(2), Ordering::Release);
    }

    /// The latest report, read whole; `None` before the first. Retries while the watchdog writes, which it does
    /// at most [`REPORTS_PER_MINUTE`] times a minute, so the loop ends.
    fn read(&self) -> Option<StallReport> {
        loop {
            let before = self.seq.load(Ordering::Acquire);
            if before == 0 {
                return None;
            }
            if before & 1 == 1 {
                spin();
                continue;
            }
            let task = self.task.load(Ordering::Relaxed);
            let at = self.spawned_at.load(Ordering::Relaxed);
            let held_ns = self.report_held_ns.load(Ordering::Relaxed);
            let cpu_ns = self.cpu_ns.load(Ordering::Relaxed);
            let whose = self.whose.load(Ordering::Relaxed);
            let number = self.reports.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) != before {
                continue;
            }
            return Some(StallReport {
                number,
                task: TaskId(Encoded::from_word(task)),
                spawned_at: registry::location(at),
                held_ns,
                cpu_ns: (cpu_ns != u64::MAX).then_some(cpu_ns),
                whose: Whose::from_u8(whose),
            });
        }
    }
}

/// A wait for a writer mid-report: a spin, or under loom a yield, which loom needs to schedule the writer.
#[cfg(not(loom))]
fn spin() {
    std::hint::spin_loop();
}

/// A wait for a writer mid-report: a spin, or under loom a yield, which loom needs to schedule the writer.
#[cfg(loom)]
fn spin() {
    loom::thread::yield_now();
}

/// Whether the watchdog sees the shard `holder` names held in one poll past its limit now: placement sends no
/// new work there while it is. `false` for a shard gone, and for one no watchdog watches (a local or simulated
/// runtime's).
pub fn stalled(holder: SlotHolder) -> bool {
    registry::with_holder(holder, |entry| entry.board.stalled.load(Ordering::Acquire))
        .unwrap_or(false)
}

/// The shard's stalls as the watchdog counted them; `None` for a shard gone.
pub fn stalls(holder: SlotHolder) -> Option<Stalls> {
    registry::with_holder(holder, |entry| entry.board.stalls())
}

/// The latest stall reported on the shard; `None` before its first, and for a shard gone.
pub fn report(holder: SlotHolder) -> Option<StallReport> {
    registry::with_holder(holder, |entry| entry.board.read()).flatten()
}

/// How the watchdog judges a runtime's shards: from its configuration.
#[derive(Clone, Copy, Debug)]
struct Limits {
    batch: u64,
    /// The configured step budget: the quantum of a shard that tracks no wake.
    fixed_quantum_ns: u64,
    /// The prior of a shard that tracks its wake, its quantum until it has measured one.
    tracked_prior_ns: Option<u64>,
    /// The timer wheel's tick: the finest wait the runtime keeps, the watchdog's shortest look.
    tick_ns: u64,
}

impl Limits {
    fn of(config: &crate::runtime::RuntimeConfig) -> Limits {
        Limits {
            batch: u64::try_from(config.batch.max(1)).unwrap_or(u64::MAX),
            fixed_quantum_ns: config.step_budget_ns.max(1),
            tracked_prior_ns: config.wake_tracking.map(|tracking| tracking.prior_ns.max(1)),
            tick_ns: config.timer_tick_ns.max(1),
        }
    }

    /// The shard's quantum now: its online wake estimate while it tracks one (mirrored on its pulse at each fold;
    /// the prior before its first), else the configured step budget, as `Shard::quantum_ns` has it.
    fn quantum_ns(&self, entry: &Entry) -> u64 {
        match self.tracked_prior_ns {
            Some(prior) => match entry.pulse.wake_cost_ns() {
                0 => prior,
                measured => measured,
            },
            None => self.fixed_quantum_ns,
        }
    }

    /// The limit: a step's worth of polls that use their shares (the module's notes).
    fn limit_ns(&self, quantum_ns: u64) -> u64 {
        quantum_ns.saturating_mul(self.batch)
    }

    /// How long to wait between looks given the shortest limit now: half of it, never under a tick.
    fn look_every_ns(&self, shortest_limit_ns: u64) -> u64 {
        (shortest_limit_ns / 2).max(self.tick_ns)
    }
}

/// A poll the watchdog has seen running.
#[derive(Clone, Copy, Debug)]
struct Seen {
    polls: u64,
    task: u64,
    /// When the watchdog first saw it, on the monotonic clock.
    since_ns: u64,
    /// The shard thread's CPU clock then.
    cpu: Option<CpuReading>,
    /// Whether it has been judged held past the limit.
    stalled: bool,
}

/// One shard as the watchdog keeps it.
#[derive(Debug)]
struct Watched {
    holder: SlotHolder,
    seen: Option<Seen>,
    /// Reports it may still make this minute, and when the allowance last grew.
    tokens: u64,
    refilled_ns: u64,
}

impl Watched {
    fn new(holder: SlotHolder, now_ns: u64) -> Watched {
        Watched {
            holder,
            seen: None,
            tokens: REPORTS_PER_MINUTE,
            refilled_ns: now_ns,
        }
    }

    /// One look at `now_ns`: a poll seen at two looks is held at least the time between; past the limit it is a
    /// stall, reported (within the cap) and marked for placement; a poll that ended ends its episode.
    fn look(&mut self, limits: &Limits, now_ns: u64) -> Option<u64> {
        let holder = self.holder;
        let looked = registry::with_holder(holder, |entry| {
            (entry.watch.running(), limits.quantum_ns(entry))
        });
        let Some((running, quantum_ns)) = looked else {
            self.end_episode(now_ns);
            self.seen = None;
            return None;
        };
        let limit_ns = limits.limit_ns(quantum_ns);
        match (running, self.seen) {
            (Some((polls, _)), Some(seen)) if seen.polls == polls => {
                if !seen.stalled && now_ns.saturating_sub(seen.since_ns) >= limit_ns {
                    self.stall(seen, now_ns, quantum_ns);
                }
            }
            _ => {
                self.end_episode(now_ns);
                self.seen = running.map(|(polls, task)| Seen {
                    polls,
                    task,
                    since_ns: now_ns,
                    cpu: registry::shard_cpu(holder),
                    stalled: false,
                });
            }
        }
        Some(limit_ns)
    }

    /// The poll `seen` has held the shard past the limit: marked, counted and (within the cap) reported.
    fn stall(&mut self, seen: Seen, now_ns: u64, quantum_ns: u64) {
        self.seen = Some(Seen {
            stalled: true,
            ..seen
        });
        let holder = self.holder;
        let held_ns = now_ns.saturating_sub(seen.since_ns);
        let cpu_now = registry::shard_cpu(holder);
        let cpu_ns = match (seen.cpu, cpu_now) {
            (Some(before), Some(after)) => Some(after.spent_ns.saturating_sub(before.spent_ns)),
            _ => None,
        };
        let whose = match cpu_ns {
            None => Whose::Unknown,
            Some(ran) if ran > quantum_ns => Whose::Task,
            Some(_) => off_cpu(holder),
        };
        let reportable = self.take_token(now_ns);
        let _ = registry::with_holder(holder, |entry| {
            let board = &entry.board;
            board.stalled.store(true, Ordering::Release);
            board.episodes.fetch_add(1, Ordering::Relaxed);
            if !reportable {
                board.suppressed.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let number = board.reports.load(Ordering::Relaxed).saturating_add(1);
            let task = TaskId(Encoded::from_word(seen.task));
            board.write(&StallReport {
                number,
                task,
                spawned_at: entry.spawned_at(task.0.slot()),
                held_ns,
                cpu_ns,
                whose,
            });
        });
    }

    /// One report's allowance, grown at [`REPORTS_PER_MINUTE`] a minute up to that many.
    fn take_token(&mut self, now_ns: u64) -> bool {
        let every_ns = NS_PER_MINUTE / REPORTS_PER_MINUTE;
        let grown = now_ns.saturating_sub(self.refilled_ns) / every_ns;
        if grown > 0 {
            self.tokens = self.tokens.saturating_add(grown).min(REPORTS_PER_MINUTE);
            self.refilled_ns = self
                .refilled_ns
                .saturating_add(grown.saturating_mul(every_ns));
        }
        match self.tokens.checked_sub(1) {
            Some(left) => {
                self.tokens = left;
                true
            }
            None => false,
        }
    }

    /// The poll it saw ended (or the shard went): an episode under way ends, its time counted, its mark lifted.
    fn end_episode(&mut self, now_ns: u64) {
        let Some(seen) = self.seen.filter(|seen| seen.stalled) else {
            return;
        };
        let held_ns = now_ns.saturating_sub(seen.since_ns);
        let _ = registry::with_holder(self.holder, |entry| {
            entry.board.held_ns.fetch_add(held_ns, Ordering::Relaxed);
            entry.board.stalled.store(false, Ordering::Release);
        });
    }

    fn stalled(&self) -> bool {
        self.seen.is_some_and(|seen| seen.stalled)
    }
}

/// Whose the time off the CPU was: the thread's state now, where the platform tells it.
fn off_cpu(holder: SlotHolder) -> Whose {
    match registry::shard_runnable(holder) {
        Some(true) => Whose::Host,
        Some(false) => Whose::Blocked,
        None => Whose::OffCpu,
    }
}

/// A runtime's watchdog: its thread, stopped and joined when this drops.
///
/// **It allocates nothing once started**: what it keeps is made on the starting thread and moved to it, it waits
/// by parking (std's channel would make its waiter's context at its first blocking receive) and reads the stop
/// by a receive that never blocks, and `start` returns only once the thread runs. So a runtime's work measured
/// for allocations (kiln-rt's `tests/service_driverless_receipts.rs` counts every allocation in the process) never
/// counts one of the watchdog's, as it did while it built its list and its channel's waiter on its own thread,
/// whenever the OS first ran it.
#[derive(Debug)]
pub(crate) struct Watchdog {
    /// Dropped to stop the thread, which is unparked to see it.
    stop: Option<SyncSender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Starts watching `holders`, the shards of a runtime built from `config` whose movable queues are `movable`
    /// (in the same order); returns once its thread runs.
    pub(crate) fn start(
        holders: Vec<SlotHolder>,
        config: &crate::runtime::RuntimeConfig,
        movable: Option<std::sync::Arc<crate::movable::Pool>>,
    ) -> Result<Watchdog, RtError> {
        let limits = Limits::of(config);
        let now = crate::machine::clock::monotonic_ns();
        let shards: Vec<Watched> = holders
            .into_iter()
            .map(|holder| Watched::new(holder, now))
            .collect();
        let (stop, stopped) = sync_channel(1);
        let (running, started) = sync_channel(1);
        #[allow(
            clippy::disallowed_methods,
            reason = "a runtime's watchdog is one thread outside its shards that it starts and joins (kiln's goal 6, crate::watch)"
        )]
        let thread = std::thread::Builder::new()
            .name("kiln-rt-watchdog".into())
            .spawn(move || {
                let _ = running.send(());
                // Let go now, while the starting thread still holds its end: the channel is freed there.
                drop(running);
                watch(shards, limits, &stopped, movable.as_deref());
            })
            .map_err(|error| RtError::DriverRefused {
                call: "thread spawn",
                code: error.raw_os_error(),
            })?;
        let mut watchdog = Watchdog {
            stop: Some(stop),
            thread: Some(thread),
        };
        if started.recv().is_err() {
            watchdog.stop();
            return Err(RtError::DriverRefused {
                call: "thread spawn",
                code: None,
            });
        }
        Ok(watchdog)
    }

    /// Stops the thread and joins it.
    pub(crate) fn stop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            if thread.join().is_err() {
                registry::note_unreported_failure();
            }
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The watchdog's thread: looks at every shard each half limit until its runtime drops its end of `stopped`.
fn watch(
    mut shards: Vec<Watched>,
    limits: Limits,
    stopped: &Receiver<()>,
    movable: Option<&crate::movable::Pool>,
) {
    let mut held: Option<Box<SpawnRequest>> = None;
    let mut every_ns = limits.look_every_ns(limits.limit_ns(limits.fixed_quantum_ns));
    loop {
        // A wait that ends early (a spurious unpark) only looks sooner.
        std::thread::park_timeout(Duration::from_nanos(every_ns));
        // Until the runtime drops its end (or sends on it).
        if !matches!(stopped.try_recv(), Err(TryRecvError::Empty)) {
            break;
        }
        let now = crate::machine::clock::monotonic_ns();
        let shortest = shards
            .iter_mut()
            .filter_map(|shard| shard.look(&limits, now))
            .min();
        if let Some(shortest) = shortest {
            every_ns = limits.look_every_ns(shortest);
        }
        move_waiting(&shards, &mut held);
        if let Some(pool) = movable {
            hand_off(&shards, pool);
        }
    }
    // A request held for want of a shard to take it is dropped, which answers its receipt `Terminated`.
    drop(held);
    for shard in &mut shards {
        shard.end_episode(crate::machine::clock::monotonic_ns());
    }
}

/// Hands the movable spawns that wait for held shards to the runtime's other shards: the one held from the last
/// look first, then each held shard's queue in turn. One that no shard takes now waits in `held` for the next
/// look.
fn move_waiting(shards: &[Watched], held: &mut Option<Box<SpawnRequest>>) {
    if shards.iter().all(Watched::stalled) {
        return;
    }
    if let Some(request) = held.take()
        && let Err(request) = place(shards, None, request)
    {
        *held = Some(request);
        return;
    }
    for (index, from) in shards.iter().enumerate().filter(|(_, shard)| shard.stalled()) {
        while let Some(request) =
            registry::with_holder(from.holder, Entry::take_movable).flatten()
        {
            match place(shards, Some(index), request) {
                Ok(()) => {
                    let _ = registry::with_holder(from.holder, |entry| {
                        entry.board.moved.fetch_add(1, Ordering::Relaxed);
                    });
                }
                Err(request) => {
                    *held = Some(request);
                    return;
                }
            }
        }
    }
}

/// Hands the movable tasks queued on each held shard to the runtime's shared queues, parked siblings kicked to take
/// them (`crate::movable::Pool::hand_off`); counted as moved on the held shard's board. Not while every shard is
/// held: none could take them.
fn hand_off(shards: &[Watched], pool: &crate::movable::Pool) {
    if shards.iter().all(Watched::stalled) {
        return;
    }
    for (index, shard) in shards.iter().enumerate().filter(|(_, shard)| shard.stalled()) {
        let moved = pool.hand_off(index);
        if moved > 0 {
            let _ = registry::with_holder(shard.holder, |entry| {
                entry.board.moved.fetch_add(moved, Ordering::Relaxed);
            });
        }
    }
}

/// Queues `request` on the first shard after `from` that is not held and has room; the request back when none.
fn place(
    shards: &[Watched],
    from: Option<usize>,
    mut request: Box<SpawnRequest>,
) -> Result<(), Box<SpawnRequest>> {
    let count = shards.len();
    let first = from.map_or(0, |from| from.saturating_add(1));
    for step in 0..count {
        let Some(index) = first.wrapping_add(step).checked_rem(count) else {
            break;
        };
        if Some(index) == from {
            continue;
        }
        let Some(to) = shards.get(index).filter(|shard| !shard.stalled()) else {
            continue;
        };
        match registry::push_movable(to.holder, request) {
            Ok(()) => return Ok(()),
            Err(back) => request = back,
        }
    }
    Err(request)
}

#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
    use super::*;
    use crate::mem::loom_bounds;

    /// The shard begins a poll, ends it and begins the next while the watchdog reads which poll runs: a read whole
    /// names the task of the poll its count is, never the next poll's task under the last one's count. Without the
    /// count read again after the word, an interleaving pairs the first poll's count with the second's task.
    #[test]
    fn a_poll_read_whole_names_its_own_task() {
        loom_bounds::explore("watch: a poll's count and its task", || {
            let watch: &'static Watch = Box::leak(Box::new(Watch::default()));
            let shard = loom::thread::spawn(move || {
                watch.begin(11);
                watch.end();
                watch.begin(22);
            });
            let watchdog = loom::thread::spawn(move || watch.running());
            shard.join().unwrap();
            let seen = watchdog.join().unwrap();
            if let Some((polls, task)) = seen {
                assert!(
                    (polls == 1 && task == 11) || (polls == 3 && task == 22),
                    "poll {polls} read with task {task}"
                );
            }
        });
    }

    /// The watchdog writes one report and then another while a reader reads: what the reader takes is one report
    /// whole, every field of the same one.
    #[test]
    fn a_report_read_whole_is_one_report() {
        loom_bounds::explore("watch: a report read whole", || {
            let board: &'static Board = Box::leak(Box::new(Board::default()));
            let report = |n: u64| StallReport {
                number: n,
                task: TaskId(Encoded::from_word(n)),
                spawned_at: None,
                held_ns: n,
                cpu_ns: Some(n),
                whose: Whose::Task,
            };
            let writer = loom::thread::spawn(move || {
                board.write(&report(1));
                board.write(&report(2));
            });
            let reader = loom::thread::spawn(move || board.read());
            writer.join().unwrap();
            let read = reader.join().unwrap();
            if let Some(r) = read {
                assert!(r.number == 1 || r.number == 2, "{r:?}");
                assert_eq!(r, report(r.number), "a report read whole");
            }
        });
    }
}
