//! Turns: who goes next, with no lock (goal 5 of the API runtime program, plan 01a1a59c2aae, its top goal: no
//! request, websocket or runner path takes a lock shared between shards).
//!
//! What waits for its turn here waited on async-lock's `Mutex`, `RwLock`, `Semaphore` and `OnceCell`. Their
//! waits and wakes are event-listener's, whose list of listeners sits behind a std `Mutex` that every wait,
//! every wait given up and every wake finding a waiter takes: goal 5's census of train 8's tree on fedora found
//! it the one lock the shards still shared, 1.8 ms a second waited with hogs and 2.4 ms at the longest
//! (docs/runtime-lab.md). They held password sign-ins, learners' copies of kiln.db, pushes to native
//! repositories, CI's uploads and steps in flight, the changes to goals, plans, programs and research, mirrors'
//! fetches and the looks at them, and more.
//!
//! - [`Gate`]: `n` at a time (one, as a mutex), each in the order it came.
//! - [`Baton`]: a value one holder has at a time, in turn.
//! - [`ReadWrite`]: readers together, a writer alone, each in the order it came.
//! - [`Keyed`]: a gate, or a [`ReadWrite`], per key while anyone holds or waits for it.
//! - [`Once`]: a value made once, by the first to ask; the others wait their turn and find it made.
//!
//! A gate's free turns are a count, and who waits stands in a line of places (concurrent-queue's unbounded
//! queue, as `crate::memory::queue`'s items are). A turn given back goes to the first place still waiting, else
//! to the count, and one who comes takes from the count only while no one stands in line: none waiting is passed
//! over by one who came later. async-lock's mutex let a newcomer take a lock its oldest waiter had been woken
//! for until a waiter had starved half a millisecond, and its semaphore any newcomer; its RwLock kept readers
//! behind a writer waiting, as [`ReadWrite`] does. A waiter stands in line and then looks at the count again,
//! and whoever adds to the count looks at the line again, each after a fence (a Dekker handshake, as
//! `crate::memory::queue`'s waiters): one of the two sees the other, so no turn stays in the count while a place
//! waits. A wait given up leaves its place, which the next turn given passes by: a place goes from waiting to
//! given, or from waiting to left, each in one atomic exchange, so of a wait given up just as its turn comes one
//! wins; left, the turn goes on to the next place, and given, the wait going gives it on itself.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use concurrent_queue::ConcurrentQueue;
use futures::task::AtomicWaker;

use crate::rcu::Rcu;

// ------------------------------------------------------------------------------------------- gate

/// Where a waiter stands in a gate's line, and what wakes it.
struct Place {
    stands: AtomicU8,
    waker: AtomicWaker,
}

/// Format: a place's waiter waits for its turn.
const WAITING: u8 = 0;
/// Format: a turn was handed to the place's waiter, whose it is now.
const GIVEN: u8 = 1;
/// Format: the place's waiter gave up before its turn came; the next turn given passes it by.
const LEFT: u8 = 2;

/// `n` at a time (one, as a mutex), each in the order it came: a turn given back goes to whoever has waited
/// longest, and one who comes while others wait waits behind them. With no lock (the module's doc).
pub struct Gate {
    /// Turns no one holds, nor is being handed.
    free: AtomicUsize,
    /// Who waits, in the order they came (and places left, until a turn given passes them by).
    line: ConcurrentQueue<Arc<Place>>,
}

impl Gate {
    /// A gate of `at_once` turns: as many held at once at most.
    pub const fn new(at_once: usize) -> Self {
        Self { free: AtomicUsize::new(at_once), line: ConcurrentQueue::unbounded() }
    }

    /// One turn: one holder at a time.
    pub const fn one() -> Self {
        Self::new(1)
    }

    /// A turn now, if one is free and no one waits for one.
    pub fn try_turn(&self) -> Option<Turn<'_>> {
        // Made only once the turn is taken (`then`, not `then_some`): a turn made and dropped gives one back.
        (self.line.is_empty() && self.take_free()).then(|| Turn { gate: self })
    }

    /// A turn: at once where one is free and no one waits, else once those who came before have had theirs; a
    /// charge of the task's turn on its shard first (`hyper_rt::coop`, as `crate::rt::Notify`'s waits), so a loop
    /// whose turns are always free yields at its share. Given back as it's dropped. A wait dropped before its turn
    /// comes (a select's arm another won, a timeout) leaves the line, and a turn handed to it as it went goes on.
    pub async fn turn(&self) -> Turn<'_> {
        hyper_rt::coop::consume_budget().await;
        if let Some(turn) = self.try_turn() {
            return turn;
        }
        let mut waiting = Waiting { gate: self, place: Arc::new(Place { stands: AtomicU8::new(WAITING), waker: AtomicWaker::new() }), holds: false };
        // Unbounded and never closed: the push always goes.
        let _ = self.line.push(Arc::clone(&waiting.place));
        fence(Ordering::SeqCst);
        // A turn given back between the look above and the place in line went to the count, the line empty then
        // (`give`): it's this one's, and its place leaves the line (a turn handed to it meanwhile goes on).
        if self.take_free() {
            drop(waiting);
            return Turn { gate: self };
        }
        std::future::poll_fn(|cx| waiting.poll_given(cx)).await;
        Turn { gate: self }
    }

    /// One turn off the count, if it has one.
    fn take_free(&self) -> bool {
        self.free.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)).is_ok()
    }

    /// A turn given back: to the first place still waiting, else to the count, and the line looked at again after
    /// (a waiter that stood in line meanwhile may have looked at the count before it rose): the turn is taken back
    /// for it, unless another took it first.
    fn give(&self) {
        loop {
            while let Ok(place) = self.line.pop() {
                if place.stands.compare_exchange(WAITING, GIVEN, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    place.waker.wake();
                    return;
                }
            }
            self.free.fetch_add(1, Ordering::AcqRel);
            fence(Ordering::SeqCst);
            if self.line.is_empty() || !self.take_free() {
                return;
            }
        }
    }
}

impl Default for Gate {
    /// One at a time.
    fn default() -> Self {
        Self::one()
    }
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate").field("free", &self.free.load(Ordering::Relaxed)).field("line", &self.line.len()).finish()
    }
}

/// A wait for a gate's turn, its place in line: dropped before its turn comes, it leaves the line, and a turn
/// handed to it as it went goes on to the next.
struct Waiting<'a> {
    gate: &'a Gate,
    place: Arc<Place>,
    /// Its turn came: whoever holds the turn gives it back now.
    holds: bool,
}

impl Waiting<'_> {
    /// Ready once a turn is handed to it; the waker it's polled with is woken when one is.
    fn poll_given(&mut self, cx: &Context<'_>) -> Poll<()> {
        if !self.given() {
            self.place.waker.register(cx.waker());
            if !self.given() {
                return Poll::Pending;
            }
        }
        Poll::Ready(())
    }

    fn given(&mut self) -> bool {
        self.holds = self.place.stands.load(Ordering::Acquire) == GIVEN;
        self.holds
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if !self.holds && self.place.stands.compare_exchange(WAITING, LEFT, Ordering::AcqRel, Ordering::Acquire).is_err() {
            // Handed a turn as it went: the turn goes on.
            self.gate.give();
        }
    }
}

/// A gate's turn, held: given back as it's dropped, to whoever has waited longest.
#[must_use = "a turn is given back as soon as it's dropped"]
pub struct Turn<'a> {
    gate: &'a Gate,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.gate.give();
    }
}

impl std::fmt::Debug for Turn<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Turn")
    }
}

// ------------------------------------------------------------------------------------------ baton

/// A value one holder has at a time, in turn ([`Gate::one`]'s order): what an async-lock `Mutex` of a value was.
/// The holder has it by value, taken out of its slot (concurrent-queue's of one place) as its turn comes and put
/// back as its turn goes, so only a turn's holder ever reaches it, and nothing borrows it in place.
pub struct Baton<T: Default> {
    gate: Gate,
    slot: ConcurrentQueue<T>,
}

impl<T: Default> Baton<T> {
    pub fn new(value: T) -> Self {
        let slot = ConcurrentQueue::bounded(1);
        // One place, empty: the push goes.
        let _ = slot.push(value);
        Self { gate: Gate::one(), slot }
    }

    /// The value, once those who came before have let it go ([`Gate::turn`]): back in its slot for the next as
    /// this is dropped.
    pub async fn turn(&self) -> Held<'_, T> {
        let turn = self.gate.turn().await;
        // Only a turn's holder takes it out and puts it back, so it's there.
        let value = self.slot.pop().unwrap_or_default();
        Held { value, slot: &self.slot, _turn: turn }
    }
}

impl<T: Default> Default for Baton<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// A [`Baton`]'s value and its turn: the value back in its slot as this is dropped, before the turn goes on.
#[must_use = "the value goes back, and the turn on, as soon as it's dropped"]
pub struct Held<'a, T: Default> {
    value: T,
    slot: &'a ConcurrentQueue<T>,
    _turn: Turn<'a>,
}

impl<T: Default> Deref for Held<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T: Default> DerefMut for Held<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T: Default> Drop for Held<'_, T> {
    fn drop(&mut self) {
        // Before the turn goes on (a struct's fields drop after its `drop`): the next holder finds it there. The
        // slot is empty while a turn is held, so the push goes.
        let _ = self.slot.push(std::mem::take(&mut self.value));
    }
}

// ------------------------------------------------------------------------------------- read, write

/// Readers together, a writer alone, each in the order it came: what an async-lock `RwLock` was, whose writers
/// are never starved (a writer waiting keeps out the readers who come after it). A reader and a writer each take
/// a turn of one gate: a reader gives it back once it has counted itself in, so readers in line go in one after
/// another and are in together; a writer keeps it, so no one comes in behind it, and waits for the readers already
/// in to go.
pub struct ReadWrite {
    gate: Gate,
    readers: AtomicUsize,
    /// Told as the last reader in goes.
    out: crate::rt::Changed,
}

impl ReadWrite {
    pub fn new() -> Self {
        Self { gate: Gate::one(), readers: AtomicUsize::new(0), out: crate::rt::Changed::new() }
    }

    /// In as a reader, beside the others: once no writer who came before holds it or waits for it.
    pub async fn read(&self) -> Reading<'_> {
        let turn = self.gate.turn().await;
        self.readers.fetch_add(1, Ordering::SeqCst);
        drop(turn);
        Reading { rw: self }
    }

    /// In as the one writer: once those who came before (readers and writers) have gone.
    pub async fn write(&self) -> Writing<'_> {
        let turn = self.gate.turn().await;
        loop {
            // Listening before the look: the last reader's going after the look is a notify after the listen.
            let out = self.out.listen();
            if self.readers.load(Ordering::SeqCst) == 0 {
                break;
            }
            out.await;
        }
        Writing { _turn: turn }
    }
}

impl Default for ReadWrite {
    fn default() -> Self {
        Self::new()
    }
}

/// A reader in a [`ReadWrite`]: out as it's dropped.
#[must_use = "a reader goes as soon as it's dropped"]
pub struct Reading<'a> {
    rw: &'a ReadWrite,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        if self.rw.readers.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.rw.out.notify();
        }
    }
}

/// The writer in a [`ReadWrite`]: out as it's dropped, the gate's turn going on to whoever came next.
#[must_use = "a writer goes as soon as it's dropped"]
pub struct Writing<'a> {
    _turn: Turn<'a>,
}

// ------------------------------------------------------------------------------------------- keyed

/// One `V` (a [`Gate`], a [`ReadWrite`]) per key while anyone holds or waits for it: a repository's, a run's, a
/// folder's at a time. The map of them is a copy swapped in whole (`crate::rcu`), not a lock as it was: an entry's
/// first user puts it in, and its last takes it out, so the map holds the keys in use, not every key ever used.
/// Each entry counts its users ([`Entry`]: a holder, a waiter, a handle); the last to go marks it gone, unless
/// another came meanwhile, and whoever finds an entry gone puts a new one in its place, so two users of one key
/// never use two entries at once.
pub struct Keyed<K, V> {
    map: Rcu<HashMap<K, Arc<Users<V>>>>,
}

/// A keyed entry's value and its users.
struct Users<V> {
    count: AtomicUsize,
    value: V,
}

/// Format: an entry's count once its last user went: no one joins it after.
const GONE: usize = usize::MAX;

impl<V> Users<V> {
    /// One more user, unless it's gone.
    fn join(&self) -> bool {
        self.count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1).filter(|m| *m != GONE)).is_ok()
    }
}

impl<K: Clone + Eq + Hash, V: Default> Keyed<K, V> {
    pub fn new() -> Self {
        Self { map: Rcu::new(HashMap::new()) }
    }

    /// `key`'s entry, put in if it has none: its `V` for as long as this is held.
    pub fn of(&self, key: &K) -> Entry<'_, K, V> {
        loop {
            let found = self.map.load().get(key).cloned();
            match found {
                Some(users) if users.join() => return Entry { keyed: self, key: key.clone(), users },
                // Gone as its last user went: out of the map (if that user hasn't taken it out yet), then afresh.
                Some(gone) => self.forget(key, &gone),
                None => {
                    let fresh = Arc::new(Users { count: AtomicUsize::new(1), value: V::default() });
                    let put = self.map.update(|m| {
                        if m.contains_key(key) {
                            return false;
                        }
                        m.insert(key.clone(), Arc::clone(&fresh));
                        true
                    });
                    if put {
                        return Entry { keyed: self, key: key.clone(), users: fresh };
                    }
                }
            }
        }
    }
}

impl<K: Clone + Eq + Hash, V> Keyed<K, V> {
    /// Takes `users` out of the map, if it's still `key`'s entry there.
    fn forget(&self, key: &K, users: &Arc<Users<V>>) {
        self.map.update(|m| {
            if m.get(key).is_some_and(|u| Arc::ptr_eq(u, users)) {
                m.remove(key);
            }
        });
    }

    /// The keys with an entry now.
    pub fn len(&self) -> usize {
        self.map.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<K: Clone + Eq + Hash, V: Default> Default for Keyed<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

/// A key's entry in a [`Keyed`], held: its `V` (`Deref`), its users one fewer as it's dropped.
pub struct Entry<'a, K: Clone + Eq + Hash, V> {
    keyed: &'a Keyed<K, V>,
    key: K,
    users: Arc<Users<V>>,
}

impl<K: Clone + Eq + Hash, V> Deref for Entry<'_, K, V> {
    type Target = V;

    fn deref(&self) -> &V {
        &self.users.value
    }
}

impl<K: Clone + Eq + Hash, V> Drop for Entry<'_, K, V> {
    fn drop(&mut self) {
        // The last user out marks it gone, unless another came meanwhile, and takes it out of the map.
        if self.users.count.fetch_sub(1, Ordering::AcqRel) == 1 && self.users.count.compare_exchange(0, GONE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.keyed.forget(&self.key, &self.users);
        }
    }
}

// -------------------------------------------------------------------------------------------- once

/// A value made once, by the first to ask: the others wait their turn and find it made, as async-lock's
/// `OnceCell` had them. A making dropped before it ends (its task cancelled) leaves it unmade, and the next to ask
/// makes it.
pub struct Once<T> {
    value: OnceLock<T>,
    making: Gate,
}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Self { value: OnceLock::new(), making: Gate::one() }
    }

    /// The value, if it's made.
    pub fn get(&self) -> Option<&T> {
        self.value.get()
    }

    /// The value: made now by `make` where no one has made it, one making at a time.
    pub async fn get_or_init<F: Future<Output = T>>(&self, make: impl FnOnce() -> F) -> &T {
        if let Some(v) = self.value.get() {
            return v;
        }
        let _turn = self.making.turn().await;
        if let Some(v) = self.value.get() {
            return v;
        }
        let made = make().await;
        self.value.get_or_init(|| made)
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rt::{block_on, sleep, spawn_spread_joined, timeout};
    use std::pin::Pin;
    use std::time::{Duration, Instant};

    /// What a source file's code says outside its tests, its comments left out (CRLF read as LF).
    fn code_of(src: &str) -> String {
        let src = src.replace("\r\n", "\n");
        let code = src.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default();
        code.lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n")
    }

    /// Gives the shard to its other tasks once, whatever its share: a holder's turn spans other tasks' polls.
    async fn yield_once() {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                return Poll::Ready(());
            }
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await;
    }

    /// Waits until `holds` holds, up to 5 s: `what` says what didn't come.
    async fn until(what: &str, holds: impl Fn() -> bool) -> anyhow::Result<()> {
        let until = Instant::now() + Duration::from_secs(5);
        while !holds() {
            anyhow::ensure!(Instant::now() < until, "{what} didn't come in 5 s");
            sleep(Duration::from_millis(1)).await;
        }
        Ok(())
    }

    /// Polls the wait `f` until `holds` (where it stands: its place in line, the turn it took), pending all the
    /// while: its first poll is a charge of the task's turn, which may yield before it stands anywhere.
    async fn stand<F: Future + ?Sized>(mut f: Pin<&mut F>, what: &str, holds: impl Fn() -> bool) -> anyhow::Result<()> {
        for _ in 0..10_000 {
            anyhow::ensure!(futures::poll!(f.as_mut()).is_pending(), "{what}: it went in");
            if holds() {
                return Ok(());
            }
            yield_once().await;
        }
        anyhow::bail!("{what}: it never stood")
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal), run by kiln's CI with every test: turns take no lock and wait on
    /// nothing that does (event-listener's list sits behind a `Mutex`, and async-lock's and async-channel's waits
    /// are event-listener's).
    #[test]
    fn g5_turns_take_no_lock() {
        let code = code_of(include_str!("turn.rs"));
        assert!(code.contains("pub struct Gate") && code.contains("pub struct Keyed"), "not the turns' code:\n{code}");
        for lock in ["Mutex", "RwLock", "Condvar", "parking_lot", "async_lock", "event_listener", "Event::new", "async_channel"] {
            assert!(!code.contains(lock), "crate::turn takes a lock, or waits on something that does: {lock}");
        }
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal), run by kiln's CI with every test: what waited on async-lock, or on
    /// an event-listener `Event` of kiln's own, on a request's, a websocket's or a runner's path or beside them on
    /// the shards, waits on `crate::turn` or `crate::rt::Changed` now: none of these files names either crate
    /// outside its tests. (`scripts/locks.py check` holds all of kiln's code to it: `waits` in
    /// scripts/locks/hot.toml.)
    #[test]
    fn g5_what_waited_on_async_lock_or_an_event_waits_on_turns_now() {
        let files = [
            ("api.rs", include_str!("api.rs")),
            ("blobs/local.rs", include_str!("blobs/local.rs")),
            ("ci/hosting.rs", include_str!("ci/hosting.rs")),
            ("ci/oidc.rs", include_str!("ci/oidc.rs")),
            ("ci/search.rs", include_str!("ci/search.rs")),
            ("ci/services/cas.rs", include_str!("ci/services/cas.rs")),
            ("ci/services/memo.rs", include_str!("ci/services/memo.rs")),
            ("ci/services/pipelines.rs", include_str!("ci/services/pipelines.rs")),
            ("convo/store.rs", include_str!("convo/store.rs")),
            ("deploy/remote.rs", include_str!("deploy/remote.rs")),
            ("gitops.rs", include_str!("gitops.rs")),
            ("goals/mod.rs", include_str!("goals/mod.rs")),
            ("goals/sim.rs", include_str!("goals/sim.rs")),
            ("goals/subagents.rs", include_str!("goals/subagents.rs")),
            ("harness/host.rs", include_str!("harness/host.rs")),
            ("headroom/mod.rs", include_str!("headroom/mod.rs")),
            ("observe/catalog/mod.rs", include_str!("observe/catalog/mod.rs")),
            ("observe/dora/mod.rs", include_str!("observe/dora/mod.rs")),
            ("plans/agents.rs", include_str!("plans/agents.rs")),
            ("plans/review.rs", include_str!("plans/review.rs")),
            ("programs/mod.rs", include_str!("programs/mod.rs")),
            ("programs/ops.rs", include_str!("programs/ops.rs")),
            ("records.rs", include_str!("records.rs")),
            ("records/turso.rs", include_str!("records/turso.rs")),
            ("research/mod.rs", include_str!("research/mod.rs")),
            ("runner/docker.rs", include_str!("runner/docker.rs")),
            ("runner/runtime.rs", include_str!("runner/runtime.rs")),
            ("runner/work/service.rs", include_str!("runner/work/service.rs")),
            ("sync/tasks.rs", include_str!("sync/tasks.rs")),
        ];
        for (file, src) in files {
            let code = code_of(src);
            for named in ["async_lock", "event_listener"] {
                assert!(!code.contains(named), "{file} still names {named}");
            }
        }
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a gate of one lets one in at a time, whatever shard each comes
    /// from, each holding its turn across other tasks' polls; every turn comes, and the gate ends as it began.
    #[test]
    fn g5_a_gate_lets_one_in_at_a_time_from_every_shard() -> anyhow::Result<()> {
        block_on(async {
            let gate = Arc::new(Gate::one());
            let (inside, most, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let (tasks, rounds) = (16usize, 200usize);
            let mut all = Vec::new();
            for _ in 0..tasks {
                let (gate, inside, most, done) = (Arc::clone(&gate), Arc::clone(&inside), Arc::clone(&most), Arc::clone(&done));
                all.push(spawn_spread_joined(async move {
                    for _ in 0..rounds {
                        let _turn = gate.turn().await;
                        most.fetch_max(inside.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                        yield_once().await;
                        inside.fetch_sub(1, Ordering::SeqCst);
                        done.fetch_add(1, Ordering::SeqCst);
                    }
                }));
            }
            for t in all {
                timeout(Duration::from_secs(60), t).await?.map_err(|_| anyhow::anyhow!("a task ended without its turns"))?;
            }
            anyhow::ensure!(done.load(Ordering::SeqCst) == tasks * rounds, "{} of {} turns", done.load(Ordering::SeqCst), tasks * rounds);
            anyhow::ensure!(most.load(Ordering::SeqCst) == 1, "{} held a gate of one at once", most.load(Ordering::SeqCst));
            anyhow::ensure!(gate.free.load(Ordering::SeqCst) == 1 && gate.line.is_empty(), "{gate:?} after every turn");
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a gate of three lets three in at once and no more: a fourth waits
    /// until one goes, and a newcomer can't take the turn handed to it; from every shard, never more than three in.
    #[test]
    fn g5_a_gate_of_n_holds_n_at_once_and_no_more() -> anyhow::Result<()> {
        block_on(async {
            let gate = Arc::new(Gate::new(3));
            let [a, b, c] = [gate.turn().await, gate.turn().await, gate.turn().await];
            anyhow::ensure!(gate.try_turn().is_none(), "a fourth turn with three held");
            let mut fourth = std::pin::pin!(gate.turn());
            stand(fourth.as_mut(), "a fourth turn with three held", || gate.line.len() == 1).await?;
            drop(a);
            anyhow::ensure!(gate.try_turn().is_none(), "a newcomer took the turn handed to the one waiting");
            let fourth = timeout(Duration::from_secs(1), fourth).await?;
            drop((b, c, fourth));
            anyhow::ensure!(gate.free.load(Ordering::SeqCst) == 3, "{gate:?}");
            let (inside, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let mut all = Vec::new();
            for _ in 0..12 {
                let (gate, inside, most) = (Arc::clone(&gate), Arc::clone(&inside), Arc::clone(&most));
                all.push(spawn_spread_joined(async move {
                    for _ in 0..100 {
                        let _turn = gate.turn().await;
                        most.fetch_max(inside.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                        yield_once().await;
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                }));
            }
            for t in all {
                timeout(Duration::from_secs(60), t).await?.map_err(|_| anyhow::anyhow!("a task ended without its turns"))?;
            }
            anyhow::ensure!((1..=3).contains(&most.load(Ordering::SeqCst)), "{} inside a gate of three at once", most.load(Ordering::SeqCst));
            anyhow::ensure!(gate.free.load(Ordering::SeqCst) == 3, "{gate:?} after every turn");
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): turns go in the order they were asked for, from whatever shard,
    /// and the turn given back goes to the first waiting, not to a newcomer.
    #[test]
    fn g5_turns_go_in_the_order_they_were_asked_for() -> anyhow::Result<()> {
        block_on(async {
            let gate = Arc::new(Gate::one());
            let held = gate.turn().await;
            let order: Arc<ConcurrentQueue<usize>> = Arc::new(ConcurrentQueue::unbounded());
            let waiters = 32usize;
            let mut waiting = Vec::new();
            for i in 0..waiters {
                let (g, order) = (Arc::clone(&gate), Arc::clone(&order));
                waiting.push(spawn_spread_joined(async move {
                    let _turn = g.turn().await;
                    let _ = order.push(i);
                }));
                // Each stands in line before the next asks.
                until(&format!("waiter {i}'s place in line"), || gate.line.len() > i).await?;
            }
            drop(held);
            anyhow::ensure!(gate.try_turn().is_none(), "a newcomer went past those waiting");
            for w in waiting {
                timeout(Duration::from_secs(5), w).await?.map_err(|_| anyhow::anyhow!("a waiter ended without its turn"))?;
            }
            let went: Vec<usize> = order.try_iter().collect();
            anyhow::ensure!(went == (0..waiters).collect::<Vec<_>>(), "turns went {went:?}");
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a wait given up leaves the line and the next turn passes it by;
    /// one handed its turn as it's dropped gives the turn on; and waits given up on every shard at every moment lose
    /// no turn and make none: the gate ends with its one turn free.
    #[test]
    fn g5_a_wait_given_up_leaves_and_a_turn_handed_to_it_goes_on() -> anyhow::Result<()> {
        block_on(async {
            let gate = Arc::new(Gate::one());
            let held = gate.turn().await;
            anyhow::ensure!(timeout(Duration::from_millis(20), gate.turn()).await.is_err(), "a turn came with the gate held");
            let g = Arc::clone(&gate);
            let next = spawn_spread_joined(async move {
                let _turn = g.turn().await;
            });
            until("the next waiter's place, behind the one that left", || gate.line.len() >= 2).await?;
            drop(held);
            timeout(Duration::from_secs(5), next).await?.map_err(|_| anyhow::anyhow!("the waiter behind one that left had no turn"))?;
            anyhow::ensure!(gate.free.load(Ordering::SeqCst) == 1 && gate.line.is_empty(), "{gate:?}");
            // Handed its turn, then dropped before it looks: the turn goes back.
            let held = gate.turn().await;
            {
                let mut wait = std::pin::pin!(gate.turn());
                stand(wait.as_mut(), "a turn with the gate held", || gate.line.len() == 1).await?;
                drop(held);
                anyhow::ensure!(gate.free.load(Ordering::SeqCst) == 0, "the turn wasn't handed to the one waiting: {gate:?}");
            }
            anyhow::ensure!(gate.try_turn().is_some(), "the turn handed to a wait dropped was lost: {gate:?}");
            // Given up at every moment, from every shard.
            let mut all = Vec::new();
            for t in 0..16u64 {
                let g = Arc::clone(&gate);
                all.push(spawn_spread_joined(async move {
                    let mut had = 0usize;
                    for r in 0..200u64 {
                        match timeout(Duration::from_micros((t * 37 + r * 11) % 300), g.turn()).await {
                            Ok(_turn) => {
                                had += 1;
                                yield_once().await;
                            }
                            Err(_) => yield_once().await,
                        }
                    }
                    had
                }));
            }
            let mut had = 0;
            for t in all {
                had += timeout(Duration::from_secs(60), t).await?.map_err(|_| anyhow::anyhow!("a task ended without its turns"))?;
            }
            anyhow::ensure!(had > 0, "no turn came in 3,200 tries");
            anyhow::ensure!(gate.free.load(Ordering::SeqCst) == 1, "a turn was lost or made: {gate:?}");
            let last = timeout(Duration::from_secs(1), gate.turn()).await?;
            drop(last);
            anyhow::ensure!(gate.free.load(Ordering::SeqCst) == 1 && gate.line.is_empty(), "{gate:?}");
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a gate's turns under wakers that aren't a task's own (plain threads'
    /// executors), from threads and shards at once: one at a time throughout.
    #[test]
    fn g5_a_gate_takes_any_waker() -> anyhow::Result<()> {
        let gate = Arc::new(Gate::one());
        let inside = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (gate, inside) = (Arc::clone(&gate), Arc::clone(&inside));
                std::thread::spawn(move || {
                    futures::executor::block_on(async {
                        for _ in 0..500 {
                            let _turn = gate.turn().await;
                            if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                                return false;
                            }
                            std::thread::yield_now();
                            inside.fetch_sub(1, Ordering::SeqCst);
                        }
                        true
                    })
                })
            })
            .collect();
        let (g, i) = (Arc::clone(&gate), Arc::clone(&inside));
        let on_shard = block_on(async move {
            for _ in 0..500 {
                let _turn = g.turn().await;
                anyhow::ensure!(i.fetch_add(1, Ordering::SeqCst) == 0, "two held a gate of one");
                yield_once().await;
                i.fetch_sub(1, Ordering::SeqCst);
            }
            Ok(())
        });
        for t in threads {
            anyhow::ensure!(t.join().map_err(|_| anyhow::anyhow!("a thread panicked"))?, "two held a gate of one");
        }
        on_shard?;
        anyhow::ensure!(gate.free.load(Ordering::SeqCst) == 1, "{gate:?}");
        Ok(())
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a baton's value goes from holder to holder whole: each holder,
    /// across other tasks' polls, finds what the one before left, so of tasks on every shard each adding one, none
    /// is lost or doubled.
    #[test]
    fn g5_a_batons_value_goes_from_holder_to_holder_whole() -> anyhow::Result<()> {
        block_on(async {
            let baton: Arc<Baton<Vec<usize>>> = Arc::new(Baton::default());
            let (tasks, rounds) = (8usize, 100usize);
            let mut all = Vec::new();
            for _ in 0..tasks {
                let baton = Arc::clone(&baton);
                all.push(spawn_spread_joined(async move {
                    for _ in 0..rounds {
                        let mut held = baton.turn().await;
                        let next = held.len();
                        yield_once().await;
                        held.push(next);
                    }
                }));
            }
            for t in all {
                timeout(Duration::from_secs(60), t).await?.map_err(|_| anyhow::anyhow!("a task ended without its turns"))?;
            }
            let held = baton.turn().await;
            anyhow::ensure!(*held == (0..tasks * rounds).collect::<Vec<_>>(), "the value went astray: {} items", held.len());
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): readers are in together; a writer waits for them, and a reader who
    /// comes after it waits for it; it's alone while in; and from every shard no writer is ever beside anyone.
    #[test]
    fn g5_readers_go_together_and_a_writer_alone_in_the_order_they_came() -> anyhow::Result<()> {
        block_on(async {
            let rw = Arc::new(ReadWrite::new());
            let (r1, r2) = (rw.read().await, rw.read().await);
            let mut writer = std::pin::pin!(rw.write());
            stand(writer.as_mut(), "a writer beside readers", || rw.gate.free.load(Ordering::SeqCst) == 0).await?;
            let mut late = std::pin::pin!(rw.read());
            stand(late.as_mut(), "a reader past a writer waiting", || rw.gate.line.len() == 1).await?;
            drop(r1);
            anyhow::ensure!(futures::poll!(writer.as_mut()).is_pending(), "a writer went in beside a reader");
            drop(r2);
            let w = timeout(Duration::from_secs(1), writer).await?;
            anyhow::ensure!(futures::poll!(late.as_mut()).is_pending(), "a reader went in beside the writer");
            drop(w);
            let late = timeout(Duration::from_secs(1), late).await?;
            drop(late);
            let (readers, writers) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let mut all = Vec::new();
            for t in 0..12usize {
                let (rw, readers, writers) = (Arc::clone(&rw), Arc::clone(&readers), Arc::clone(&writers));
                all.push(spawn_spread_joined(async move {
                    for r in 0..100usize {
                        let beside = if (t + r) % 4 == 0 {
                            let _w = rw.write().await;
                            let beside = writers.fetch_add(1, Ordering::SeqCst) + readers.load(Ordering::SeqCst);
                            yield_once().await;
                            writers.fetch_sub(1, Ordering::SeqCst);
                            beside
                        } else {
                            let _r = rw.read().await;
                            readers.fetch_add(1, Ordering::SeqCst);
                            let beside = writers.load(Ordering::SeqCst);
                            yield_once().await;
                            readers.fetch_sub(1, Ordering::SeqCst);
                            beside
                        };
                        if beside != 0 {
                            return false;
                        }
                    }
                    true
                }));
            }
            for t in all {
                anyhow::ensure!(timeout(Duration::from_secs(60), t).await?.map_err(|_| anyhow::anyhow!("a task ended early"))?, "a writer was in beside another");
            }
            anyhow::ensure!(rw.readers.load(Ordering::SeqCst) == 0 && rw.gate.free.load(Ordering::SeqCst) == 1, "readers {}, {:?}", rw.readers.load(Ordering::SeqCst), rw.gate);
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a key lets one in at a time, another key's holder goes beside it,
    /// and an entry goes once no one holds it, waits for it or has it; from every shard, a new handle each round (so
    /// entries go and come back while others wait), no key ever has two inside, and the map ends empty.
    #[test]
    fn g5_a_key_lets_one_in_at_a_time_and_its_entry_goes_once_unused() -> anyhow::Result<()> {
        block_on(async {
            let keyed: Arc<Keyed<String, Gate>> = Arc::new(Keyed::new());
            let a = keyed.of(&"a".to_owned());
            let held = a.turn().await;
            let a2 = keyed.of(&"a".to_owned());
            {
                let mut second = std::pin::pin!(a2.turn());
                stand(second.as_mut(), "two holding one key", || a2.line.len() == 1).await?;
                let b = keyed.of(&"b".to_owned());
                let other = timeout(Duration::from_secs(1), b.turn()).await?;
                drop(held);
                let second = timeout(Duration::from_secs(1), second).await?;
                drop(second);
                drop(other);
                drop(b);
            }
            anyhow::ensure!(keyed.len() == 1, "{} keys with an entry, \"a\"'s users still there", keyed.len());
            drop(a2);
            drop(a);
            anyhow::ensure!(keyed.is_empty(), "{} entries left with no user", keyed.len());
            let inside: Arc<[AtomicUsize; 4]> = Arc::new(Default::default());
            let mut all = Vec::new();
            for t in 0..16usize {
                let (keyed, inside) = (Arc::clone(&keyed), Arc::clone(&inside));
                all.push(spawn_spread_joined(async move {
                    for r in 0..150usize {
                        let k = (t + r) % 4;
                        let entry = keyed.of(&format!("key {k}"));
                        let _turn = entry.turn().await;
                        let Some(count) = inside.get(k) else { return false };
                        if count.fetch_add(1, Ordering::SeqCst) != 0 {
                            return false;
                        }
                        yield_once().await;
                        count.fetch_sub(1, Ordering::SeqCst);
                    }
                    true
                }));
            }
            for t in all {
                anyhow::ensure!(timeout(Duration::from_secs(60), t).await?.map_err(|_| anyhow::anyhow!("a task ended early"))?, "two were inside one key at once");
            }
            anyhow::ensure!(keyed.is_empty(), "{} entries left after every user went", keyed.len());
            Ok(())
        })
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a value is made once, by the first of many asking at once on every
    /// shard, and each finds it; a making given up (its task dropped) leaves it unmade for the next to make.
    #[test]
    fn g5_once_is_made_once_and_a_making_given_up_passes_to_the_next() -> anyhow::Result<()> {
        block_on(async {
            let once: Arc<Once<u64>> = Arc::new(Once::new());
            let made = Arc::new(AtomicUsize::new(0));
            let mut all = Vec::new();
            for _ in 0..16 {
                let (once, made) = (Arc::clone(&once), Arc::clone(&made));
                all.push(spawn_spread_joined(async move {
                    *once
                        .get_or_init(|| async {
                            made.fetch_add(1, Ordering::SeqCst);
                            sleep(Duration::from_millis(10)).await;
                            7
                        })
                        .await
                }));
            }
            for t in all {
                let v = timeout(Duration::from_secs(10), t).await?.map_err(|_| anyhow::anyhow!("a task ended early"))?;
                anyhow::ensure!(v == 7, "found {v}");
            }
            anyhow::ensure!(made.load(Ordering::SeqCst) == 1, "made {} times", made.load(Ordering::SeqCst));
            let again: Once<u64> = Once::new();
            let stalled = timeout(Duration::from_millis(20), again.get_or_init(futures::future::pending::<u64>)).await;
            anyhow::ensure!(stalled.is_err() && again.get().is_none(), "a making given up left a value");
            anyhow::ensure!(*timeout(Duration::from_secs(1), again.get_or_init(|| async { 9 })).await? == 9, "the next making's value wasn't kept");
            Ok(())
        })
    }
}
