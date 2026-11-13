//! Fresh `/dev` mounts made ahead (goal 5 extended step 3, runtime architecture decision 5).
//! Pure ownership and refill decisions: LRU keys, fresh mounts per key, each count learned from demand
//! ([`Shape`]; Ada's rule, 2026-11-06). A take moves a mount out forever; a cold/dry take uses ordinary
//! setup. One maker fills outside the take's lock, woken only when a key falls to its low mark, itself
//! learned from the takes that came while the key was filled before, and then filling the key in one pass
//! (goal 5 extended, step 6). An evicted or shutdown job is discarded, and a failed key stops asking until
//! another take.

use std::collections::VecDeque;

use crate::demand::{Depth, Learned};

/// How many configurations the pool keeps and how many mounts each, where each starts and what bounds it.
/// - `keys`: configurations kept at first, the least recently requested evicted past them. A cold take of a
///   configuration evicted earlier (a ghost, as ARC's: Megiddo and Modha, FAST 2003, §4.3) keeps one more from
///   then on.
/// - `per_key`: fresh mounts a configuration keeps at first. A take that finds its configuration's mounts all
///   taken raises its count to the draw plus one for each found short ([`Depth`]).
/// - `cap`: the most of either, and the most mounts held in all: no more creates take a mount at once than hold
///   a seed, so the node gives its seeds' bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub keys: usize,
    pub per_key: usize,
    pub cap: usize,
}

/// Counts saturate rather than wrap. Every completed successful make is held, taken or discarded.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub made: u64,
    pub taken: u64,
    pub on_path: u64,
    pub discarded: u64,
    pub failed: u64,
    /// Fixed miss reasons; their sum is `on_path`.
    pub cold: u64,
    pub dry: u64,
    pub disabled: u64,
    pub after_failure: u64,
    pub unpooled_missing: u64,
    pub unpooled_devices: u64,
    pub unpooled_duplicate: u64,
    pub unpooled_kind: u64,
    pub unpooled_readonly: u64,
    pub unpooled_inodes: u64,
    pub unpooled_key_bytes: u64,
    pub unpooled_capacity: u64,
    pub unpooled_user_namespace: u64,
    pub unpooled_pid_namespace: u64,
    pub unpooled_beneath: u64,
    pub unpooled_mount_label: u64,
    /// Fixed preparation stages; their sum is `failed`.
    pub failed_filesystem: u64,
    pub failed_device: u64,
    pub failed_link: u64,
    pub failed_namespace: u64,
    /// Takes after which the maker was to be woken: a key at or under its low mark, cold or dry among them.
    pub wakes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    #[error("the prepared /dev pool is shut down")]
    ShutDown,
    #[error("the prepared /dev pool exhausted its configuration generations")]
    Generations,
}

/// The failed preparation stage, not a user-controlled label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MakeFailure {
    Filesystem,
    Device,
    Link,
    /// A prepared proc's pid namespace, which the kernel wouldn't hand over (decision 6's update, part 2).
    Namespace,
}

/// Fixed reasons ordinary construction is required before a key can be requested. These are
/// optimization misses, never new specification refusals or user-controlled metric labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unpooled {
    Missing,
    Devices,
    Duplicate,
    Kind,
    Readonly,
    Inodes,
    KeyBytes,
    Capacity,
    /// A container with a user namespace of its own, made through the shim: the node's default nodes
    /// can't serve it, so it binds the host's in (goal 5 extended, step 3).
    UserNamespace,
    /// A prepared proc (decision 6's update, part 2): the container has no pid namespace of its own for
    /// one to be made ahead for.
    PidNamespace,
    /// A prepared proc: another mount at or beneath `/proc`, or a mask or read-only path of `/proc` itself
    /// or reached through `..`, which ordinary construction orders against the proc.
    Beneath,
    /// A container with a mount label (`linux.mountLabel`): the node made its pieces unlabelled, so the
    /// init makes its own, each with the label (goal 6, docs/design/runtime.md).
    MountLabel,
}

#[derive(Clone, Copy)]
enum Miss {
    Cold,
    Dry,
    Disabled,
    AfterFailure,
}

/// What a take hands the shell: the mount, if one was ready; the evicted key's mounts, to close outside the
/// lock; and whether to wake the maker.
#[derive(Debug, PartialEq, Eq)]
pub struct Took<T> {
    pub mount: Option<T>,
    pub discarded: Vec<T>,
    pub wake: bool,
}

/// One make reserved by the single worker; its generation cannot insert into a replacement key.
pub struct Job<K> {
    key: K,
    generation: u64,
}

impl<K> Job<K> {
    pub fn key(&self) -> &K {
        &self.key
    }
}

struct Entry<K, T> {
    key: K,
    generation: u64,
    asking: bool,
    ready: VecDeque<T>,
    /// How many it keeps ready, learned from its draws.
    depth: Depth,
    /// How many it falls to before the maker is woken to fill it back to its depth in one pass, rather than woken
    /// a take: learned, the most takes that came during one of its fill passes, from the take that woke the maker
    /// to its being full, so a pass as busy as any seen finds a mount for each of its takes in what the key held at
    /// its wake. It starts at 0, a wake when a take leaves the key empty, none seen yet, and never falls; at its
    /// depth less one every take wakes the maker, as before the low mark. What a wake at a low mark saves against a
    /// wake a take, measured with the fixed mark this replaces (half of eight): at concurrency 1 a quarter of the
    /// futex calls a mount, and 70 us on-CPU against 77 (bench/competitive/probes/devfs_clone.md, the low mark's
    /// A B B A).
    low: Learned,
    /// Being filled to its depth: from the take that left it at its low mark until it holds its depth.
    filling: bool,
    /// Takes since its fill pass began.
    during: usize,
}

impl<K, T> Entry<K, T> {
    /// The mark the key is filled from: its learned low mark, below its depth.
    fn low_mark(&self) -> usize {
        self.low.size().min(self.depth.size().saturating_sub(1))
    }

    /// Whether the maker should make one for it: it asks, is being filled, and is below its depth.
    fn wants(&self) -> bool {
        self.asking && self.filling && self.ready.len() < self.depth.size()
    }

    /// A take of it: counted in its fill pass when one is under way, or the start of one when the take leaves it
    /// at or under its low mark.
    fn took(&mut self) {
        if self.filling {
            self.during = self.during.saturating_add(1);
        } else if self.ready.len() <= self.low_mark() {
            self.filling = true;
            self.during = 0;
        }
    }

    /// It holds its depth again: the pass is over, and the takes that came during it teach its low mark.
    fn filled(&mut self) {
        self.depth.full();
        if self.filling {
            self.low.observe(self.during);
        }
        self.filling = false;
        self.during = 0;
    }
}

pub struct Pool<K, T> {
    shape: Shape,
    /// How many configurations it keeps, learned from its ghosts.
    keys: Learned,
    /// Configurations evicted, the oldest first, at most `shape.cap`.
    ghosts: VecDeque<K>,
    entries: VecDeque<Entry<K, T>>,
    next_generation: u64,
    working: bool,
    closed: bool,
    enabled: bool,
    counts: Counts,
}

impl<K: Clone + Eq, T> Pool<K, T> {
    pub fn new(enabled: bool, shape: Shape) -> Self {
        Self {
            shape,
            keys: Learned::new(shape.keys, shape.cap),
            ghosts: VecDeque::new(),
            entries: VecDeque::new(),
            next_generation: 0,
            working: false,
            closed: false,
            enabled,
            counts: Counts::default(),
        }
    }

    /// Never waits for preparation. A new key evicts the least recently requested past the keys kept; all
    /// its unused mounts are handed back to drop, bounded by its count. A failed key resumes after this
    /// request. The maker is to be woken only when there's work for it: a key at or under its low mark.
    pub fn take(&mut self, key: K) -> Result<Took<T>, Refused> {
        if self.closed {
            return Err(Refused::ShutDown);
        }
        let mut discarded = Vec::new();
        let mut miss = Miss::Disabled;
        let taken = if !self.enabled {
            None
        } else {
            let entry = self
                .entries
                .iter()
                .position(|e| e.key == key)
                .and_then(|i| self.entries.remove(i));
            let mut entry = match entry {
                Some(mut entry) => {
                    miss = if entry.asking {
                        Miss::Dry
                    } else {
                        Miss::AfterFailure
                    };
                    entry.depth.took(entry.asking && entry.ready.is_empty());
                    entry
                }
                None => {
                    miss = Miss::Cold;
                    let generation = self
                        .next_generation
                        .checked_add(1)
                        .ok_or(Refused::Generations)?;
                    self.next_generation = generation;
                    if let Some(at) = self.ghosts.iter().position(|g| *g == key) {
                        self.ghosts.remove(at);
                        self.keys.observe(self.keys.size().saturating_add(1));
                    }
                    // One entry is added a take, so at most one evicted.
                    if self.entries.len() >= self.keys.size().max(1)
                        && let Some(old) = self.entries.pop_front()
                    {
                        discarded.extend(old.ready);
                        if self.ghosts.len() >= self.shape.cap {
                            self.ghosts.pop_front();
                        }
                        if self.shape.cap > 0 {
                            self.ghosts.push_back(old.key);
                        }
                    }
                    Entry {
                        key,
                        generation,
                        asking: true,
                        ready: VecDeque::new(),
                        depth: Depth::new(self.shape.per_key, self.shape.cap),
                        low: Learned::new(0, self.shape.cap),
                        filling: false,
                        during: 0,
                    }
                }
            };
            entry.asking = true;
            let taken = entry.ready.pop_front();
            entry.took();
            self.entries.push_back(entry);
            taken
        };
        if taken.is_some() {
            self.counts.taken = self.counts.taken.saturating_add(1);
        } else {
            self.counts.on_path = self.counts.on_path.saturating_add(1);
            let counter = match miss {
                Miss::Cold => &mut self.counts.cold,
                Miss::Dry => &mut self.counts.dry,
                Miss::Disabled => &mut self.counts.disabled,
                Miss::AfterFailure => &mut self.counts.after_failure,
            };
            *counter = counter.saturating_add(1);
        }
        self.counts.discarded = self
            .counts
            .discarded
            .saturating_add(u64::try_from(discarded.len()).unwrap_or(u64::MAX));
        let wake = self.wants_work();
        if wake {
            self.counts.wakes = self.counts.wakes.saturating_add(1);
        }
        Ok(Took {
            mount: taken,
            discarded,
            wake,
        })
    }

    /// Count a specification or descriptor-capacity miss without retaining its configuration or
    /// waking the maker. The create continues through ordinary setup, with its original outcome.
    pub fn unpooled(&mut self, reason: Unpooled) -> Result<(), Refused> {
        if self.closed {
            return Err(Refused::ShutDown);
        }
        self.counts.on_path = self.counts.on_path.saturating_add(1);
        let count = match reason {
            Unpooled::Missing => &mut self.counts.unpooled_missing,
            Unpooled::Devices => &mut self.counts.unpooled_devices,
            Unpooled::Duplicate => &mut self.counts.unpooled_duplicate,
            Unpooled::Kind => &mut self.counts.unpooled_kind,
            Unpooled::Readonly => &mut self.counts.unpooled_readonly,
            Unpooled::Inodes => &mut self.counts.unpooled_inodes,
            Unpooled::KeyBytes => &mut self.counts.unpooled_key_bytes,
            Unpooled::Capacity => &mut self.counts.unpooled_capacity,
            Unpooled::UserNamespace => &mut self.counts.unpooled_user_namespace,
            Unpooled::PidNamespace => &mut self.counts.unpooled_pid_namespace,
            Unpooled::Beneath => &mut self.counts.unpooled_beneath,
            Unpooled::MountLabel => &mut self.counts.unpooled_mount_label,
        };
        *count = count.saturating_add(1);
        Ok(())
    }

    pub fn wants_work(&self) -> bool {
        !self.closed
            && !self.working
            && self.held() < self.shape.cap
            && self.entries.iter().any(Entry::wants)
    }

    /// The next make, for a key being filled from its low mark, which is filled to its depth before it asks again.
    pub fn next_job(&mut self) -> Option<Job<K>> {
        if !self.wants_work() {
            return None;
        }
        let e = self.entries.iter().find(|e| e.wants())?;
        let job = Job {
            key: e.key.clone(),
            generation: e.generation,
        };
        self.working = true;
        Some(job)
    }

    /// Completes one make; a stale/closed job returns its mount for the shell to close outside the
    /// lock. The worker's error is counted and suppresses retries for that key until the next take.
    pub fn complete(&mut self, job: Job<K>, made: Result<T, MakeFailure>) -> Option<T> {
        self.working = false;
        match made {
            Ok(mount) => {
                self.counts.made = self.counts.made.saturating_add(1);
                let room = self.held() < self.shape.cap;
                let e = self
                    .entries
                    .iter_mut()
                    .find(|e| e.generation == job.generation);
                if !self.closed
                    && room
                    && let Some(e) = e
                    && e.ready.len() < e.depth.size()
                {
                    e.ready.push_back(mount);
                    if e.ready.len() >= e.depth.size() {
                        e.filled();
                    }
                    None
                } else {
                    self.counts.discarded = self.counts.discarded.saturating_add(1);
                    Some(mount)
                }
            }
            Err(stage) => {
                self.counts.failed = self.counts.failed.saturating_add(1);
                let counter = match stage {
                    MakeFailure::Filesystem => &mut self.counts.failed_filesystem,
                    MakeFailure::Device => &mut self.counts.failed_device,
                    MakeFailure::Link => &mut self.counts.failed_link,
                    MakeFailure::Namespace => &mut self.counts.failed_namespace,
                };
                *counter = counter.saturating_add(1);
                if let Some(e) = self
                    .entries
                    .iter_mut()
                    .find(|e| e.generation == job.generation)
                {
                    // Its pass ends with the failure; the next take starts another from its low mark.
                    e.asking = false;
                    e.filling = false;
                    e.during = 0;
                }
                None
            }
        }
    }

    /// Stops making for good, where the kernel can make none (decision 6's update, part 2: a pid namespace
    /// made ahead can't be opened below Linux 7.1). What is held is handed back to drop, counted discarded;
    /// every later take is a disabled miss and asks for nothing.
    pub fn disable(&mut self) -> Vec<T> {
        self.enabled = false;
        let held: Vec<T> = self.entries.drain(..).flat_map(|e| e.ready).collect();
        self.counts.discarded = self
            .counts
            .discarded
            .saturating_add(u64::try_from(held.len()).unwrap_or(u64::MAX));
        held
    }

    pub fn shut_down(&mut self) -> Vec<T> {
        self.closed = true;
        let held: Vec<T> = self.entries.drain(..).flat_map(|e| e.ready).collect();
        self.counts.discarded = self
            .counts
            .discarded
            .saturating_add(u64::try_from(held.len()).unwrap_or(u64::MAX));
        held
    }

    /// What it has learned: the keys it keeps, and the most mounts any key it keeps keeps.
    pub fn learned(&self) -> (usize, usize) {
        let per_key = self
            .entries
            .iter()
            .map(|e| e.depth.size())
            .max()
            .unwrap_or(0);
        (self.keys.size(), per_key)
    }

    pub fn closed(&self) -> bool {
        self.closed
    }
    pub fn held(&self) -> usize {
        self.entries.iter().map(|e| e.ready.len()).sum()
    }
    pub fn counts(&self) -> Counts {
        self.counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the pool had before its counts were learned: four keys of eight, at most 32 held.
    const KEYS: usize = 4;
    const PER_KEY: usize = 8;
    const FIXED: Shape = Shape {
        keys: KEYS,
        per_key: PER_KEY,
        cap: KEYS * PER_KEY,
    };

    /// AC-T7.1.4, goal 5 step 3: fill four configurations, then churn and evict while a make is
    /// outstanding. Expect bounded resources, unique ownership and exact successful-make accounting.
    #[test]
    fn fresh_mounts_are_bounded_and_never_reused_under_churn() {
        // A cap past the run's 2,000 makes, so the learned keys and depths never meet it.
        let shape = Shape {
            cap: 4_096,
            ..FIXED
        };
        let mut p = Pool::<usize, usize>::new(true, shape);
        let mut made = 0;
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..2000 {
            if let Some(got) = p.take(i % 7).unwrap().mount {
                assert!(seen.insert(got));
            }
            let job = p.next_job();
            assert!(p.next_job().is_none(), "only one worker");
            if i % 3 == 0 {
                for key in 10..15 {
                    let _ = p.take(key).unwrap();
                }
            }
            if let Some(job) = job {
                made += 1;
                let _ = p.complete(job, Ok(made));
            }
            assert!(p.entries.len() <= shape.cap);
            assert!(p.held() <= shape.cap);
            let c = p.counts();
            assert_eq!(c.made, c.taken + c.discarded + p.held() as u64);
        }
        let _ = p.shut_down();
        assert_eq!(p.held(), 0);
        assert_eq!(p.counts().made, p.counts().taken + p.counts().discarded);
    }

    /// HEX-MNTNS-6: take two keys, make one, then disable the pool as a kernel that can make none would.
    /// Expect the held one handed back and counted discarded, no job asked for after, and each later take a
    /// disabled miss that retains no key.
    #[test]
    fn a_disabled_pool_hands_back_what_it_held_and_asks_for_nothing() {
        let mut p = Pool::<usize, usize>::new(true, FIXED);
        let _ = p.take(1).unwrap();
        let _ = p.take(2).unwrap();
        let j = p.next_job().unwrap();
        assert!(p.complete(j, Ok(7)).is_none());
        assert_eq!(p.disable(), vec![7]);
        assert!(!p.wants_work());
        assert!(p.next_job().is_none());
        assert!(p.take(1).unwrap().mount.is_none());
        assert!(!p.wants_work());
        let c = p.counts();
        assert_eq!((c.made, c.discarded, c.disabled, c.cold), (1, 1, 1, 2));
        assert_eq!(p.held(), 0);
    }

    /// AC-T7.1.4: make fails, key is evicted/recreated mid-make, then shutdown races completion.
    /// Expect no retry spin, no stale insertion and every unconsumed resource handed back.
    #[test]
    fn failure_eviction_and_shutdown_do_not_reuse_a_mount() {
        let mut p = Pool::<usize, usize>::new(true, FIXED);
        assert!(p.take(1).unwrap().mount.is_none());
        let j = p.next_job().unwrap();
        assert!(p.complete(j, Err(MakeFailure::Filesystem)).is_none());
        assert!(p.next_job().is_none());
        let _ = p.take(1).unwrap();
        let j = p.next_job().unwrap();
        for k in 2..7 {
            let _ = p.take(k).unwrap();
        }
        let _ = p.take(1).unwrap();
        assert_eq!(p.complete(j, Ok(99)), Some(99));
        let j = p.next_job().unwrap();
        assert!(p.shut_down().is_empty());
        assert_eq!(p.complete(j, Ok(100)), Some(100));
        assert_eq!(p.take(1), Err(Refused::ShutDown));
        assert!(p.next_job().is_none());
    }

    /// AC-T7.1.4: fill one configuration to capacity, then take two. Expect FIFO, eight held at
    /// most, and no refill request while the key is above its low mark (goal 5 extended, step 6).
    /// A disabled pool always uses ordinary setup.
    #[test]
    fn a_full_key_stops_refill_and_takes_are_fifo() {
        let mut p = Pool::new(true, FIXED);
        let _ = p.take("a").unwrap();
        for i in 0..PER_KEY {
            let j = p.next_job().unwrap();
            assert_eq!(j.key(), &"a");
            assert!(p.complete(j, Ok(i)).is_none());
        }
        assert!(p.next_job().is_none());
        assert_eq!(p.take("a").unwrap().mount, Some(0));
        assert_eq!(p.take("a").unwrap().mount, Some(1));
        assert!(!p.wants_work(), "six left, above the low mark");
        let mut off = Pool::<&str, usize>::new(false, FIXED);
        assert!(off.take("a").unwrap().mount.is_none());
        assert!(off.next_job().is_none());
    }
    /// AC-T7.1.4: fill all four configurations, touch the oldest, then add a fifth. Expect exactly
    /// the least recently requested key's eight resources evicted, and all counters conserved.
    #[test]
    fn full_pool_evicts_lru_and_counts_fixed_failure_and_miss_reasons() {
        let mut p = Pool::<usize, usize>::new(true, FIXED);
        for k in 0..KEYS {
            assert!(p.take(k).unwrap().mount.is_none());
            for i in 0..PER_KEY {
                let j = p.next_job().unwrap();
                assert_eq!(*j.key(), k);
                assert!(p.complete(j, Ok(k * 100 + i)).is_none());
            }
        }
        assert_eq!(p.held(), KEYS * PER_KEY);
        assert_eq!(p.take(0).unwrap().mount, Some(0));
        let new = p.take(99).unwrap();
        assert!(new.mount.is_none());
        assert_eq!(new.discarded, (100..100 + PER_KEY).collect::<Vec<_>>());
        while let Some(j) = p.next_job() {
            assert!(p.complete(j, Ok(999)).is_none());
        }
        // Key 0, one taken, is above its low mark: not refilled.
        assert_eq!(p.held(), KEYS * PER_KEY - 1);
        for stage in [
            MakeFailure::Filesystem,
            MakeFailure::Device,
            MakeFailure::Link,
        ] {
            let _ = p
                .take(1000 + usize::try_from(p.counts().failed).unwrap())
                .unwrap();
            let j = p.next_job().unwrap();
            assert!(p.complete(j, Err(stage)).is_none());
        }
        let key = p.entries.back().unwrap().key;
        assert!(p.take(key).unwrap().mount.is_none());
        assert!(p.take(key).unwrap().mount.is_none());
        let c = p.counts();
        assert_eq!(
            (c.failed_filesystem, c.failed_device, c.failed_link),
            (1, 1, 1)
        );
        assert_eq!(
            c.failed,
            c.failed_filesystem + c.failed_device + c.failed_link
        );
        assert_eq!((c.after_failure, c.dry), (1, 1));
        assert_eq!(c.on_path, c.cold + c.dry + c.disabled + c.after_failure);
        let mut off = Pool::<usize, usize>::new(false, FIXED);
        let _ = off.take(0).unwrap();
        assert_eq!(off.counts().disabled, 1);
    }

    /// Goal 5 extended step 6 (the lead's 2026-11-03T18:52Z: the pool's own wakes), its low mark learned (Ada's rule,
    /// 2026-11-06): a take wakes the maker only when its key falls to its low mark, and the maker then fills the key
    /// to its depth in one pass. Do: fill one key of eight; then, twice, take one at a time down to the low mark and
    /// let the maker run. Expect: no wake for a take that leaves the key above the mark, one for the take that
    /// reaches it, the maker's jobs then filling the key to eight with no other wake, and two wakes counted in all,
    /// where a wake a take would count sixteen.
    #[test]
    fn a_take_wakes_the_maker_only_at_the_low_mark_and_the_maker_fills_in_one_pass() {
        let mut p = Pool::new(true, FIXED);
        assert!(p.take("a").unwrap().wake, "a cold key wakes the maker");
        while let Some(j) = p.next_job() {
            assert!(p.complete(j, Ok(0)).is_none());
        }
        assert_eq!(p.held(), PER_KEY);
        let low = p.entries.back().unwrap().low_mark();
        let woke = p.counts().wakes;
        for round in 0..2 {
            for left in (low..PER_KEY).rev() {
                let took = p.take("a").unwrap();
                assert!(took.mount.is_some());
                assert_eq!(
                    took.wake,
                    left == low,
                    "round {round}: the take leaving {left} wakes the maker only at the low mark"
                );
            }
            let mut jobs = 0;
            while let Some(j) = p.next_job() {
                jobs += 1;
                assert!(p.complete(j, Ok(0)).is_none());
            }
            assert_eq!(
                (jobs, p.held()),
                (PER_KEY - low, PER_KEY),
                "round {round}: the maker fills the key in one pass"
            );
        }
        assert_eq!(p.counts().wakes - woke, 2);
    }

    /// Ada's rule (2026-11-06) for the low mark: it is the most takes that came during one of the key's fill passes,
    /// learned, never picked, and never falls. Do: a key of eight, filled; take it to empty, the first pass's wake;
    /// take three more while its maker fills it, the first found dry; fill it; take it down again; fill it with no
    /// take beside. Expect: the low mark 0 before any pass, then 3, the number of takes that came during the first
    /// pass; the next wake at the take leaving three; and the mark still 3 after a pass with none.
    #[test]
    fn the_low_mark_is_the_most_takes_seen_during_a_fill_pass() {
        let mut p = Pool::new(true, FIXED);
        let fill = |p: &mut Pool<&str, usize>| {
            while let Some(j) = p.next_job() {
                assert!(p.complete(j, Ok(0)).is_none());
            }
        };
        let _ = p.take("a").unwrap();
        fill(&mut p);
        let low = |p: &Pool<&str, usize>| p.entries.back().unwrap().low_mark();
        assert_eq!(low(&p), 0, "no pass seen yet");
        let woke: Vec<bool> = (0..PER_KEY).map(|_| p.take("a").unwrap().wake).collect();
        assert_eq!(woke.iter().filter(|w| **w).count(), 1);
        assert_eq!(woke.last(), Some(&true), "the take leaving none wakes it");
        // The pass: three takes come while the maker fills, the first finding the key empty.
        let mut takes = 0;
        while let Some(j) = p.next_job() {
            if takes < 3 {
                let _ = p.take("a").unwrap();
                takes += 1;
            }
            assert!(p.complete(j, Ok(0)).is_none());
        }
        let depth = p.entries.back().unwrap().depth.size();
        assert_eq!(p.held(), depth);
        assert_eq!(low(&p), 3, "the takes that came during the pass");
        let woke: Vec<bool> = (0..depth - 3).map(|_| p.take("a").unwrap().wake).collect();
        assert_eq!(
            woke.iter().position(|w| *w),
            Some(depth - 4),
            "the wake at the take leaving three"
        );
        fill(&mut p);
        assert_eq!(low(&p), 3, "a pass with no take beside it lowers nothing");
    }

    /// AC-T7.1.4: count every fixed ineligibility reason without keeping a key or making a mount.
    /// Expect one reason per on-path create, no refill, and a typed refusal after shutdown.
    #[test]
    fn unpooled_specs_are_counted_by_reason_without_allocating_pool_entries() {
        let mut p = Pool::<usize, usize>::new(true, FIXED);
        for reason in [
            Unpooled::Missing,
            Unpooled::Devices,
            Unpooled::Duplicate,
            Unpooled::Kind,
            Unpooled::Readonly,
            Unpooled::Inodes,
            Unpooled::KeyBytes,
            Unpooled::Capacity,
            Unpooled::UserNamespace,
            Unpooled::MountLabel,
        ] {
            p.unpooled(reason).unwrap();
        }
        let c = p.counts();
        assert_eq!(
            [
                c.unpooled_missing,
                c.unpooled_devices,
                c.unpooled_duplicate,
                c.unpooled_kind,
                c.unpooled_readonly,
                c.unpooled_inodes,
                c.unpooled_key_bytes,
                c.unpooled_capacity,
                c.unpooled_user_namespace,
                c.unpooled_mount_label
            ],
            [1; 10]
        );
        assert_eq!(
            c.on_path,
            c.cold
                + c.dry
                + c.disabled
                + c.after_failure
                + c.unpooled_missing
                + c.unpooled_devices
                + c.unpooled_duplicate
                + c.unpooled_kind
                + c.unpooled_readonly
                + c.unpooled_inodes
                + c.unpooled_key_bytes
                + c.unpooled_capacity
                + c.unpooled_user_namespace
                + c.unpooled_mount_label
        );
        assert!(p.entries.is_empty());
        assert_eq!(p.held(), 0);
        assert!(p.next_job().is_none());
        let _ = p.shut_down();
        assert_eq!(p.unpooled(Unpooled::Missing), Err(Refused::ShutDown));
        assert_eq!(p.counts(), c);
    }

    /// Ada's rule, 2026-11-06 (AC-T7.1.3): KEYS' 4 and PER_KEY's 8 were picked; both are learned. Do: a pool of
    /// one key and two mounts at first, at most 16: take key a three times with no make between, refill it, take
    /// it three times again; then request b, a again (a evicted) and b. Expect: the first draw finds a short and
    /// raises its count (no short take in the second); a's return after its eviction keeps two keys from then on,
    /// so b stays; and nothing held passes the cap.
    #[test]
    fn keys_and_mounts_a_key_are_learned_from_their_misses() {
        let mut p = Pool::<&str, usize>::new(
            true,
            Shape {
                keys: 1,
                per_key: 2,
                cap: 16,
            },
        );
        let mut n = 0;
        let mut fill = |p: &mut Pool<&str, usize>| {
            while let Some(j) = p.next_job() {
                n += 1;
                assert!(p.complete(j, Ok(n)).is_none());
            }
        };
        let _ = p.take("a").unwrap();
        fill(&mut p);
        let short = (0..3)
            .filter(|_| p.take("a").unwrap().mount.is_none())
            .count();
        assert_eq!(short, 1);
        fill(&mut p);
        let short = (0..3)
            .filter(|_| p.take("a").unwrap().mount.is_none())
            .count();
        assert_eq!(short, 0, "the draw it ran short at is held");
        let evicted = p.take("b").unwrap().discarded;
        assert!(!evicted.is_empty(), "one key kept: a evicted for b");
        let _ = p.take("a").unwrap();
        let evicted = p.take("b").unwrap().discarded;
        assert!(evicted.is_empty(), "a's return kept a second key");
        assert_eq!(p.entries.len(), 2);
        fill(&mut p);
        assert!(p.held() <= 16);
    }
}
