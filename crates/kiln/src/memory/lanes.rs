//! Fan-out lanes with no lock on any path (goal 4, plan 01a1a59c2a9c step 5): what a [`super::ring::Ring`] fans
//! out, without its channel's lock. A ring's every send and every subscriber's receive took that one lock
//! (async-broadcast's `Mutex`, which wakes the waiting subscribers while it's held): for agent sessions' events,
//! every change of a session's info from that session's owner shard, and every listener's receive of it on its
//! own, so a holder the OS preempted held up the shards behind it (goal 5's rule: no lock two shards share on a
//! request's, a websocket's or a runner's path; the lab's many-sessions A/B, 10 to 142 polls a second blocked
//! in a call with CPU hogs).
//!
//! Here each sender sends on a lane of its own (an agent session's events on its owner shard's: the lane its
//! id hashes to, `crate::harness::copies::owner_of`), and each reader has a channel of its own for each lane
//! (`hyper_rt::sync::channel`: Vyukov's bounded queue, and the runtime's waiter word for waiting, so neither end
//! waits for the other to leave a critical section), which it takes from in turn.
//!
//! - **Order.** What's sent on one lane reaches each reader in the order it was sent; what's sent on two lanes,
//!   in either order. A sender that keeps a kind of item on one lane (a session's events on the session's)
//!   keeps that kind's order for every reader.
//! - **Readers** are a copy of the list of them, swapped in whole (`arc_swap`): a send reads it; a reader
//!   joining or going swaps in the next, a compare-and-swap tried again on a race. A reader has what's sent
//!   once it's in the list: what a send read the list for before is from before it joined.
//! - **Bounds.** Each of a reader's lanes holds what it asked for ([`Lanes::subscribe`]), made as it
//!   subscribes; past it what comes on that lane is let go of for that reader, counted, and the reader is told
//!   how many it lost (`RecvError::Overflowed`, as a ring's slow subscriber is) before what it takes next. Each
//!   item is counted once by the bytes it holds past its place, from its send until the last reader lets it
//!   go, against the lanes' bound: past it what's sent is let go of for every reader, each told so, and one
//!   larger than the whole bound still goes when nothing is held. The items' bytes and each reader's places
//!   are their [`Owner`]'s on `/metrics` (`kiln_memory_bytes{owner}`, `kiln_memory_bound_bytes{owner}`).

use std::future::Future as _;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use arc_swap::ArcSwap;
pub use async_broadcast::{RecvError, TryRecvError};
use hyper_rt::sync::{ChannelReceiver, Sender, SyncError};

use super::Owner;
use super::queue::Weigh;

/// What the lanes' items hold against their bound, each counted once.
struct Space {
    owner: &'static Owner,
    bound: usize,
    held: AtomicUsize,
}

impl Space {
    /// Whether `n` bytes more would pass the bound: never while nothing is held, so one larger than the whole
    /// bound still goes.
    fn would_pass(&self, n: usize) -> bool {
        let held = self.held.load(Ordering::Acquire);
        held > 0 && held.saturating_add(n) > self.bound
    }

    fn take(&self, n: usize) {
        self.held.fetch_add(n, Ordering::AcqRel);
        self.owner.add(n);
    }

    /// Lets `n` bytes go: never below none.
    fn give(&self, n: usize) {
        let mut now = self.held.load(Ordering::Acquire);
        while let Err(seen) = self.held.compare_exchange_weak(now, now.saturating_sub(n), Ordering::AcqRel, Ordering::Acquire) {
            now = seen;
        }
        self.owner.sub(n.min(now));
    }
}

impl Drop for Space {
    fn drop(&mut self) {
        self.owner.sub(*self.held.get_mut());
        self.owner.bound_sub(self.bound);
    }
}

/// An item as the lanes carry it: one allocation its readers share, its bytes counted until the last lets it go.
struct Kept<T> {
    item: T,
    bytes: usize,
    space: Arc<Space>,
}

impl<T> Drop for Kept<T> {
    fn drop(&mut self) {
        self.space.give(self.bytes);
    }
}

/// What a reader's lane carries: an item, or word that one was let go of for it (counted in its `lost`), which
/// wakes a reader whose lanes held nothing to wake it with.
enum Carried<T> {
    Item(Arc<Kept<T>>),
    Lost,
}

/// A reader as the senders hold it: a sender into each of its lanes, and how many items it lost since it last
/// said so. Its reader keeps the count alone, so the lanes' senders go with the list, and its lanes then end.
struct Inbox<T> {
    lanes: Box<[Sender<Carried<T>>]>,
    lost: Arc<AtomicU64>,
}

impl<T> Inbox<T> {
    /// One of what was sent on `lane` let go of for this reader: counted, and said in that lane where there's
    /// room (none: its reader has what's there to take, and finds the count as it takes).
    fn lose(&self, lane: usize) {
        self.lost.fetch_add(1, Ordering::AcqRel);
        if let Some(tx) = self.lanes.get(lane) {
            let _ = tx.try_send(Carried::Lost);
        }
    }
}

/// What the lanes and their readers share.
struct Shared<T> {
    space: Arc<Space>,
    lanes: usize,
    readers: ArcSwap<Vec<Arc<Inbox<T>>>>,
}

/// Items fanned out to every reader, each on its sender's lane, with no lock on any path (the module's doc).
pub struct Lanes<T> {
    shared: Arc<Shared<T>>,
}

impl<T: Clone + Weigh + Send + Sync + 'static> Lanes<T> {
    /// `lanes` lanes (one at the least) whose items hold at most `bytes` past their places, counted as `owner`'s.
    pub fn new(owner: &'static Owner, lanes: usize, bytes: usize) -> Self {
        owner.bound_add(bytes);
        let space = Arc::new(Space { owner, bound: bytes, held: AtomicUsize::new(0) });
        Self { shared: Arc::new(Shared { space, lanes: lanes.max(1), readers: ArcSwap::from_pointee(Vec::new()) }) }
    }

    /// Its lanes: what [`Lanes::send`]'s `lane` is taken modulo.
    pub fn lanes(&self) -> usize {
        self.shared.lanes
    }

    /// Readers now, read off their list (no lock).
    pub fn receiver_count(&self) -> usize {
        self.shared.readers.load().len()
    }

    /// What its items hold now past their places, and its readers' places, in bytes.
    pub fn held(&self) -> usize {
        self.shared.space.held.load(Ordering::Acquire)
    }

    /// Sends `item` on lane `lane` (modulo [`Lanes::lanes`]) to every reader now: none, and it's dropped. A
    /// reader whose lane is full loses it (counted, and said before what it takes next); past the lanes' bytes
    /// every reader does.
    pub fn send(&self, lane: usize, item: T) {
        let readers = self.shared.readers.load();
        if readers.is_empty() {
            return;
        }
        let lane = lane % self.shared.lanes;
        let bytes = item.weight().saturating_sub(size_of::<T>());
        let space = &self.shared.space;
        if space.would_pass(bytes) {
            for reader in readers.iter() {
                reader.lose(lane);
            }
            return;
        }
        space.take(bytes);
        let kept = Arc::new(Kept { item, bytes, space: Arc::clone(space) });
        for reader in readers.iter() {
            let Some(tx) = reader.lanes.get(lane) else { continue };
            // Gone (its reader dropped): the reader takes itself off the list as it goes.
            if let Err(SyncError::Full(_)) = tx.try_send(Carried::Item(Arc::clone(&kept))) {
                reader.lost.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    /// A reader from now, each of its lanes holding up to `per_lane` items (one at the least): what's sent once it
    /// is in the readers' list. Refused when the runtime has no channel to give it (its cells all in use).
    pub fn subscribe(&self, per_lane: usize) -> Result<Reader<T>, hyper_rt::RtError> {
        let per_lane = per_lane.max(1);
        let (mut txs, mut rxs) = (Vec::with_capacity(self.shared.lanes), Vec::with_capacity(self.shared.lanes));
        for _ in 0..self.shared.lanes {
            // A place for one sender waiting for room: none ever waits (`try_send`).
            let (tx, rx) = hyper_rt::sync::channel_with(per_lane, 1)?;
            txs.push(tx);
            rxs.push(rx);
        }
        let lost = Arc::new(AtomicU64::new(0));
        let inbox = Arc::new(Inbox { lanes: txs.into_boxed_slice(), lost: Arc::clone(&lost) });
        self.shared.readers.rcu(|now| {
            let mut next = Vec::with_capacity(now.len() + 1);
            next.extend(now.iter().cloned());
            next.push(Arc::clone(&inbox));
            next
        });
        // Its lanes' places, made now: a slot's stamp and what it carries, for each.
        let places = per_lane.saturating_mul(self.shared.lanes).saturating_mul(size_of::<usize>() + size_of::<Carried<T>>());
        self.shared.space.owner.add(places);
        self.shared.space.owner.bound_add(places);
        Ok(Reader { shared: Arc::clone(&self.shared), lost, lanes: rxs.into_boxed_slice(), next: 0, places })
    }
}

impl<T> Drop for Lanes<T> {
    fn drop(&mut self) {
        // Its readers end once they've taken what was sent: each lane's sender goes with the list.
        self.shared.readers.store(Arc::new(Vec::new()));
    }
}

/// A reader of [`Lanes`]: its own channel for each lane, taken from in turn. A hyper-rt task's, as the runtime's
/// channels are (polled from anything else, its lanes read as ended).
pub struct Reader<T> {
    shared: Arc<Shared<T>>,
    lost: Arc<AtomicU64>,
    lanes: Box<[ChannelReceiver<Carried<T>>]>,
    /// The lane it takes from first next: the one after the last it took from, so a busy lane doesn't keep
    /// the others waiting.
    next: usize,
    places: usize,
}

impl<T: Clone> Reader<T> {
    /// The next item, after a charge of the task's turn (`hyper_rt::coop`, as a ring's subscriber's receive):
    /// `RecvError::Overflowed` with how many it lost first, when it lost some; `Closed` once its lanes are gone
    /// and empty.
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        hyper_rt::coop::consume_budget().await;
        std::future::poll_fn(|cx| self.poll_take(cx)).await
    }

    /// The next item if one waits: `TryRecvError::Empty` when none does; `Overflowed` and `Closed` as [`Reader::recv`]'s.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(n) = self.took_lost() {
            return Err(TryRecvError::Overflowed(n));
        }
        let n = self.lanes.len();
        let mut ended = 0;
        for i in 0..n {
            let k = (self.next + i) % n;
            let Some(lane) = self.lanes.get_mut(k) else { continue };
            loop {
                match lane.try_recv() {
                    Ok(Some(Carried::Item(kept))) => {
                        self.next = k + 1;
                        return Ok(kept.item.clone());
                    }
                    // What it lost is in its count, said below or as it next takes.
                    Ok(Some(Carried::Lost)) => {}
                    Ok(None) => break,
                    Err(_) => {
                        ended += 1;
                        break;
                    }
                }
            }
        }
        if let Some(n) = self.took_lost() {
            return Err(TryRecvError::Overflowed(n));
        }
        if ended == n { Err(TryRecvError::Closed) } else { Err(TryRecvError::Empty) }
    }

    /// How many it lost since it last said so, if any.
    fn took_lost(&self) -> Option<u64> {
        let n = self.lost.swap(0, Ordering::AcqRel);
        (n > 0).then_some(n)
    }

    /// Takes from its lanes in turn, each lane's own wait registering the task with it (and looking again
    /// after), so a send on any of them wakes it.
    fn poll_take(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        loop {
            if let Some(n) = self.took_lost() {
                return Poll::Ready(Err(RecvError::Overflowed(n)));
            }
            let n = self.lanes.len();
            let (mut ended, mut marked) = (0, false);
            for i in 0..n {
                let k = (self.next + i) % n;
                let Some(lane) = self.lanes.get_mut(k) else { continue };
                match Pin::new(&mut lane.recv()).poll(cx) {
                    Poll::Ready(Ok(Carried::Item(kept))) => {
                        self.next = k + 1;
                        return Poll::Ready(Ok(kept.item.clone()));
                    }
                    // Word of a loss: counted already, said as it looks again.
                    Poll::Ready(Ok(Carried::Lost)) => {
                        marked = true;
                        break;
                    }
                    Poll::Ready(Err(_)) => ended += 1,
                    Poll::Pending => {}
                }
            }
            if marked {
                continue;
            }
            // A loss counted after its first look (its lane full: no word of it comes).
            if let Some(n) = self.took_lost() {
                return Poll::Ready(Err(RecvError::Overflowed(n)));
            }
            return if ended == n { Poll::Ready(Err(RecvError::Closed)) } else { Poll::Pending };
        }
    }
}

impl<T> Drop for Reader<T> {
    fn drop(&mut self) {
        // Off the list: its lanes' senders go with its entry (and its lanes, with it, refuse what's sent now).
        let lost = Arc::clone(&self.lost);
        self.shared.readers.rcu(|now| now.iter().filter(|r| !Arc::ptr_eq(&r.lost, &lost)).cloned().collect::<Vec<_>>());
        self.shared.space.owner.sub(self.places);
        self.shared.space.owner.bound_sub(self.places);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's item: its sender, its number, and `0` bytes past its place.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Item(u32, u64, usize);

    impl Weigh for Item {
        fn weight(&self) -> usize {
            size_of::<Self>() + self.2
        }
    }

    /// What a source file's code says outside its tests, its comments left out (CRLF read as LF).
    fn code_of(src: &str) -> String {
        let src = src.replace("\r\n", "\n");
        let code = src.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default();
        code.lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n")
    }

    /// Goal 4, plan 01a1a59c2a9c step 5 (goal 5's rule), run by kiln's CI with every test: the lanes take no
    /// lock of their own and use no channel that takes one. Its readers' list is an `arc_swap` copy, and its
    /// lanes are hyper-rt's channels (no lock on any path: `hyper_rt::sync`'s doc).
    #[test]
    fn g4_s5_the_lanes_take_no_lock() {
        let code = code_of(include_str!("lanes.rs"));
        for lock in ["Mutex", "RwLock", "Condvar", "parking_lot", "async_lock", "ReentrantLock", "async_channel", "event_listener", "broadcast("] {
            assert!(!code.contains(lock), "memory/lanes.rs takes a lock, or a channel that does: {lock}");
        }
        assert!(code_of("a // Mutex\nb\n#[cfg(test)]\nmod tests {\n Mutex").find("Mutex").is_none(), "the check reads code alone");
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: senders on every shard at once, each on a lane of its own, and readers
    /// taking on every shard: each reader has every item once, each sender's in the order it sent them.
    #[test]
    fn g4_s5_each_lanes_items_reach_every_reader_in_order_none_lost_or_doubled() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_lanes_order");
        crate::rt::block_on(async {
            let (senders, per, lanes) = (8u32, 400u64, 4usize);
            let fan = Arc::new(Lanes::<Item>::new(&OWNER, lanes, 1 << 20));
            // Each lane's room for all its senders' items: nothing is let go of, so none may be missing.
            let room = usize::try_from(per)? * usize::try_from(senders)?.div_ceil(lanes);
            let mut readers = Vec::new();
            for _ in 0..3 {
                let mut reader = fan.subscribe(room)?;
                let want = usize::try_from(per)? * usize::try_from(senders)?;
                readers.push(crate::rt::spawn_spread_joined(async move {
                    let mut got: Vec<Item> = Vec::with_capacity(want);
                    while got.len() < want {
                        match reader.recv().await {
                            Ok(item) => got.push(item),
                            Err(e) => return Err(anyhow::anyhow!("a reader was told {e:?} after {} items", got.len())),
                        }
                    }
                    Ok(got)
                }));
            }
            anyhow::ensure!(fan.receiver_count() == 3);
            let mut sent = Vec::new();
            for s in 0..senders {
                let fan = Arc::clone(&fan);
                sent.push(crate::rt::spawn_spread_joined(async move {
                    for n in 1..=per {
                        fan.send(usize::try_from(s).unwrap_or(0), Item(s, n, 0));
                        if n % 50 == 0 {
                            crate::rt::maybe_yield().await;
                        }
                    }
                }));
            }
            for s in sent {
                s.await.map_err(|_| anyhow::anyhow!("a sender ended early"))?;
            }
            for r in readers {
                let got = crate::rt::timeout(std::time::Duration::from_secs(30), r).await.map_err(|_| anyhow::anyhow!("a reader never had them all"))?.map_err(|_| anyhow::anyhow!("a reader ended"))??;
                for s in 0..senders {
                    let theirs: Vec<u64> = got.iter().filter(|i| i.0 == s).map(|i| i.1).collect();
                    anyhow::ensure!(theirs == (1..=per).collect::<Vec<_>>(), "sender {s}'s items: {} of {per}, in order: {}", theirs.len(), theirs.windows(2).all(|w| w.first() < w.get(1)));
                }
            }
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a reader that joins while items come has each sent after it joined,
    /// in order, and none from before; the reader there from the start has them all.
    #[test]
    fn g4_s5_a_reader_joining_mid_stream_has_what_comes_after() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_lanes_join");
        crate::rt::block_on(async {
            let fan = Lanes::<Item>::new(&OWNER, 2, 1 << 20);
            let mut first = fan.subscribe(64)?;
            for n in 1..=5 {
                fan.send(0, Item(0, n, 0));
            }
            let mut late = fan.subscribe(64)?;
            for n in 6..=9 {
                fan.send(0, Item(0, n, 0));
            }
            fan.send(1, Item(1, 1, 0));
            let take = |r: &mut Reader<Item>| {
                let mut out = Vec::new();
                while let Ok(item) = r.try_recv() {
                    out.push((item.0, item.1));
                }
                out
            };
            let (all, after) = (take(&mut first), take(&mut late));
            let lane0 = |v: &[(u32, u64)]| v.iter().filter(|(s, _)| *s == 0).map(|(_, n)| *n).collect::<Vec<_>>();
            anyhow::ensure!(lane0(&all) == (1..=9).collect::<Vec<_>>() && all.contains(&(1, 1)), "{all:?}");
            anyhow::ensure!(lane0(&after) == (6..=9).collect::<Vec<_>>() && after.contains(&(1, 1)), "{after:?}");
            anyhow::ensure!(matches!(late.try_recv(), Err(TryRecvError::Empty)));
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a reader whose lane is full loses what comes and is told how many
    /// first, the rest of what it had then; another with room has it all; and past the lanes' bytes, every
    /// reader loses what's sent and is told so, the bytes given back as the last reader lets each item go.
    #[test]
    fn g4_s5_a_slow_reader_is_told_what_it_lost_and_the_bytes_are_bounded() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_lanes_bounds");
        crate::rt::block_on(async {
            let fan = Lanes::<Item>::new(&OWNER, 1, 1 << 20);
            let (mut slow, mut roomy) = (fan.subscribe(2)?, fan.subscribe(16)?);
            for n in 1..=5 {
                fan.send(0, Item(0, n, 0));
            }
            anyhow::ensure!(matches!(slow.try_recv(), Err(TryRecvError::Overflowed(3))), "told it lost three");
            anyhow::ensure!(slow.try_recv().ok() == Some(Item(0, 1, 0)) && slow.try_recv().ok() == Some(Item(0, 2, 0)));
            anyhow::ensure!(matches!(slow.try_recv(), Err(TryRecvError::Empty)));
            for n in 1..=5 {
                anyhow::ensure!(roomy.try_recv().ok() == Some(Item(0, n, 0)), "the reader with room, {n}");
            }
            drop((slow, roomy));
            anyhow::ensure!(OWNER.bytes() == 0 && fan.receiver_count() == 0, "places and items let go: {}", OWNER.bytes());
            // Bytes: 600 the bound, items of 400 past their places.
            let small = Lanes::<Item>::new(&OWNER, 1, 600);
            let (mut a, mut b) = (small.subscribe(8)?, small.subscribe(8)?);
            let places = OWNER.bytes();
            small.send(0, Item(0, 1, 400));
            anyhow::ensure!(OWNER.bytes() == places + 400, "counted once for both readers");
            small.send(0, Item(0, 2, 400));
            for r in [&mut a, &mut b] {
                anyhow::ensure!(matches!(r.try_recv(), Err(TryRecvError::Overflowed(1))), "told the second was let go of");
                anyhow::ensure!(r.try_recv().ok() == Some(Item(0, 1, 400)));
                anyhow::ensure!(matches!(r.try_recv(), Err(TryRecvError::Empty)));
            }
            anyhow::ensure!(OWNER.bytes() == places, "the first given back once both took it");
            // b keeps the third, a takes it and waits: the fourth, past the bytes, is let go of, and its word wakes a.
            small.send(0, Item(0, 3, 400));
            anyhow::ensure!(a.try_recv().ok() == Some(Item(0, 3, 400)));
            let waiting = crate::rt::spawn(async move { a.recv().await });
            crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            small.send(0, Item(0, 4, 400));
            let woken = crate::rt::timeout(std::time::Duration::from_secs(5), waiting).await.map_err(|_| anyhow::anyhow!("the waiting reader wasn't woken"))?.map_err(|_| anyhow::anyhow!("the task ended"))?;
            anyhow::ensure!(woken == Err(RecvError::Overflowed(1)), "{woken:?}");
            anyhow::ensure!(matches!(b.try_recv(), Err(TryRecvError::Overflowed(1))) && b.try_recv().ok() == Some(Item(0, 3, 400)));
            anyhow::ensure!(matches!(b.try_recv(), Err(TryRecvError::Empty)));
            drop(b);
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: lanes dropped, their readers take what was sent, then are told
    /// they've ended; a reader dropped leaves the list.
    #[test]
    fn g4_s5_lanes_dropped_end_their_readers_after_what_was_sent() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_lanes_end");
        crate::rt::block_on(async {
            let fan = Lanes::<Item>::new(&OWNER, 3, 1 << 20);
            let (mut r, gone) = (fan.subscribe(4)?, fan.subscribe(4)?);
            anyhow::ensure!(fan.receiver_count() == 2);
            drop(gone);
            anyhow::ensure!(fan.receiver_count() == 1, "a reader dropped leaves the list");
            fan.send(2, Item(2, 1, 0));
            drop(fan);
            anyhow::ensure!(r.recv().await == Ok(Item(2, 1, 0)), "what was sent comes first");
            anyhow::ensure!(r.recv().await == Err(RecvError::Closed), "then the end");
            Ok(())
        })
    }
}
