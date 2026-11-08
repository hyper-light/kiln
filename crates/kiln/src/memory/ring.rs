//! Fan-out rings bounded in bytes (goal 11, plan 01a1a87b2e56 step 3), with no lock on any path (goal 4, plan
//! 01a1a59c2a9c step 5; goal 5's rule): every subscriber gets every message, and one that falls behind loses
//! what it has no room for (it's told how many, `RecvError::Overflowed`) instead of holding up the sender or
//! the other subscribers. What each subscriber holds is bounded by the bytes its messages take ([`Weigh`]) as
//! well as by their count, so a ring of terminal output or log chunks that vary a thousandfold holds no more
//! than its bound whichever it carries.
//!
//! - A ring is one lane of [`super::lanes`]: each subscriber its own channel (`hyper_rt::sync::channel`), its
//!   places made as it subscribes, the senders' list of subscribers a copy swapped in whole. async-broadcast's
//!   one channel, which rings were, took its `Mutex` for every send and every subscriber's receive: a
//!   terminal's output from its relay and every viewer's read of it, a job's log chunks from its writer and
//!   every viewer's, across their shards.
//! - What's let go of at the bound can be had again: a subscriber that fell behind starts over from after
//!   what it missed (`super::lanes`), as each use says: a log's viewer reads its file, a terminal's viewer is
//!   repainted from the session's backlog.
//! - Each ring's bytes and its subscribers' bounds are its [`Owner`]'s on `/metrics` (`kiln_memory_bytes{owner}`,
//!   `kiln_memory_bound_bytes{owner}`): an owner of many rings (one a job, one a terminal) shows their sum.

use std::pin::Pin;
use std::task::{Context, Poll};

pub use async_broadcast::{RecvError, TryRecvError};

use super::Owner;
use super::lanes::{Lanes, Reader};
use super::queue::Weigh;

/// What one of a subscriber's places takes: a slot's stamp, and a pointer to the message it shares.
pub const fn place<T>() -> usize {
    super::lanes::place::<T>()
}

/// A fan-out ring whose every subscriber holds at most `places` messages and `bytes` of theirs past their
/// places, counted as an [`Owner`]'s.
pub struct Ring<T> {
    lanes: Lanes<T>,
    places: usize,
}

impl<T: Clone + Weigh + Send + Sync + 'static> Ring<T> {
    /// A ring each of whose subscribers holds `places` messages (one at the least) and `bytes` of what they
    /// hold past their places, counted as `owner`'s: a subscriber's bound is its places' bytes and `bytes`.
    pub fn new(owner: &'static Owner, places: usize, bytes: usize) -> Self {
        Self { lanes: Lanes::new(owner, 1, bytes), places: places.max(1) }
    }

    /// Delivers `item` to every subscriber now (none: it's dropped). A subscriber with no room for it loses it,
    /// and starts over from after it.
    pub fn send(&self, item: T) {
        self.lanes.send(0, item);
    }

    /// A subscriber from now: each message sent after this. When the runtime has no channel to give it (its
    /// cells all in use, said in the log), one that has ended: its reader goes as one whose ring went.
    pub fn subscribe(&self) -> Subscription<T> {
        Subscription { reader: self.lanes.subscribe_or_ended(self.places) }
    }

    /// Subscribers now.
    pub fn receiver_count(&self) -> usize {
        self.lanes.receiver_count()
    }

    /// What it holds now, in bytes: its messages past their places (each counted once) and its subscribers'
    /// places.
    pub fn held(&self) -> usize {
        self.lanes.held()
    }

    /// The most one subscriber holds, in bytes: its places and its messages past them.
    pub fn bound(&self) -> usize {
        self.lanes.bound_of(self.places)
    }
}

/// A subscriber of a [`Ring`]: its receive charges the task's turn on its shard as it takes (`hyper_rt::coop`,
/// kiln's goal 6), so a subscriber whose ring is always full yields at its share.
pub struct Subscription<T> {
    reader: Reader<T>,
}

impl<T: Clone> Subscription<T> {
    /// The next message, a charge of the task's turn as it's taken: `RecvError::Overflowed` when it missed some (it
    /// goes on from after them), `Closed` once the ring is gone and nothing waits.
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        self.reader.recv().await
    }

    /// The next message if one waits: `TryRecvError::Empty` when none does, `Overflowed` and `Closed` as
    /// [`Subscription::recv`]'s.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.reader.try_recv()
    }
}

/// As a stream (what `select`s and `map`s over subscriptions take), each item charged as it's taken too, and a wait
/// not at all; what was missed is passed over, as async-broadcast's stream passed it over.
impl<T: Clone> futures::Stream for Subscription<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        loop {
            match self.reader.poll_recv(cx) {
                Poll::Ready(Ok(item)) => return Poll::Ready(Some(item)),
                Poll::Ready(Err(RecvError::Overflowed(_))) => {}
                Poll::Ready(Err(RecvError::Closed)) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's message, holding `.0` bytes past its place.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Item(usize, u32);

    impl Weigh for Item {
        fn weight(&self) -> usize {
            size_of::<Self>() + self.0
        }
    }

    /// Goal 11, plan 01a1a87b2e56 step 3 (goal 4 step 5's lanes): a ring counts its messages' bytes while a
    /// subscriber keeps them, each once; past a subscriber's bytes it misses what comes, is told how many (those
    /// it held from before let go of with them), and goes on from after them; and its places and bound go with
    /// it.
    #[test]
    fn g11_s3_a_subscriber_past_its_bytes_starts_over_from_after_what_it_missed() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_ring_bytes");
        crate::rt::block_on(async {
            let ring = Ring::<Item>::new(&OWNER, 100, 1000);
            let places = 100 * place::<Item>();
            anyhow::ensure!(OWNER.bound().is_none() && OWNER.bytes() == 0, "no subscriber, nothing made: {:?} {}", OWNER.bound(), OWNER.bytes());
            let mut slow = ring.subscribe();
            anyhow::ensure!(OWNER.bound() == Some(places + 1000) && OWNER.bytes() == places && ring.bound() == places + 1000, "{:?} {}", OWNER.bound(), OWNER.bytes());
            for n in 1..=5 {
                ring.send(Item(300, n));
                anyhow::ensure!(ring.held() <= ring.bound(), "{} past {} after {n}", ring.held(), ring.bound());
            }
            anyhow::ensure!(ring.held() == places + 900, "the first three kept: {}", ring.held() - places);
            anyhow::ensure!(matches!(slow.recv().await, Err(RecvError::Overflowed(5))), "told it missed them all: two lost, three from before them");
            anyhow::ensure!(OWNER.bytes() == places, "let go of: {}", OWNER.bytes() - places);
            // It goes on from after them; the newest goes whatever it holds into a subscriber holding none.
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
            anyhow::ensure!(OWNER.bytes() == 0, "no one subscribed: nothing kept");
            let (mut a, mut b) = (ring.subscribe(), ring.subscribe());
            ring.send(Item(400, 2));
            anyhow::ensure!(a.recv().await? == Item(400, 2));
            anyhow::ensure!(OWNER.bytes() == 2 * places + 400, "kept for the other");
            anyhow::ensure!(b.recv().await? == Item(400, 2));
            anyhow::ensure!(OWNER.bytes() == 2 * places, "taken by both");
            ring.send(Item(700, 3));
            drop(b);
            anyhow::ensure!(OWNER.bytes() == places + 700, "kept for the one left");
            drop(a);
            anyhow::ensure!(OWNER.bytes() == 0, "no one left to take it");
            anyhow::ensure!(ring.receiver_count() == 0);
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5 (goal 5's rule), run by kiln's CI with every test: a ring takes no lock,
    /// nor uses a channel that takes one (async-broadcast's, which rings were).
    #[test]
    fn g4_s5_the_ring_takes_no_lock() {
        let src = include_str!("ring.rs").replace("\r\n", "\n");
        let code = src.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default();
        let code: String = code.lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
        for lock in ["Mutex", "RwLock", "Condvar", "parking_lot", "broadcast(", "async_channel", "event_listener"] {
            assert!(!code.contains(lock), "memory/ring.rs takes a lock, or a channel that does: {lock}");
        }
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a ring's subscribers are counted as they come and go, read off their
    /// list (no lock).
    #[test]
    fn g4_s5_a_rings_subscribers_are_counted_without_its_lock() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_ring_subscribers");
        crate::rt::block_on(async {
            let ring = Ring::<Item>::new(&OWNER, 4, 1 << 10);
            anyhow::ensure!(ring.receiver_count() == 0);
            let (a, b) = (ring.subscribe(), ring.subscribe());
            anyhow::ensure!(ring.receiver_count() == 2);
            drop(a);
            anyhow::ensure!(ring.receiver_count() == 1);
            let c = ring.subscribe();
            drop(b);
            anyhow::ensure!(ring.receiver_count() == 1);
            drop(c);
            anyhow::ensure!(ring.receiver_count() == 0);
            Ok(())
        })
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: past its count a subscriber misses what comes as well, and goes on from
    /// after it; one keeping up has every message, in order.
    #[test]
    fn g11_s3_a_subscriber_keeps_its_count_too() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_ring_count");
        crate::rt::block_on(async {
            let ring = Ring::<Item>::new(&OWNER, 4, 1000);
            let (mut slow, mut keeping) = (ring.subscribe(), ring.subscribe());
            for n in 1..=8 {
                ring.send(Item(10, n));
                anyhow::ensure!(keeping.recv().await? == Item(10, n), "the one keeping up, {n}");
            }
            anyhow::ensure!(matches!(slow.recv().await, Err(RecvError::Overflowed(8))), "four lost past its count, four from before them");
            ring.send(Item(10, 9));
            anyhow::ensure!(slow.recv().await? == Item(10, 9) && keeping.recv().await? == Item(10, 9));
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: as a stream, a subscriber passes over what it missed and ends with the
    /// ring.
    #[test]
    fn g4_s5_a_subscription_as_a_stream_passes_over_what_it_missed() -> anyhow::Result<()> {
        use futures::StreamExt as _;
        static OWNER: Owner = Owner::new("test_ring_stream");
        crate::rt::block_on(async {
            let ring = Ring::<Item>::new(&OWNER, 2, 1000);
            let mut s = ring.subscribe();
            for n in 1..=4 {
                ring.send(Item(0, n));
            }
            ring.send(Item(0, 5));
            drop(ring);
            let got: Vec<u32> = s.by_ref().map(|i| i.1).collect().await;
            anyhow::ensure!(got.is_empty() || got == [5], "what it missed passed over: {got:?}");
            Ok(())
        })
    }
}
