//! The compute pool (kiln's departure, ORIGIN.md): a small work-stealing pool for CPU-heavy jobs (building
//! snapshots, serializing big payloads, compressing, diffing, hashing), so that none of it runs on a shard,
//! where one heavy request holds up every other request on that shard [nginx's thread pools; Node's "Don't
//! Block the Event Loop"]. The blocking pool ([`crate::blocking`]) is for calls that block a thread; this one
//! is for work that keeps a CPU busy, and the two are kept apart: CPU-bound jobs on the pool meant for
//! blocking I/O starve it, and that pool's size is set by devices, not cores [tokio's `spawn_blocking`;
//! Erlang's dirty CPU and dirty I/O schedulers].
//!
//! **Its size** is the CPU the process is granted less its shards ([`Grant`], [`workers_for`]): a cgroup's
//! quota or the cores its affinity mask (a cpuset) allows, else the OS's count, so a busy pool and busy shards
//! together ask no more of the machine than it gives the process. Erlang caps its dirty CPU schedulers at its
//! normal ones for the same reason: "If the amount of dirty CPU schedulers was allowed to be unlimited, dirty
//! CPU bound jobs would potentially starve normal jobs."
//!
//! **Its threads run below the shards** (`crate::prioritysys`, with the measurements that chose it), at a rank
//! that keeps a share of the CPU against other work at the process's priority ([`Rank::Below`]: Linux's batch
//! policy at nice +10, macOS's utility QoS class, Windows' below-normal level), as requests wait on its jobs and
//! the OS's idle rank would leave them waiting on whatever else busies the process's cgroup. On Linux a shard
//! asks for a slice of its step ([`prefer_current_thread`]), so where it wakes on a CPU a pool thread holds, it
//! takes the CPU at once. Where the machine has room the pool runs at full speed. A job holds no lock a shard
//! may wait on: its thread may be kept off the CPU while it holds it.
//!
//! **Work-stealing** [Blumofe and Leiserson; rayon]: jobs from the shards and other threads go into one
//! injector queue; each worker keeps a queue of its own, where a job in pieces goes on after a piece, and a
//! worker with nothing to do takes from the injector, then steals from the others' queues. So when every
//! worker is busy the jobs run one after another, and when one is idle it takes what waits
//! (`crossbeam-deque`: Chase and Lev's deque and a lock-free injector, no lock on any path).
//!
//! **In pieces.** A job that may run long is written in pieces ([`run_pieces`]): between units of its work it
//! asks its [`Turn`] whether to stop, and is told yes once its piece has run [`Config::piece_ns`] and another
//! job waits for a thread; it then returns [`Step::More`] and goes on after the jobs waiting. A job does not
//! hold a thread for long while others wait [Node: "each relatively long Task effectively decreases the size of
//! the Worker Pool by one until it is completed"], and where none waits it goes on at once, as a shard's
//! `maybe_yield` does. A job of one closure ([`run`]) is one piece.
//!
//! **Results** return through a [`crate::sync::oneshot`], which the asking task awaits. A job that unwinds ends
//! the job, not its worker (an unwind boundary); its receiver sees the sender gone. A job gives back its place
//! in the pool's bound before its result is delivered, so a task that sees it end may submit again at once.
//!
//! **Bounded by admission:** at most [`Config::queued`] jobs are admitted at once, running or waiting; past it a
//! job is refused `Capacity` and handed back ([`Refused`]), before anything is queued.
//!
//! **Observed:** a consumer may be told how long each job waited for a thread and how long each piece ran
//! ([`observe`]), to show where its heavy work went and that no piece held a thread long.
//!
//! **No locks** (ORIGIN.md's rule 1): the queues are crossbeam's; an idle worker parks on its own thread after
//! saying so in its slot's state word, and a submitter wakes one idle worker by claiming that word with a
//! compare-and-swap, one wake to one worker. One pool per process, set once by [`start`].

use std::fmt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::Thread;

use crossbeam_deque::{Injector, Steal, Stealer, Worker};

use crate::error::RtError;
use crate::machine::clock::shard_clock_ns;
use crate::machine::facts::CpuBudget;
use crate::sync::{OneshotReceiver, OneshotSender, oneshot};
use crate::unwind::contained;

// ------------------------------------------------------------------------------------------ sizing

/// The CPU this process is granted, and what granted it: the compute pool's size is this less the shards
/// ([`workers_for`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    /// CPUs the process's threads can run on at once.
    pub cpus: usize,
    /// What set it.
    pub by: GrantedBy,
}

/// What set a [`Grant`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantedBy {
    /// A cgroup's CPU quota (v2's `cpu.max`; v1's `cpu.cfs_quota_us` over `cpu.cfs_period_us`): the whole CPUs
    /// it buys at once, fewer than the cores the process may run on.
    Quota {
        /// Microseconds of CPU time in each period.
        quota_us: u64,
        /// The period, microseconds.
        period_us: u64,
        /// The cores the process may run on.
        cores: usize,
    },
    /// The cores the process may run on: its affinity mask, which a cpuset narrows; no quota is tighter.
    Cores,
    /// The OS's count of the CPUs the process may use (`available_parallelism`): the cores could not be read.
    Parallelism,
}

impl Grant {
    /// This process's grant, queried now ([`crate::machine::facts::cpu_grant`]).
    pub fn of_process() -> Grant {
        let (cores, budget) = crate::machine::facts::cpu_grant();
        let parallelism = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        Grant::of(cores, budget, parallelism)
    }

    /// The grant of `cores` the process may run on under `budget`, with `parallelism` the OS's count where no
    /// core could be read: the tighter of the cores and the whole CPUs the budget buys.
    pub fn of(cores: usize, budget: Option<CpuBudget>, parallelism: usize) -> Grant {
        if cores == 0 {
            return Grant {
                cpus: parallelism.max(1),
                by: GrantedBy::Parallelism,
            };
        }
        let buys = budget.map(|budget| {
            (
                budget,
                usize::try_from(budget.whole_cpus()).unwrap_or(usize::MAX),
            )
        });
        match buys {
            Some((budget, cpus)) if cpus < cores => Grant {
                cpus,
                by: GrantedBy::Quota {
                    quota_us: budget.quota_us,
                    period_us: budget.period_us,
                    cores,
                },
            },
            _ => Grant {
                cpus: cores,
                by: GrantedBy::Cores,
            },
        }
    }
}

impl fmt::Display for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.by {
            GrantedBy::Quota {
                quota_us,
                period_us,
                cores,
            } => write!(
                f,
                "{} CPUs: the cgroup's quota of {quota_us} µs per {period_us} µs, over {cores} cores",
                self.cpus
            ),
            GrantedBy::Cores => write!(f, "{} CPUs: the cores this process may run on", self.cpus),
            GrantedBy::Parallelism => write!(
                f,
                "{} CPUs: the OS's count, as the cores could not be read",
                self.cpus
            ),
        }
    }
}

/// The pool's threads for a process with `shards` shards: its grant less the shards, at least one.
pub fn workers_for(grant: &Grant, shards: usize) -> usize {
    grant.cpus.saturating_sub(shards).max(1)
}

// ---------------------------------------------------------------------------------------- priority

/// A thread's scheduling priority as its OS reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// Linux's idle policy, `SCHED_IDLE` (its nice value counts only if it leaves the policy).
    Idle {
        /// The nice value.
        nice: i32,
    },
    /// Linux's batch policy, `SCHED_BATCH`, at a nice value.
    Batch {
        /// The nice value.
        nice: i32,
    },
    /// Linux's normal policy (or another but idle and batch), at a nice value.
    Other {
        /// The nice value.
        nice: i32,
    },
    /// A macOS QoS class, as `sys/qos.h` numbers it ([`QOS_CLASS_UTILITY`]).
    Qos {
        /// The class.
        class: u32,
    },
    /// A Windows thread priority level (`THREAD_PRIORITY_BELOW_NORMAL` is -1).
    Level {
        /// The level.
        level: i32,
    },
    /// The OS gave no reading.
    Unknown,
}

/// Format: macOS's utility QoS class (`sys/qos.h`, `QOS_CLASS_UTILITY`).
pub const QOS_CLASS_UTILITY: u32 = 0x11;
/// Format: macOS's background QoS class (`sys/qos.h`, `QOS_CLASS_BACKGROUND`).
pub const QOS_CLASS_BACKGROUND: u32 = 0x09;
/// Format: macOS's default QoS class (`sys/qos.h`, `QOS_CLASS_DEFAULT`).
pub const QOS_CLASS_DEFAULT: u32 = 0x15;

impl Priority {
    /// Whether a thread at this priority runs below one at `other`, as their OS ranks them: on Linux the idle
    /// policy below every other, then a higher nice value, then the batch policy at the same one; on macOS a
    /// lower QoS class (unspecified, zero, reads as the default); on Windows a lower level. Priorities of
    /// different OSes compare as neither below.
    pub fn is_below(self, other: Priority) -> bool {
        match (self.linux_rank(), other.linux_rank()) {
            (Some(mine), Some(theirs)) => return mine > theirs,
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => {}
        }
        match (self, other) {
            (Priority::Qos { class }, Priority::Qos { class: base }) => {
                let effective = |class: u32| if class == 0 { QOS_CLASS_DEFAULT } else { class };
                effective(class) < effective(base)
            }
            (Priority::Level { level }, Priority::Level { level: base }) => level < base,
            _ => false,
        }
    }

    /// A Linux priority's lowness, compared in order: the idle policy, the nice value, the batch policy.
    fn linux_rank(self) -> Option<(bool, i32, bool)> {
        match self {
            Priority::Idle { nice } => Some((true, nice, false)),
            Priority::Batch { nice } => Some((false, nice, true)),
            Priority::Other { nice } => Some((false, nice, false)),
            Priority::Qos { .. } | Priority::Level { .. } | Priority::Unknown => None,
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Priority::Idle { nice } => write!(f, "SCHED_IDLE (nice {nice})"),
            Priority::Batch { nice } => write!(f, "SCHED_BATCH at nice {nice}"),
            Priority::Other { nice } => write!(f, "nice {nice}"),
            Priority::Qos { class } if class == QOS_CLASS_UTILITY => f.write_str("the utility QoS class"),
            Priority::Qos { class } if class == QOS_CLASS_BACKGROUND => f.write_str("the background QoS class"),
            Priority::Qos { class } => write!(f, "QoS class {class:#x}"),
            Priority::Level { level } => write!(f, "thread priority level {level}"),
            Priority::Unknown => f.write_str("unknown"),
        }
    }
}

/// A priority call the OS refused: the thread runs at the process's priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The call.
    pub call: &'static str,
    /// The OS's error code, when it gave one.
    pub code: Option<i32>,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "{} refused (OS error {code})", self.call),
            None => write!(f, "{} refused", self.call),
        }
    }
}

/// How far below the shards the pool's threads go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rank {
    /// Below the shards, with a share of the CPU against any thread at the process's priority all the same, so
    /// a job a request waits on never waits on other work for good: Linux's `SCHED_BATCH` at a nice value 10
    /// above (a weight of 110 against 1024: beside one thread spinning on its core, a 5 ms job took 48 ms at
    /// the median, `benches/compute_wait.rs`), macOS's utility QoS class, Windows' below-normal level. A shard
    /// that asks for a slice of its step ([`prefer_current_thread`]) takes the CPU from it at once on Linux.
    Below,
    /// The OS's lowest that still runs a thread whenever its CPU has nothing else: Linux's `SCHED_IDLE`, macOS's
    /// background QoS class. A waking shard takes the CPU from it at once, but any thread at the process's
    /// priority that shares its CPU budget holds it off almost entirely (a weight of 3 against 1024: beside
    /// one spinning thread, a 5 ms job took 6.1 s), and a thread can't leave Linux's idle policy without
    /// `RLIMIT_NICE`'s room: for a process that knows it stays alone in its cgroup, which kiln doesn't (its
    /// UI server and its agents' processes join its cgroup as it runs).
    Idle,
}

/// Asks the OS to let the calling thread, a shard, take its CPU from threads whose slices are longer as it
/// wakes: on Linux an EEVDF slice of `slice_ns` (`sched_attr`'s `sched_runtime` on the normal policy, Linux
/// 6.12 and later; the kernel holds it between 0.1 ms and 100 ms), so a shard whose slice is its step quantum
/// preempts a pool thread at [`Rank::Below`] where the two share a CPU. Measured on one core shared with four
/// such threads, a shard's p99 wake lateness was 1.1 ms without it and 0.21 ms with a 250 µs slice, its calm
/// rounds 0.20 to 0.28 ms (`benches/compute_priority.rs`, `SHARD_SLICE_NS`, 2026-11-04). Its nice value and
/// policy stay as they are. Elsewhere there is no such call: refused, and nothing changes.
pub fn prefer_current_thread(slice_ns: u64) -> Result<(), Refusal> {
    crate::prioritysys::prefer_current_thread(slice_ns)
}

/// The calling thread's priority as its OS reports it (a pool thread's is lowered; a shard's is not).
pub fn thread_priority() -> Priority {
    crate::prioritysys::of_current_thread()
}

/// Lowers the calling thread's priority to `rank` as the pool's threads lower theirs (the module's notes):
/// for a thread that does the pool's kind of work outside it (a consumer's fallback where no pool runs).
pub fn lower_current_thread(rank: Rank) -> Result<Priority, Refusal> {
    crate::prioritysys::lower_current_thread(rank)
}

// --------------------------------------------------------------------------------------------- jobs

/// What a job in pieces says after a piece.
#[derive(Debug, PartialEq, Eq)]
pub enum Step<T> {
    /// Done, with its result.
    Done(T),
    /// More to do: it goes on in a later piece, after the jobs waiting.
    More,
}

/// A job as the pool holds it: called once a piece with the piece's turn, until it says it is done.
type Job = Box<dyn FnMut(&Turn<'_>) -> bool + Send>;

/// A piece's turn on a pool thread: whether the job should stop here and let a waiting job have the thread.
pub struct Turn<'a> {
    started_ns: u64,
    piece_ns: u64,
    local: &'a Worker<Job>,
    injector: &'a Injector<Job>,
}

impl Turn<'_> {
    /// Whether the piece should end here: it has run its piece's time ([`Config::piece_ns`]) and another job
    /// waits for a thread. A job in pieces asks between units of its work and returns [`Step::More`] when told
    /// yes; where no job waits it goes on.
    pub fn over(&self) -> bool {
        shard_clock_ns().saturating_sub(self.started_ns) >= self.piece_ns
            && (!self.injector.is_empty() || !self.local.is_empty())
    }
}

/// A job the pool did not take, handed back with why.
pub struct Refused<J> {
    /// Why: `Capacity` at the pool's bound, `BadConfig` with no pool running or one stopping, or the one-shot
    /// table's refusal.
    pub error: RtError,
    /// The job, to submit again or run elsewhere.
    pub job: J,
}

impl<J> fmt::Debug for Refused<J> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Refused")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// The pool's shape, the consumer's to derive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Threads: [`workers_for`] the process's [`Grant`].
    pub workers: usize,
    /// The most jobs admitted at once, running or waiting.
    pub queued: usize,
    /// A piece's time in nanoseconds: how long a job in pieces goes on while another waits.
    pub piece_ns: u64,
    /// How far below the shards its threads go ([`Rank`]; kiln's is [`Rank::Below`]).
    pub rank: Rank,
}

/// What [`start`] started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Started {
    /// Threads running (fewer than asked when the OS refused some).
    pub workers: usize,
    /// The priority the first thread took, or the call its OS refused (every thread asks for the same).
    pub priority: Result<Priority, Refusal>,
    /// The threads whose priority the OS lowered.
    pub lowered: usize,
}

/// What the pool has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Jobs accepted.
    pub accepted: u64,
    /// Jobs refused at the pool's bound.
    pub refused: u64,
    /// Jobs finished, with a result or without (unwound).
    pub finished: u64,
    /// Of those, jobs that unwound: their receivers see the sender gone; the pool lives on.
    pub panicked: u64,
    /// Pieces run (a job of one closure is one).
    pub pieces: u64,
    /// Pieces after which their job went on later, giving its thread to a waiting job.
    pub yielded: u64,
    /// Jobs a worker took from another's queue.
    pub stolen: u64,
    /// The longest piece any worker has run, nanoseconds.
    pub longest_piece_ns: u64,
}

/// What the pool tells its consumer ([`observe`]), in nanoseconds on the pool's clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observed {
    /// A job's first piece started, this long after the job was submitted.
    Waited(u64),
    /// A piece ran this long.
    Ran(u64),
}

/// The consumer's observer of the pool's jobs ([`observe`]).
pub type Observer = fn(Observed);

/// The consumer's observer, once set.
static OBSERVER: OnceLock<Observer> = OnceLock::new();

/// Has the pool tell `observer` how long each job waited for a thread, as its first piece starts, and how long
/// each piece ran: the consumer's measurements of how the pool serves the work it sends, beside [`stats`]'
/// counts. The pieces' times are read already; a job's submission reads the clock only when observed. Once per
/// process: false when an observer was set already.
pub fn observe(observer: Observer) -> bool {
    OBSERVER.set(observer).is_ok()
}

/// Tells the consumer's observer, if any, inside an unwind boundary (its code).
fn tell(observed: Observed) {
    if let Some(observer) = OBSERVER.get() {
        let _ = contained(|| observer(observed));
    }
}

static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static FINISHED: AtomicU64 = AtomicU64::new(0);
static PANICKED: AtomicU64 = AtomicU64::new(0);
static PIECES: AtomicU64 = AtomicU64::new(0);
static YIELDED: AtomicU64 = AtomicU64::new(0);
static STOLEN: AtomicU64 = AtomicU64::new(0);
static LONGEST_PIECE_NS: AtomicU64 = AtomicU64::new(0);

/// What the pool has done.
pub fn stats() -> Stats {
    Stats {
        accepted: ACCEPTED.load(Ordering::Relaxed),
        refused: REFUSED.load(Ordering::Relaxed),
        finished: FINISHED.load(Ordering::Relaxed),
        panicked: PANICKED.load(Ordering::Relaxed),
        pieces: PIECES.load(Ordering::Relaxed),
        yielded: YIELDED.load(Ordering::Relaxed),
        stolen: STOLEN.load(Ordering::Relaxed),
        longest_piece_ns: LONGEST_PIECE_NS.load(Ordering::Relaxed),
    }
}

// --------------------------------------------------------------------------------------------- pool

/// Format: a worker's slot state while it runs or looks for work.
const RUNNING: u8 = 0;
/// Format: a worker's slot state while it is parked, or about to park, with nothing to do.
const IDLE: u8 = 1;

/// One worker as the others see it: its state word and its thread, to unpark.
struct Slot {
    state: AtomicU8,
    thread: Thread,
}

/// The pool, once started.
struct Pool {
    injector: Injector<Job>,
    stealers: Box<[Stealer<Job>]>,
    slots: Box<[Slot]>,
    /// Workers idle or about to be: a submitter wakes one only when some are.
    idle: AtomicUsize,
    /// Where the next wake's search starts, so wakes go round the workers.
    next: AtomicUsize,
    /// Jobs admitted and not yet ended.
    held: AtomicUsize,
    queued: usize,
    piece_ns: u64,
    stopping: AtomicBool,
    /// Workers still serving; the last out wakes the stopper.
    live: AtomicUsize,
    stopper: OnceLock<Thread>,
}

static POOL: OnceLock<Pool> = OnceLock::new();
/// Claimed by the one [`start`] that builds the pool.
static STARTING: AtomicBool = AtomicBool::new(false);

/// A job's place in the pool's bound, given back when the job ends: done, unwound, or dropped unrun.
struct Held(&'static Pool);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.held.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Starts the pool: `config.workers` threads, each lowering its own priority first. Refused `BadConfig` for no
/// workers or no room, or when one was started already in this process. A thread the OS refuses ends the
/// starting there and the pool runs with those it has; none at all is refused.
pub fn start(config: Config) -> Result<Started, RtError> {
    if config.workers == 0 || config.queued == 0 {
        return Err(RtError::BadConfig {
            what: "a compute pool with no workers or no room",
        });
    }
    if POOL.get().is_some() || STARTING.swap(true, Ordering::AcqRel) {
        return Err(RtError::BadConfig {
            what: "a second compute pool in one process",
        });
    }
    let (report, reports) = sync_channel(config.workers);
    let (stealers, slots) = spawn_workers(config.workers, config.rank, &report);
    drop(report);
    let workers = slots.len();
    if workers == 0 {
        return Err(RtError::BadConfig {
            what: "the OS refused every compute pool thread",
        });
    }
    let pool = POOL.get_or_init(|| Pool {
        injector: Injector::new(),
        stealers: stealers.into_boxed_slice(),
        slots: slots.into_boxed_slice(),
        idle: AtomicUsize::new(0),
        next: AtomicUsize::new(0),
        held: AtomicUsize::new(0),
        queued: config.queued,
        piece_ns: config.piece_ns,
        stopping: AtomicBool::new(false),
        live: AtomicUsize::new(workers),
        stopper: OnceLock::new(),
    });
    // Each worker waits for the pool before its first job: let them go.
    for slot in &pool.slots {
        slot.thread.unpark();
    }
    let outcomes: Vec<Result<Priority, Refusal>> = reports.iter().take(workers).collect();
    Ok(Started {
        workers,
        priority: outcomes.first().copied().unwrap_or(Err(Refusal {
            call: "no worker reported its priority",
            code: None,
        })),
        lowered: outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
    })
}

/// Starts up to `count` workers, each with its own queue; stops at the first thread the OS refuses.
fn spawn_workers(
    count: usize,
    rank: Rank,
    report: &SyncSender<Result<Priority, Refusal>>,
) -> (Vec<Stealer<Job>>, Vec<Slot>) {
    let mut stealers = Vec::new();
    let mut slots = Vec::new();
    for index in 0..count {
        let local = Worker::new_fifo();
        let stealer = local.stealer();
        let report = report.clone();
        #[allow(
            clippy::disallowed_methods,
            reason = "the compute pool's threads (ORIGIN.md, kiln's departures): started once, at the pool's start"
        )]
        let spawned = std::thread::Builder::new()
            .name(format!("kiln-compute-{index}"))
            .spawn(move || work(index, rank, &local, &report));
        let Ok(handle) = spawned else {
            break;
        };
        stealers.push(stealer);
        slots.push(Slot {
            state: AtomicU8::new(RUNNING),
            thread: handle.thread().clone(),
        });
    }
    (stealers, slots)
}

/// A worker: lowers its own priority to `rank` and says what the OS took, waits for the pool, then serves it
/// until it stops.
fn work(
    index: usize,
    rank: Rank,
    local: &Worker<Job>,
    report: &SyncSender<Result<Priority, Refusal>>,
) {
    let _ = report.send(crate::prioritysys::lower_current_thread(rank));
    let pool = loop {
        match POOL.get() {
            Some(pool) => break pool,
            None => std::thread::park(),
        }
    };
    pool.serve(index, local);
    pool.exited();
}

impl Pool {
    /// Runs pieces until the pool stops and nothing is left: the next job found, else a rest.
    fn serve(&self, index: usize, local: &Worker<Job>) {
        // Turns alternate which queue is asked first, so neither the injector's jobs nor the ones going on
        // here wait more than a turn of the other's.
        let mut global_first = false;
        loop {
            global_first = !global_first;
            if let Some(job) = self.find(index, local, global_first) {
                if !self.injector.is_empty() || !local.is_empty() {
                    self.wake_one();
                }
                self.run_piece(job, local);
            } else if !self.rest(index, local) {
                return;
            }
        }
    }

    /// The next job: the injector's and this worker's own, in the turn's order, else one stolen.
    fn find(&self, index: usize, local: &Worker<Job>, global_first: bool) -> Option<Job> {
        let found = if global_first {
            take(|| self.injector.steal()).or_else(|| local.pop())
        } else {
            local.pop().or_else(|| take(|| self.injector.steal()))
        };
        found.or_else(|| self.steal(index))
    }

    /// A job from another worker's queue, the others asked in turn from the next one along.
    fn steal(&self, index: usize) -> Option<Job> {
        let count = self.stealers.len();
        let stolen = (1..count).find_map(|offset| {
            let at = index.checked_add(offset)?.checked_rem(count)?;
            let stealer = self.stealers.get(at)?;
            take(|| stealer.steal())
        });
        if stolen.is_some() {
            STOLEN.fetch_add(1, Ordering::Relaxed);
        }
        stolen
    }

    /// Runs one piece of `job`; a job that goes on waits in this worker's queue, where an idle worker may
    /// steal it.
    fn run_piece(&self, mut job: Job, local: &Worker<Job>) {
        let started_ns = shard_clock_ns();
        let turn = Turn {
            started_ns,
            piece_ns: self.piece_ns,
            local,
            injector: &self.injector,
        };
        let done = job(&turn);
        PIECES.fetch_add(1, Ordering::Relaxed);
        let ran_ns = shard_clock_ns().saturating_sub(started_ns);
        LONGEST_PIECE_NS.fetch_max(ran_ns, Ordering::Relaxed);
        tell(Observed::Ran(ran_ns));
        if !done {
            YIELDED.fetch_add(1, Ordering::Relaxed);
            local.push(job);
        }
    }

    /// Nothing found: says so in the worker's slot, looks once more, and parks until a submitter claims the
    /// slot. False once the pool stops and nothing is left, when the worker ends.
    ///
    /// The slot is marked idle and counted (sequentially consistent) before the second look, and a submitter
    /// queues its job before it reads the count: either the look finds the job, or the submitter finds the
    /// count and claims an idle slot. No job waits while every worker parks.
    fn rest(&self, index: usize, local: &Worker<Job>) -> bool {
        let Some(slot) = self.slots.get(index) else {
            return false;
        };
        slot.state.store(IDLE, Ordering::SeqCst);
        self.idle.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let stopping = self.stopping.load(Ordering::SeqCst);
        if stopping || self.any_work(local) {
            self.claim(slot);
            return !stopping || self.any_work(local);
        }
        while slot.state.load(Ordering::Acquire) == IDLE {
            std::thread::park();
        }
        true
    }

    /// Whether any queue holds a job.
    fn any_work(&self, local: &Worker<Job>) -> bool {
        !self.injector.is_empty()
            || !local.is_empty()
            || self.stealers.iter().any(|stealer| !stealer.is_empty())
    }

    /// Claims an idle slot for running: whether this call took it from idle (and so uncounted it).
    fn claim(&self, slot: &Slot) -> bool {
        let claimed = slot
            .state
            .compare_exchange(IDLE, RUNNING, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if claimed {
            self.idle.fetch_sub(1, Ordering::SeqCst);
        }
        claimed
    }

    /// Wakes one idle worker, if any is: the first slot it can claim, from where the last search left off.
    fn wake_one(&self) {
        if self.idle.load(Ordering::SeqCst) == 0 {
            return;
        }
        let count = self.slots.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let woken = (0..count).find_map(|offset| {
            let at = start.wrapping_add(offset).checked_rem(count)?;
            let slot = self.slots.get(at)?;
            self.claim(slot).then_some(slot)
        });
        if let Some(slot) = woken {
            slot.thread.unpark();
        }
    }

    /// Queues an admitted job and wakes a worker for it.
    fn submit(&self, job: Job) {
        self.injector.push(job);
        ACCEPTED.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        self.wake_one();
        // A job that raced the stop past the last worker's exit is dropped here unrun, its receiver seeing the
        // sender gone, as the stop drops what reached the queue before the last exit.
        if self.stopping.load(Ordering::SeqCst) && self.live.load(Ordering::SeqCst) == 0 {
            self.drain();
        }
    }

    /// A worker leaving: the last one out wakes the stopper.
    fn exited(&self) {
        if self.live.fetch_sub(1, Ordering::AcqRel) == 1
            && let Some(stopper) = self.stopper.get()
        {
            stopper.unpark();
        }
    }

    /// Drops every job still queued in the injector, unrun, each inside an unwind boundary (its drop is the
    /// job's code).
    fn drain(&self) {
        while let Some(job) = take(|| self.injector.steal()) {
            let _ = contained(move || drop(job));
        }
    }
}

/// A steal tried again while it lost a race: `Retry` means another thief's claim of the same job succeeded, so
/// every retry follows another's progress (lock-free).
fn take(mut attempt: impl FnMut() -> Steal<Job>) -> Option<Job> {
    loop {
        match attempt() {
            Steal::Success(job) => return Some(job),
            Steal::Empty => return None,
            Steal::Retry => {}
        }
    }
}

/// A place in the pool's bound for one job, or why not.
fn admit() -> Result<Held, RtError> {
    let pool = POOL.get().ok_or(RtError::BadConfig {
        what: "a compute job with no pool running",
    })?;
    if pool.stopping.load(Ordering::Acquire) {
        return Err(RtError::BadConfig {
            what: "a compute job after the pool stopped",
        });
    }
    if pool.held.fetch_add(1, Ordering::AcqRel) >= pool.queued {
        pool.held.fetch_sub(1, Ordering::AcqRel);
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(RtError::Capacity {
            what: "compute pool",
            bound: pool.queued,
        });
    }
    Ok(Held(pool))
}

/// Runs `job` on the pool, as one piece; its result arrives on the returned receiver. Refused (and handed back)
/// at the pool's bound, with no pool running, or with one stopping.
pub fn run<T, F>(job: F) -> Result<OneshotReceiver<T>, Refused<F>>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let held = match admit() {
        Ok(held) => held,
        Err(error) => return Err(Refused { error, job }),
    };
    let (sender, receiver) = match oneshot() {
        Ok(pair) => pair,
        Err(error) => return Err(Refused { error, job }),
    };
    let pool = held.0;
    pool.submit(once(job, sender, held));
    Ok(receiver)
}

/// Runs a job in pieces: `step` is called with `state` and its piece's [`Turn`] until it returns
/// [`Step::Done`], whose result arrives on the returned receiver. Between units of its work it asks
/// [`Turn::over`], and returns [`Step::More`] when told yes. Refused (and handed back) as [`run`] is.
pub fn run_pieces<S, T, F>(state: S, step: F) -> Result<OneshotReceiver<T>, Refused<(S, F)>>
where
    S: Send + 'static,
    T: Send + 'static,
    F: FnMut(&mut S, &Turn<'_>) -> Step<T> + Send + 'static,
{
    let held = match admit() {
        Ok(held) => held,
        Err(error) => {
            return Err(Refused {
                error,
                job: (state, step),
            });
        }
    };
    let (sender, receiver) = match oneshot() {
        Ok(pair) => pair,
        Err(error) => {
            return Err(Refused {
                error,
                job: (state, step),
            });
        }
    };
    let pool = held.0;
    pool.submit(pieces(state, step, sender, held));
    Ok(receiver)
}

/// Runs a job of pieces to its end on the calling thread, its turn never over: what a consumer does with a job
/// where no pool runs.
pub fn run_here<S, T, F>(mut state: S, mut step: F) -> T
where
    F: FnMut(&mut S, &Turn<'_>) -> Step<T>,
{
    let local = Worker::new_fifo();
    let injector = Injector::new();
    let turn = Turn {
        started_ns: shard_clock_ns(),
        piece_ns: u64::MAX,
        local: &local,
        injector: &injector,
    };
    loop {
        if let Step::Done(value) = step(&mut state, &turn) {
            return value;
        }
    }
}

/// Whether the pool runs in this process: started, and not stopped.
pub fn running() -> bool {
    POOL.get()
        .is_some_and(|pool| !pool.stopping.load(Ordering::Acquire))
}

/// How full the pool is now: its threads, and the jobs it holds, running or waiting for a thread (a job between
/// its pieces among them). `None` with no pool started. Kiln's departure (ORIGIN.md; goal 6, plan
/// `01a1a59c2abe` step 4): a job's wait is told as its first piece starts, so a pool whose jobs all wait tells
/// nothing; this says whether anything waits now. One atomic read; no lock.
pub fn occupancy() -> Option<crate::blocking::Occupancy> {
    let pool = POOL.get()?;
    Some(crate::blocking::Occupancy {
        places: pool.slots.len(),
        held: pool.held.load(Ordering::Acquire),
    })
}

/// When a job was submitted, where the pool is observed ([`observe`]): its first piece tells how long it waited.
fn submitted() -> Option<u64> {
    OBSERVER.get().map(|_| shard_clock_ns())
}

/// A job's first piece, starting on `turn`: how long the job waited for it, told once.
fn first_piece(submitted_ns: &mut Option<u64>, turn: &Turn<'_>) {
    if let Some(at) = submitted_ns.take() {
        tell(Observed::Waited(turn.started_ns.saturating_sub(at)));
    }
}

/// A closure as a job of one piece.
fn once<T, F>(job: F, sender: OneshotSender<T>, held: Held) -> Job
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let mut parts = Some((job, sender, held));
    let mut submitted_ns = submitted();
    Box::new(move |turn: &Turn<'_>| {
        first_piece(&mut submitted_ns, turn);
        if let Some((job, sender, held)) = parts.take() {
            finish(contained(job), sender, held);
        }
        true
    })
}

/// A state and its step as a job of pieces.
fn pieces<S, T, F>(state: S, step: F, sender: OneshotSender<T>, held: Held) -> Job
where
    S: Send + 'static,
    T: Send + 'static,
    F: FnMut(&mut S, &Turn<'_>) -> Step<T> + Send + 'static,
{
    let mut parts = Some((state, step, sender, held));
    let mut submitted_ns = submitted();
    Box::new(move |turn: &Turn<'_>| {
        first_piece(&mut submitted_ns, turn);
        let outcome = match parts.as_mut() {
            Some((state, step, _, _)) => contained(|| step(state, turn)),
            None => return true,
        };
        let value = match outcome {
            Some(Step::More) => return false,
            Some(Step::Done(value)) => Some(value),
            None => None,
        };
        if let Some((state, step, sender, held)) = parts.take() {
            // The state and the step are the job's code: dropped inside a boundary of their own.
            let _ = contained(move || drop((state, step)));
            finish(value, sender, held);
        }
        true
    })
}

/// A job's end: its place in the bound given back first, then its result sent (or its sender dropped, for one
/// that unwound), each drop of the job's values inside an unwind boundary.
fn finish<T>(outcome: Option<T>, sender: OneshotSender<T>, held: Held) {
    if outcome.is_none() {
        PANICKED.fetch_add(1, Ordering::Relaxed);
    }
    drop(held);
    FINISHED.fetch_add(1, Ordering::Relaxed);
    match outcome {
        // A dropped receiver hands the value back, whose drop is the job's code.
        Some(value) => {
            let _ = contained(move || drop(sender.send(value)));
        }
        None => drop(sender),
    }
}

/// Stops the pool: a job submitted from now on is refused; the jobs admitted still run, then every worker
/// ends. Blocks the calling thread until the last worker has left; call it from the consumer's shutdown path,
/// not from a shard. Refused `BadConfig` with no pool, or when it was stopped already.
pub fn stop() -> Result<(), RtError> {
    let pool = POOL.get().ok_or(RtError::BadConfig {
        what: "stopping a compute pool that never started",
    })?;
    if pool.stopper.set(std::thread::current()).is_err() {
        return Err(RtError::BadConfig {
            what: "the compute pool stopped already",
        });
    }
    pool.stopping.store(true, Ordering::SeqCst);
    for slot in &pool.slots {
        pool.claim(slot);
        slot.thread.unpark();
    }
    while pool.live.load(Ordering::Acquire) > 0 {
        std::thread::park();
    }
    pool.drain();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(quota_us: u64, period_us: u64) -> Option<CpuBudget> {
        Some(CpuBudget {
            quota_us,
            period_us,
        })
    }

    /// The grant is the tighter of the cores and the whole CPUs a quota buys, said with what set it.
    #[test]
    fn a_grant_is_the_tighter_of_the_cores_and_the_quota() {
        let cores = Grant::of(32, None, 7);
        assert_eq!(cores, Grant { cpus: 32, by: GrantedBy::Cores });
        assert_eq!(cores.to_string(), "32 CPUs: the cores this process may run on");
        let quota = Grant::of(32, budget(800_000, 100_000), 32);
        assert_eq!(quota.cpus, 8);
        assert_eq!(
            quota.to_string(),
            "8 CPUs: the cgroup's quota of 800000 µs per 100000 µs, over 32 cores"
        );
        // A quota past the cores grants the cores; a fraction of a CPU grants none whole.
        assert_eq!(Grant::of(4, budget(800_000, 100_000), 4).by, GrantedBy::Cores);
        assert_eq!(Grant::of(4, budget(50_000, 100_000), 4).cpus, 0);
        // No cores read: the OS's count.
        let counted = Grant::of(0, budget(200_000, 100_000), 12);
        assert_eq!(counted, Grant { cpus: 12, by: GrantedBy::Parallelism });
    }

    /// The pool is the grant less the shards, and at least one thread.
    #[test]
    fn the_pool_is_the_grant_less_the_shards_and_at_least_one() {
        let grant = |cpus| Grant { cpus, by: GrantedBy::Cores };
        assert_eq!(workers_for(&grant(32), 4), 28);
        assert_eq!(workers_for(&grant(4), 4), 1);
        assert_eq!(workers_for(&grant(0), 2), 1);
    }

    /// Which priorities rank below which.
    #[test]
    fn priorities_compare_as_their_os_ranks_them() {
        let other = |nice| Priority::Other { nice };
        let batch = |nice| Priority::Batch { nice };
        let idle = |nice| Priority::Idle { nice };
        assert!(idle(0).is_below(other(19)), "the idle policy below every other");
        assert!(idle(0).is_below(batch(19)));
        assert!(!other(19).is_below(idle(0)));
        assert!(idle(10).is_below(idle(0)));
        assert!(batch(10).is_below(other(0)));
        assert!(batch(10).is_below(other(10)), "the batch policy at the same nice value");
        assert!(other(19).is_below(batch(10)));
        assert!(!batch(10).is_below(batch(10)));
        assert!(!other(0).is_below(batch(10)));
        let qos = |class| Priority::Qos { class };
        assert!(qos(QOS_CLASS_UTILITY).is_below(qos(QOS_CLASS_DEFAULT)));
        assert!(qos(QOS_CLASS_UTILITY).is_below(qos(0)), "unspecified reads as the default");
        assert!(!qos(QOS_CLASS_DEFAULT).is_below(qos(0)));
        assert!(Priority::Level { level: -1 }.is_below(Priority::Level { level: 0 }));
        assert!(!Priority::Level { level: 0 }.is_below(Priority::Level { level: -1 }));
        assert!(!Priority::Unknown.is_below(other(0)));
        assert!(!batch(10).is_below(Priority::Level { level: 0 }));
        assert_eq!(Priority::Batch { nice: 10 }.to_string(), "SCHED_BATCH at nice 10");
    }
}
