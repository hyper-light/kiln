//! Fan-out lanes with no lock on any path (goal 4, plan 01a1a59c2a9c step 5): a fan-out without a channel's
//! lock. async-broadcast's, which kiln's rings were, takes its `Mutex` for every send and every subscriber's
//! receive, and wakes the waiting subscribers while it holds it: agent sessions' events (every change of a
//! session's info, from that session's owner shard), the hub's other events, terminals' output and jobs' logs
//! all took it from the senders' shards and every reader's own, so a holder the OS preempted held up the shards
//! behind it (goal 5's rule: no lock two shards share on a request's, a websocket's or a runner's path; the lab's
//! many-sessions A/B, 10 to 142 polls a second blocked in a call with CPU hogs).
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
//! - **Bounds, each reader's own.** A reader's lanes hold as many items as it asked for each, made as it
//!   subscribes, and as many bytes as the lanes allow a reader ([`Lanes::new`]'s `bytes`): one larger than that
//!   still goes into lanes holding nothing. What comes past either is let go of for that reader alone, so a
//!   reader that stalls holds up none of the others, nor the senders. Each item is counted once by the bytes it
//!   holds past its place, from its send until the last reader lets it go; each reader's places and bytes are
//!   the bound. Both are their [`Owner`]'s on `/metrics` (`kiln_memory_bytes{owner}`,
//!   `kiln_memory_bound_bytes{owner}`).
//! - **A reader that lost some starts over** from what came after its last loss: it's told how many it missed
//!   (`RecvError::Overflowed`, those let go of and those it held from before them), and what it held from
//!   before is let go of, so what it takes next is newer than the gap, as a ring's slow subscriber went on from
//!   past what it lost. Its readers start over the same way: a page fetches again, a terminal is repainted, a
//!   log's viewer is told to reload.

use std::collections::VecDeque;
use std::future::Future as _;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use arc_swap::ArcSwap;
pub use async_broadcast::{RecvError, TryRecvError};
use hyper_rt::sync::{ChannelReceiver, Sender};

use super::Owner;
use super::queue::Weigh;

/// What the lanes' items hold, each counted once.
struct Space {
    owner: &'static Owner,
    held: AtomicUsize,
}

impl Space {
    fn take(&self, n: usize) {
        self.held.fetch_add(n, Ordering::AcqRel);
        self.owner.add(n);
    }

    /// Lets `n` bytes go: never below none.
    fn give(&self, n: usize) {
        let now = give_back(&self.held, n);
        self.owner.sub(n.min(now));
    }
}

impl Drop for Space {
    fn drop(&mut self) {
        self.owner.sub(*self.held.get_mut());
    }
}

/// `n` taken off `count`, never below none: what it was.
fn give_back(count: &AtomicUsize, n: usize) -> usize {
    let mut now = count.load(Ordering::Acquire);
    while let Err(seen) = count.compare_exchange_weak(now, now.saturating_sub(n), Ordering::AcqRel, Ordering::Acquire) {
        now = seen;
    }
    now
}

/// An item as the lanes carry it: one allocation its readers share, its number among the lanes' sends (what
/// a reader that lost some starts over from), its bytes counted until the last reader lets it go.
struct Kept<T> {
    item: T,
    seq: u64,
    bytes: usize,
    space: Arc<Space>,
}

impl<T> Drop for Kept<T> {
    fn drop(&mut self) {
        self.space.give(self.bytes);
    }
}

/// What a reader and the senders share of it: the bytes of what its lanes hold, how many items it lost
/// since it last said so, and the number of the newest of them (what it held from before that it lets go of).
struct Account {
    queued: AtomicUsize,
    lost: AtomicU64,
    lost_up_to: AtomicU64,
}

impl Account {
    /// Item `seq` let go of for this reader.
    fn lose(&self, seq: u64) {
        self.lost_up_to.fetch_max(seq.saturating_add(1), Ordering::AcqRel);
        self.lost.fetch_add(1, Ordering::AcqRel);
    }
}

/// A reader as the senders hold it: a sender into each of its lanes, and its account. Its reader keeps the
/// account alone, so the lanes' senders go with the list, and its lanes then end.
struct Inbox<T> {
    lanes: Box<[Sender<Arc<Kept<T>>>]>,
    account: Arc<Account>,
}

/// What the lanes and their readers share.
struct Shared<T> {
    space: Arc<Space>,
    /// The bytes a reader's lanes may hold.
    bytes: usize,
    lanes: usize,
    sent: AtomicU64,
    readers: ArcSwap<Vec<Arc<Inbox<T>>>>,
    /// Its readers' places, made as each subscribed.
    places: AtomicUsize,
}

/// Items fanned out to every reader, each on its sender's lane, with no lock on any path (the module's doc).
pub struct Lanes<T> {
    shared: Arc<Shared<T>>,
}

/// What one of a reader's places takes: a slot's stamp and what it carries (a pointer to the shared item).
pub const fn place<T>() -> usize {
    size_of::<usize>() + size_of::<Arc<Kept<T>>>()
}

impl<T: Clone + Weigh + Send + Sync + 'static> Lanes<T> {
    /// `lanes` lanes (one at the least), each of whose readers may hold `bytes` of its items past their places,
    /// counted as `owner`'s.
    pub fn new(owner: &'static Owner, lanes: usize, bytes: usize) -> Self {
        let space = Arc::new(Space { owner, held: AtomicUsize::new(0) });
        Self { shared: Arc::new(Shared { space, bytes, lanes: lanes.max(1), sent: AtomicU64::new(0), readers: ArcSwap::from_pointee(Vec::new()), places: AtomicUsize::new(0) }) }
    }

    /// Its lanes: what [`Lanes::send`]'s `lane` is taken modulo.
    pub fn lanes(&self) -> usize {
        self.shared.lanes
    }

    /// Readers now, read off their list (no lock).
    pub fn receiver_count(&self) -> usize {
        self.shared.readers.load().len()
    }

    /// What its items hold now past their places (each counted once), and its readers' places, in bytes.
    pub fn held(&self) -> usize {
        self.shared.space.held.load(Ordering::Acquire).saturating_add(self.shared.places.load(Ordering::Acquire))
    }

    /// The most one reader holds that asked for `per_lane` items a lane: its places, and the bytes its lanes
    /// may hold.
    pub fn bound_of(&self, per_lane: usize) -> usize {
        per_lane.max(1).saturating_mul(self.shared.lanes).saturating_mul(place::<T>()).saturating_add(self.shared.bytes)
    }

    /// Sends `item` on lane `lane` (modulo [`Lanes::lanes`]) to every reader now: none, and it's dropped. A
    /// reader with no room for it (its lane full, or its bytes) loses it, and starts over from after it.
    pub fn send(&self, lane: usize, item: T) {
        let readers = self.shared.readers.load();
        if readers.is_empty() {
            return;
        }
        let lane = lane % self.shared.lanes;
        let bytes = item.weight().saturating_sub(size_of::<T>());
        let seq = self.shared.sent.fetch_add(1, Ordering::AcqRel);
        self.shared.space.take(bytes);
        let kept = Arc::new(Kept { item, seq, bytes, space: Arc::clone(&self.shared.space) });
        for reader in readers.iter() {
            let Some(tx) = reader.lanes.get(lane) else { continue };
            let account = &reader.account;
            // Its bytes, unless it holds none: one larger than them all still goes alone.
            let had = account.queued.fetch_add(bytes, Ordering::AcqRel);
            if had > 0 && had.saturating_add(bytes) > self.shared.bytes {
                give_back(&account.queued, bytes);
                account.lose(seq);
                continue;
            }
            match tx.try_send(Arc::clone(&kept)) {
                Ok(()) => {}
                Err(refused) => {
                    give_back(&account.queued, bytes);
                    // Full: lost to it. Closed: its reader went, and takes itself off the list as it goes.
                    if matches!(refused, hyper_rt::sync::SyncError::Full(_)) {
                        account.lose(seq);
                    }
                }
            }
        }
    }

    /// A reader from now, each of its lanes holding up to `per_lane` items (one at the least): what's sent once
    /// it is in the readers' list. Refused when the runtime has no channel to give it (its cells all in use).
    pub fn subscribe(&self, per_lane: usize) -> Result<Reader<T>, hyper_rt::RtError> {
        let per_lane = per_lane.max(1);
        let (mut txs, mut rxs) = (Vec::with_capacity(self.shared.lanes), Vec::with_capacity(self.shared.lanes));
        for _ in 0..self.shared.lanes {
            // A place for one sender waiting for room: none ever waits (`try_send`).
            let (tx, rx) = hyper_rt::sync::channel_with(per_lane, 1)?;
            txs.push(tx);
            rxs.push(rx);
        }
        let account = Arc::new(Account { queued: AtomicUsize::new(0), lost: AtomicU64::new(0), lost_up_to: AtomicU64::new(0) });
        let inbox = Arc::new(Inbox { lanes: txs.into_boxed_slice(), account: Arc::clone(&account) });
        self.shared.readers.rcu(|now| {
            let mut next = Vec::with_capacity(now.len() + 1);
            next.extend(now.iter().cloned());
            next.push(Arc::clone(&inbox));
            next
        });
        // Its lanes' places, made now, and the bytes they may hold.
        let places = per_lane.saturating_mul(self.shared.lanes).saturating_mul(place::<T>());
        let owner = self.shared.space.owner;
        self.shared.places.fetch_add(places, Ordering::AcqRel);
        owner.add(places);
        owner.bound_add(places.saturating_add(self.shared.bytes));
        let pending = (0..self.shared.lanes).map(|_| VecDeque::new()).collect();
        Ok(Reader { shared: Arc::clone(&self.shared), account, lanes: rxs.into_boxed_slice(), pending, per_lane, next: 0, places, counted: true })
    }

    /// [`Lanes::subscribe`], or a reader whose lanes have already ended when the runtime has no channel to give
    /// it (said in the log): its reader ends as one whose sender went.
    pub fn subscribe_or_ended(&self, per_lane: usize) -> Reader<T> {
        self.subscribe(per_lane).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "a reader of kiln's fan-out couldn't have a channel: it ends at once");
            Reader { shared: Arc::clone(&self.shared), account: Arc::new(Account { queued: AtomicUsize::new(0), lost: AtomicU64::new(0), lost_up_to: AtomicU64::new(0) }), lanes: Box::new([]), pending: Box::new([]), per_lane: 0, next: 0, places: 0, counted: false }
        })
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
    account: Arc<Account>,
    lanes: Box<[ChannelReceiver<Arc<Kept<T>>>]>,
    /// For each lane, the items newer than a loss, taken off its lane as what came before them was let go of:
    /// what that lane gives first, in the order they came.
    pending: Box<[VecDeque<Arc<Kept<T>>>]>,
    /// The items each of its lanes holds at the most.
    per_lane: usize,
    /// The lane it takes from first next: the one after the last it took from, so a busy lane doesn't keep
    /// the others waiting.
    next: usize,
    places: usize,
    /// Whether it's in the list and counted (a reader refused a channel is neither).
    counted: bool,
}

/// What one look at a lane found.
enum Looked<T> {
    Item(Arc<Kept<T>>),
    Empty,
    Ended,
}

impl<T: Clone> Reader<T> {
    /// The next item, after a charge of the task's turn (`hyper_rt::coop`, as a ring's subscriber's receive):
    /// `RecvError::Overflowed` with how many it missed first, when it lost some; `Closed` once its lanes are gone
    /// and empty.
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        hyper_rt::coop::consume_budget().await;
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// The next item if one waits: `TryRecvError::Empty` when none does; `Overflowed` and `Closed` as [`Reader::recv`]'s.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(n) = self.start_over() {
            return Err(TryRecvError::Overflowed(n));
        }
        let n = self.lanes.len();
        let mut ended = 0;
        for i in 0..n {
            let k = (self.next + i) % n;
            match self.look(k, None) {
                Looked::Item(kept) => return Ok(self.took(k, &kept)),
                Looked::Empty => {}
                Looked::Ended => ended += 1,
            }
        }
        if let Some(n) = self.start_over() {
            return Err(TryRecvError::Overflowed(n));
        }
        if ended == n { Err(TryRecvError::Closed) } else { Err(TryRecvError::Empty) }
    }

    /// [`Reader::recv`]'s poll: its lanes in turn, each lane's own wait registering the task with it (and
    /// looking again after), so a send on any of them wakes it.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        if let Some(n) = self.start_over() {
            return Poll::Ready(Err(RecvError::Overflowed(n)));
        }
        let n = self.lanes.len();
        let mut ended = 0;
        for i in 0..n {
            let k = (self.next + i) % n;
            match self.look(k, Some(&mut *cx)) {
                Looked::Item(kept) => return Poll::Ready(Ok(self.took(k, &kept))),
                Looked::Empty => {}
                Looked::Ended => ended += 1,
            }
        }
        // A loss counted after its first look (the senders found no room): said now.
        if let Some(n) = self.start_over() {
            return Poll::Ready(Err(RecvError::Overflowed(n)));
        }
        if ended == n { Poll::Ready(Err(RecvError::Closed)) } else { Poll::Pending }
    }

    /// Lane `k`'s next item: the one it held back, else its channel's (waiting on it, with `cx`).
    fn look(&mut self, k: usize, cx: Option<&mut Context<'_>>) -> Looked<T> {
        if let Some(kept) = self.pending.get_mut(k).and_then(VecDeque::pop_front) {
            return Looked::Item(kept);
        }
        let Some(lane) = self.lanes.get_mut(k) else { return Looked::Ended };
        let taken = match cx {
            Some(cx) => match Pin::new(&mut lane.recv()).poll(cx) {
                Poll::Ready(taken) => taken.map(Some),
                Poll::Pending => Ok(None),
            },
            None => lane.try_recv(),
        };
        match taken {
            Ok(Some(kept)) => Looked::Item(kept),
            Ok(None) => Looked::Empty,
            Err(_) => Looked::Ended,
        }
    }

    /// `kept`, taken from lane `k`: its bytes off the reader's account, and the next lane looked at first next.
    fn took(&mut self, k: usize, kept: &Arc<Kept<T>>) -> T {
        give_back(&self.account.queued, kept.bytes);
        self.next = k + 1;
        kept.item.clone()
    }

    /// When it lost some since it last said so: what it holds from before its last loss let go of, what's newer
    /// held back for it in the order it came, and how many it missed in all.
    fn start_over(&mut self) -> Option<u64> {
        let lost = self.account.lost.swap(0, Ordering::AcqRel);
        if lost == 0 {
            return None;
        }
        let up_to = self.account.lost_up_to.load(Ordering::Acquire);
        let mut skipped = 0u64;
        for k in 0..self.lanes.len() {
            // What it held back before, and what its lane holds now (as many as a lane holds at the most).
            let mut held: VecDeque<Arc<Kept<T>>> = self.pending.get_mut(k).map(std::mem::take).unwrap_or_default();
            for _ in 0..self.per_lane {
                match self.lanes.get_mut(k).map(ChannelReceiver::try_recv) {
                    Some(Ok(Some(kept))) => held.push_back(kept),
                    _ => break,
                }
            }
            for kept in held {
                if kept.seq >= up_to {
                    if let Some(lane) = self.pending.get_mut(k) {
                        lane.push_back(kept);
                    }
                } else {
                    give_back(&self.account.queued, kept.bytes);
                    skipped += 1;
                }
            }
        }
        Some(lost.saturating_add(skipped))
    }
}

impl<T> Drop for Reader<T> {
    fn drop(&mut self) {
        if !self.counted {
            return;
        }
        // Off the list: its lanes' senders go with its entry (and its lanes, with it, refuse what's sent now).
        let account = Arc::clone(&self.account);
        self.shared.readers.rcu(|now| now.iter().filter(|r| !Arc::ptr_eq(&r.account, &account)).cloned().collect::<Vec<_>>());
        let owner = self.shared.space.owner;
        give_back(&self.shared.places, self.places);
        owner.sub(self.places);
        owner.bound_sub(self.places.saturating_add(self.shared.bytes));
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

    /// Goal 4, plan 01a1a59c2a9c step 5: a reader whose lane is full loses what comes, and starts over from
    /// after it: told how many it missed (those lost, and those it held from before them), it takes only
    /// what's newer; another reader with room has it all; a reader's bytes are its own bound, so one that stalls
    /// makes none of the others lose; and each item's bytes are counted once, given back as its last reader
    /// lets it go.
    #[test]
    fn g4_s5_a_reader_that_falls_behind_starts_over_and_holds_up_no_other() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_lanes_bounds");
        crate::rt::block_on(async {
            let fan = Lanes::<Item>::new(&OWNER, 1, 1 << 20);
            let (mut slow, mut roomy) = (fan.subscribe(2)?, fan.subscribe(16)?);
            for n in 1..=5 {
                fan.send(0, Item(0, n, 0));
            }
            anyhow::ensure!(matches!(slow.try_recv(), Err(TryRecvError::Overflowed(5))), "three lost and the two from before them let go of");
            anyhow::ensure!(matches!(slow.try_recv(), Err(TryRecvError::Empty)), "nothing from before the gap");
            fan.send(0, Item(0, 6, 0));
            anyhow::ensure!(slow.try_recv().ok() == Some(Item(0, 6, 0)), "it goes on from after the gap");
            for n in 1..=6 {
                anyhow::ensure!(roomy.try_recv().ok() == Some(Item(0, n, 0)), "the reader with room, {n}");
            }
            drop((slow, roomy));
            anyhow::ensure!(OWNER.bytes() == 0 && fan.receiver_count() == 0, "places and items let go: {}", OWNER.bytes());
            // Bytes: each reader's lanes may hold 600, items of 400 past their places. One reader stalls.
            let small = Lanes::<Item>::new(&OWNER, 1, 600);
            let (mut keeping, mut stalled) = (small.subscribe(8)?, small.subscribe(8)?);
            let places = OWNER.bytes();
            anyhow::ensure!(OWNER.bound().is_some_and(|b| b >= 2 * small.bound_of(8)), "each reader's places and bytes are the bound");
            for n in 1..=4 {
                small.send(0, Item(0, n, 400));
                anyhow::ensure!(keeping.try_recv().ok() == Some(Item(0, n, 400)), "the reader keeping up has {n}");
            }
            anyhow::ensure!(OWNER.bytes() == places + 400, "the stalled reader's one item, counted once: {}", OWNER.bytes() - places);
            anyhow::ensure!(matches!(stalled.try_recv(), Err(TryRecvError::Overflowed(4))), "three lost to the stalled reader, the one before them let go of");
            anyhow::ensure!(OWNER.bytes() == places, "given back as its last reader let it go");
            // Both go on: the stalled one from after its gap, and a reader waiting with nothing to take is woken
            // by what comes.
            small.send(0, Item(0, 5, 400));
            anyhow::ensure!(stalled.try_recv().ok() == Some(Item(0, 5, 400)) && keeping.try_recv().ok() == Some(Item(0, 5, 400)));
            let waiting = crate::rt::spawn(async move { keeping.recv().await });
            crate::rt::sleep(std::time::Duration::from_millis(20)).await;
            small.send(0, Item(0, 6, 400));
            let woken = crate::rt::timeout(std::time::Duration::from_secs(5), waiting).await.map_err(|_| anyhow::anyhow!("the waiting reader wasn't woken"))?.map_err(|_| anyhow::anyhow!("the task ended"))?;
            anyhow::ensure!(woken == Ok(Item(0, 6, 400)), "{woken:?}");
            drop(stalled);
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
