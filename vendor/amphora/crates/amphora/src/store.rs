//! A database's commit pipeline and its readers ([`Store`], CP-T1.1).
//!
//! - **One writer.** A thread owns the engine. Rows reach it by channel, never by lock, and are
//!   committed as soon as it takes them, with whatever else came while it committed the last: a
//!   group commit, no row waiting for a timer. A full queue refuses rows ([`Refused::Full`]) rather
//!   than growing without bound; [`Store::put_wait`] waits for room instead.
//! - **Durable beside the writer.** A commit doesn't wait for the disk: a flusher thread flushes
//!   what was committed, over and over, and a write is durable once a flush that began after its
//!   commit ends ([`Store::durable`] waits for that, by the [`Ticket`] its batch was handed with).
//! - **One batch's failure is its own.** A commit that fails is tried again batch by batch: the
//!   batches that commit alone are kept, and one that can't is refused to its sender
//!   ([`Refused::Failed`]), nothing of it written.
//! - **Readers beside the writer.** A read takes a connection of its own from a pool and runs on
//!   its caller's thread, once the writes of its key handed over before it are committed.
//! - **No checkpoint in a commit's way.** A checkpointer with a connection of its own copies the log
//!   into the file without waiting for anyone. Once the log has grown past its bound it copies
//!   without pausing until little is left, and then the writer starts the log again between
//!   commits, holding the readers back for the moment it takes (a log only starts again once
//!   nothing reads from it).
//!
//! What it counts is in [`Meter`].

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::meter::{Counters, Meter};

/// Flushes what an engine has committed to its disk, from the store's flusher thread.
pub type Flush = Box<dyn Fn() -> std::io::Result<()> + Send + Sync>;

/// Opens a connection for reading on an engine's database, from any thread.
pub type Opener<R> = Arc<dyn Fn() -> anyhow::Result<R> + Send + Sync>;

/// What a store's writer drives.
pub trait Engine: Send + 'static {
    /// What a write carries.
    type Row: Send + 'static;
    /// A connection of its own for reading.
    type Reader: Send + 'static;

    /// The key a row is of: a read of a key waits for its writes handed over before it.
    fn key(row: &Self::Row) -> &str;

    /// The rows, in one transaction, committed whole or not at all: how many it wrote. They stay
    /// the store's: a commit that fails is tried again with fewer of them.
    fn apply(&mut self, rows: &[Self::Row]) -> anyhow::Result<usize>;

    /// The rows of whole batches (batch `i`'s end in `rows` at `ends[i]`), as many from the first
    /// as one commit of the engine holds, at least one, in one transaction: how many batches it
    /// committed and how many rows it wrote. The rest are handed to it again. By default all of
    /// them, as [`Engine::apply`] commits them; an engine whose commit outgrows its memory takes
    /// fewer (docs/design/store.md, decision 23).
    fn apply_batches(
        &mut self,
        rows: &[Self::Row],
        ends: &[usize],
    ) -> anyhow::Result<(usize, usize)> {
        let written = self.apply(rows)?;
        Ok((ends.len(), written))
    }

    /// How what it committed is flushed beside its writer (None: a commit is durable as it ends).
    fn flusher(&self) -> Option<Flush> {
        None
    }

    /// Opens readers on its database, from any thread (None: it reads only on itself).
    fn opener(&self) -> Option<Opener<Self::Reader>> {
        None
    }

    /// Copies what the log holds into the file as far as it can without waiting for the writer or
    /// the readers: what the log holds after.
    fn checkpoint(reader: &mut Self::Reader) -> anyhow::Result<Checkpointed> {
        let _ = reader;
        Ok(Checkpointed::default())
    }

    /// Copies the rest of the log into the file and starts it again, with no reader reading (the
    /// store holds them back meanwhile): whether it did (not: something was in the way).
    fn restart_log(&mut self) -> anyhow::Result<bool> {
        Ok(true)
    }

    /// Its own upkeep, asked after every turn of the writer (`quiet`: nothing waited for it): when
    /// to ask again while nothing comes, and what its readers must do. An error stops the store.
    fn upkeep(&mut self, quiet: bool) -> anyhow::Result<Upkept> {
        let _ = quiet;
        Ok(Upkept::default())
    }
}

/// What a checkpoint left, in frames of the log: how many it holds, and how many of those aren't in
/// the file yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Checkpointed {
    pub log: u64,
    pub left: u64,
}

/// What an engine's upkeep asks of its store.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Upkept {
    /// Ask again within this, while nothing comes (None: the store's idle period).
    pub again_in: Option<Duration>,
    pub readers: ForReaders,
}

/// What an engine's readers must do around its upkeep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ForReaders {
    #[default]
    Keep,
    /// It is about to replace its file: every reader lets go of it first.
    Release,
    /// Its file was replaced: readers open on the new one.
    Again,
}

/// A store's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Batches of rows waiting for the writer, at most.
    pub queue: usize,
    /// Rows that make a commit at once, however many more wait. A commit of fewer rows still ends
    /// where its engine's commit is full (decision 23).
    pub commit_rows: usize,
    /// Reading connections, at most.
    pub readers: usize,
    /// How often the checkpointer copies the log into the file while the log is within its bound.
    pub checkpoint_every: Duration,
    /// The log's bound, in frames: past it the checkpointer copies without pausing, and once
    /// little is left (a 64th of the bound) the writer starts the log again. The log stops at
    /// twice its bound: the writer starts it again then, however much is left.
    pub log_frames: u64,
    /// How often the writer asks its engine's upkeep while nothing comes, unless it asks sooner.
    pub idle_every: Duration,
    /// Flusher threads: with more than one, a commit that lands while a flush is under way starts
    /// a flush of its own on an idle flusher instead of waiting for that one to end.
    pub flushers: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // A channel of this bound takes its slots at once: a full queue refuses (`put`) or
            // waits for room (`put_wait`), so it needn't be deep.
            queue: 4096,
            // A commit's pages fit its writer's cache: at 10,000 writers a commit of 512 rows wrote
            // 35,000-41,000 rows a second, of 2,048 rows 22,000-24,000 (fedora, 2026-10-20).
            commit_rows: 512,
            readers: 4,
            checkpoint_every: Duration::from_millis(25),
            // Each restart of the log holds the writer for 10-14 ms (on fedora's btrfs: what is
            // left copied, the file flushed, a new header synced). At 32 writers, whose commits
            // write about 73 frames each, a bound of 16,384 restarted 27 times in 20 s, and the
            // writes it held made p99.9 12-16 ms; at 131,072, 3-4 times, p99.9 9.1-10.6 ms, as with
            // no checkpoint at all (fedora, 2026-10-20). A log takes up to twice this many frames
            // of disk (1 GiB of 4 KiB pages) and keeps it.
            log_frames: 131_072,
            idle_every: Duration::from_secs(1),
            flushers: 1,
        }
    }
}

/// Why a store didn't take or answer something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Its writer's queue is full.
    Full,
    /// Its writer has stopped.
    Stopped,
    /// It didn't answer in time.
    Timeout,
    /// The batch's commit failed: nothing of it was written.
    Failed,
    /// A flush of its disk failed: nothing more is made durable until the store is started again
    /// on what its disk holds. After a failed flush the system may have dropped what it couldn't
    /// write, so a flush that succeeds after it proves nothing.
    Disk,
    /// The batch's rows are in more than one group ([`crate::groups`]): a batch is one group's.
    Groups,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => "the store's writer is full",
            Self::Stopped => "the store's writer has stopped",
            Self::Timeout => "the store didn't answer in time",
            Self::Failed => "the batch couldn't be committed; nothing of it was written",
            Self::Disk => {
                "a flush of the store's disk failed; nothing more is made durable until it is started again"
            }
            Self::Groups => "the batch's rows are in more than one group; a batch is one group's",
        })
    }
}

impl std::error::Error for Refused {}

type Job<E> = Box<dyn FnOnce(&mut E) + Send>;

enum Msg<E: Engine> {
    /// Rows, the keys they are of, and what becomes of them.
    Rows(Vec<E::Row>, Vec<String>, Arc<Fate>),
    /// Run on the engine, between commits (what was sent before it is committed first).
    Run(Job<E>),
}

/// Told after every commit; false: it doesn't want to be told again.
pub type Watcher = Box<dyn Fn() -> bool + Send + Sync>;

/// A batch handed to a store's writer: [`Store::durable`] says what became of it.
#[derive(Debug, Clone)]
pub struct Ticket(Option<Arc<Fate>>);

/// Told once its batch is durable, or why it won't be; on the store's notifier thread, so quickly.
pub type Then = Box<dyn FnOnce(Result<(), Refused>) + Send>;

/// What becomes of a batch: its state, and who is told once it is durable.
struct Fate {
    /// [`UNSETTLED`], [`FAILED`], or the number of the commit its batch is in (numbered from 1).
    state: AtomicU64,
    then: Mutex<Option<Then>>,
}

impl std::fmt::Debug for Fate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fate")
            .field("state", &self.state.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// A batch not yet committed, nor failed.
const UNSETTLED: u64 = 0;
/// A batch whose commit failed.
const FAILED: u64 = u64::MAX;

/// The batches whose senders are told once they are durable ([`Store::put_then`]), for the
/// notifier.
#[derive(Default)]
struct Notices {
    /// Committed, in commit order: told once a flush covers their commit.
    awaiting: VecDeque<(u64, Arc<Fate>)>,
    /// Refused: told at once.
    refused: Vec<(Arc<Fate>, Refused)>,
    /// Commits durable.
    durable: u64,
    disk_failed: bool,
}

/// Batches settled (committed, failed or let go) and commits made, counted together.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Progress {
    batches: u64,
    commits: u64,
}

/// What was handed to the writer, and how much of it is committed and durable, counted rather
/// than numbered in order (that took one lock with every batch queued and every read):
/// - a read of a key waits until as many of the key's batches are committed as had been handed
///   over when it began;
/// - a sync waits until as many batches in all are durable as had been handed over when it began.
struct Sends {
    /// The keys with batches handed and not yet committed, in shards by key: an entry goes once all
    /// it was handed is committed.
    keys: Vec<Mutex<HashMap<String, Counts>>>,
    /// Where a key would go if a shard were missing (none is: the index is taken modulo their number).
    spare: Mutex<HashMap<String, Counts>>,
    /// Gives each key's entry its own number: a reader that finds another entry (or none) where it
    /// read one knows everything its entry was handed is committed.
    epochs: AtomicU64,
    handed: AtomicU64,
    /// Batches settled in all (committed, failed or let go), and commits made: readers see them.
    committed: Mutex<Progress>,
    done: Condvar,
    /// Batches settled and commits made, durable: a flush that began after them ended.
    durable: Mutex<Progress>,
    flushed: Condvar,
    /// Its engine's commits are durable as they end: no flusher.
    durable_as_committed: AtomicBool,
    /// The writer has stopped: the flusher flushes what is left, then stops.
    stopping: AtomicBool,
    /// A flush failed: the store takes nothing more, and acknowledges nothing more durable.
    disk_failed: AtomicBool,
    /// The writer took batches off its queue: a sender waiting for room looks again.
    room: (Mutex<()>, Condvar),
    /// The most batches any flusher has begun a flush for: an idle flusher begins one once more are
    /// committed.
    claimed: Mutex<u64>,
    notices: Mutex<Notices>,
    noticed: Condvar,
    /// Since when the writer has had nothing to do (nanoseconds past `began`; 0 while it is busy):
    /// it committed what it had and found its queue empty.
    idle_since: AtomicU64,
    /// The times the writer went idle.
    idles: AtomicU64,
    began: Instant,
}

/// A key's batches handed and committed, under its entry's number.
struct Counts {
    epoch: u64,
    handed: u64,
    committed: u64,
}

/// Shards of the keys' counts.
const SHARDS: u64 = 64;

/// A sender waiting for room looks again at least this often (a wakeup lost between its try and
/// its wait costs no more).
const ROOM_LOOK: Duration = Duration::from_millis(5);

impl Default for Sends {
    fn default() -> Self {
        Self {
            keys: (0..SHARDS).map(|_| Mutex::default()).collect(),
            spare: Mutex::default(),
            epochs: AtomicU64::new(1),
            handed: AtomicU64::new(0),
            committed: Mutex::new(Progress::default()),
            done: Condvar::new(),
            durable: Mutex::new(Progress::default()),
            flushed: Condvar::new(),
            durable_as_committed: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            disk_failed: AtomicBool::new(false),
            room: (Mutex::new(()), Condvar::new()),
            claimed: Mutex::new(0),
            notices: Mutex::default(),
            noticed: Condvar::new(),
            idle_since: AtomicU64::new(0),
            idles: AtomicU64::new(0),
            began: Instant::now(),
        }
    }
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Sends {
    fn shard(&self, key: &str) -> MutexGuard<'_, HashMap<String, Counts>> {
        let h = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        });
        let i = usize::try_from(h.checked_rem(SHARDS).unwrap_or(0)).unwrap_or(0);
        locked(self.keys.get(i).unwrap_or(&self.spare))
    }

    /// A batch with rows of `keys` handed over (before it is queued).
    fn handing(&self, keys: &[String]) {
        for k in keys {
            let mut shard = self.shard(k);
            let c = shard.entry(k.clone()).or_insert_with(|| Counts {
                epoch: self.epochs.fetch_add(1, Ordering::Relaxed),
                handed: 0,
                committed: 0,
            });
            c.handed = c.handed.saturating_add(1);
        }
        self.handed.fetch_add(1, Ordering::Relaxed);
    }

    /// A batch of `keys` committed, or let go (a send that failed, a commit that failed): nothing
    /// waits on it any longer once [`Sends::wake`] counts it.
    fn settled(&self, keys: &[String]) {
        for k in keys {
            let mut shard = self.shard(k);
            if let Some(c) = shard.get_mut(k) {
                c.committed = c.committed.saturating_add(1);
                if c.committed >= c.handed {
                    shard.remove(k);
                }
            }
        }
    }

    /// `batches` more are settled, and `commits` made in all: readers and senders waiting on them
    /// look again.
    fn wake(&self, batches: u64, commits: u64) {
        let now = {
            let mut committed = locked(&self.committed);
            committed.batches = committed.batches.saturating_add(batches);
            committed.commits = committed.commits.max(commits);
            self.done.notify_all();
            *committed
        };
        if self.durable_as_committed.load(Ordering::Relaxed) {
            self.made_durable(now);
        }
    }

    /// Waits until the batches of `key` handed over before now are committed (every key's: None),
    /// up to `until`: whether they are.
    fn wait(&self, key: Option<&str>, until: Instant) -> bool {
        match key {
            None => {
                let target = self.handed.load(Ordering::Relaxed);
                self.wait_until(until, |_| true, target)
            }
            Some(k) => {
                let Some((epoch, target)) = self.shard(k).get(k).map(|c| (c.epoch, c.handed))
                else {
                    return true;
                };
                self.wait_until(
                    until,
                    |this| {
                        this.shard(k)
                            .get(k)
                            .is_none_or(|c| c.epoch != epoch || c.committed >= target)
                    },
                    0,
                )
            }
        }
    }

    /// Waits until `ready` holds and the batches settled reach `total`, up to `until`.
    fn wait_until(&self, until: Instant, ready: impl Fn(&Self) -> bool, total: u64) -> bool {
        let mut committed = locked(&self.committed);
        while !(committed.batches >= total && ready(self)) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            committed = self
                .done
                .wait_timeout(committed, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    fn made_durable(&self, through: Progress) {
        {
            let mut durable = locked(&self.durable);
            if through.batches <= durable.batches && through.commits <= durable.commits {
                return;
            }
            durable.batches = durable.batches.max(through.batches);
            durable.commits = durable.commits.max(through.commits);
            self.flushed.notify_all();
        }
        let mut notices = locked(&self.notices);
        notices.durable = notices.durable.max(through.commits);
        self.noticed.notify_all();
    }

    /// The commits durable so far, once more than `seen` are, or `most` has passed, or the store
    /// stops or its disk failed: what a checkpointer that copied everything durable waits for
    /// (decision 25).
    fn flushed_past(&self, seen: u64, most: Duration) -> u64 {
        let began = Instant::now();
        let mut durable = locked(&self.durable);
        while durable.commits <= seen
            && !self.stopping.load(Ordering::Relaxed)
            && !self.disk_failed.load(Ordering::SeqCst)
        {
            let left = most.saturating_sub(began.elapsed());
            if left.is_zero() {
                break;
            }
            durable = self
                .flushed
                .wait_timeout(durable, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        durable.commits
    }

    /// The batches of a commit (numbered `commit`, or none: it failed) that have a sender to tell.
    fn to_tell(&self, fates: impl Iterator<Item = (Arc<Fate>, Option<u64>)>) {
        let mut notices = locked(&self.notices);
        for (fate, commit) in fates {
            match commit {
                Some(c) => notices.awaiting.push_back((c, fate)),
                None => notices.refused.push((fate, Refused::Failed)),
            }
        }
        self.noticed.notify_all();
    }

    /// Waits until as many batches are durable as had been handed over when it was asked, up to
    /// `until`.
    fn wait_durable(&self, until: Instant) -> Result<(), Refused> {
        let target = self.handed.load(Ordering::Relaxed);
        let mut durable = locked(&self.durable);
        while durable.batches < target {
            if self.disk_failed.load(Ordering::SeqCst) {
                return Err(Refused::Disk);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Refused::Timeout);
            }
            durable = self
                .flushed
                .wait_timeout(durable, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(())
    }

    /// Waits until the batch whose fate is `fate` is committed and then flushed, up to `until`; see
    /// [`Store::durable`].
    fn wait_ticket(&self, fate: &Fate, until: Instant) -> Result<(), Refused> {
        let commit = {
            let mut committed = locked(&self.committed);
            loop {
                match fate.state.load(Ordering::Acquire) {
                    FAILED => return Err(Refused::Failed),
                    UNSETTLED if self.disk_failed.load(Ordering::SeqCst) => {
                        return Err(Refused::Disk);
                    }
                    UNSETTLED if self.stopping.load(Ordering::Relaxed) => {
                        return Err(Refused::Stopped);
                    }
                    UNSETTLED => {}
                    commit => break commit,
                }
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(Refused::Timeout);
                }
                committed = self
                    .done
                    .wait_timeout(committed, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        };
        let mut durable = locked(&self.durable);
        while durable.commits < commit {
            if self.disk_failed.load(Ordering::SeqCst) {
                return Err(Refused::Disk);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Refused::Timeout);
            }
            durable = self
                .flushed
                .wait_timeout(durable, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(())
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
        let _committed = locked(&self.committed);
        self.done.notify_all();
    }

    /// Nanoseconds since the store began, never 0 (which `idle_since` keeps for busy).
    fn now(&self) -> u64 {
        u64::try_from(self.began.elapsed().as_nanos())
            .unwrap_or(u64::MAX)
            .max(1)
    }

    /// The writer committed what it had and found its queue empty: a flusher gathering looks again.
    fn went_idle(&self) {
        if self.idle_since.load(Ordering::Relaxed) == 0 {
            self.idle_since.store(self.now(), Ordering::Relaxed);
            self.idles.fetch_add(1, Ordering::Relaxed);
            let _committed = locked(&self.committed);
            self.done.notify_all();
        }
    }

    /// The writer took something off its queue.
    fn went_busy(&self) {
        if self.idle_since.load(Ordering::Relaxed) != 0 {
            self.idle_since.store(0, Ordering::Relaxed);
        }
    }

    /// How much longer a flusher gathering since `since` waits before it begins a flush, its
    /// flushes taking `flush` (smoothed): none once the writer has been idle for a 32nd of a flush
    /// (what the last flush's senders send next has come in by then) or half a flush has gone by
    /// (what waits already waits no more than that again).
    fn gathering(&self, since: Instant, flush: Duration) -> Option<Duration> {
        let most = flush.checked_div(2).unwrap_or_default();
        let left = most.checked_sub(since.elapsed()).filter(|d| !d.is_zero())?;
        match self.idle_since.load(Ordering::Relaxed) {
            0 => Some(left),
            idle => {
                let idle = Duration::from_nanos(self.now().saturating_sub(idle));
                let quiet = flush
                    .checked_div(32)
                    .unwrap_or_default()
                    .checked_sub(idle)
                    .filter(|d| !d.is_zero())?;
                Some(quiet.min(left))
            }
        }
    }

    /// A flush failed: whoever waits for a commit or a flush is told.
    fn disk_failed(&self) {
        self.disk_failed.store(true, Ordering::SeqCst);
        {
            let _committed = locked(&self.committed);
            self.done.notify_all();
        }
        {
            let _durable = locked(&self.durable);
            self.flushed.notify_all();
        }
        let mut notices = locked(&self.notices);
        notices.disk_failed = true;
        self.noticed.notify_all();
    }
}

/// The notifier: tells each sender of [`Store::put_then`] once its batch is durable, or why it won't
/// be, on a thread of its own so no flush waits for a sender's code. It stops once the writer has
/// stopped and no one is left to tell.
fn notifier(sends: &Sends) {
    loop {
        let (ready, refused) = {
            let mut notices = locked(&sends.notices);
            loop {
                let durable = notices.durable;
                let due = notices.awaiting.front().is_some_and(|(c, _)| *c <= durable);
                if due
                    || !notices.refused.is_empty()
                    || (notices.disk_failed && !notices.awaiting.is_empty())
                {
                    break;
                }
                if notices.awaiting.is_empty() && sends.stopping.load(Ordering::Relaxed) {
                    return;
                }
                notices = sends
                    .noticed
                    .wait_timeout(notices, Duration::from_secs(1))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            let durable = notices.durable;
            let mut ready = Vec::new();
            while notices.awaiting.front().is_some_and(|(c, _)| *c <= durable) {
                if let Some((_, fate)) = notices.awaiting.pop_front() {
                    ready.push(fate);
                }
            }
            let mut refused = std::mem::take(&mut notices.refused);
            // Committed but never to be durable: a flush failed.
            if notices.disk_failed {
                refused.extend(
                    notices
                        .awaiting
                        .drain(..)
                        .map(|(_, fate)| (fate, Refused::Disk)),
                );
            }
            (ready, refused)
        };
        let tell = |fate: &Fate, answer: Result<(), Refused>| {
            if let Some(then) = locked(&fate.then).take() {
                then(answer);
            }
        };
        for fate in &ready {
            tell(fate, Ok(()));
        }
        for (fate, why) in &refused {
            tell(fate, Err(*why));
        }
    }
}

/// A flusher: flushes what was committed as soon as there is some no flusher has begun a flush
/// for, again and again; each flush makes durable what was committed before it began. With one
/// flusher a commit waits for the flush under way and the next; with more, an idle one begins the
/// next at once. A flush that fails is the last.
///
/// Under load the writer never runs out of work, and flushes follow one another. When it ran out
/// during a flush, what comes next is mostly what that flush's senders send once told, and a flush
/// begun at the first of it would leave the rest to the flush after (each sender then waits two
/// flushes, half of them during each other's): so the flusher first gathers, waiting until the
/// writer has taken it all in ([`Sends::gathering`]).
fn flusher(sends: &Sends, flush: &Flush, meter: &Meter) {
    // The writer ran out of work during the last flush.
    let mut kept_up = false;
    // Flushes' time, smoothed (the first, then an eighth of each new one).
    let mut flush_time = Duration::ZERO;
    loop {
        let target = {
            let mut committed = locked(&sends.committed);
            let mut gathering: Option<Instant> = None;
            loop {
                let stopping = sends.stopping.load(Ordering::Relaxed);
                if committed.batches > *locked(&sends.claimed) {
                    let since = *gathering.get_or_insert_with(Instant::now);
                    match sends
                        .gathering(since, flush_time)
                        .filter(|_| kept_up && !stopping)
                    {
                        Some(wait) => {
                            committed = sends
                                .done
                                .wait_timeout(committed, wait)
                                .unwrap_or_else(PoisonError::into_inner)
                                .0;
                            continue;
                        }
                        None => {
                            let mut claimed = locked(&sends.claimed);
                            if committed.batches > *claimed {
                                *claimed = committed.batches;
                                break *committed;
                            }
                        }
                    }
                }
                if stopping || sends.disk_failed.load(Ordering::SeqCst) {
                    return;
                }
                committed = sends
                    .done
                    .wait_timeout(committed, Duration::from_secs(1))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        };
        let idles = sends.idles.load(Ordering::Relaxed);
        let began = Instant::now();
        match flush() {
            Ok(()) => {
                let took = began.elapsed();
                let eighth = |d: Duration| d.checked_div(8).unwrap_or_default();
                flush_time = if flush_time.is_zero() {
                    took
                } else {
                    flush_time
                        .saturating_sub(eighth(flush_time))
                        .saturating_add(eighth(took))
                };
                kept_up = sends.idle_since.load(Ordering::Relaxed) != 0
                    || sends.idles.load(Ordering::Relaxed) != idles;
                meter.flushed(took);
                sends.made_durable(target);
            }
            Err(e) => {
                // The last: after a failed flush the system may have dropped what it couldn't
                // write (Linux marks those pages clean), so a later flush that succeeds proves
                // nothing (docs/design/store.md, decision 9).
                tracing::error!(target: "amphora.store", event = "flush_failed", error = %e, "the log couldn't be flushed; the store makes nothing more durable until it is started again");
                sends.disk_failed();
                return;
            }
        }
    }
}

/// The readers: connections of their own for reading, which a read takes from the pool and uses on
/// its own thread (no thread between a read and its answer), opened as reads want them, up to the
/// store's bound. None while the engine replaces its file: reads go to the writer then.
struct Readers<R> {
    /// Opens a reader on the database in use; none while its file is being replaced.
    open: Mutex<Option<Opener<R>>>,
    pool: Mutex<Pool<R>>,
    returned: Condvar,
    most: usize,
    /// The checkpointer: a connection of its own copying the log into the file, never in a
    /// commit's way. It goes and comes back with the readers.
    checkpointer: Mutex<Option<(mpsc::Sender<()>, std::thread::JoinHandle<()>)>>,
    /// Held by each read and checkpoint as it runs, and by the writer alone while it starts the log
    /// again.
    gate: Arc<RwLock<()>>,
    /// The checkpointer found the log past its bound: the writer starts it again.
    restart: Arc<AtomicBool>,
    meter: Arc<Meter>,
}

/// The readers not reading, how many are reading, and which database they were opened on (a reader
/// of an earlier one, given back, is let go).
struct Pool<R> {
    idle: Vec<R>,
    out: usize,
    generation: u64,
}

impl<R: Send + 'static> Readers<R> {
    fn new(most: usize, meter: Arc<Meter>) -> Self {
        Self {
            open: Mutex::new(None),
            pool: Mutex::new(Pool {
                idle: Vec::new(),
                out: 0,
                generation: 0,
            }),
            returned: Condvar::new(),
            most: most.max(1),
            checkpointer: Mutex::new(None),
            gate: Arc::new(RwLock::new(())),
            restart: Arc::new(AtomicBool::new(false)),
            meter,
        }
    }

    /// Reads with `open`, and a checkpointer every `every` while the log is within `bound` frames.
    fn start(
        &self,
        open: Opener<R>,
        checkpoint: fn(&mut R) -> anyhow::Result<Checkpointed>,
        every: Duration,
        bound: u64,
        sends: Arc<Sends>,
    ) {
        *locked(&self.open) = Some(open.clone());
        let checkpointer = match open() {
            Ok(mut reader) => {
                let (tx, rx) = mpsc::channel::<()>();
                let (gate, restart, meter) =
                    (self.gate.clone(), self.restart.clone(), self.meter.clone());
                let thread = std::thread::Builder::new()
                    .name("amphora-checkpoint".into())
                    .spawn(move || {
                        let mut failures = 0u64;
                        // Past its bound, the log is copied without pausing, until a pass copies
                        // nothing new: only what a flush made durable is copied (decision 15), so
                        // the next pass waits for a flush (decision 25).
                        let mut draining = false;
                        let mut caught_up = false;
                        // The frames the last pass left copied, and the commits durable then.
                        let mut copied_through: Option<u64> = None;
                        let mut durable = 0u64;
                        let drained = (bound / 64).max(1);
                        loop {
                            // A restart asked for is the writer's to make: no pass holds it up.
                            let pause = if !draining || restart.load(Ordering::Relaxed) {
                                every
                            } else {
                                if caught_up {
                                    durable = sends.flushed_past(durable, every);
                                }
                                Duration::ZERO
                            };
                            if !matches!(
                                rx.recv_timeout(pause),
                                Err(mpsc::RecvTimeoutError::Timeout)
                            ) {
                                break;
                            }
                            let _copying = gate.read().unwrap_or_else(PoisonError::into_inner);
                            let began = Instant::now();
                            let copied = checkpoint(&mut reader);
                            meter.checkpointed(began.elapsed());
                            match copied {
                                Ok(c) => {
                                    draining = c.log > bound;
                                    let through = c.log.saturating_sub(c.left);
                                    caught_up = copied_through == Some(through);
                                    copied_through = Some(through);
                                    // What the writer's restart copies is what is left, and what
                                    // was committed since.
                                    if draining
                                        && (c.left <= drained || c.log > bound.saturating_mul(2))
                                    {
                                        restart.store(true, Ordering::Relaxed);
                                    }
                                }
                                Err(e) => {
                                    draining = false;
                                    caught_up = false;
                                    failures = failures.saturating_add(1);
                                    if failures.is_power_of_two() {
                                        tracing::warn!(target: "amphora.store", event = "checkpoint_failed", error = %format!("{e:#}"), failures, "the log couldn't be copied into the file");
                                    }
                                }
                            }
                        }
                    });
                thread.ok().map(|handle| (tx, handle))
            }
            Err(e) => {
                tracing::warn!(target: "amphora.store", event = "checkpointer_unopened", error = %format!("{e:#}"), "the checkpointer couldn't be opened");
                None
            }
        };
        *locked(&self.checkpointer) = checkpointer;
    }

    /// Lets every reader go, once each read under way is done: none holds the database after.
    fn release(&self) {
        *locked(&self.open) = None;
        {
            let mut pool = locked(&self.pool);
            pool.generation = pool.generation.wrapping_add(1);
            pool.idle.clear();
            while pool.out > 0 {
                pool = self
                    .returned
                    .wait(pool)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        if let Some((tx, handle)) = locked(&self.checkpointer).take() {
            drop(tx);
            let _ = handle.join();
        }
    }

    /// Starts the log again with `restart`, if the checkpointer asked, holding the readers and the
    /// checkpointer back meanwhile (each finishes what it is doing first).
    fn restart_if_asked(&self, restart: impl FnOnce() -> anyhow::Result<bool>) {
        if !self.restart.swap(false, Ordering::Relaxed) {
            return;
        }
        let asked = Instant::now();
        let _alone = self.gate.write().unwrap_or_else(PoisonError::into_inner);
        let began = Instant::now();
        let restarted = restart();
        self.meter
            .restarted(began.saturating_duration_since(asked), began.elapsed());
        match restarted {
            Ok(true) => {}
            Ok(false) => self.restart.store(true, Ordering::Relaxed),
            Err(e) => {
                tracing::warn!(target: "amphora.store", event = "restart_failed", error = %format!("{e:#}"), "the log couldn't be started again");
            }
        }
    }

    /// Runs `f` on a reader, on this thread: one idle, or one opened now, or the first given back
    /// before `until`. None when there are no readers now (the writer reads instead).
    fn with<T>(
        &self,
        until: Instant,
        f: impl FnOnce(&mut R) -> anyhow::Result<T>,
    ) -> Option<anyhow::Result<T>> {
        let (mut reader, generation) = {
            let mut pool = locked(&self.pool);
            loop {
                if let Some(r) = pool.idle.pop() {
                    pool.out = pool.out.saturating_add(1);
                    break (r, pool.generation);
                }
                let open = locked(&self.open).clone()?;
                if pool.out < self.most {
                    pool.out = pool.out.saturating_add(1);
                    let generation = pool.generation;
                    drop(pool);
                    match open() {
                        Ok(r) => break (r, generation),
                        Err(e) => {
                            self.give_back(None, generation);
                            return Some(Err(e));
                        }
                    }
                }
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Some(Err(Refused::Timeout.into()));
                }
                pool = self
                    .returned
                    .wait_timeout(pool, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        };
        let answer = {
            let _reading = self.gate.read().unwrap_or_else(PoisonError::into_inner);
            f(&mut reader)
        };
        self.give_back(Some(reader), generation);
        Some(answer)
    }

    fn give_back(&self, reader: Option<R>, generation: u64) {
        let mut pool = locked(&self.pool);
        pool.out = pool.out.saturating_sub(1);
        if let Some(r) = reader.filter(|_| pool.generation == generation) {
            pool.idle.push(r);
        }
        self.returned.notify_all();
    }
}

/// A handle to a store: cheap to clone, usable from any thread.
pub struct Store<E: Engine> {
    tx: mpsc::SyncSender<Msg<E>>,
    meter: Arc<Meter>,
    watchers: Arc<Mutex<Vec<Watcher>>>,
    sends: Arc<Sends>,
    readers: Arc<Readers<E::Reader>>,
}

impl<E: Engine> Clone for Store<E> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            meter: self.meter.clone(),
            watchers: self.watchers.clone(),
            sends: self.sends.clone(),
            readers: self.readers.clone(),
        }
    }
}

impl<E: Engine> Store<E> {
    /// The store, its writer thread started on `engine`.
    pub fn start(engine: E, config: Config) -> anyhow::Result<Self> {
        Self::start_with(engine, config, None)
    }

    /// [`Store::start`], its engine's flusher replaced by `flush` when given (tests).
    pub fn start_with(engine: E, config: Config, flush: Option<Flush>) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Msg<E>>(config.queue.max(1));
        let meter: Arc<Meter> = Arc::default();
        let watchers: Arc<Mutex<Vec<Watcher>>> = Arc::default();
        let sends: Arc<Sends> = Arc::default();
        let readers: Arc<Readers<E::Reader>> =
            Arc::new(Readers::new(config.readers, meter.clone()));
        if let Some(open) = engine.opener() {
            readers.start(
                open,
                E::checkpoint,
                config.checkpoint_every,
                config.log_frames,
                sends.clone(),
            );
        }
        match flush.or_else(|| engine.flusher()) {
            Some(flush) => {
                let flush = Arc::new(flush);
                for _ in 0..config.flushers.max(1) {
                    let (s, m, f) = (sends.clone(), meter.clone(), flush.clone());
                    std::thread::Builder::new()
                        .name("amphora-flush".into())
                        .spawn(move || flusher(&s, &f, &m))?;
                }
            }
            None => sends.durable_as_committed.store(true, Ordering::Relaxed),
        }
        let told = sends.clone();
        std::thread::Builder::new()
            .name("amphora-notify".into())
            .spawn(move || notifier(&told))?;
        let (m, w, s, r) = (
            meter.clone(),
            watchers.clone(),
            sends.clone(),
            readers.clone(),
        );
        std::thread::Builder::new()
            .name("amphora-writer".into())
            .spawn(move || writer(engine, &rx, &config, &m, &w, &s, &r))?;
        Ok(Self {
            tx,
            meter,
            watchers,
            sends,
            readers,
        })
    }

    /// What it has done.
    #[must_use]
    pub fn counters(&self) -> Counters {
        self.meter.counters()
    }

    /// Rows to be committed with the next batch. Never blocks: a full queue refuses them, counted.
    pub fn put(&self, rows: Vec<E::Row>) -> Result<Ticket, Refused> {
        self.hand(rows, None, None)
    }

    /// Rows to be committed with the next batch, waiting for room until `until`.
    pub fn put_wait(&self, rows: Vec<E::Row>, until: Instant) -> Result<Ticket, Refused> {
        self.hand(rows, Some(until), None)
    }

    /// [`Store::put_wait`], and `then` is told once the batch is durable, or why it won't be
    /// ([`Store::durable`]'s answer), on the store's notifier thread. Refused here, `then` is
    /// dropped untold: the refusal is the answer.
    pub fn put_then(
        &self,
        rows: Vec<E::Row>,
        until: Instant,
        then: impl FnOnce(Result<(), Refused>) + Send + 'static,
    ) -> Result<Ticket, Refused> {
        self.hand(rows, Some(until), Some(Box::new(then)))
    }

    fn hand(
        &self,
        rows: Vec<E::Row>,
        until: Option<Instant>,
        then: Option<Then>,
    ) -> Result<Ticket, Refused> {
        if self.sends.disk_failed.load(Ordering::SeqCst) {
            return Err(Refused::Disk);
        }
        if rows.is_empty() {
            if let Some(then) = then {
                then(Ok(()));
            }
            return Ok(Ticket(None));
        }
        let mut keys: Vec<String> = Vec::new();
        for r in &rows {
            let k = E::key(r);
            if !keys.iter().any(|have| have == k) {
                keys.push(k.to_owned());
            }
        }
        // Counted before it is queued: a read that begins once it is queued waits for it.
        self.sends.handing(&keys);
        let fate = Arc::new(Fate {
            state: AtomicU64::new(UNSETTLED),
            then: Mutex::new(then),
        });
        let mut m = Msg::Rows(rows, keys, fate.clone());
        loop {
            match self.tx.try_send(m) {
                Ok(()) => return Ok(Ticket(Some(fate))),
                Err(mpsc::TrySendError::Disconnected(back)) => {
                    self.let_go(&back);
                    return Err(Refused::Stopped);
                }
                Err(mpsc::TrySendError::Full(back)) => m = back,
            }
            let left = until.map_or(Duration::ZERO, |u| {
                u.saturating_duration_since(Instant::now())
            });
            if left.is_zero() {
                self.let_go(&m);
                self.meter.dropped();
                return Err(Refused::Full);
            }
            let room = locked(&self.sends.room.0);
            let _looked = self.sends.room.1.wait_timeout(room, left.min(ROOM_LOOK));
        }
    }

    /// A batch that will never be committed: nothing waits on it.
    fn let_go(&self, m: &Msg<E>) {
        if let Msg::Rows(_, keys, _) = m {
            self.sends.settled(keys);
            self.sends.wake(1, 0);
        }
    }

    /// Run `f` on a reader, on this thread, once the writes of `key` (of every key: None) handed
    /// over before are committed; or `on_writer` on the writer when the store has no reader now.
    /// Blocking, until `until` at most.
    pub fn read<T: Send + 'static>(
        &self,
        key: Option<&str>,
        until: Instant,
        f: impl FnOnce(&mut E::Reader) -> anyhow::Result<T>,
        on_writer: impl FnOnce(&mut E) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let asked = Instant::now();
        if !self.sends.wait(key, until) {
            self.meter.timed_out();
            return Err(Refused::Timeout.into());
        }
        match self.readers.with(until, f) {
            Some(answer) => {
                self.meter.waited(asked.elapsed());
                answer
            }
            None => self.run(until, on_writer),
        }
    }

    /// Run `f` on a reader, on this thread, on what is committed now, waiting for no write: what a
    /// reader that isn't the writer of what it reads sees in any database. Or `on_writer` on the
    /// writer when the store has no reader now (which runs once what was sent before is committed).
    /// Blocking, until `until` at most.
    pub fn read_committed<T: Send + 'static>(
        &self,
        until: Instant,
        f: impl FnOnce(&mut E::Reader) -> anyhow::Result<T>,
        on_writer: impl FnOnce(&mut E) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let asked = Instant::now();
        match self.readers.with(until, f) {
            Some(answer) => {
                self.meter.waited(asked.elapsed());
                answer
            }
            None => self.run(until, on_writer),
        }
    }

    /// Run `f` on the engine once what was sent before is committed. Blocking, until `until` at
    /// most.
    pub fn run<T: Send + 'static>(
        &self,
        until: Instant,
        f: impl FnOnce(&mut E) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let (rtx, rrx) = mpsc::channel();
        let asked = Instant::now();
        let mut m: Msg<E> = Msg::Run(Box::new(move |e| {
            let _ = rtx.send(f(e));
        }));
        loop {
            match self.tx.try_send(m) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Disconnected(_)) => return Err(Refused::Stopped.into()),
                Err(mpsc::TrySendError::Full(back)) => m = back,
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                self.meter.timed_out();
                return Err(Refused::Timeout.into());
            }
            let room = locked(&self.sends.room.0);
            let _looked = self.sends.room.1.wait_timeout(room, left.min(ROOM_LOOK));
        }
        match rrx.recv_timeout(until.saturating_duration_since(Instant::now())) {
            Ok(answer) => {
                self.meter.waited(asked.elapsed());
                answer
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.meter.timed_out();
                Err(Refused::Timeout.into())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Refused::Stopped.into()),
        }
    }

    /// Waits until the batch of `ticket` is durable: committed with whatever else the writer took
    /// meanwhile (it asks for no commit of its own), then flushed by a flush that began after its
    /// commit. Refused if its commit failed ([`Refused::Failed`]: nothing of it was written), if a
    /// flush failed first ([`Refused::Disk`]), or if the writer stopped before committing it. A
    /// batch never acknowledged durable may be found after a crash, whole, or not at all.
    /// Blocking, until `until` at most.
    pub fn durable(&self, ticket: &Ticket, until: Instant) -> Result<(), Refused> {
        let Some(fate) = &ticket.0 else {
            return Ok(());
        };
        let answer = self.sends.wait_ticket(fate, until);
        if answer == Err(Refused::Timeout) {
            self.meter.timed_out();
        }
        answer
    }

    /// Waits until everything handed over so far is settled and durable: committed (or failed:
    /// [`Store::durable`] tells a batch which), then flushed. Blocking, until `until` at most.
    pub fn sync(&self, until: Instant) -> Result<(), Refused> {
        let answer = self.sends.wait_durable(until);
        if answer == Err(Refused::Timeout) {
            self.meter.timed_out();
        }
        answer
    }

    /// `w` is told after every commit, until it says it doesn't want to be.
    pub fn on_commit(&self, w: Watcher) {
        locked(&self.watchers).push(w);
    }
}

fn notify(watchers: &Mutex<Vec<Watcher>>) {
    locked(watchers).retain(|w| w());
}

/// A batch taken off the queue and not yet committed: where its rows end among those pending, the
/// keys they are of, and what becomes of them.
struct Batch {
    end: usize,
    keys: Vec<String>,
    fate: Arc<Fate>,
}

/// The writer: rows are committed as soon as nothing else waits in the queue (or at the commit's
/// row bound), so what came while it committed shares the next commit, in as many commits as the
/// engine needs to hold them (decision 23); a job commits what waits first, so it sees everything
/// sent before it. A commit that fails is tried again batch by batch.
fn writer<E: Engine>(
    mut engine: E,
    rx: &mpsc::Receiver<Msg<E>>,
    config: &Config,
    meter: &Meter,
    watchers: &Mutex<Vec<Watcher>>,
    sends: &Arc<Sends>,
    readers: &Readers<E::Reader>,
) {
    let mut pending: Vec<E::Row> = Vec::new();
    let mut batches: Vec<Batch> = Vec::new();
    // Commits made, which number the batches' fates.
    let mut commits = 0u64;
    let mut idle = config.idle_every;
    let commit = |engine: &mut E,
                  pending: &mut Vec<E::Row>,
                  batches: &mut Vec<Batch>,
                  commits: &mut u64| {
        if batches.is_empty() {
            return;
        }
        // A commit made: counted, its watchers told; its number, for its batches' fates.
        let made = |commits: &mut u64, written: usize, began: Instant| {
            *commits = commits.saturating_add(1);
            meter.committed(written, began.elapsed());
            notify(watchers);
            *commits
        };
        // The batches in as few commits as the engine takes them in (decision 23): `from` the
        // first batch not committed yet, `start` its first row.
        let (mut from, mut start) = (0usize, 0usize);
        while let Some(rest) = batches.get(from..).filter(|rest| !rest.is_empty()) {
            let ends: Vec<usize> = rest.iter().map(|b| b.end.saturating_sub(start)).collect();
            let began = Instant::now();
            let failure = match engine
                .apply_batches(pending.get(start..).unwrap_or_default(), &ends)
            {
                Ok((n, written)) => {
                    let n = n.min(rest.len());
                    match n.checked_sub(1).and_then(|last| rest.get(last)) {
                        Some(last) => {
                            let commit = made(commits, written, began);
                            for b in rest.iter().take(n) {
                                b.fate.state.store(commit, Ordering::Release);
                            }
                            start = last.end;
                            from = from.saturating_add(n);
                            continue;
                        }
                        None => {
                            anyhow::anyhow!("the engine committed none of {} batches", rest.len())
                        }
                    }
                }
                Err(e) => e,
            };
            tracing::warn!(target: "amphora.store", event = "commit_failed", error = %format!("{failure:#}"), batches = rest.len(), "a commit failed; its batches are tried one by one");
            // Each batch alone: one that can't be committed takes no other with it.
            for b in rest {
                let rows = pending.get(start..b.end).unwrap_or_default();
                start = b.end;
                let began = Instant::now();
                match engine.apply(rows) {
                    Ok(written) => {
                        let commit = made(commits, written, began);
                        b.fate.state.store(commit, Ordering::Release);
                    }
                    Err(e) => {
                        b.fate.state.store(FAILED, Ordering::Release);
                        let failed = meter.failed();
                        if failed.is_power_of_two() {
                            tracing::warn!(target: "amphora.store", event = "batch_failed", error = %format!("{e:#}"), failed, "a batch couldn't be committed; its sender is told");
                        }
                    }
                }
            }
            break;
        }
        pending.clear();
        // Senders to tell once their batches are durable, or that they failed.
        let telling: Vec<(Arc<Fate>, Option<u64>)> = batches
            .iter()
            .filter(|b| locked(&b.fate.then).is_some())
            .map(|b| {
                let state = b.fate.state.load(Ordering::Acquire);
                (
                    b.fate.clone(),
                    if state == FAILED { None } else { Some(state) },
                )
            })
            .collect();
        if !telling.is_empty() {
            sends.to_tell(telling.into_iter());
        }
        // Committed or failed: either way no read waits on it any longer.
        let n = u64::try_from(batches.len()).unwrap_or(u64::MAX);
        for b in batches.drain(..) {
            sends.settled(&b.keys);
        }
        sends.wake(n, *commits);
    };
    loop {
        // Rows pending are committed once the queue has nothing more for them at once.
        let wait = if pending.is_empty() {
            idle
        } else {
            Duration::ZERO
        };
        let got = rx.recv_timeout(wait);
        let began = Instant::now();
        let quiet = matches!(got, Err(mpsc::RecvTimeoutError::Timeout)) && pending.is_empty();
        match got {
            Ok(Msg::Rows(rows, keys, fate)) => {
                sends.went_busy();
                sends.room.1.notify_all();
                pending.extend(rows);
                batches.push(Batch {
                    end: pending.len(),
                    keys,
                    fate,
                });
                if pending.len() >= config.commit_rows {
                    commit(&mut engine, &mut pending, &mut batches, &mut commits);
                    readers.restart_if_asked(|| engine.restart_log());
                }
            }
            Ok(Msg::Run(job)) => {
                sends.went_busy();
                sends.room.1.notify_all();
                commit(&mut engine, &mut pending, &mut batches, &mut commits);
                job(&mut engine);
                meter.ran();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                commit(&mut engine, &mut pending, &mut batches, &mut commits);
                readers.restart_if_asked(|| engine.restart_log());
                sends.went_idle();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                commit(&mut engine, &mut pending, &mut batches, &mut commits);
                readers.release();
                sends.stop();
                return;
            }
        }
        match engine.upkeep(quiet) {
            Ok(up) => {
                idle = up.again_in.unwrap_or(config.idle_every);
                match up.readers {
                    ForReaders::Keep => {}
                    ForReaders::Release => readers.release(),
                    ForReaders::Again => {
                        if let Some(open) = engine.opener() {
                            readers.start(
                                open,
                                E::checkpoint,
                                config.checkpoint_every,
                                config.log_frames,
                                sends.clone(),
                            );
                        }
                    }
                }
            }
            Err(e) => {
                tracing::error!(target: "amphora.store", event = "upkeep_failed", error = %format!("{e:#}"), "the engine's upkeep failed; the store stops");
                commit(&mut engine, &mut pending, &mut batches, &mut commits);
                readers.release();
                sends.stop();
                return;
            }
        }
        meter.held(began.elapsed());
    }
}
