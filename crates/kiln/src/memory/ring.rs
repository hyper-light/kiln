//! Fan-out rings bounded in bytes (goal 11, plan 01a1a87b2e56 step 3): every subscriber gets every
//! message, and a slow one loses the oldest (it's told how many, `RecvError::Overflowed`) instead of
//! holding up the sender, as `crate::rt::Broadcast` does; what the ring holds for its slowest subscriber
//! is bounded by the bytes its messages take ([`Weigh`]) as well as by their count, so a ring of
//! event texts, log chunks or terminal output that vary a thousandfold holds no more than its bound
//! whichever it carries.
//!
//! - What's let go at the bound can be had again: each ring's subscribers start over from where it's
//!   kept (a page refetches what it shows, a log's viewer reads its file, a terminal's viewer is
//!   repainted from the session's backlog). Each use says which.
//! - Its places are async-broadcast's queue, made at its count as the ring is made: they're counted from
//!   then. Each message is counted by what it holds past its place, from when it's sent until the ring
//!   lets it go (its last subscriber took it, or it went to make room).
//! - Each ring's bytes and bound are its [`Owner`]'s on `/metrics` (`kiln_memory_bytes{owner}`,
//!   `kiln_memory_bound_bytes{owner}`): an owner of many rings (one a job, one a terminal) shows their
//!   sum.
//!
//! No lock of its own: the ring's count is an atomic, and the queue is async-broadcast's, as
//! `crate::rt::Broadcast`'s is.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use async_broadcast::RecvError;

use super::Owner;
use super::queue::Weigh;

/// What a ring holds against its bound: its places, and its messages' bytes past them.
struct Space {
    owner: &'static Owner,
    bound: usize,
    held: AtomicUsize,
}

impl Space {
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

    fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    fn over(&self) -> bool {
        self.held() > self.bound
    }
}

impl Drop for Space {
    fn drop(&mut self) {
        // Its places go with the last of its ends (the ring, its subscribers, its messages), and its
        // bound with them.
        self.owner.sub(*self.held.get_mut());
        self.owner.bound_sub(self.bound);
    }
}

/// A message's bytes, counted while the ring keeps it, given back as it lets it go.
struct Charge {
    space: Arc<Space>,
    bytes: usize,
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.space.give(self.bytes);
    }
}

/// A message as the ring's queue keeps it: its item, and, on the ring's own copy alone, its count. A
/// subscriber's copy is the item alone, so the count is given back once, as the queue lets the ring's
/// copy go: to its last subscriber, which takes it, or out to make room.
pub struct Kept<T> {
    item: T,
    _charge: Option<Charge>,
}

impl<T: Clone> Clone for Kept<T> {
    fn clone(&self) -> Self {
        Self { item: self.item.clone(), _charge: None }
    }
}

/// What one of a ring's places takes: async-broadcast keeps each message with its subscribers' count.
pub const fn place<T>() -> usize {
    size_of::<(Kept<T>, usize)>()
}

/// A fan-out ring of at most `places` messages and `bytes` of theirs past their places, counted as an
/// [`Owner`]'s.
pub struct Ring<T> {
    tx: async_broadcast::Sender<Kept<T>>,
    /// Keeps the channel open while no one subscribes.
    _keep_open: async_broadcast::InactiveReceiver<Kept<T>>,
    space: Arc<Space>,
    places: usize,
}

impl<T: Clone + Weigh> Ring<T> {
    /// A ring of `places` messages, and `bytes` of what they hold past their places, counted as
    /// `owner`'s: its bound is its places' bytes and `bytes`.
    pub fn new(owner: &'static Owner, places: usize, bytes: usize) -> Self {
        let places = places.max(1);
        let (mut tx, rx) = async_broadcast::broadcast(places);
        tx.set_overflow(true);
        tx.set_await_active(false);
        let made = places.saturating_mul(place::<T>());
        let bound = made.saturating_add(bytes);
        owner.bound_add(bound);
        let space = Arc::new(Space { owner, bound, held: AtomicUsize::new(0) });
        space.take(made);
        Self { tx, _keep_open: rx.deactivate(), space, places }
    }

    /// Delivers `item` to every subscriber now (none: it's dropped). Past the ring's count its oldest
    /// message goes, and past its bytes as many of its oldest as it takes to be within them, its newest
    /// always kept: whoever hadn't taken them is told how many it lost.
    pub fn send(&self, item: T) {
        let bytes = item.weight().saturating_sub(size_of::<T>());
        self.space.take(bytes);
        let kept = Kept { item, _charge: Some(Charge { space: Arc::clone(&self.space), bytes }) };
        // The oldest let go past its count comes back here, and goes (its bytes with it); so does the
        // message itself when no one subscribes.
        drop(self.tx.try_broadcast(kept));
        if self.space.over() {
            self.trim();
        }
    }

    /// Lets the oldest messages go until the ring is within its bytes, the newest kept however large.
    /// async-broadcast lets a channel's oldest go as its capacity is set below what it holds: so it's set
    /// one below, as often as it takes, and back to its count (on a handle of its own: setting it takes
    /// one). A send meanwhile may find it at the lower count and let one more of the oldest go.
    fn trim(&self) {
        let mut tx = self.tx.clone();
        while self.space.over() {
            let n = tx.len();
            if n <= 1 {
                break;
            }
            tx.set_capacity(n - 1);
        }
        tx.set_capacity(self.places);
    }
}

impl<T> Ring<T> {
    /// A subscriber from now: each message sent after this.
    pub fn subscribe(&self) -> Subscription<T> {
        Subscription { rx: self.tx.new_receiver(), _space: Arc::clone(&self.space) }
    }

    /// Subscribers now.
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// What it holds now, in bytes: its places and its messages past them.
    pub fn held(&self) -> usize {
        self.space.held()
    }

    /// The most it holds, in bytes: its places and its messages past them.
    pub fn bound(&self) -> usize {
        self.space.bound
    }
}

/// A subscriber of a [`Ring`]: its receive charges the task's turn on its shard (`hyper_rt::coop`, kiln's
/// goal 6), as `crate::rt::Subscription`'s does, so a subscriber whose ring is always full yields at its
/// share.
pub struct Subscription<T> {
    rx: async_broadcast::Receiver<Kept<T>>,
    /// The ring's places are let go with the last of its ends: its subscribers among them.
    _space: Arc<Space>,
}

impl<T: Clone> Subscription<T> {
    /// The next message, after a charge of the task's turn: `RecvError::Overflowed` when the ring let some
    /// go before this subscriber took them, `Closed` once the ring is gone and nothing waits.
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        hyper_rt::coop::consume_budget().await;
        self.rx.recv().await.map(|k| k.item)
    }

    /// The next message if one waits: `TryRecvError::Empty` when none does, `Overflowed` and `Closed` as
    /// [`Subscription::recv`]'s.
    pub fn try_recv(&mut self) -> Result<T, async_broadcast::TryRecvError> {
        self.rx.try_recv().map(|k| k.item)
    }
}

/// As a stream (what `select`s and `map`s over subscriptions take), each item a charge of the task's turn
/// too; what was let go is skipped, as async-broadcast's stream skips it.
impl<T: Clone> futures::Stream for Subscription<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if hyper_rt::coop::poll_proceed(cx).is_pending() {
            return Poll::Pending;
        }
        futures::StreamExt::poll_next_unpin(&mut self.rx, cx).map(|o| o.map(|k| k.item))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's message, holding `0` bytes past its place.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Item(usize, u32);

    impl Weigh for Item {
        fn weight(&self) -> usize {
            size_of::<Self>() + self.0
        }
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a ring counts its messages' bytes while it keeps them; past
    /// its bytes it lets its oldest go (a subscriber that hadn't taken them is told how many) and keeps
    /// its newest; what's taken is let go.
    #[test]
    fn g11_s3_a_ring_lets_its_oldest_go_past_its_bytes() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_ring_bytes");
        crate::rt::block_on(async {
            let ring = Ring::<Item>::new(&OWNER, 100, 1000);
            let places = 100 * place::<Item>();
            anyhow::ensure!(OWNER.bound() == Some(places + 1000) && OWNER.bytes() == places, "{:?} {}", OWNER.bound(), OWNER.bytes());
            let mut slow = ring.subscribe();
            for n in 1..=5 {
                ring.send(Item(300, n));
                anyhow::ensure!(ring.held() <= ring.bound(), "{} past {} after {n}", ring.held(), ring.bound());
            }
            anyhow::ensure!(ring.held() == places + 900, "the newest three kept: {}", ring.held() - places);
            anyhow::ensure!(matches!(slow.recv().await, Err(RecvError::Overflowed(2))), "told it lost the oldest two");
            for n in 3..=5 {
                anyhow::ensure!(slow.recv().await? == Item(300, n), "then the rest, in order");
            }
            anyhow::ensure!(OWNER.bytes() == places, "taken, let go: {}", OWNER.bytes() - places);
            // The newest stays whatever it holds.
            ring.send(Item(5000, 6));
            anyhow::ensure!(ring.held() == places + 5000);
            anyhow::ensure!(slow.recv().await? == Item(5000, 6));
            drop((ring, slow));
            anyhow::ensure!(OWNER.bytes() == 0 && OWNER.bound().is_none(), "its places and bound go with it");
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: with no one subscribed nothing is kept; a message is kept until
    /// its last subscriber takes it; a subscriber that goes lets go of what it hadn't taken.
    #[test]
    fn g11_s3_a_ring_keeps_a_message_until_its_last_subscriber_takes_it() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_ring_last");
        crate::rt::block_on(async {
            let ring = Ring::<Item>::new(&OWNER, 8, 10_000);
            let places = 8 * place::<Item>();
            ring.send(Item(400, 1));
            anyhow::ensure!(OWNER.bytes() == places, "no one subscribed: nothing kept");
            let (mut a, mut b) = (ring.subscribe(), ring.subscribe());
            ring.send(Item(400, 2));
            anyhow::ensure!(a.recv().await? == Item(400, 2));
            anyhow::ensure!(OWNER.bytes() == places + 400, "kept for the other");
            anyhow::ensure!(b.recv().await? == Item(400, 2));
            anyhow::ensure!(OWNER.bytes() == places, "taken by both");
            ring.send(Item(700, 3));
            drop(b);
            anyhow::ensure!(OWNER.bytes() == places + 700, "kept for the one left");
            drop(a);
            anyhow::ensure!(OWNER.bytes() == places, "no one left to take it");
            anyhow::ensure!(ring.receiver_count() == 0);
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: past its count a ring lets its oldest go as before, its bytes
    /// with it, and it's as many places as it was made with after trimming by bytes.
    #[test]
    fn g11_s3_a_ring_keeps_its_count_too() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_ring_count");
        crate::rt::block_on(async {
            let ring = Ring::<Item>::new(&OWNER, 4, 1000);
            let places = 4 * place::<Item>();
            let mut slow = ring.subscribe();
            // Bytes first: two of 600 can't both be kept.
            ring.send(Item(600, 1));
            ring.send(Item(600, 2));
            anyhow::ensure!(ring.held() == places + 600);
            // Then the count: six small ones, four kept.
            for n in 3..=8 {
                ring.send(Item(10, n));
            }
            anyhow::ensure!(ring.held() == places + 40, "{}", ring.held() - places);
            anyhow::ensure!(matches!(slow.recv().await, Err(RecvError::Overflowed(4))));
            for n in 5..=8 {
                anyhow::ensure!(slow.recv().await? == Item(10, n));
            }
            Ok(())
        })
    }
}
