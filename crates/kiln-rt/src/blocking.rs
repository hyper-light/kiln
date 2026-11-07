//! Blocking work (docs/runtime.md §9): one bounded pool per process for work that blocks a thread — a
//! synchronous library call, `getaddrinfo`, a code sandbox. `workers` threads are started once and reused.
//! The consumer divides the pool into named shares so one kind of work cannot take all of it: a share runs
//! at most `limit` of its jobs at once, and the jobs past those wait for one of its places, in the order
//! given, up to `waiting` of them; past that a job is refused `Capacity` before any thread is asked. A job
//! is never refused because its share is busy while its line has room: essential work queues for its
//! turn, as a busy machine's work must. A job's result returns through a [`crate::sync::oneshot`], which a
//! task awaits.
//!
//! **Critical first** (kiln's departure, ORIGIN.md; kiln's goal 6, plan `01a1a59c2abe` step 3): a job submitted
//! from a task of [`crate::coop::Class::Critical`], what a person waits on or the platform checks, waits ahead of
//! the rest of its share's line, as a shard's run queue takes that class first. Its share's places bound it as
//! they bound the rest, and each kind keeps its order. A share's line was first come, first served whatever a
//! job's class: a person's read waited behind every read of the jobs before it (kiln's census, train 8: custody's
//! reads held its `store` share's line at about 14 jobs, 80 ms each on average).
//!
//! **No locks** (the workspace's wall: single owners and moves over channels). A dispatcher thread owns
//! the shares' lines, their running counts and the list of idle workers; each worker owns a one-job slot
//! and tells the dispatcher when it is idle again and which share's job it finished, so a job is handed to
//! exactly one idle worker, one to one. A share's admitted jobs (running or waiting) are an atomic count,
//! checked as a job is submitted.
//!
//! **Bounded by admission.** The dispatcher's channel holds at most the jobs admitted (each share's
//! `limit + waiting`), one idle notice per worker and one stop, so it is bounded by the admission count,
//! not preallocated: the pool's memory is that of the jobs it holds.
//!
//! A job cannot be cancelled (the call is the OS's): dropping its receiver discards the result, and the
//! job's share stays charged until it ends — its share's bound bounds that damage, stated. A job that
//! panics ends the job, not its worker (an unwind boundary, CLAUDE.md §1), and its share is given back.
//! A job's share is given back before its result is delivered or its sender dropped, so a task that sees
//! the job end can submit again under the same share without a refusal.
//!
//! **One per process.** The pool's channels and shares are a process-wide static set once by [`start`];
//! [`stop`] runs the jobs admitted and joins every thread, after which a second start is refused.
//!
//! **Observed** (kiln's departure, ORIGIN.md): a consumer may be told of each job as it ends, its share, its
//! wait for a place and its run ([`observe`]), to derive each share's size from what its calls do.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;

use crate::error::RtError;
use crate::idle::Cold;
use crate::sync::{OneshotReceiver, oneshot};

/// One kind of work: the most jobs of it running at once, and the most waiting for one of those places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
    /// The kind's name, as a job names it.
    pub name: &'static str,
    /// The most of its jobs running at once: the threads it may hold.
    pub limit: usize,
    /// The most of its jobs waiting for one of its places, in the order given; past them a job is
    /// refused. Zero refuses a job the moment the share is full.
    pub waiting: usize,
}

impl Share {
    /// A share of `limit` places and no line: past its places a job is refused at once, as the pool
    /// refused before shares had lines. The constructor a consumer's `Share { name, limit }` from
    /// before `waiting` becomes.
    #[must_use]
    pub const fn new(name: &'static str, limit: usize) -> Self {
        Self {
            name,
            limit,
            waiting: 0,
        }
    }
}

/// The pool's shape, the consumer's (derived from its own measurement: a device's queue and knee, a
/// sandbox's concurrency budget).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Threads.
    pub workers: usize,
    /// The kinds of work, their limits and their lines.
    pub shares: Vec<Share>,
}

/// What the pool has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Jobs accepted.
    pub accepted: u64,
    /// Jobs refused for a share whose places and line were full.
    pub refused: u64,
    /// Jobs finished.
    pub finished: u64,
    /// Of those, jobs that panicked: their receivers see the sender gone; the worker and the share live on.
    pub panicked: u64,
}

/// What the pool tells its consumer of each job as it ends ([`observe`]): the job's share, how long it waited
/// for one of the share's places (from its submission to its start) and how long it ran, in nanoseconds.
pub type Observer = fn(share: &'static str, waited_ns: u64, ran_ns: u64);

/// The consumer's observer, once set.
static OBSERVER: OnceLock<Observer> = OnceLock::new();

/// Has the pool tell `observer` of each job it runs, as the job ends: its share, how long it waited for one of
/// its share's places and how long it ran. These are the consumer's measurements to size its shares from
/// (Little's law: the jobs a share holds at once are the rate they come at times the time each takes); a
/// pool no one observes reads no clock. Once per process: false when an observer was set already.
pub fn observe(observer: Observer) -> bool {
    OBSERVER.set(observer).is_ok()
}

/// How full a share (or the compute pool, `crate::compute::occupancy`) is now: its places and the jobs it holds,
/// running or waiting. Past its places, the rest wait. Kiln's departure (ORIGIN.md; goal 6, plan `01a1a59c2abe`
/// step 4): an observer is told of a job's wait only once it starts or ends, so a share whose jobs are all
/// waiting tells nothing; this says whether anything waits now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Occupancy {
    /// The most of its jobs that run at once.
    pub places: usize,
    /// Its jobs admitted and not yet ended.
    pub held: usize,
}

impl Occupancy {
    /// The jobs waiting for a place now.
    #[must_use]
    pub const fn waiting(&self) -> usize {
        self.held.saturating_sub(self.places)
    }
}

/// How full share `share` is now: `None` with no pool running or no such share. One atomic read; no lock.
pub fn occupancy(share: &str) -> Option<Occupancy> {
    let state = POOL.get()?.shares.iter().find(|state| state.share.name == share)?;
    Some(Occupancy {
        places: state.share.limit,
        held: state.held.load(Ordering::Acquire),
    })
}

type Job = Box<dyn FnOnce() + Send>;

/// What the dispatcher receives.
enum Message {
    /// A job of the share at `share`; `critical` where its submitter ran in the Critical class.
    Job {
        share: usize,
        job: Job,
        critical: bool,
    },
    /// A worker is idle again, after finishing a job of `finished` when it ran one.
    Idle {
        worker: usize,
        finished: Option<usize>,
    },
    /// Run the jobs admitted, stop the workers, join them, and say so.
    Stop(SyncSender<bool>),
}

/// A share's bounds and the jobs it has admitted, running or waiting.
struct ShareState {
    share: Share,
    held: AtomicUsize,
}

/// The pool, once started.
struct Pool {
    submit: Sender<Message>,
    shares: Box<[ShareState]>,
    /// Set as [`stop`] begins: a job submitted from then on is refused.
    stopping: AtomicBool,
}

static POOL: OnceLock<Pool> = OnceLock::new();
static ACCEPTED: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static FINISHED: AtomicU64 = AtomicU64::new(0);
static PANICKED: AtomicU64 = AtomicU64::new(0);

/// Starts a named thread of the pool.
fn thread(name: String, body: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>, RtError> {
    #[allow(
        clippy::disallowed_methods,
        reason = "the blocking pool's threads (docs/runtime.md §9): started once, joined by `stop`"
    )]
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        crate::parking::mark_thread(crate::parking::KickFrom::Blocking);
        body();
    });
    spawned.map_err(|_| RtError::BadConfig {
        what: "the OS refused a blocking pool thread",
    })
}

/// Starts the pool. Refused `BadConfig` when one was started already in this process, for no workers or
/// no shares, or for a share with no place; a thread the OS refuses stops the start (the threads started
/// end with their channels).
pub fn start(config: Config) -> Result<(), RtError> {
    if config.workers == 0
        || config.shares.is_empty()
        || config.shares.iter().any(|share| share.limit == 0)
    {
        return Err(RtError::BadConfig {
            what: "a blocking pool with no workers, no shares, or a share with no place",
        });
    }
    if POOL.get().is_some() {
        return Err(RtError::BadConfig {
            what: "a second blocking pool in one process",
        });
    }
    let (submit, messages) = channel();
    let mut slots = Vec::new();
    let mut workers = Vec::new();
    for index in 0..config.workers {
        let (slot, take) = sync_channel::<(usize, Job)>(1);
        let notify = submit.clone();
        workers.push(thread(format!("hyper-rt-blocking-{index}"), move || {
            work(index, &take, &notify);
        })?);
        slots.push(slot);
    }
    let shares: Box<[ShareState]> = config
        .shares
        .iter()
        .map(|share| ShareState {
            share: *share,
            held: AtomicUsize::new(0),
        })
        .collect();
    let limits: Vec<usize> = config.shares.iter().map(|share| share.limit).collect();
    let pool = Pool {
        submit,
        shares,
        stopping: AtomicBool::new(false),
    };
    if POOL.set(pool).is_err() {
        return Err(RtError::BadConfig {
            what: "a second blocking pool in one process",
        });
    }
    thread("hyper-rt-blocking-dispatch".to_owned(), move || {
        Dispatcher::new(slots, limits).run(&messages, workers);
    })?;
    Ok(())
}

/// A worker: does what the process said a worker does as it starts ([`crate::idle::started`]), then tells the
/// dispatcher it is idle (and which share's job it finished), takes the job it is handed, runs it, and again; ends
/// when its slot or the dispatcher's channel closes.
fn work(index: usize, take: &Receiver<(usize, Job)>, notify: &Sender<Message>) {
    crate::idle::started(crate::idle::Pool::Blocking);
    let mut finished = None;
    loop {
        let idle = Message::Idle {
            worker: index,
            finished,
        };
        if notify.send(idle).is_err() {
            return;
        }
        let Some((share, job)) = next(take, crate::idle::cold()) else {
            return;
        };
        job();
        finished = Some(share);
    }
}

/// What this worker is handed next: waited for `cold.after` first, then, cold, waited for as long as it takes
/// once it has done `cold.then` on its own thread (kiln's goal 11, plan 01a1a87b2e56 step 5: [`crate::idle`]).
/// None once the dispatcher is gone.
fn next<T>(take: &Receiver<T>, cold: Option<Cold>) -> Option<T> {
    if let Some(cold) = cold {
        match take.recv_timeout(cold.after) {
            Ok(handed) => return Some(handed),
            Err(RecvTimeoutError::Disconnected) => return None,
            Err(RecvTimeoutError::Timeout) => (cold.then)(),
        }
    }
    take.recv().ok()
}

/// A share's jobs waiting for one of its places: those submitted from a Critical task first, each kind in the
/// order given (the module's notes, "Critical first").
#[derive(Default)]
struct Line {
    critical: VecDeque<Job>,
    rest: VecDeque<Job>,
}

impl Line {
    /// Joins `job` at the back of its kind.
    fn push(&mut self, job: Job, critical: bool) {
        if critical {
            self.critical.push_back(job);
        } else {
            self.rest.push_back(job);
        }
    }

    /// The next to run: the first Critical job waiting, else the first of the rest.
    fn pop(&mut self) -> Option<Job> {
        self.critical.pop_front().or_else(|| self.rest.pop_front())
    }

    fn is_empty(&self) -> bool {
        self.critical.is_empty() && self.rest.is_empty()
    }
}

/// The dispatcher's own state: each share's line and running count, and the workers idle.
struct Dispatcher {
    slots: Vec<SyncSender<(usize, Job)>>,
    limits: Vec<usize>,
    running: Vec<usize>,
    lines: Vec<Line>,
    idle: Vec<usize>,
    /// The share the next hand-out looks at first, so the shares take the idle workers in turn.
    next: usize,
}

impl Dispatcher {
    fn new(slots: Vec<SyncSender<(usize, Job)>>, limits: Vec<usize>) -> Self {
        let shares = limits.len();
        Self {
            slots,
            limits,
            running: vec![0; shares],
            lines: (0..shares).map(|_| Line::default()).collect(),
            idle: Vec::new(),
            next: 0,
        }
    }

    /// Takes each message, hands out what can run, and on `Stop` runs what was admitted before closing
    /// the slots, joining the workers and only then answering.
    fn run(mut self, messages: &Receiver<Message>, workers: Vec<JoinHandle<()>>) {
        let total = workers.len();
        let mut stopping = None;
        while let Ok(message) = messages.recv() {
            match message {
                Message::Job {
                    share,
                    job,
                    critical,
                } => {
                    if let Some(line) = self.lines.get_mut(share) {
                        line.push(job, critical);
                    }
                }
                Message::Idle { worker, finished } => self.idle_again(worker, finished),
                Message::Stop(done) => stopping = Some(done),
            }
            self.hand_out();
            let drained = self.lines.iter().all(Line::is_empty) && self.idle.len() == total;
            if drained && let Some(done) = stopping.take() {
                // A job submitted once the stop began is refused. One that raced the stop's start ran
                // if it reached the lines before they drained, and is dropped unrun after, its
                // receiver seeing the sender gone. The slots close and the channel's receiver goes
                // before the stop is answered, so a `run` once `stop` has returned is refused (CI,
                // ubuntu-24.04, run 38003581811).
                drop(self.slots);
                let mut clean = true;
                for worker in workers {
                    clean &= worker.join().is_ok();
                }
                let _ = done.send(clean);
                return;
            }
        }
    }

    /// A worker idle again: the share whose job it finished gives back a place.
    fn idle_again(&mut self, worker: usize, finished: Option<usize>) {
        if let Some(running) = finished.and_then(|share| self.running.get_mut(share)) {
            *running = running.saturating_sub(1);
        }
        self.idle.push(worker);
    }

    /// Hands each idle worker the next job of a share with a free place, the shares taken in turn and each
    /// share's jobs Critical first, each kind in the order given ([`Line::pop`]).
    fn hand_out(&mut self) {
        let shares = self.lines.len();
        while !self.idle.is_empty() {
            let Some(share) = (0..shares)
                .filter_map(|offset| self.next.checked_add(offset)?.checked_rem(shares))
                .find(|&share| self.can_run(share))
            else {
                return;
            };
            let (Some(job), Some(worker)) = (
                self.lines.get_mut(share).and_then(Line::pop),
                self.idle.pop(),
            ) else {
                return;
            };
            if let Some(running) = self.running.get_mut(share) {
                *running = running.saturating_add(1);
            }
            self.next = share
                .checked_add(1)
                .and_then(|next| next.checked_rem(shares))
                .unwrap_or(0);
            // A worker announced idle holds an empty slot of one, so the send never waits; a worker
            // whose thread ended leaves the job unrun, its receiver seeing the sender gone.
            if let Some(slot) = self.slots.get(worker) {
                let _ = slot.send((share, job));
            }
        }
    }

    /// Whether the share at `share` has a job waiting and a place free.
    fn can_run(&self, share: usize) -> bool {
        let waiting = self.lines.get(share).is_some_and(|line| !line.is_empty());
        let free = match (self.running.get(share), self.limits.get(share)) {
            (Some(running), Some(limit)) => running < limit,
            _ => false,
        };
        waiting && free
    }
}

/// Runs `job` on the pool under `share`; its result arrives on the returned receiver. The job runs when
/// one of its share's places frees, after the share's jobs submitted before it, or, submitted from a task of
/// the Critical class, after its Critical ones alone (the module's notes). Refused `Capacity` when the
/// share's places and line are full, `BadConfig` for an unknown share, a pool not running or one stopping.
pub fn run<T, F>(share: &str, job: F) -> Result<OneshotReceiver<T>, RtError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let pool = POOL.get().ok_or(RtError::BadConfig {
        what: "a blocking job with no pool running",
    })?;
    if pool.stopping.load(Ordering::Acquire) {
        return Err(RtError::BadConfig {
            what: "a blocking job after the pool stopped",
        });
    }
    let (index, state) = pool
        .shares
        .iter()
        .enumerate()
        .find(|(_, state)| state.share.name == share)
        .ok_or(RtError::BadConfig {
            what: "a blocking job of an unknown share",
        })?;
    let bound = state.share.limit.saturating_add(state.share.waiting);
    if state.held.fetch_add(1, Ordering::AcqRel) >= bound {
        state.held.fetch_sub(1, Ordering::AcqRel);
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(RtError::Capacity {
            what: "blocking pool share",
            bound,
        });
    }
    let (sender, receiver) = oneshot().inspect_err(|_| {
        state.held.fetch_sub(1, Ordering::AcqRel);
    })?;
    // Only an observed pool reads the clock: as the job is submitted, as it starts and as it ends.
    let observer = OBSERVER.get().copied();
    let submitted_ns = observer.map(|_| crate::machine::clock::monotonic_ns());
    let name = state.share.name;
    let wrapped: Job = Box::new(move || {
        let started_ns = submitted_ns.map(|_| crate::machine::clock::monotonic_ns());
        let outcome = crate::unwind::contained(job);
        if let (Some(observer), Some(submitted_ns), Some(started_ns)) =
            (observer, submitted_ns, started_ns)
        {
            let ran_ns = crate::machine::clock::monotonic_ns().saturating_sub(started_ns);
            let waited_ns = started_ns.saturating_sub(submitted_ns);
            // The consumer's code: inside an unwind boundary of its own.
            let _ = crate::unwind::contained(|| observer(name, waited_ns, ran_ns));
        }
        if outcome.is_none() {
            PANICKED.fetch_add(1, Ordering::Relaxed);
        }
        // The share is given back before the receiver can see anything: a task that awaited this job's
        // result, or saw its sender gone, may submit at once and must find the slot free.
        if let Some(state) = POOL.get().and_then(|pool| pool.shares.get(index)) {
            state.held.fetch_sub(1, Ordering::AcqRel);
        }
        FINISHED.fetch_add(1, Ordering::Relaxed);
        // A dropped receiver discards the result, whose drop is the job's code: inside an unwind
        // boundary too.
        match outcome {
            Some(value) => {
                let _ = crate::unwind::contained(move || drop(sender.send(value)));
            }
            None => drop(sender),
        }
    });
    // The submitting task's class, read here where it runs (none off a shard: the rest's line).
    let critical = crate::coop::class() == Some(crate::coop::Class::Critical);
    match pool.submit.send(Message::Job {
        share: index,
        job: wrapped,
        critical,
    }) {
        Ok(()) => {
            ACCEPTED.fetch_add(1, Ordering::Relaxed);
            Ok(receiver)
        }
        Err(_) => {
            state.held.fetch_sub(1, Ordering::AcqRel);
            Err(RtError::BadConfig {
                what: "a blocking job after the pool stopped",
            })
        }
    }
}

/// What the pool has done.
pub fn stats() -> Stats {
    Stats {
        accepted: ACCEPTED.load(Ordering::Relaxed),
        refused: REFUSED.load(Ordering::Relaxed),
        finished: FINISHED.load(Ordering::Relaxed),
        panicked: PANICKED.load(Ordering::Relaxed),
    }
}

/// Stops the pool: the jobs admitted still run, then every thread ends and is joined; a job submitted
/// once the stop began is refused. Blocks the calling thread on the jobs running and waiting, which
/// cannot be cancelled; call it from the consumer's shutdown path, not from a shard.
pub fn stop() -> Result<(), RtError> {
    let pool = POOL.get().ok_or(RtError::BadConfig {
        what: "stopping a blocking pool that never started",
    })?;
    if pool.stopping.swap(true, Ordering::AcqRel) {
        return Err(RtError::BadConfig {
            what: "the blocking pool stopped already",
        });
    }
    let (done, joined) = sync_channel(1);
    pool.submit
        .send(Message::Stop(done))
        .map_err(|_| RtError::BadConfig {
            what: "the blocking pool stopped already",
        })?;
    match joined.recv() {
        Ok(true) => Ok(()),
        Ok(false) => Err(RtError::BadConfig {
            what: "a blocking pool worker ended abnormally",
        }),
        Err(_) => Err(RtError::BadConfig {
            what: "the blocking pool's dispatcher ended abnormally",
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    static COLD: AtomicUsize = AtomicUsize::new(0);
    static ASKING: OnceLock<std::thread::Thread> = OnceLock::new();

    /// Kiln's goal 6, plan 01a1a59c2abe step 3: a share's jobs from Critical tasks run before the rest waiting for its
    /// places, each kind in the order given; its places bound them as they bound the rest, and another share keeps its
    /// own turn.
    #[test]
    fn g6_s3_critical_jobs_go_first_in_their_share_s_line() {
        let (done, order) = channel::<u32>();
        let job = |id: u32| -> Job {
            let done = done.clone();
            Box::new(move || {
                let _ = done.send(id);
            })
        };
        // One worker; share 0 of one place, share 1 of one.
        let (slot, take) = sync_channel::<(usize, Job)>(1);
        let mut d = Dispatcher::new(vec![slot], vec![1, 1]);
        for (id, critical) in [(1, false), (2, false), (3, true), (4, true), (5, false)] {
            d.lines[0].push(job(id), critical);
        }
        d.lines[1].push(job(6), false);
        d.idle_again(0, None);
        let mut ran = Vec::new();
        let mut shares = Vec::new();
        while !d.lines.iter().all(Line::is_empty) {
            d.hand_out();
            let (share, handed) = take.try_recv().unwrap();
            handed();
            ran.push(order.try_recv().unwrap());
            shares.push(share);
            d.idle_again(0, Some(share));
        }
        // The shares take the worker in turn; share 0's Critical jobs first, then the rest, each in order.
        assert_eq!(ran, vec![3, 6, 4, 1, 2, 5]);
        assert_eq!(shares, vec![0, 1, 0, 0, 0, 0]);
        // A full share's place holds its Critical jobs as it holds the rest.
        d.running[0] = 1;
        d.lines[0].push(job(7), true);
        d.hand_out();
        assert!(take.try_recv().is_err(), "share 0's one place is taken");
        assert!(!d.lines[0].is_empty());
    }

    fn went_cold() {
        COLD.fetch_add(1, Ordering::SeqCst);
        if let Some(asking) = ASKING.get() {
            asking.unpark();
        }
    }

    /// Kiln's goal 11, plan 01a1a87b2e56 step 5: a worker handed a job within its wait takes it as before; one that
    /// waits past it goes cold once, on its own thread, then takes the job that comes; one whose dispatcher is
    /// gone ends.
    #[test]
    fn g11_s5_a_worker_idle_past_its_wait_goes_cold_once_then_takes_its_job() {
        let (give, take) = sync_channel::<u32>(1);
        let cold = Cold {
            after: Duration::from_millis(20),
            then: went_cold,
        };
        give.send(1).unwrap();
        assert_eq!(next(&take, Some(cold)), Some(1));
        assert_eq!(COLD.load(Ordering::SeqCst), 0, "handed a job within its wait");
        let late = std::thread::spawn(move || {
            ASKING.set(std::thread::current()).unwrap();
            // The job comes once the worker went cold, at the machine's pace; the bound only ends a hang.
            let hang = Instant::now() + Duration::from_secs(300);
            while COLD.load(Ordering::SeqCst) == 0 && Instant::now() < hang {
                std::thread::park_timeout(hang.saturating_duration_since(Instant::now()));
            }
            give.send(2).unwrap();
            give
        });
        assert_eq!(next(&take, Some(cold)), Some(2));
        assert_eq!(COLD.load(Ordering::SeqCst), 1, "cold once in a wait past 20 ms");
        drop(late.join().unwrap());
        assert_eq!(next(&take, Some(cold)), None, "the dispatcher gone");
        assert_eq!(next(&take, None), None);
        assert_eq!(COLD.load(Ordering::SeqCst), 1);
    }
}
