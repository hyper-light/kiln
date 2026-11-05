//! Queues bounded in bytes (goal 11, plan 01a1a87b2e56 step 3): what a queue holds is counted by what its
//! items take ([`Weigh`]), never by how many there are, so a queue of frames, updates or jobs whose sizes
//! vary a thousandfold holds no more than its bound whichever it carries.
//!
//! - Made as it fills: the places are an unbounded channel's, made in blocks as items come and let go as
//!   they leave. A bounded async-channel makes every place as it's made (concurrent-queue writes each
//!   one's stamp), so a queue of 16,384 places held its whole size from the start, empty or not (research
//!   01a1a87b2e57, f10 and f11: 12 MB of an idle server's heap).
//! - At its bound a sender waits for room ([`Sender::send`], [`Sender::send_blocking`]), or is told it's
//!   full ([`Sender::try_send`]) where it has something better to do than wait: nothing is dropped here.
//!   One item larger than the whole bound still goes, alone, into an empty queue: a bound never refuses
//!   what fits nowhere else.
//! - Each queue's bytes and bound are its [`Owner`]'s on `/metrics` (`kiln_memory_bytes{owner}`,
//!   `kiln_memory_bound_bytes{owner}`): an owner of many queues (one a shard, one a viewer) shows their
//!   sum.
//!
//! Lockless: an atomic count of what's held, and an event its receivers tell as they take, as the
//! channel itself is.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use async_channel::{RecvError, SendError, TryRecvError, TrySendError};
use event_listener::{Event, Listener as _};

use super::Owner;

/// What a queued item takes of memory, in bytes: its place, and what it holds on the heap as near as can
/// be said without walking it all (each implementation says what it counts).
pub trait Weigh {
    fn weight(&self) -> usize;
}

impl Weigh for Vec<u8> {
    fn weight(&self) -> usize {
        size_of::<Self>().saturating_add(self.capacity())
    }
}

impl Weigh for bytes::Bytes {
    fn weight(&self) -> usize {
        size_of::<Self>().saturating_add(self.len())
    }
}

impl Weigh for String {
    fn weight(&self) -> usize {
        size_of::<Self>().saturating_add(self.capacity())
    }
}

/// What a value's JSON would take, counted as it's written and none of it kept: a weight for structured
/// items whose texts dominate what they hold (a runner's job event, a pool's word), near enough without a
/// walk of its own for each type.
pub fn json_bytes<T: serde::Serialize + ?Sized>(v: &T) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(b.len());
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut n = Count(0);
    let _ = serde_json::to_writer(&mut n, v);
    n.0
}

/// A JSON value as serde_json keeps it, walked (a Kubernetes object, a render's output): its place, and
/// what it holds past it ([`json_heap`]). Several times its text's bytes: each value has a place of its
/// own, and each of an object's keys its entry.
impl Weigh for serde_json::Value {
    fn weight(&self) -> usize {
        size_of::<Self>().saturating_add(json_heap(self))
    }
}

/// One of a JSON object's entries beside its key's bytes and what its value holds: its key's and its
/// value's places, and the map's own two words for it (an ordered map's hash and index, a B-tree's share
/// of its node), whichever map serde_json was built with.
const JSON_ENTRY: usize = size_of::<String>() + size_of::<serde_json::Value>() + 2 * size_of::<usize>();

/// What a JSON value holds past its own place (goal 11, plan 01a1a87b2e56 step 3): its text's bytes, an
/// array's places and what each of its values holds, an object's entries ([`JSON_ENTRY`]) with their keys'
/// bytes and what each value holds. A walk of it, cheaper than the parse that made it; as deep as
/// serde_json parses (128 levels).
pub fn json_heap(v: &serde_json::Value) -> usize {
    use serde_json::Value;
    match v {
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
        Value::String(s) => s.capacity(),
        Value::Array(a) => json_values_heap(a),
        Value::Object(m) => m.iter().map(|(k, v)| JSON_ENTRY.saturating_add(k.capacity()).saturating_add(json_heap(v))).fold(0, usize::saturating_add),
    }
}

/// What a list of JSON values holds past its own place: its places, and what each value holds.
pub fn json_values_heap(a: &Vec<serde_json::Value>) -> usize {
    a.capacity().saturating_mul(size_of::<serde_json::Value>()).saturating_add(a.iter().map(json_heap).fold(0, usize::saturating_add))
}

/// A shared text: its place, its two counts and its bytes, as one holder's (whoever holds it last lets it
/// go).
impl Weigh for std::sync::Arc<str> {
    fn weight(&self) -> usize {
        size_of::<Self>().saturating_add(2 * size_of::<usize>()).saturating_add(self.len())
    }
}

impl<T: Weigh + ?Sized> Weigh for Box<T> {
    fn weight(&self) -> usize {
        size_of::<Self>().saturating_add((**self).weight())
    }
}

impl<A: Weigh, B: Weigh> Weigh for (A, B) {
    fn weight(&self) -> usize {
        self.0.weight().saturating_add(self.1.weight())
    }
}

/// A queue's room: the bytes it holds, the most it may, and a wait for room. Counted into its owner's
/// figures as it changes; its bound joins the owner's as it's made and leaves it as it goes.
pub struct Room {
    owner: &'static Owner,
    bound: usize,
    held: AtomicUsize,
    /// No receiver any more: whoever waits for room stops waiting (what it sends is refused).
    closed: AtomicBool,
    /// Told as bytes are let go, and as the queue closes.
    freed: Event,
}

impl Room {
    /// A room of `bound` bytes, counted as `owner`'s.
    pub fn new(owner: &'static Owner, bound: usize) -> Self {
        owner.bound_add(bound);
        Self { owner, bound, held: AtomicUsize::new(0), closed: AtomicBool::new(false), freed: Event::new() }
    }

    /// The most it may hold, in bytes.
    pub fn bound(&self) -> usize {
        self.bound
    }

    /// What it holds now, in bytes.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// Takes `n` bytes if they fit: within the bound, or into a room holding nothing (an item larger than
    /// the bound goes alone). False when they don't.
    pub fn try_take(&self, n: usize) -> bool {
        let mut now = self.held.load(Ordering::Acquire);
        loop {
            let after = now.saturating_add(n);
            if now > 0 && after > self.bound {
                return false;
            }
            match self.held.compare_exchange_weak(now, after, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    self.owner.add(n);
                    return true;
                }
                Err(seen) => now = seen,
            }
        }
    }

    /// Takes `n` bytes, waiting (a task's wait: its thread goes on with others) until they fit; false when
    /// the queue closed first.
    pub async fn take(&self, n: usize) -> bool {
        loop {
            if self.is_closed() {
                return false;
            }
            if self.try_take(n) {
                return true;
            }
            let freed = self.freed.listen();
            if self.is_closed() {
                return false;
            }
            if self.try_take(n) {
                return true;
            }
            freed.await;
        }
    }

    /// [`Room::take`], blocking its thread: for a thread of its own, never the runtime's. Waits until
    /// `until` at most (none: as long as it takes); false when it closed or the time ran out first.
    pub fn take_blocking(&self, n: usize, until: Option<Instant>) -> bool {
        loop {
            if self.is_closed() {
                return false;
            }
            if self.try_take(n) {
                return true;
            }
            let freed = self.freed.listen();
            if self.is_closed() {
                return false;
            }
            if self.try_take(n) {
                return true;
            }
            match until {
                Some(t) => {
                    if freed.wait_deadline(t).is_none() && Instant::now() >= t {
                        return false;
                    }
                }
                None => freed.wait(),
            }
        }
    }

    /// Takes `n` bytes whatever it holds: for a sender that must never wait (each use says why).
    pub fn force(&self, n: usize) {
        self.held.fetch_add(n, Ordering::AcqRel);
        self.owner.add(n);
    }

    /// Lets `n` bytes go: never below none. Whoever waits for room looks again.
    pub fn give(&self, n: usize) {
        let mut now = self.held.load(Ordering::Acquire);
        while let Err(seen) = self.held.compare_exchange_weak(now, now.saturating_sub(n), Ordering::AcqRel, Ordering::Acquire) {
            now = seen;
        }
        self.owner.sub(n.min(now));
        self.freed.notify(usize::MAX);
    }

    /// No one takes from it any more: whoever waits for room stops.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.freed.notify(usize::MAX);
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        // What it still held goes with it (its items are dropped with their channel), and so does its bound.
        self.owner.sub(*self.held.get_mut());
        self.owner.bound_sub(self.bound);
    }
}

/// What a place in the queue takes beside its item: the channel's word of state for the place, and the
/// count kept beside the item ([`Weighed`]). Counted with each item, so a queue of many small items is
/// bounded too.
pub const PLACE: usize = 2 * size_of::<usize>();

/// What `item` is counted at: its weight and its place.
fn counted<T: Weigh>(item: &T) -> usize {
    item.weight().saturating_add(PLACE)
}

/// An item and the bytes it was counted at, so the receiver lets go of exactly what the sender took.
struct Weighed<T> {
    bytes: usize,
    item: T,
}

/// A queue of `bound` bytes, counted as `owner`'s. Its places are made as it fills.
pub fn bounded<T: Weigh>(owner: &'static Owner, bound: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = async_channel::unbounded();
    let room = Arc::new(Room::new(owner, bound));
    (Sender { tx, room: Arc::clone(&room) }, Receiver { inner: Arc::new(Taking { rx, room }) })
}

/// A queue's sending end: cheap to clone, usable from any thread.
pub struct Sender<T> {
    tx: async_channel::Sender<Weighed<T>>,
    room: Arc<Room>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), room: Arc::clone(&self.room) }
    }
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender").field("held", &self.room.held()).field("bound", &self.room.bound).finish()
    }
}

impl<T: Weigh> Sender<T> {
    /// Queues `item`, waiting first while the queue has no room for it: an error, with the item, once no
    /// one takes from the queue any more.
    pub async fn send(&self, item: T) -> Result<(), SendError<T>> {
        let bytes = counted(&item);
        if self.tx.is_closed() || !self.room.take(bytes).await {
            return Err(SendError(item));
        }
        self.put(bytes, item).map_err(SendError)
    }

    /// Queues `item` if it has room now: `Full` (with the item) when it hasn't, `Closed` when no one takes
    /// from it any more.
    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        let bytes = counted(&item);
        if self.tx.is_closed() {
            return Err(TrySendError::Closed(item));
        }
        if !self.room.try_take(bytes) {
            return Err(TrySendError::Full(item));
        }
        self.put(bytes, item).map_err(TrySendError::Closed)
    }

    /// [`Sender::send`], blocking its thread: for a thread of its own, never the runtime's.
    pub fn send_blocking(&self, item: T) -> Result<(), SendError<T>> {
        let bytes = counted(&item);
        if self.tx.is_closed() || !self.room.take_blocking(bytes, None) {
            return Err(SendError(item));
        }
        self.put(bytes, item).map_err(SendError)
    }

    /// Queues `item` whatever the queue holds: for a sender that must never wait, its item counted all the
    /// same (each use says why it can't wait).
    pub fn force_send(&self, item: T) -> Result<(), SendError<T>> {
        let bytes = counted(&item);
        if self.tx.is_closed() {
            return Err(SendError(item));
        }
        self.room.force(bytes);
        self.put(bytes, item).map_err(SendError)
    }

    /// Into the channel, its bytes already taken: given back if the channel closed meanwhile.
    fn put(&self, bytes: usize, item: T) -> Result<(), T> {
        self.tx.try_send(Weighed { bytes, item }).map_err(|e| {
            self.room.give(bytes);
            e.into_inner().item
        })
    }
}

impl<T> Sender<T> {
    /// Whether no one takes from the queue any more.
    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// Closes the queue: what waits in it can still be taken, nothing more goes in, and whoever waits for
    /// room stops waiting.
    pub fn close(&self) -> bool {
        let closed = self.tx.close();
        self.room.close();
        closed
    }

    /// Items waiting now.
    pub fn len(&self) -> usize {
        self.tx.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tx.is_empty()
    }

    /// Bytes waiting now.
    pub fn held(&self) -> usize {
        self.room.held()
    }

    /// The most the queue holds, in bytes.
    pub fn bound(&self) -> usize {
        self.room.bound
    }

    /// Whether `other` sends into the same queue.
    pub fn same_channel(&self, other: &Self) -> bool {
        self.tx.same_channel(&other.tx)
    }

    /// A handle that doesn't keep the queue open ([`WeakSender::upgrade`]).
    pub fn downgrade(&self) -> WeakSender<T> {
        WeakSender { tx: self.tx.downgrade(), room: Arc::downgrade(&self.room) }
    }
}

/// A sending end that doesn't keep the queue open: upgraded while a [`Sender`] still is.
pub struct WeakSender<T> {
    tx: async_channel::WeakSender<Weighed<T>>,
    room: Weak<Room>,
}

impl<T> Clone for WeakSender<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), room: Weak::clone(&self.room) }
    }
}

impl<T> std::fmt::Debug for WeakSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeakSender")
    }
}

impl<T> WeakSender<T> {
    pub fn upgrade(&self) -> Option<Sender<T>> {
        Some(Sender { tx: self.tx.upgrade()?, room: self.room.upgrade()? })
    }
}

/// A queue's receiving end: what it takes lets its bytes go, and wakes whoever waits for room. Cheap to
/// clone; the last of its clones to go closes the queue.
pub struct Receiver<T> {
    inner: Arc<Taking<T>>,
}

/// The receiving end itself, one for all of a queue's [`Receiver`]s: dropped with the last, it closes the
/// queue, so a sender waiting for room it would never get stops waiting.
struct Taking<T> {
    rx: async_channel::Receiver<Weighed<T>>,
    room: Arc<Room>,
}

impl<T> Drop for Taking<T> {
    fn drop(&mut self) {
        self.rx.close();
        self.room.close();
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver").field("held", &self.inner.room.held()).field("bound", &self.inner.room.bound).finish()
    }
}

impl<T> Receiver<T> {
    /// The next item, waiting for one: an error once the queue is empty and no one sends to it any more.
    /// Cancel-safe: an item is taken and its bytes let go in the same poll.
    pub async fn recv(&self) -> Result<T, RecvError> {
        let w = self.inner.rx.recv().await?;
        Ok(self.taken(w))
    }

    /// The next item if one waits.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.inner.rx.try_recv().map(|w| self.taken(w))
    }

    /// [`Receiver::recv`], blocking its thread: for a thread of its own, never the runtime's.
    pub fn recv_blocking(&self) -> Result<T, RecvError> {
        self.inner.rx.recv_blocking().map(|w| self.taken(w))
    }

    fn taken(&self, w: Weighed<T>) -> T {
        self.inner.room.give(w.bytes);
        w.item
    }

    /// Items waiting now.
    pub fn len(&self) -> usize {
        self.inner.rx.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.rx.is_empty()
    }

    /// Bytes waiting now.
    pub fn held(&self) -> usize {
        self.inner.room.held()
    }

    /// The most the queue holds, in bytes.
    pub fn bound(&self) -> usize {
        self.inner.room.bound
    }

    /// Whether the queue is closed (no sender, or [`Receiver::close`]).
    pub fn is_closed(&self) -> bool {
        self.inner.rx.is_closed()
    }

    /// Closes the queue: what waits in it can still be taken; whoever waits for room stops waiting.
    pub fn close(&self) -> bool {
        let closed = self.inner.rx.close();
        self.inner.room.close();
        closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test's item, counted (its place with it) at `0` bytes: at least [`PLACE`].
    struct Item(usize);

    impl Weigh for Item {
        fn weight(&self) -> usize {
            self.0.saturating_sub(PLACE)
        }
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a queue counts bytes, not items; past its bound a sender is told
    /// it's full (or waits), and taking lets the bytes go, into its owner's figures too.
    #[test]
    fn g11_s3_a_queue_is_bounded_in_bytes() {
        static OWNER: Owner = Owner::new("test_queue_bytes");
        let (tx, rx) = bounded::<Item>(&OWNER, 1000);
        assert_eq!(OWNER.bound(), Some(1000));
        assert!(tx.try_send(Item(600)).is_ok());
        assert!(tx.try_send(Item(400)).is_ok(), "exactly the bound fits");
        assert!(matches!(tx.try_send(Item(PLACE)), Err(TrySendError::Full(_))), "an item past it doesn't");
        assert_eq!((tx.held(), OWNER.bytes(), tx.len()), (1000, 1000, 2));
        assert!(rx.try_recv().is_ok_and(|i| i.0 == 600));
        assert_eq!((rx.held(), OWNER.bytes()), (400, 400));
        // Many small items fit where few large ones did: bytes, not a count (each its place at least).
        let small = 600 / PLACE;
        for _ in 0..small {
            assert!(tx.try_send(Item(PLACE)).is_ok());
        }
        assert!(tx.try_send(Item(PLACE)).is_err());
        assert_eq!(tx.len(), 1 + small);
        drop((tx, rx));
        assert_eq!((OWNER.bytes(), OWNER.bound()), (0, None), "a queue's bytes and bound go with it");
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: an item larger than the whole bound still goes, alone, into an
    /// empty queue (nothing is refused for good), and nothing goes after it until it's taken.
    #[test]
    fn g11_s3_an_item_past_the_bound_goes_alone() {
        static OWNER: Owner = Owner::new("test_queue_alone");
        let (tx, rx) = bounded::<Item>(&OWNER, 100);
        assert!(tx.try_send(Item(5000)).is_ok());
        assert!(tx.try_send(Item(PLACE)).is_err());
        assert!(rx.try_recv().is_ok());
        assert!(tx.try_send(Item(PLACE)).is_ok());
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a sender at the bound waits for room, not dropping its item; it
    /// goes on once the receiver takes, and every item arrives once, in order.
    #[test]
    fn g11_s3_a_sender_at_the_bound_waits_and_nothing_is_lost() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_queue_waits");
        let (tx, rx) = bounded::<Item>(&OWNER, 64 + PLACE);
        let sender = std::thread::spawn(move || {
            for n in 1..=200 {
                if tx.send_blocking(Item(n % 40 + PLACE)).is_err() {
                    return false;
                }
            }
            true
        });
        let mut got = Vec::new();
        let mut most = 0;
        while got.len() < 200 {
            most = most.max(rx.held());
            match rx.recv_blocking() {
                Ok(i) => got.push(i.0),
                Err(_) => break,
            }
        }
        anyhow::ensure!(sender.join().unwrap_or(false), "the sender was refused");
        let want: Vec<usize> = (1..=200).map(|n| n % 40 + PLACE).collect();
        anyhow::ensure!(got == want, "items lost or out of order");
        anyhow::ensure!(most <= 64 + PLACE, "the queue held {most} bytes, past its bound of {}", 64 + PLACE);
        anyhow::ensure!(OWNER.bytes() == 0, "{} bytes still counted", OWNER.bytes());
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a task waiting for room waits as a task, and stops waiting (its
    /// item refused, handed back) once the receiver is gone.
    #[test]
    fn g11_s3_a_waiting_sender_stops_when_the_receiver_goes() {
        static OWNER: Owner = Owner::new("test_queue_gone");
        let (tx, rx) = bounded::<Item>(&OWNER, 2 * PLACE);
        assert!(tx.try_send(Item(2 * PLACE)).is_ok());
        let waiting = std::thread::spawn({
            let tx = tx.clone();
            move || futures::executor::block_on(tx.send(Item(PLACE + 5))).err().map(|e| e.0.0)
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(rx);
        assert_eq!(waiting.join().ok().flatten(), Some(PLACE + 5), "the waiting send ended, its item handed back");
        assert!(matches!(tx.try_send(Item(PLACE)), Err(TrySendError::Closed(_))));
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a JSON value weighs what serde_json keeps of it, at least its
    /// text's bytes: its strings' bytes, an array's places, an object's entries and keys.
    #[test]
    fn g11_s3_a_json_value_weighs_what_serde_json_keeps() -> anyhow::Result<()> {
        use serde_json::{Value, json};
        let small = json!({"kind": "ConfigMap"});
        let data = json!({"kind": "ConfigMap", "data": "x".repeat(1 << 20)});
        anyhow::ensure!(data.weight() >= small.weight() + (1 << 20), "a megabyte's text weighs a megabyte: {} against {}", data.weight(), small.weight());
        let list = Value::Array((0..1000).map(|n| json!(n)).collect());
        anyhow::ensure!(list.weight() >= 1000 * size_of::<Value>(), "each of an array's values has its place: {}", list.weight());
        let keys: serde_json::Map<String, Value> = (0..100).map(|n| (format!("key-{n}"), Value::Null)).collect();
        let object = Value::Object(keys);
        anyhow::ensure!(object.weight() >= 100 * (JSON_ENTRY + "key-0".len()), "each entry and its key counted: {}", object.weight());
        // What a Kubernetes object takes kept is more than its text.
        let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "web-0", "namespace": "shop", "labels": {"app": "web", "tier": "front"}},
            "spec": {"containers": [{"name": "web", "image": "nginx:1.27", "ports": [{"containerPort": 80}], "args": ["--a", "--b"]}]}});
        let text = serde_json::to_vec(&pod)?.len();
        anyhow::ensure!(pod.weight() > text, "a pod of {text} bytes of JSON weighs {}", pod.weight());
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a forced send goes past the bound (counted all the same), a weak
    /// sender sends while the queue is open, and an owner of several queues shows their sums.
    #[test]
    fn g11_s3_forced_sends_count_and_owners_sum_their_queues() {
        static OWNER: Owner = Owner::new("test_queue_sum");
        let (a, _ra) = bounded::<Item>(&OWNER, 100);
        let (b, rb) = bounded::<Item>(&OWNER, 50);
        assert_eq!(OWNER.bound(), Some(150));
        assert!(a.try_send(Item(100)).is_ok());
        assert!(a.force_send(Item(30)).is_ok());
        assert!(b.downgrade().upgrade().is_some_and(|b| b.try_send(Item(20)).is_ok()));
        assert_eq!(OWNER.bytes(), 150);
        drop((b, rb));
        assert_eq!((OWNER.bytes(), OWNER.bound()), (130, Some(100)));
    }
}
