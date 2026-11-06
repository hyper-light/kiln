//! Movable tasks (kiln's goal 7, plan 01a1a59c2aea step 2; ORIGIN.md, "What differs from upstream"): a class of
//! task beside the pinned one, whose queued polls an idle shard of the same runtime may take, so that a shard
//! held by one long poll, or one with more ready work than its siblings, does not keep that work waiting while
//! they sit idle (research 01a1a59c2aeb, f5, f7, f9).
//!
//! **Pinned and movable.** A pinned task (every task upstream has) lives in its shard's arena for its whole life,
//! named by a `Copy` word (`shard:16 | slot:24 | generation:24`) that its leaves register and that a wake routes
//! back to that one shard: a word names a slot on one shard, so a pinned task cannot move. A movable task is a
//! reference-counted header ([`Arc`], which ORIGIN.md's first rule allows for a task that moves between shards)
//! holding its future (`Send`), its state and its class, and its waker is the header itself (std's [`Wake`]), so
//! a wake reaches it on whichever shard runs it next.
//!
//! **Sockets and timers stay on their shard.** hyper-rt's leaves accept only a pinned task's own waker, so under
//! a movable task's they refuse (`NotOnShardThread`), as under any waker not the polling task's. kiln's wrappers
//! then wait in a helper task on the shard the wait began on (kiln's `rt::Bridge`), which wakes the movable task
//! when the wait ends: the registration stays with the shard that made it, and the task does not.
//!
//! **Queues** (Tokio's layout, research f14 and f16):
//! - each shard keeps a queue of its own for each class (crossbeam-deque's `Worker`, first in, first out: Chase
//!   and Lev's deque), and its runtime a shared queue for each class (crossbeam's `Injector`);
//! - a task woken on a shard of its runtime goes on that shard's queue; woken anywhere else (a pool's thread,
//!   another runtime's shard), on the shared queue, and a parked shard is kicked;
//! - a shard's queue holds at most its step's poll bound ([`crate::runtime::RuntimeConfig::batch`]): a task
//!   queued past it would wait more than a step of the shard's own work, so the older half of the queue moves to
//!   the shared queue, as Tokio moves half of its full 256-slot queue (f14);
//! - an idle shard, before it spins or parks, takes from the shared queues and then half of a sibling's queue,
//!   starting at a random sibling (f14, f15), with at most half the shards looking at once (f6);
//! - a shard that queues a task behind another while no shard is looking kicks a parked sibling to come and look,
//!   as Tokio notifies a parked worker of a push, so a busy shard's backlog is seen by the idle ones.
//!
//! **Fair turns, hyper-rt's.** A shard polls its movable tasks of a class in a carrier, a pinned task of that
//! class: each turn of the carrier polls the tasks queued as the turn began, each once, until the turn has used
//! its share ([`crate::coop`]), then yields behind the shard's inputs while any wait. A movable task woken in its
//! own poll (a yield, a charge past its share) goes to the back of its queue and runs at the carrier's next turn:
//! one turn of a round, as a pinned task's yield is. Research f13 found hyper-rt's yield fair where Tokio's
//! next-task slot is not, so no such slot is kept. A charge under a movable task's own waker counts as its own,
//! so a movable task yields at its share as a pinned one does.
//!
//! **A held shard.** The watchdog (`crate::watch`) hands the queued movable tasks of a shard held in one poll past
//! its limit to the shared queues and kicks parked siblings: the held shard's queue is detached from it, as Tokio
//! hands a blocked worker's core to another thread (f15).
//!
//! **No locks.** The queues are crossbeam's; a header's state is one atomic word, and its future is in a cell
//! that whoever the state word names owner takes and puts back (crossbeam-utils' `AtomicCell`, a lock-free swap of
//! a pointer). A runtime with no pool (a local or simulated one) runs a movable spawn as a pinned task.

use std::cell::{Cell, OnceCell};
use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
#[cfg(loom)]
use loom::sync::atomic::{AtomicU8, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Wake, Waker};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use crossbeam_utils::atomic::AtomicCell;

use crate::coop::Class;
use crate::error::RtError;
use crate::registry::{self, SlotHolder};
use crate::shard::{Incoming, ShardContext, TaskId};
use crate::task::TaskFuture;

/// Format: the classes, one queue each.
const CLASSES: usize = 3;

/// The order a shard looks in: the most urgent class first (`crate::coop::Class`).
const URGENCY: [Class; CLASSES] = [Class::Critical, Class::Normal, Class::Background];

/// A movable task's future.
type MovableFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// What a header holds of its task while no shard polls it: its future, in a box of its own so the cell holds a
/// thin pointer.
struct Body {
    future: MovableFuture,
}

/// A movable task: its header, shared by its wakers and the queue it waits in.
pub(crate) type Task = Arc<Header>;

// ---------------------------------------------------------------------------------------- state

/// In a queue, its future in its cell.
const QUEUED: u8 = 0;
/// A shard polls it: its future is out of its cell, with that shard.
const RUNNING: u8 = 1;
/// Woken while a shard polled it: queued again once the poll ends.
const NOTIFIED: u8 = 2;
/// Waiting for a wake, its future in its cell.
const IDLE: u8 = 3;
/// Ended: its future returned, unwound or was dropped.
const DONE: u8 = 4;

/// A movable task's state word: who may take its future, and what a wake does. Only a wake moves a task out of
/// `IDLE` (to `QUEUED`, and the waker queues it) or `RUNNING` (to `NOTIFIED`); only the shard that took it from a
/// queue moves it out of `QUEUED`, `RUNNING` or `NOTIFIED`. So a task is in at most one queue, and its future
/// is in its cell whenever it is queued. Its loom model is below (`loom_tests`).
#[derive(Debug)]
pub(crate) struct State(AtomicU8);

impl State {
    /// A task as it is spawned: queued.
    pub(crate) fn queued() -> State {
        State(AtomicU8::new(QUEUED))
    }

    /// A wake: whether the waker queues the task (it was idle). A task a shard polls now is marked, and queued
    /// again after its poll; one queued or ended is left as it is.
    pub(crate) fn wake(&self) -> bool {
        let mut now = self.0.load(Ordering::Acquire);
        loop {
            let next = match now {
                IDLE => QUEUED,
                RUNNING => NOTIFIED,
                _ => return false,
            };
            match self
                .0
                .compare_exchange_weak(now, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return next == QUEUED,
                Err(seen) => now = seen,
            }
        }
    }

    /// A shard took the task from a queue to poll it: false when it ended meanwhile.
    pub(crate) fn begin(&self) -> bool {
        self.0
            .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Its poll returned pending and its future is back in its cell: whether it goes on a queue again, woken
    /// during the poll. Release: a waker that sees it idle, and the shard that takes it next, see the future put
    /// back.
    pub(crate) fn pending(&self) -> bool {
        match self
            .0
            .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => false,
            // Woken during the poll: only a wake moves a running task, and only to `NOTIFIED`.
            Err(_) => {
                self.0.store(QUEUED, Ordering::Release);
                true
            }
        }
    }

    /// The task ended.
    pub(crate) fn done(&self) {
        self.0.store(DONE, Ordering::Release);
    }
}

// --------------------------------------------------------------------------------------- header

/// A movable task's header: what its wakers and its queue share.
pub(crate) struct Header {
    state: State,
    /// Its future while no shard polls it.
    body: AtomicCell<Option<Box<Body>>>,
    /// Its class, `Class as u8`: the queue it waits in.
    class: AtomicU8,
    /// Its runtime's queues: a wake after the runtime has gone drops the task.
    pool: Weak<Pool>,
    /// The runtime's index of the shard that polled it last: the one a wake from off the runtime kicks.
    last: AtomicUsize,
    /// Where it was spawned (`#[track_caller]`).
    at: &'static Location<'static>,
}

impl std::fmt::Debug for Header {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Header")
            .field("state", &self.state)
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

impl Header {
    fn new(pool: &Arc<Pool>, class: Class, future: MovableFuture, at: &'static Location<'static>, last: usize) -> Task {
        Arc::new(Header {
            state: State::queued(),
            body: AtomicCell::new(Some(Box::new(Body { future }))),
            class: AtomicU8::new(class as u8),
            pool: Arc::downgrade(pool),
            last: AtomicUsize::new(last),
            at,
        })
    }

    fn class(&self) -> Class {
        class_of(self.class.load(Ordering::Relaxed))
    }

    /// Ends a task no shard will poll (its runtime gone): its future dropped here.
    fn abandon(&self) {
        self.state.done();
        let _ = crate::unwind::contained(|| drop(self.body.take()));
    }
}

/// A class from its `u8`.
fn class_of(value: u8) -> Class {
    match value {
        0 => Class::Background,
        2 => Class::Critical,
        _ => Class::Normal,
    }
}

/// A class's place among the queues.
fn index_of(class: Class) -> usize {
    usize::from(class as u8)
}

impl Wake for Header {
    fn wake(self: Arc<Self>) {
        if self.state.wake() {
            enqueue(self);
        }
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if self.state.wake() {
            enqueue(Arc::clone(self));
        }
    }
}

/// Queues a task a wake made queued: on the queue of the shard this thread runs when that shard is of the task's
/// runtime, else on the runtime's shared queue, a parked shard kicked.
fn enqueue(task: Task) {
    let Some(pool) = task.pool.upgrade() else {
        task.abandon();
        return;
    };
    let left = registry::with_here(|here| match here {
        Some(ctx) => ctx.movable.push_if_ours(ctx, &pool, task),
        None => Err(task),
    });
    if let Err(task) = left {
        pool.inject(task);
    }
}

// ----------------------------------------------------------------------------------------- pool

/// What a runtime's movable tasks did, counted since it started ([`crate::runtime::Runtime::movable_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Movable tasks spawned.
    pub spawned: u64,
    /// Their polls.
    pub polls: u64,
    /// Tasks idle shards took from their siblings' queues.
    pub stolen: u64,
    /// Tasks shards took from the shared queues.
    pub from_shared: u64,
    /// Tasks a full queue moved to the shared queue.
    pub overflowed: u64,
    /// Tasks the watchdog moved off a held shard's queue to the shared queue.
    pub handed_off: u64,
    /// Parked shards kicked to come and take queued tasks.
    pub notified: u64,
}

/// The counters behind [`Stats`].
#[derive(Debug, Default)]
struct Counts {
    spawned: AtomicU64,
    polls: AtomicU64,
    stolen: AtomicU64,
    from_shared: AtomicU64,
    overflowed: AtomicU64,
    handed_off: AtomicU64,
    notified: AtomicU64,
}

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

fn read(counter: &AtomicU64) -> u64 {
    counter.load(std::sync::atomic::Ordering::Relaxed)
}

/// A runtime's movable queues: a shared queue for each class, and the stealing ends of every shard's own.
pub struct Pool {
    shared: [Injector<Task>; CLASSES],
    /// Each shard's queues' stealing ends, in the runtime's order.
    stealers: Box<[[Stealer<Task>; CLASSES]]>,
    /// Each shard's registration, in the same order: whom to kick.
    holders: Box<[SlotHolder]>,
    /// A shard's queue's bound (the runtime's poll batch).
    bound: usize,
    /// Shards looking for work in the others' queues now.
    searching: AtomicUsize,
    counts: Counts,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("shards", &self.holders.len())
            .field("bound", &self.bound)
            .finish_non_exhaustive()
    }
}

/// One shard's queues, its owning ends: moved to its thread as it starts.
pub(crate) type Queues = [Worker<Task>; CLASSES];

impl Pool {
    /// The pool of a runtime of `holders`' shards, each queue bounded at `bound`; and each shard's queues, in the
    /// same order, for the shard to take.
    pub(crate) fn new(holders: Vec<SlotHolder>, bound: usize) -> (Arc<Pool>, Vec<Queues>) {
        let queues: Vec<Queues> = holders
            .iter()
            .map(|_| [Worker::new_fifo(), Worker::new_fifo(), Worker::new_fifo()])
            .collect();
        let stealers = queues
            .iter()
            .map(|[a, b, c]| [a.stealer(), b.stealer(), c.stealer()])
            .collect();
        let pool = Arc::new(Pool {
            shared: [Injector::new(), Injector::new(), Injector::new()],
            stealers,
            holders: holders.into_boxed_slice(),
            bound: bound.max(1),
            searching: AtomicUsize::new(0),
            counts: Counts::default(),
        });
        (pool, queues)
    }

    /// What its tasks did so far.
    pub fn stats(&self) -> Stats {
        let c = &self.counts;
        Stats {
            spawned: read(&c.spawned),
            polls: read(&c.polls),
            stolen: read(&c.stolen),
            from_shared: read(&c.from_shared),
            overflowed: read(&c.overflowed),
            handed_off: read(&c.handed_off),
            notified: read(&c.notified),
        }
    }

    /// Spawns a movable task from off its shards (a test, a thread of the process's own): on the shared queue, a
    /// parked shard kicked.
    #[track_caller]
    pub(crate) fn spawn_shared<F: Future<Output = ()> + Send + 'static>(self: &Arc<Pool>, class: Class, future: F) {
        let task = Header::new(self, class, Box::pin(future), Location::caller(), 0);
        add(&self.counts.spawned, 1);
        self.inject(task);
    }

    /// Puts `task` on the shared queue of its class and kicks the shard that polled it last if that one is
    /// parked, else another parked one: whoever wakes takes it as it looks for work.
    fn inject(&self, task: Task) {
        let last = task.last.load(std::sync::atomic::Ordering::Relaxed);
        let class = index_of(task.class());
        if let Some(shared) = self.shared.get(class) {
            shared.push(task);
        }
        let kicked = self
            .holders
            .get(last)
            .is_some_and(|holder| registry::kick_if_parked(*holder));
        if !kicked {
            self.notify(None);
        }
    }

    /// Kicks one parked shard other than `except` to come and look for work: whether one was.
    fn notify(&self, except: Option<usize>) -> bool {
        let count = self.holders.len();
        let first = except.map_or(0, |at| at.saturating_add(1));
        for step in 0..count {
            let Some(at) = first.wrapping_add(step).checked_rem(count) else {
                break;
            };
            if Some(at) == except {
                continue;
            }
            if self
                .holders
                .get(at)
                .is_some_and(|holder| registry::kick_if_parked(*holder))
            {
                add(&self.counts.notified, 1);
                return true;
            }
        }
        false
    }

    /// Whether any shared queue holds a task (a shard's re-check before it parks).
    pub(crate) fn shared_waiting(&self) -> bool {
        self.shared.iter().any(|shared| !shared.is_empty())
    }

    /// Whether a movable task was ever spawned on this runtime (kiln's goal 9, plan 01a1a87b2cec step 5). Before the
    /// first, every queue is empty and every task a shard runs is pinned, so an idle shard's look and a parking
    /// shard's re-check of the shared queues have nothing to find: both skip. kiln's default placement makes no
    /// movable task, and each idle turn paid a pool-wide count's two read-modify-writes and a read of every
    /// sibling's queues, and each park an `Arc` count's two, on lines every shard shares. A spawn counts itself
    /// before its task is queued; a task put on a shared queue kicks a parked shard after (`inject`), the kick's
    /// fence ordering the count before the park's read of it, so a shard that parked having read none is kicked.
    fn in_use(&self) -> bool {
        read(&self.counts.spawned) != 0
    }

    /// Moves the queued tasks of the shard at `index` to the shared queues and kicks parked siblings, as many as
    /// tasks moved: the watchdog's, for a shard held in one poll past its limit. How many moved.
    pub(crate) fn hand_off(&self, index: usize) -> u64 {
        let Some(stealers) = self.stealers.get(index) else {
            return 0;
        };
        let mut moved = 0u64;
        for (stealer, shared) in stealers.iter().zip(&self.shared) {
            while let Some(task) = take(|| stealer.steal()) {
                shared.push(task);
                moved = moved.saturating_add(1);
            }
        }
        add(&self.counts.handed_off, moved);
        let mut told = 0u64;
        while told < moved && self.notify(Some(index)) {
            told = told.saturating_add(1);
        }
        moved
    }
}

/// A steal, retried while the queue says to.
fn take(mut attempt: impl FnMut() -> Steal<Task>) -> Option<Task> {
    loop {
        match attempt() {
            Steal::Success(task) => return Some(task),
            Steal::Empty => return None,
            Steal::Retry => {}
        }
    }
}

// ---------------------------------------------------------------------------------------- local

/// A shard's side of its runtime's movable tasks, on its context: its queues once its runtime installs them.
#[derive(Debug, Default)]
pub(crate) struct Local {
    installed: OnceCell<Installed>,
}

/// A shard's queues and carriers.
struct Installed {
    pool: Arc<Pool>,
    /// This shard's place among the pool's.
    index: usize,
    queues: Queues,
    /// Each class's carrier, while it lives.
    carriers: [Cell<Option<TaskId>>; CLASSES],
    /// Whether each class's carrier is listed or polling: a push wakes one that is not.
    awake: [Cell<bool>; CLASSES],
    /// The movable task being polled now, and its header's address (its waker's data pointer).
    current: Cell<Option<Task>>,
    current_at: Cell<usize>,
    /// Where a look for work starts among the siblings (xorshift64).
    random: Cell<u64>,
    /// Polls since the carriers last took from the shared queues.
    since_shared: Cell<usize>,
}

impl std::fmt::Debug for Installed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Installed")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl Local {
    /// Gives the shard its queues (its runtime's, as the shard's thread starts). A second install is refused.
    pub(crate) fn install(&self, pool: Arc<Pool>, index: usize, queues: Queues) -> Result<(), RtError> {
        let seed = crate::machine::clock::monotonic_ns() ^ u64::try_from(index).unwrap_or(0);
        self.installed
            .set(Installed {
                pool,
                index,
                queues,
                carriers: Default::default(),
                awake: Default::default(),
                current: Cell::new(None),
                current_at: Cell::new(0),
                // xorshift64 needs a seed other than zero.
                random: Cell::new(seed | 1),
                since_shared: Cell::new(0),
            })
            .map_err(|_| RtError::BadConfig {
                what: "a shard's movable queues installed twice",
            })
    }

    fn installed(&self) -> Option<&Installed> {
        self.installed.get()
    }

    /// The shard's runtime's pool, once installed and in use ([`Pool::in_use`]): what the shard's park re-reads after
    /// its announcement.
    pub(crate) fn pool(&self) -> Option<Arc<Pool>> {
        self.installed()
            .filter(|local| local.pool.in_use())
            .map(|local| Arc::clone(&local.pool))
    }

    /// Queues `task` here when this shard is of its runtime (`pool`); the task back otherwise.
    fn push_if_ours(&self, ctx: &ShardContext, pool: &Arc<Pool>, task: Task) -> Result<(), Task> {
        match self.installed() {
            Some(local) if Arc::ptr_eq(&local.pool, pool) => {
                local.push(ctx, task);
                Ok(())
            }
            _ => Err(task),
        }
    }

    /// Whether `waker` is the movable task this shard polls now: its charges are its own (`crate::coop`).
    pub(crate) fn polling(&self, waker: &Waker) -> bool {
        self.installed().is_some_and(|local| {
            let at = local.current_at.get();
            at != 0 && waker.data().addr() == at
        })
    }

    /// The class of the movable task polled now, when one is.
    pub(crate) fn current_class(&self) -> Option<Class> {
        let local = self.installed()?;
        let task = local.current.take()?;
        let class = task.class();
        local.current.set(Some(task));
        Some(class)
    }

    /// Sets the class of the movable task polled now, when one is: the class it had.
    pub(crate) fn set_current_class(&self, class: Class) -> Option<Class> {
        let local = self.installed()?;
        let task = local.current.take()?;
        let had = class_of(task.class.swap(class as u8, Ordering::Relaxed));
        local.current.set(Some(task));
        Some(had)
    }

    /// Whether a shared queue holds a task: a shard re-checks before it parks, so a push to one between its last
    /// look and its park is not left behind.
    pub(crate) fn shared_waiting(&self) -> bool {
        self.installed()
            .is_some_and(|local| local.pool.in_use() && local.pool.shared_waiting())
    }

    /// The shard is idle: takes tasks from the shared queues, else half of a sibling's queue, and wakes the
    /// carriers that run them. Whether it took any.
    pub(crate) fn look(&self, ctx: &ShardContext) -> bool {
        self.installed()
            .is_some_and(|local| local.pool.in_use() && local.look(ctx))
    }
}

impl Installed {
    fn queue(&self, class: Class) -> Option<&Worker<Task>> {
        self.queues.get(index_of(class))
    }

    /// Queues `task` on this shard's queue of its class: the older half moved to the shared queue first when it is
    /// full; the class's carrier woken; a parked sibling kicked when the task waits behind another and no shard
    /// looks.
    fn push(&self, ctx: &ShardContext, task: Task) {
        let class = task.class();
        let Some(queue) = self.queue(class) else {
            return;
        };
        if queue.len() >= self.pool.bound {
            self.overflow(class, queue);
        }
        queue.push(task);
        self.wake_carrier(ctx, class);
        if queue.len() > 1 && self.pool.searching.load(std::sync::atomic::Ordering::Acquire) == 0 {
            self.pool.notify(Some(self.index));
        }
    }

    /// Moves the older half of a full queue to the shared queue.
    fn overflow(&self, class: Class, queue: &Worker<Task>) {
        let Some(shared) = self.pool.shared.get(index_of(class)) else {
            return;
        };
        let half = queue.len() / 2;
        let mut moved = 0u64;
        for _ in 0..half {
            match queue.pop() {
                Some(task) => {
                    shared.push(task);
                    moved = moved.saturating_add(1);
                }
                None => break,
            }
        }
        add(&self.pool.counts.overflowed, moved);
    }

    /// Wakes the carrier of `class`, starting it when it has none.
    fn wake_carrier(&self, ctx: &ShardContext, class: Class) {
        let at = index_of(class);
        let (Some(awake), Some(carrier)) = (self.awake.get(at), self.carriers.get(at)) else {
            return;
        };
        if awake.replace(true) {
            return;
        }
        match carrier.get() {
            Some(id) => ctx.wake_local(id.0.slot()),
            None => match start_carrier(ctx, class) {
                Ok(id) => carrier.set(Some(id)),
                // The arena full: the tasks wait for the next push, or for a sibling to take them.
                Err(_) => awake.set(false),
            },
        }
    }

    /// Moves a batch of the shared queue of `class` onto this shard's: whether any came.
    fn refill(&self, class: Class) -> bool {
        let (Some(shared), Some(queue)) = (self.pool.shared.get(index_of(class)), self.queue(class)) else {
            return false;
        };
        if shared.is_empty() {
            return false;
        }
        let before = queue.len();
        let Some(task) = take(|| shared.steal_batch_and_pop(queue)) else {
            return false;
        };
        queue.push(task);
        let came = queue.len().saturating_sub(before);
        add(&self.pool.counts.from_shared, u64::try_from(came).unwrap_or(u64::MAX));
        self.since_shared.set(0);
        true
    }

    /// The idle shard's look (see [`Local::look`]).
    fn look(&self, ctx: &ShardContext) -> bool {
        let mut found = false;
        for class in URGENCY {
            if self.refill(class) {
                self.wake_carrier(ctx, class);
                found = true;
            }
        }
        found || self.steal(ctx)
    }

    /// Half of one sibling's queue of the most urgent class it has, starting at a random sibling, while fewer than
    /// half the shards look (f6). Whether any came.
    fn steal(&self, ctx: &ShardContext) -> bool {
        let count = self.pool.stealers.len();
        if count < 2 {
            return false;
        }
        let most = (count / 2).max(1);
        let searching = &self.pool.searching;
        if searching.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= most {
            searching.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            return false;
        }
        let start = self.next_random();
        let found = (0..count).find_map(|step| {
            let at = usize::try_from(start)
                .unwrap_or(0)
                .wrapping_add(step)
                .checked_rem(count)?;
            if at == self.index {
                return None;
            }
            let stealers = self.pool.stealers.get(at)?;
            URGENCY
                .into_iter()
                .find_map(|class| self.steal_from(stealers.get(index_of(class))?, class))
        });
        searching.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        if let Some(class) = found {
            self.wake_carrier(ctx, class);
        }
        found.is_some()
    }

    /// Half of `stealer`'s queue of `class` onto this shard's: the class, when any came.
    fn steal_from(&self, stealer: &Stealer<Task>, class: Class) -> Option<Class> {
        // Two loads, where a steal pins crossbeam's epoch first: an idle shard asks every sibling's every class.
        if stealer.is_empty() {
            return None;
        }
        let queue = self.queue(class)?;
        let before = queue.len();
        let task = take(|| stealer.steal_batch_and_pop(queue))?;
        queue.push(task);
        let came = queue.len().saturating_sub(before);
        add(&self.pool.counts.stolen, u64::try_from(came).unwrap_or(u64::MAX));
        Some(class)
    }

    fn next_random(&self) -> u64 {
        // Marsaglia's xorshift64 (13, 7, 17).
        let mut x = self.random.get();
        x ^= x.wrapping_shl(13);
        x ^= x.wrapping_shr(7);
        x ^= x.wrapping_shl(17);
        self.random.set(x);
        x
    }

    /// One turn of the carrier of `class`: the tasks queued as it began, each polled once, until the turn has used
    /// its share; whether any still wait.
    fn turn(&self, ctx: &ShardContext, class: Class) -> bool {
        let Some(queue) = self.queue(class) else {
            return false;
        };
        // The shared queue now and then, so what waits there is not left behind a queue that is never empty: once
        // a shard's batch of polls (Go and Tokio look at theirs every 61, research f6).
        if queue.is_empty() || self.since_shared.get() >= self.pool.bound {
            let _ = self.refill(class);
        }
        let left = queue.len();
        for _ in 0..left {
            let Some(task) = queue.pop() else {
                break;
            };
            self.run(ctx, task);
            self.since_shared
                .set(self.since_shared.get().saturating_add(1));
            if ctx.coop_spent.get() || !ctx.charge(true) {
                break;
            }
        }
        !queue.is_empty()
            || self
                .pool
                .shared
                .get(index_of(class))
                .is_some_and(|shared| !shared.is_empty())
    }

    /// Polls `task` once, as a carrier does: queued again when woken during the poll, ended when it returns or
    /// unwinds.
    fn run(&self, ctx: &ShardContext, task: Task) {
        if !task.state.begin() {
            return;
        }
        task.last.store(self.index, std::sync::atomic::Ordering::Relaxed);
        let Some(mut body) = task.body.take() else {
            task.state.done();
            return;
        };
        add(&self.pool.counts.polls, 1);
        let waker = Waker::from(Arc::clone(&task));
        // Its waker's data pointer: std's `Wake` makes a waker of the header's own address.
        self.current_at.set(Arc::as_ptr(&task).addr());
        self.current.set(Some(Arc::clone(&task)));
        let polled = crate::unwind::contained(|| {
            body.future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
        });
        self.current.set(None);
        self.current_at.set(0);
        match polled {
            Some(Poll::Pending) => {
                task.body.store(Some(body));
                if task.state.pending() {
                    self.push(ctx, task);
                }
            }
            // Returned, or unwound inside the boundary: the task ends, its future dropped here.
            Some(Poll::Ready(())) | None => {
                task.state.done();
                let _ = crate::unwind::contained(move || drop(body));
            }
        }
    }
}

/// Starts the carrier of `class` on the shard `ctx` is: a pinned task of that class.
fn start_carrier(ctx: &ShardContext, class: Class) -> Result<TaskId, RtError> {
    ctx.claim(Incoming {
        future: TaskFuture::Boxed(Box::pin(carrier(class))),
        parent: None,
        joinable: false,
        service: false,
        at: Location::caller(),
        class: Some(class),
    })
}

/// A carrier: polls its shard's movable tasks of its class, a turn at a time, for as long as its shard runs.
async fn carrier(class: Class) {
    let _gone = Gone(class);
    std::future::poll_fn(move |cx: &mut Context<'_>| carry(class, cx)).await;
}

/// One turn: yields behind the shard's inputs while tasks wait, else sleeps until a push wakes it.
fn carry(class: Class, cx: &mut Context<'_>) -> Poll<()> {
    let more = registry::with_current(|ctx| {
        let local = ctx.movable.installed()?;
        let more = local.turn(ctx, class);
        if !more && let Some(awake) = local.awake.get(index_of(class)) {
            awake.set(false);
        }
        Some(more)
    })
    .flatten();
    match more {
        Some(true) => {
            cx.waker().wake_by_ref();
            Poll::Pending
        }
        Some(false) => Poll::Pending,
        // Off its shard, or a shard with no queues: nothing to carry.
        None => Poll::Ready(()),
    }
}

/// A carrier's end: its shard forgets it, so a push starts another.
struct Gone(Class);

impl Drop for Gone {
    fn drop(&mut self) {
        let _ = registry::with_current(|ctx| {
            if let Some(local) = ctx.movable.installed() {
                let at = index_of(self.0);
                if let Some(carrier) = local.carriers.get(at) {
                    carrier.set(None);
                }
                if let Some(awake) = local.awake.get(at) {
                    awake.set(false);
                }
            }
        });
    }
}

// ------------------------------------------------------------------------------------------ api

/// Spawns `future` as a movable task of `class` on the calling shard's queue: it runs there, a turn of its class
/// at a time, unless an idle shard of the same runtime takes it first; once it waits, a wake puts it on the queue
/// of the shard that woke it (the module's notes). On a shard whose runtime has no movable queues (a local or
/// simulated runtime) it is a pinned task of that class. Refused off a shard (`NotOnShardThread`), and as a
/// pinned spawn is when the shard takes no more.
#[track_caller]
pub fn spawn<F: Future<Output = ()> + Send + 'static>(class: Class, future: F) -> Result<(), RtError> {
    let at = Location::caller();
    registry::with_current(|ctx| match ctx.movable.installed() {
        Some(local) => {
            let task = Header::new(&local.pool, class, Box::pin(future), at, local.index);
            add(&local.pool.counts.spawned, 1);
            local.push(ctx, task);
            Ok(())
        }
        None => ctx
            .claim(Incoming {
                future: TaskFuture::Boxed(Box::pin(future)),
                parent: None,
                joinable: false,
                service: false,
                at,
                class: Some(class),
            })
            .map(|_| ()),
    })
    .unwrap_or(Err(RtError::NotOnShardThread))
}

/// Whether the calling shard's runtime has movable queues: [`spawn`] then makes a movable task, not a pinned one.
pub fn available() -> bool {
    registry::with_current(|ctx| ctx.movable.installed().is_some()).unwrap_or(false)
}

/// Whether a task's poll now is a movable task's (its leaves then wait through a helper on this shard).
pub fn polling_movable() -> bool {
    registry::with_current(|ctx| {
        ctx.movable
            .installed()
            .is_some_and(|local| local.current_at.get() != 0)
    })
    .unwrap_or(false)
}

#[cfg(all(test, loom))]
mod loom_tests {
    use super::*;
    use crate::mem::loom_bounds;
    use loom::sync::atomic::AtomicUsize as LoomUsize;

    /// A shard polls a queued task while another thread wakes it: whichever way they interleave, the task is queued
    /// again at most once, by exactly one of them, and only when the wake came after its poll began; and whoever
    /// queues it finds its future back in its cell (the cell modelled by a token the poll takes and puts back).
    #[test]
    fn a_wake_during_a_poll_queues_the_task_once_with_its_future() {
        loom_bounds::explore("movable: a wake against a poll", || {
            let state: &'static State = Box::leak(Box::new(State::queued()));
            let token: &'static LoomUsize = Box::leak(Box::new(LoomUsize::new(1)));
            let queued: &'static LoomUsize = Box::leak(Box::new(LoomUsize::new(0)));
            let runner = loom::thread::spawn(move || {
                assert!(state.begin(), "a queued task begins");
                assert_eq!(token.swap(0, Ordering::AcqRel), 1, "its future is in its cell");
                // The poll returns pending: the future goes back before the state says so.
                token.store(1, Ordering::Release);
                if state.pending() {
                    assert_eq!(token.load(Ordering::Acquire), 1, "queued again with its future");
                    queued.fetch_add(1, Ordering::AcqRel);
                }
            });
            let waker = loom::thread::spawn(move || {
                if state.wake() {
                    // Whoever queues it hands it to a shard that takes its future.
                    assert_eq!(token.load(Ordering::Acquire), 1, "queued by its waker with its future");
                    queued.fetch_add(1, Ordering::AcqRel);
                }
            });
            runner.join().unwrap();
            waker.join().unwrap();
            let times = queued.load(Ordering::Acquire);
            let now = state.0.load(Ordering::Acquire);
            assert!(times <= 1, "queued {times} times");
            // Not queued again only when the wake came before the poll began, which the poll then saw.
            assert!(
                (times == 1 && now == QUEUED) || (times == 0 && now == IDLE),
                "queued {times} times, state {now}"
            );
        });
    }

    /// Two wakes race while the task is idle: one of them queues it, the other does nothing.
    #[test]
    fn two_wakes_of_an_idle_task_queue_it_once() {
        loom_bounds::explore("movable: two wakes of an idle task", || {
            let state: &'static State = Box::leak(Box::new(State(AtomicU8::new(IDLE))));
            let queued: &'static LoomUsize = Box::leak(Box::new(LoomUsize::new(0)));
            let wakes: Vec<_> = (0..2)
                .map(|_| {
                    loom::thread::spawn(move || {
                        if state.wake() {
                            queued.fetch_add(1, Ordering::AcqRel);
                        }
                    })
                })
                .collect();
            for wake in wakes {
                wake.join().unwrap();
            }
            assert_eq!(queued.load(Ordering::Acquire), 1);
            assert_eq!(state.0.load(Ordering::Acquire), QUEUED);
        });
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// The state word's transitions, one thread: a wake while running marks the task, and its poll's end queues it
    /// again; a wake while idle queues it; a wake while queued or ended does nothing; an ended task does not begin.
    #[test]
    fn the_state_words_transitions() {
        let s = State::queued();
        assert!(!s.wake(), "a queued task is not queued again");
        assert!(s.begin());
        assert!(!s.wake(), "a running task is marked, not queued");
        assert!(s.pending(), "marked during its poll: queued again");
        assert!(s.begin());
        assert!(!s.pending(), "not woken: idle");
        assert!(s.wake(), "an idle task is queued by its wake");
        assert!(s.begin());
        s.done();
        assert!(!s.wake());
        assert!(!s.begin());
        assert_eq!(s.0.load(Ordering::Acquire), DONE);
    }

    /// A task of `class` on `pool`, its future a no-op.
    fn noop(pool: &Arc<Pool>, class: Class) -> Task {
        Header::new(pool, class, Box::pin(async {}), Location::caller(), 0)
    }

    /// The watchdog's hand-off: every task queued on a held shard's queues moves to the shared queues of their
    /// classes, counted; the other shard's queues are left alone.
    #[test]
    fn a_held_shards_queues_move_to_the_shared_queues() {
        let holders = vec![SlotHolder::for_test(), SlotHolder::for_test()];
        let (pool, queues) = Pool::new(holders, 64);
        let critical = &queues[0][index_of(Class::Critical)];
        let normal = &queues[0][index_of(Class::Normal)];
        for _ in 0..5 {
            critical.push(noop(&pool, Class::Critical));
        }
        for _ in 0..3 {
            normal.push(noop(&pool, Class::Normal));
        }
        queues[1][index_of(Class::Normal)].push(noop(&pool, Class::Normal));
        assert_eq!(pool.hand_off(0), 8);
        assert!(critical.is_empty() && normal.is_empty());
        assert_eq!(pool.shared[index_of(Class::Critical)].len(), 5);
        assert_eq!(pool.shared[index_of(Class::Normal)].len(), 3);
        assert_eq!(queues[1][index_of(Class::Normal)].len(), 1, "the other shard's queue stays");
        assert_eq!(pool.stats().handed_off, 8);
        assert_eq!(pool.hand_off(7), 0, "no such shard");
    }

    /// Kiln's goal 9, plan 01a1a87b2cec step 5: a pool no movable task was spawned on is not in use, so an idle
    /// shard's look and a parking shard's re-check skip it; its first spawn puts it in use, and the task that spawn
    /// put on the shared queue is there for a parking shard's re-check to find.
    #[test]
    fn g9_s5_a_pool_no_movable_task_was_spawned_on_is_skipped_until_its_first() {
        let holders = vec![SlotHolder::for_test(), SlotHolder::for_test()];
        let (pool, _queues) = Pool::new(holders, 64);
        assert!(!pool.in_use(), "nothing spawned: not in use");
        assert!(!pool.shared_waiting());
        pool.spawn_shared(Class::Normal, async {});
        assert!(pool.in_use(), "its first spawn puts it in use");
        assert!(pool.shared_waiting(), "and its task waits on the shared queue");
        assert_eq!(pool.stats().spawned, 1);
    }

    #[test]
    fn classes_round_trip() {
        for class in URGENCY {
            assert_eq!(class_of(class as u8), class);
            assert!(index_of(class) < CLASSES);
        }
    }
}
