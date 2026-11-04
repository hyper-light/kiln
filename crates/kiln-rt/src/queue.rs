//! The local run queue: a ring of ready slot indices with one pending flag per slot, so a task woken
//! many times between polls appears once and the ring never holds more than the arena's slots (slates'
//! `queue.rs`; §4.3 "intrusive run queue"; no allocation on the wake path).
//!
//! Single-threaded by construction: only the owning shard's thread pushes and pops. The rings are of
//! `Cell`s (docs/runtime.md §3.4), so nothing is borrowed and nothing is refused: slates' version kept its
//! lists in `RefCell`s and counted the re-entrant accesses it refused.
//!
//! A step takes at most its configured poll budget, oldest first. Wakes that arrive while the phase
//! runs join the same FIFO behind tasks already ready and may use its remaining budget. A task waking
//! itself cannot extend that budget, so the phase never drains an unbounded ready set. `batch` can
//! still capture the number ready now for callers that need a fixed snapshot.
//!
//! A task that wakes itself in its own poll (a yield, [`crate::futures::yield_now`], or any future with
//! more to do) is listed apart, and joins the FIFO only when the loop has next read its inputs: its
//! control, wakes from other threads, timers, pollers and the driver's completions go ahead of it.
//! Re-listed in the FIFO at once, such a task took the rest of its step's budget, poll after poll, while
//! those inputs waited for the step to end (`tests/yield_defers.rs`; tokio defers its `yield_now` past
//! its driver's poll the same way, tokio 1.23 #5223). A slot is in one list at most (its flag), and each
//! list holds one entry per slot.

use std::cell::{Cell, OnceCell};

use crate::coop::Class;

/// A first-in-first-out ring of slot numbers, one entry per slot at most: [`crate::cells::CellRing`]'s
/// shape with the numbers held bare, since a slot is read only between the head and the length, so no entry
/// needs a mark of its own.
struct SlotRing {
    slots: Box<[Cell<u32>]>,
    /// The index of the oldest number.
    head: Cell<usize>,
    /// Numbers held.
    len: Cell<usize>,
}

impl SlotRing {
    /// A ring that holds at most `capacity` numbers.
    fn new(capacity: usize) -> Self {
        Self {
            slots: (0..capacity).map(|_| Cell::new(0)).collect(),
            head: Cell::new(0),
            len: Cell::new(0),
        }
    }

    fn len(&self) -> usize {
        self.len.get()
    }

    fn is_empty(&self) -> bool {
        self.len.get() == 0
    }

    /// Appends `slot`, or hands it back when the ring is full.
    #[inline]
    fn push(&self, slot: u32) -> Result<(), u32> {
        let len = self.len.get();
        let capacity = self.slots.len();
        if len >= capacity {
            return Err(slot);
        }
        // `head` and `len` are each below the capacity, so their sum is below twice it: one subtraction wraps it.
        let end = self.head.get().wrapping_add(len);
        let at = if end >= capacity {
            end.wrapping_sub(capacity)
        } else {
            end
        };
        let Some(cell) = self.slots.get(at) else {
            return Err(slot);
        };
        cell.set(slot);
        self.len.set(len.wrapping_add(1));
        Ok(())
    }

    /// Takes the oldest number.
    #[inline]
    fn pop(&self) -> Option<u32> {
        let len = self.len.get();
        if len == 0 {
            return None;
        }
        let head = self.head.get();
        let slot = self.slots.get(head)?.get();
        // `head` is below the capacity, so the next index is at most the capacity: it wraps by a comparison.
        let next = head.wrapping_add(1);
        self.head
            .set(if next >= self.slots.len() { 0 } else { next });
        self.len.set(len.wrapping_sub(1));
        Some(slot)
    }
}

/// A class's two lists: its ready FIFO and the slots of it that woke themselves since the loop last released
/// them. A release onto an empty ready FIFO swaps the lists' roles rather than moving each entry, the case of a
/// round of yields whose batch drained the FIFO before reading its inputs again.
struct Lists {
    /// Each list holds one entry per slot.
    lists: [SlotRing; 2],
    /// Which of `lists` is the ready FIFO: 0 or 1.
    ready: Cell<usize>,
}

impl Lists {
    fn new(capacity: usize) -> Self {
        Self {
            lists: [SlotRing::new(capacity), SlotRing::new(capacity)],
            ready: Cell::new(0),
        }
    }

    /// The ready FIFO.
    #[inline]
    fn ready_list(&self) -> &SlotRing {
        let [first, second] = &self.lists;
        if self.ready.get() == 0 { first } else { second }
    }

    /// The slots that woke themselves, waiting for their release.
    #[inline]
    fn yielded_list(&self) -> &SlotRing {
        let [first, second] = &self.lists;
        if self.ready.get() == 0 { second } else { first }
    }

    fn is_empty(&self) -> bool {
        self.ready_list().is_empty() && self.yielded_list().is_empty()
    }

    fn len(&self) -> usize {
        self.ready_list()
            .len()
            .saturating_add(self.yielded_list().len())
    }

    /// Lists every slot that woke itself behind the slots ready now, oldest first; `listed` is stamped at `now_ns`
    /// for each, the moment it joins the FIFO.
    fn release(&self, queue: &LocalQueue, now_ns: u64) {
        let (ready, yielded) = (self.ready_list(), self.yielded_list());
        let mut at = yielded.head.get();
        // Each released slot waits from here (its yield was its own choice, not a wait for the shard).
        for _ in 0..yielded.len() {
            if let Some(slot) = yielded.slots.get(at) {
                queue.stamp(slot.get(), now_ns);
            }
            at = at.wrapping_add(1);
            if at >= yielded.slots.len() {
                at = 0;
            }
        }
        // Behind nothing: the lists swap roles, the released slots in the order they yielded.
        if ready.is_empty() {
            self.ready.set(if self.ready.get() == 0 { 1 } else { 0 });
            return;
        }
        while let Some(slot) = yielded.pop() {
            // Never refused: a slot is in one list at most, so both hold no more than the arena's slots.
            if ready.push(slot).is_err() {
                if let Some(flag) = queue
                    .pending
                    .get(usize::try_from(slot).unwrap_or(usize::MAX))
                {
                    flag.set(false);
                }
                queue.overflow.set(queue.overflow.get().saturating_add(1));
            }
        }
    }
}

/// The lists of the classes other than [`Class::Normal`], and the time each class has run, made as a task of the
/// shard first takes another class: a shard whose tasks never do runs Normal's lists alone, as it did.
struct Others {
    critical: Lists,
    background: Lists,
    /// The poll time each class has run while it had tasks listed, nanoseconds, by `Class as usize`.
    ran_ns: [Cell<u64>; 3],
    /// The shard's virtual time: after each poll, the least time run of the class polled and the classes listed,
    /// never falling (the minimum virtual runtime a fair scheduler keeps, Linux's CFS `min_vruntime`). A class
    /// starting to list with no other listed is raised to it ([`LocalQueue::activate`]).
    vtime_ns: Cell<u64>,
}

impl Others {
    fn new(capacity: usize) -> Self {
        Self {
            critical: Lists::new(capacity),
            background: Lists::new(capacity),
            ran_ns: [Cell::new(0), Cell::new(0), Cell::new(0)],
            vtime_ns: Cell::new(0),
        }
    }

    fn ran(&self, class: Class) -> &Cell<u64> {
        let [background, normal, critical] = &self.ran_ns;
        match class {
            Class::Background => background,
            Class::Normal => normal,
            Class::Critical => critical,
        }
    }
}

/// The queue.
pub struct LocalQueue {
    pending: Box<[Cell<bool>]>,
    /// Each slot's class, as its task last set it (`crate::coop::set_class`): the lists it joins.
    class: Box<[Cell<Class>]>,
    /// When each slot last joined a ready FIFO, on the shard's clock as the loop last read it: its wait to its
    /// poll is the shard's queueing delay (kiln's goal 6, `Counters::queue_ns_normal` and the others).
    listed_ns: Box<[Cell<u64>]>,
    /// The Normal class's lists: the default, and every task's until it takes another.
    normal: Lists,
    /// The other classes' lists, made at the first task that takes one.
    others: OnceCell<Box<Others>>,
    /// Slots `push` has listed, a count the loop reads twice to tell whether polls between listed any.
    listed: Cell<u64>,
    overflow: Cell<u64>,
}

impl std::fmt::Debug for LocalQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalQueue")
            .field("capacity", &self.pending.len())
            .field("listed", &self.len())
            .field("classes", &self.others.get().is_some())
            .finish()
    }
}

impl LocalQueue {
    /// A queue for an arena of `capacity` slots.
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: (0..capacity).map(|_| Cell::new(false)).collect(),
            class: (0..capacity).map(|_| Cell::new(Class::Normal)).collect(),
            listed_ns: (0..capacity).map(|_| Cell::new(0)).collect(),
            normal: Lists::new(capacity),
            others: OnceCell::new(),
            listed: Cell::new(0),
            overflow: Cell::new(0),
        }
    }

    /// The class of the task in `slot` (Normal for a slot past the arena).
    pub fn class_of(&self, slot: u32) -> Class {
        self.class
            .get(usize::try_from(slot).unwrap_or(usize::MAX))
            .map_or(Class::Normal, Cell::get)
    }

    /// Sets the class of the task in `slot`, from its next listing; the class it had. The first slot of a class
    /// other than Normal makes the other classes' lists.
    pub fn set_class(&self, slot: u32, class: Class) -> Class {
        let Some(cell) = self.class.get(usize::try_from(slot).unwrap_or(usize::MAX)) else {
            return Class::Normal;
        };
        if class != Class::Normal {
            let _ = self
                .others
                .get_or_init(|| Box::new(Others::new(self.pending.len())));
        }
        cell.replace(class)
    }

    /// The lists `class` joins: Normal's while no other class was taken.
    fn lists(&self, class: Class) -> &Lists {
        match (class, self.others.get()) {
            (Class::Critical, Some(others)) => &others.critical,
            (Class::Background, Some(others)) => &others.background,
            _ => &self.normal,
        }
    }

    /// When the task in `slot` last joined a ready FIFO, on the shard's clock (0 for a slot never listed).
    pub fn listed_ns(&self, slot: u32) -> u64 {
        self.listed_ns
            .get(usize::try_from(slot).unwrap_or(usize::MAX))
            .map_or(0, Cell::get)
    }

    fn stamp(&self, slot: u32, now_ns: u64) {
        if let Some(stamp) = self
            .listed_ns
            .get(usize::try_from(slot).unwrap_or(usize::MAX))
        {
            stamp.set(now_ns);
        }
    }

    /// A class with nothing listed starts listing: its time run is raised to the least of the classes with tasks
    /// listed, or with none listed to the shard's virtual time, so time it spent idle is no credit against them (a
    /// class's share is of the time it wants the shard, as a weighted fair queue's virtual time is). Without the
    /// virtual time, a class that ran alone (a quiet shard's every class, in turn) kept all that time against the
    /// next class to list while it rested, and was shut out for as long once it listed again.
    fn activate(&self, class: Class) {
        let Some(others) = self.others.get() else {
            return;
        };
        let least = [Class::Critical, Class::Normal, Class::Background]
            .into_iter()
            .filter(|other| *other != class && !self.lists(*other).is_empty())
            .map(|other| others.ran(other).get())
            .min();
        // A listed class has run at least the virtual time (it was in the least that set it, or raised to it), so
        // this is the least listed's where one is listed.
        let floor = least.unwrap_or(0).max(others.vtime_ns.get());
        let ran = others.ran(class);
        ran.set(ran.get().max(floor));
    }

    /// Marks a slot ready at `now_ns` (the shard's clock as the loop last read it); a slot already pending is not
    /// listed twice. A slot beyond the arena is counted as overflow and ignored (a stale or foreign word).
    #[inline]
    pub fn push(&self, slot: u32, now_ns: u64) {
        let Some(flag) = self
            .pending
            .get(usize::try_from(slot).unwrap_or(usize::MAX))
        else {
            self.overflow.set(self.overflow.get().saturating_add(1));
            return;
        };
        if flag.replace(true) {
            return;
        }
        let class = self.class_of(slot);
        let lists = self.lists(class);
        let idle = lists.is_empty();
        // A slot is in the ring at most once (its flag), and the ring holds one entry per slot, so it never
        // refuses; were it to, the flag is cleared so the next wake lists the slot again.
        if lists.ready_list().push(slot).is_err() {
            flag.set(false);
            self.overflow.set(self.overflow.get().saturating_add(1));
            return;
        }
        self.stamp(slot, now_ns);
        if idle {
            self.activate(class);
        }
        // Compared only with an earlier reading: 2^64 listings between two readings cannot happen.
        self.listed.set(self.listed.get().wrapping_add(1));
    }

    /// The slots `push` has listed so far: two readings differ when it listed any between them.
    pub fn listed(&self) -> u64 {
        self.listed.get()
    }

    /// Marks a slot ready behind the loop's next reading of its inputs: the task woke itself in its own
    /// poll. A slot already pending keeps its place; one beyond the arena is counted as overflow and
    /// ignored.
    #[inline]
    pub fn defer(&self, slot: u32) {
        let Some(flag) = self
            .pending
            .get(usize::try_from(slot).unwrap_or(usize::MAX))
        else {
            self.overflow.set(self.overflow.get().saturating_add(1));
            return;
        };
        if flag.replace(true) {
            return;
        }
        let class = self.class_of(slot);
        let lists = self.lists(class);
        let idle = lists.is_empty();
        // Never refused, as for `push`: one entry per slot, and the list holds one for every slot.
        if lists.yielded_list().push(slot).is_err() {
            flag.set(false);
            self.overflow.set(self.overflow.get().saturating_add(1));
            return;
        }
        if idle {
            self.activate(class);
        }
    }

    /// Lists every slot that woke itself behind the slots ready now in its class, oldest first, its wait counted
    /// from `now_ns`: the loop calls it once its inputs are in the queue.
    pub fn release_yielded(&self, now_ns: u64) {
        self.normal.release(self, now_ns);
        if let Some(others) = self.others.get() {
            others.critical.release(self, now_ns);
            others.background.release(self, now_ns);
        }
    }

    /// Whether a slot that woke itself waits for its release.
    pub fn has_yielded(&self) -> bool {
        !self.normal.yielded_list().is_empty()
            || self.others.get().is_some_and(|others| {
                !others.critical.yielded_list().is_empty()
                    || !others.background.yielded_list().is_empty()
            })
    }

    /// How many slots the step about to run may take: at most `limit`, and only those ready now, so a wake
    /// during the batch waits for the next step.
    pub fn batch(&self, limit: usize) -> usize {
        let others = self.others.get().map_or(0, |others| {
            others
                .critical
                .ready_list()
                .len()
                .saturating_add(others.background.ready_list().len())
        });
        self.normal
            .ready_list()
            .len()
            .saturating_add(others)
            .min(limit)
    }

    /// Takes the next ready slot, clearing its pending flag so a wake during its poll lists it again: the oldest of
    /// Normal's while no task took another class; else the oldest of the class that has run least while it had
    /// tasks listed, Critical before Normal before Background where they have run alike, so each class with work
    /// gets an equal share of the shard's time and the most urgent goes first (kiln's goal 6, `crate::coop::Class`).
    #[inline]
    pub fn pop(&self) -> Option<u32> {
        let lists = match self.others.get() {
            None => &self.normal,
            Some(others) => self.pick(others)?,
        };
        let slot = lists.ready_list().pop()?;
        if let Some(flag) = self
            .pending
            .get(usize::try_from(slot).unwrap_or(usize::MAX))
        {
            flag.set(false);
        }
        Some(slot)
    }

    /// The lists to take from: of those with a slot ready, the class that has run least, the first in priority
    /// order where they have run alike.
    fn pick<'a>(&'a self, others: &'a Others) -> Option<&'a Lists> {
        let mut best: Option<(u64, &Lists)> = None;
        for (class, lists) in [
            (Class::Critical, &others.critical),
            (Class::Normal, &self.normal),
            (Class::Background, &others.background),
        ] {
            if lists.ready_list().is_empty() {
                continue;
            }
            let ran = others.ran(class).get();
            if best.is_none_or(|(least, _)| ran < least) {
                best = Some((ran, lists));
            }
        }
        best.map(|(_, lists)| lists)
    }

    /// Counts `ns` of poll time to the class of the task in `slot`, while classes are kept, and moves the shard's
    /// virtual time on to the least time run of that class and the classes listed.
    #[inline]
    pub fn charge(&self, slot: u32, ns: u64) {
        if let Some(others) = self.others.get() {
            let class = self.class_of(slot);
            let ran = others.ran(class);
            ran.set(ran.get().saturating_add(ns));
            let least = [Class::Critical, Class::Normal, Class::Background]
                .into_iter()
                .filter(|other| *other == class || !self.lists(*other).is_empty())
                .map(|other| others.ran(other).get())
                .min()
                .unwrap_or(0);
            others.vtime_ns.set(others.vtime_ns.get().max(least));
        }
    }

    /// Whether nothing is ready, yielded tasks included.
    pub fn is_empty(&self) -> bool {
        self.normal.is_empty()
            && self
                .others
                .get()
                .is_none_or(|others| others.critical.is_empty() && others.background.is_empty())
    }

    /// Ready entries, yielded tasks included.
    pub fn len(&self) -> usize {
        let others = self.others.get().map_or(0, |others| {
            others.critical.len().saturating_add(others.background.len())
        });
        self.normal.len().saturating_add(others)
    }

    /// Whether a task of the shard took a class other than Normal, so the queue keeps classes: what arrives may
    /// then go ahead of what is listed.
    pub fn has_classes(&self) -> bool {
        self.others.get().is_some()
    }

    /// Wakes for slots beyond the arena, ignored.
    pub fn overflow(&self) -> u64 {
        self.overflow.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(q: &LocalQueue, limit: usize) -> Vec<u32> {
        let n = q.batch(limit);
        (0..n).filter_map(|_| q.pop()).collect()
    }

    #[test]
    fn duplicates_collapse_and_drain_returns_in_order() {
        let q = LocalQueue::new(4);
        q.push(2, 0);
        q.push(0, 0);
        q.push(2, 0);
        q.push(3, 0);
        assert_eq!(q.len(), 3);
        assert_eq!(take(&q, usize::MAX), vec![2, 0, 3]);
        assert!(q.is_empty());
        q.push(2, 0);
        assert_eq!(take(&q, usize::MAX), vec![2]);
    }

    #[test]
    fn a_slot_beyond_the_arena_is_counted_not_listed() {
        let q = LocalQueue::new(2);
        q.push(9, 0);
        assert_eq!(q.overflow(), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn a_wake_during_a_drain_lands_in_the_next_batch() {
        let q = LocalQueue::new(3);
        q.push(1, 0);
        let n = q.batch(usize::MAX);
        assert_eq!(n, 1);
        assert_eq!(q.pop(), Some(1));
        q.push(1, 0);
        assert_eq!(take(&q, usize::MAX), vec![1]);
    }

    /// A yielded slot waits for its release, then goes behind every slot listed before it, and a wake while it
    /// waits does not list it twice. Do: yield 1, wake 2 and 1, release, wake 3. Expect: 2, 1, 3.
    #[test]
    fn a_yielded_slot_goes_behind_what_was_ready_at_its_release() {
        let q = LocalQueue::new(4);
        q.defer(1);
        assert!(!q.is_empty(), "a yielded slot is work");
        assert_eq!(q.pop(), None, "but not before its release");
        q.push(2, 0);
        q.push(1, 0);
        assert_eq!(q.len(), 2, "the wake of a yielded slot collapsed");
        q.release_yielded(0);
        q.push(3, 0);
        assert_eq!(take(&q, usize::MAX), vec![2, 1, 3]);
        assert!(q.is_empty());
    }

    /// §4.3 bounded work: a drain hands out at most its batch, oldest first; the slots left behind keep
    /// their places ahead of any wake that arrives while the batch runs, and a wake for one of them is
    /// still collapsed. Do: queue five, take two, wake one of the left behind and a new one. Expect:
    /// the first two, then the other three in order, then the new one.
    #[test]
    fn a_drain_takes_at_most_its_batch_and_the_rest_keep_their_places() {
        let q = LocalQueue::new(8);
        for slot in 0..5 {
            q.push(slot, 0);
        }
        assert_eq!(take(&q, 2), vec![0, 1]);
        assert_eq!(q.len(), 3, "the other three wait, not re-queued");
        q.push(3, 0);
        q.push(7, 0);
        assert_eq!(
            take(&q, 8),
            vec![2, 3, 4, 7],
            "FIFO; the repeated wake collapsed"
        );
    }

    /// Kiln's goal 6, step 3: with every class listed and none having run, Critical goes first, then Normal, then
    /// Background; each class's poll time counts to its share, so a class that ran more waits for the others to
    /// run as much; a class that was idle takes no credit for it. Do: list one slot of each class, take one, charge
    /// it; list again. Expect: Critical, Normal, Background in turn as they run alike, the least-run first after.
    #[test]
    fn g6_s3_classes_share_the_shard_and_the_most_urgent_goes_first() {
        let q = LocalQueue::new(8);
        q.set_class(1, Class::Critical);
        q.set_class(3, Class::Background);
        for slot in [3, 2, 1] {
            q.push(slot, 0);
        }
        assert_eq!(q.pop(), Some(1), "critical first");
        q.charge(1, 100);
        assert_eq!(q.pop(), Some(2), "then normal");
        q.charge(2, 100);
        assert_eq!(q.pop(), Some(3), "then background");
        q.charge(3, 100);
        // All three have run 100 ns: listed again, priority order.
        for slot in [3, 2, 1] {
            q.push(slot, 0);
        }
        assert_eq!(q.pop(), Some(1));
        q.charge(1, 1_000);
        // Critical ran 1,100 ns against 100: Normal and Background take their turns before it again.
        q.push(1, 0);
        assert_eq!(q.pop(), Some(2));
        q.charge(2, 50);
        assert_eq!(q.pop(), Some(3));
        q.charge(3, 50);
        assert_eq!(q.pop(), Some(1));
        assert!(q.is_empty());
    }

    /// An idle class takes no credit: a Background slot listed after Normal ran a long while alone starts level
    /// with Normal, not behind it by all that time, so Normal is not shut out while Background catches up.
    #[test]
    fn g6_s3_an_idle_class_takes_no_credit_for_its_idle_time() {
        let q = LocalQueue::new(8);
        q.set_class(5, Class::Background);
        q.push(2, 0);
        assert_eq!(q.pop(), Some(2));
        q.charge(2, 1_000_000);
        q.push(2, 0);
        q.push(5, 0);
        // Background was raised to Normal's time as it listed: level, so Normal (higher) first, then Background.
        assert_eq!(q.pop(), Some(2));
        q.charge(2, 10);
        assert_eq!(q.pop(), Some(5));
    }

    /// A class that ran alone owes nothing once it rests: Normal runs a long while alone and its work ends; a
    /// Background slot lists with nothing else listed, then Normal's lists again. Expect: the two level (Background
    /// was raised to the shard's virtual time, Normal's run), so Normal goes first and is not shut out while
    /// Background makes up the time Normal ran alone. Without the virtual time Background started at nothing and
    /// took the shard for that long.
    #[test]
    fn g6_s3_a_class_that_ran_alone_owes_nothing_once_it_rests() {
        let q = LocalQueue::new(8);
        q.set_class(5, Class::Background);
        q.push(2, 0);
        assert_eq!(q.pop(), Some(2));
        q.charge(2, 1_000_000);
        assert!(q.is_empty(), "Normal's work ended");
        q.push(5, 0);
        q.push(2, 0);
        assert_eq!(q.pop(), Some(2), "level: Normal first, by urgency");
        q.charge(2, 10);
        assert_eq!(q.pop(), Some(5));
        q.charge(5, 10);
        // Both ran 1,000,010 ns: Normal's next wake goes ahead again rather than behind a million of Background's.
        q.push(5, 0);
        q.push(2, 0);
        assert_eq!(q.pop(), Some(2));
    }

    /// A slot's wait is stamped as it joins a ready FIFO: at its push, and for one that yielded, at its release.
    #[test]
    fn g6_s4_a_slot_is_stamped_as_it_joins_a_ready_list() {
        let q = LocalQueue::new(4);
        q.push(1, 10);
        assert_eq!(q.listed_ns(1), 10);
        q.defer(2);
        q.release_yielded(25);
        assert_eq!(q.listed_ns(2), 25);
        assert_eq!(q.pop(), Some(1));
        assert_eq!(q.pop(), Some(2));
    }
}
