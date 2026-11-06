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
//! - Bounded by a count, or not at all (goal 4, plan 01a1a59c2a9c step 5): a queue of [`places`] holds that many
//!   items whatever each carries, each counted at its place alone, as a channel bounded by a count did; an
//!   [`unbounded`] one takes every send at once, as an unbounded channel did. For what moved here from
//!   async-channel with a bound of its own that wasn't bytes (each use says its basis).
//! - Polled under any waker: a wait lists the waker it's polled with, so a combinator's or a library's own
//!   waker (an HTTP body's reader, the receiver as a `Stream`) works as a task's does. hyper-rt's channels take
//!   only the polling task's own.
//! - A receiver waits in a place of its own ([`Own`]), one wait at a time: a wait dropped before it ends (a
//!   select's arm another arm beat, a timeout's) leaves that one place listed, and the next wait takes it up
//!   again, so a loop selecting over a queue whose items come seldom lists one place, not one a turn.
//!
//! No lock on any path (goal 4, plan 01a1a59c2a9c step 5; goal 5's rule: no lock two shards share on a
//! request's, a websocket's or a runner's path): an atomic count of what's held, the items in a lock-free
//! queue (`concurrent_queue`'s unbounded one, its blocks made as it fills), and whoever waits (a receiver for
//! an item, a sender for room) in a lock-free list of its own ([`Waiters`]). The queue was async-channel's and
//! the room's wait event-listener's, whose waiters sit behind a std `Mutex` that every wait and every notify
//! finding a waiter took: each runner's update handed to its session's owner (`crate::harness::copies`, its
//! owner mostly waiting) took it on the hub's shard, and the owner took it again on its own as it waited.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use async_channel::{RecvError, SendError, TryRecvError, TrySendError};
use concurrent_queue::{ConcurrentQueue, PopError};
use futures::task::AtomicWaker;

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

/// One wait's place among a queue's waiters: what wakes it (its task, or its thread parked in a blocking
/// call), and whether it's in the list now (listed once a wait, however often the wait is polled). Also
/// `crate::rt::Notify`'s and `crate::rt::Shutdown`'s (goal 5, plan 01a1a59c2aae, its top goal).
pub(crate) struct Place {
    waker: AtomicWaker,
    listed: AtomicBool,
}

#[cfg(test)]
thread_local! {
    /// Places made on this thread (a test's: a wait that finds what it waits for makes none).
    static PLACES_MADE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Place {
    pub(crate) fn new() -> Arc<Self> {
        #[cfg(test)]
        PLACES_MADE.with(|n| n.set(n.get().saturating_add(1)));
        Arc::new(Self { waker: AtomicWaker::new(), listed: AtomicBool::new(false) })
    }

    /// Its wait is over: the waker it held goes, so a wake that finds it listed still wakes no one.
    pub(crate) fn let_go(&self) {
        drop(self.waker.take());
    }
}

/// Whoever waits on a queue for one thing (an item, or room), with no lock: a waiter lists its place and
/// looks again; whoever makes what it waits for (an item in, room let go) wakes every place listed then. A
/// fence on each side (a Dekker handshake) means one of the two sees the other, so no wake is lost. A place
/// listed by a wait that found what it waited for at its look again stays until the next wake, which polls
/// its task for nothing: each wait lists one place at a time, and every wake takes them all.
pub(crate) struct Waiters {
    list: ConcurrentQueue<Arc<Place>>,
}

impl Waiters {
    pub(crate) fn new() -> Self {
        Self { list: ConcurrentQueue::unbounded() }
    }

    /// How many places are listed now (a test's).
    #[cfg(test)]
    pub(crate) fn listed(&self) -> usize {
        self.list.len()
    }

    /// `place` waits, woken by `waker`; what the waiter looks at next is looked at after this.
    pub(crate) fn wait(&self, place: &Arc<Place>, waker: &Waker) {
        place.waker.register(waker);
        if !place.listed.swap(true, Ordering::AcqRel) {
            // Unbounded and never closed: the push always goes.
            let _ = self.list.push(Arc::clone(place));
        }
        fence(Ordering::SeqCst);
    }

    /// Wakes every place listed now: what the waker made (an item in, room let go) is seen by each.
    pub(crate) fn wake(&self) {
        fence(Ordering::SeqCst);
        for _ in 0..self.list.len() {
            let Ok(place) = self.list.pop() else { break };
            place.listed.store(false, Ordering::Release);
            place.waker.wake();
        }
    }
}

/// A waker that unparks the thread that made it: a blocking call's wait, on a thread of its own (never the
/// runtime's). Also `crate::rt::Listen::wait_deadline`'s (goal 5, plan 01a1a59c2aae, its top goal).
pub(crate) fn thread_waker() -> Waker {
    struct Unpark(std::thread::Thread);
    impl futures::task::ArcWake for Unpark {
        fn wake_by_ref(this: &Arc<Self>) {
            this.0.unpark();
        }
    }
    futures::task::waker(Arc::new(Unpark(std::thread::current())))
}

/// Parks the calling thread until it's unparked or `until` (none: for as long as it takes): false once
/// `until` has passed.
fn park(until: Option<Instant>) -> bool {
    match until {
        Some(t) => {
            let now = Instant::now();
            if now >= t {
                return false;
            }
            std::thread::park_timeout(t.saturating_duration_since(now));
            true
        }
        None => {
            std::thread::park();
            true
        }
    }
}

/// A queue's room: the bytes it holds, the most it may, and a wait for room. Counted into its owner's
/// figures as it changes; its bound joins the owner's as it's made and leaves it as it goes.
pub struct Room {
    owner: &'static Owner,
    bound: usize,
    /// Whether `bound` holds: an [`unbounded`] queue's room takes every send.
    limited: bool,
    held: AtomicUsize,
    /// No receiver any more: whoever waits for room stops waiting (what it sends is refused).
    closed: AtomicBool,
    /// Whoever waits for room: woken as bytes are let go, and as the queue closes.
    freed: Waiters,
}

impl Room {
    /// A room of `bound` bytes, counted as `owner`'s.
    pub fn new(owner: &'static Owner, bound: usize) -> Self {
        owner.bound_add(bound);
        Self { owner, bound, limited: true, held: AtomicUsize::new(0), closed: AtomicBool::new(false), freed: Waiters::new() }
    }

    /// A room with no bound, counted as `owner`'s: it takes whatever comes ([`unbounded`]), adding nothing to
    /// the owner's bound.
    fn unbounded(owner: &'static Owner) -> Self {
        Self { owner, bound: 0, limited: false, held: AtomicUsize::new(0), closed: AtomicBool::new(false), freed: Waiters::new() }
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
    /// the bound goes alone), or into one with no bound. False when they don't.
    pub fn try_take(&self, n: usize) -> bool {
        let mut now = self.held.load(Ordering::Acquire);
        loop {
            let after = now.saturating_add(n);
            if self.limited && now > 0 && after > self.bound {
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

    /// What a wait for `n` bytes finds now: true once they're taken, false once the queue closed, none
    /// while it waits on.
    fn look(&self, n: usize) -> Option<bool> {
        if self.is_closed() {
            return Some(false);
        }
        self.try_take(n).then_some(true)
    }

    /// Takes `n` bytes, waiting (a task's wait: its thread goes on with others) until they fit; false when
    /// the queue closed first. Room there, as a send mostly finds it, is taken with no place made for a wait
    /// (goal 9, plan 01a1a87b2cec step 5: every send made one, an allocation and a free on the sender's shard).
    pub async fn take(&self, n: usize) -> bool {
        if let Some(done) = self.look(n) {
            return done;
        }
        let place = Place::new();
        std::future::poll_fn(|cx| {
            if let Some(done) = self.look(n) {
                return Poll::Ready(done);
            }
            self.freed.wait(&place, cx.waker());
            match self.look(n) {
                Some(done) => Poll::Ready(done),
                None => Poll::Pending,
            }
        })
        .await
    }

    /// [`Room::take`], blocking its thread: for a thread of its own, never the runtime's. Waits until
    /// `until` at most (none: as long as it takes); false when it closed or the time ran out first. Room there
    /// is taken with no place or waker made for a wait, as [`Room::take`]'s.
    pub fn take_blocking(&self, n: usize, until: Option<Instant>) -> bool {
        if let Some(done) = self.look(n) {
            return done;
        }
        let (place, waker) = (Place::new(), thread_waker());
        loop {
            if let Some(done) = self.look(n) {
                return done;
            }
            self.freed.wait(&place, &waker);
            if let Some(done) = self.look(n) {
                return done;
            }
            if !park(until) {
                return false;
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
        self.freed.wake();
    }

    /// No one takes from it any more: whoever waits for room stops.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.freed.wake();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl Drop for Room {
    fn drop(&mut self) {
        // What it still held goes with it (its items are dropped with their queue), and so does its bound.
        self.owner.sub(*self.held.get_mut());
        self.owner.bound_sub(self.bound);
    }
}

/// What a place in the queue takes beside its item: the queue's word of state for the place, and the
/// count kept beside the item ([`Weighed`]). Counted with each item, so a queue of many small items is
/// bounded too.
pub const PLACE: usize = 2 * size_of::<usize>();

/// What `item` is counted at: its weight and its place.
fn counted<T: Weigh>(item: &T) -> usize {
    item.weight().saturating_add(PLACE)
}

/// What an item of a queue of [`places`] (or an [`unbounded`] one) is counted at: its place alone, its own size
/// and [`PLACE`], whatever it carries.
const fn place_of<T>() -> usize {
    size_of::<T>().saturating_add(PLACE)
}

/// [`place_of`] as a queue's weigher ([`Chan::weigh`]).
fn at_place<T>(_: &T) -> usize {
    place_of::<T>()
}

/// An item and the bytes it was counted at, so the receiver lets go of exactly what the sender took.
struct Weighed<T> {
    bytes: usize,
    item: T,
}

/// What a queue's ends share: its items in order, whoever waits for one, and how many senders live (the
/// last to go closes it, so its receivers end once they've taken what's in it).
struct Chan<T> {
    items: ConcurrentQueue<Weighed<T>>,
    takers: Waiters,
    senders: AtomicUsize,
    /// What an item is counted at: its weight and its place ([`bounded`]), or its place alone ([`places`],
    /// [`unbounded`]).
    weigh: fn(&T) -> usize,
}

impl<T> Chan<T> {
    /// Closes it: what's in it can still be taken, nothing more goes in, and whoever waits to take looks
    /// again. Whether this call closed it.
    fn close(&self) -> bool {
        let closed = self.items.close();
        self.takers.wake();
        closed
    }
}

/// A queue of `bound` bytes, counted as `owner`'s. Its places are made as it fills.
pub fn bounded<T: Weigh>(owner: &'static Owner, bound: usize) -> (Sender<T>, Receiver<T>) {
    made(Room::new(owner, bound), counted::<T>)
}

/// A queue of `places` items whatever each carries (one at the least), counted as `owner`'s at their places
/// alone ([`place_of`]): a sender past them waits, or is told it's full, as a channel bounded by a count did
/// (goal 4, plan 01a1a59c2a9c step 5). Its places are made as it fills.
pub fn places<T>(owner: &'static Owner, places: usize) -> (Sender<T>, Receiver<T>) {
    made(Room::new(owner, places.saturating_mul(place_of::<T>())), at_place::<T>)
}

/// A queue with no bound, counted as `owner`'s at its items' places ([`place_of`]): every send goes at once,
/// none waits or is told it's full, as into an unbounded channel (goal 4, plan 01a1a59c2a9c step 5). For
/// senders that must never wait, whose items something else bounds (each use says what).
pub fn unbounded<T>(owner: &'static Owner) -> (Sender<T>, Receiver<T>) {
    made(Room::unbounded(owner), at_place::<T>)
}

/// A queue of one place for one answer (goal 5, plan 01a1a59c2aae, its top goal): an owner's reply to whoever
/// asked it (CI's dispatcher's, Mantle's client's, the browser protocol's topics' owner's), counted as
/// [`super::owners::CHANNELS`]'s. Its sender clones and its `try_send` never waits, as the async-channel bounded
/// at one it replaces did, so a reply kept with what it answers (a run's start, `crate::ci::dispatch::Given`) goes
/// once; and its receiver waits under any waker (a combinator's own among them), where hyper-rt's one-shot takes
/// only its polling task's.
pub fn answer<T>() -> (Sender<T>, Receiver<T>) {
    places(&super::owners::CHANNELS, 1)
}

/// A queue's two ends, about `room`, its items counted by `weigh`.
fn made<T>(room: Room, weigh: fn(&T) -> usize) -> (Sender<T>, Receiver<T>) {
    made_in(Arc::new(room), weigh)
}

/// A queue's two ends about `room`, which other queues may share ([`bounded_in`]), its items counted by `weigh`.
fn made_in<T>(room: Arc<Room>, weigh: fn(&T) -> usize) -> (Sender<T>, Receiver<T>) {
    let chan = Arc::new(Chan { items: ConcurrentQueue::unbounded(), takers: Waiters::new(), senders: AtomicUsize::new(1), weigh });
    (Sender { chan: Arc::clone(&chan), room: Arc::clone(&room) }, Receiver { inner: Arc::new(Taking { chan, room }), own: Own::new() })
}

/// A queue of its own channel whose bytes count against `room` with those of every other queue made in it:
/// together they hold at most its bound, as one queue would (goal 10, plan 01a1a87b2dae step 4: the hub's
/// loops, one a shard, each take what the net thread hands them from one window of what runners sent, as
/// the one loop did, `crate::wire::fan`). The last receiver of any of them to go closes the room for all.
pub fn bounded_in<T: Weigh>(room: &Arc<Room>) -> (Sender<T>, Receiver<T>) {
    made_in(Arc::clone(room), counted::<T>)
}

/// A queue's sending end: cheap to clone, usable from any thread.
pub struct Sender<T> {
    chan: Arc<Chan<T>>,
    room: Arc<Room>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.chan.senders.fetch_add(1, Ordering::AcqRel);
        Self { chan: Arc::clone(&self.chan), room: Arc::clone(&self.room) }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        // The last sender closes it for its receivers, once they've taken what's in it.
        if self.chan.senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.chan.close();
        }
    }
}

impl<T> std::fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender").field("held", &self.room.held()).field("bound", &self.room.bound).finish()
    }
}

#[cfg(test)]
impl<T> Sender<T> {
    /// How many receivers' waits are listed now, each one a send would wake (a test's: a receiver that never
    /// waits on the queue lists none, so a send wakes no one).
    pub(crate) fn takers_listed(&self) -> usize {
        self.chan.takers.listed()
    }
}

impl<T> Sender<T> {
    /// Queues `item`, waiting first while the queue has no room for it: an error, with the item, once no
    /// one takes from the queue any more.
    pub async fn send(&self, item: T) -> Result<(), SendError<T>> {
        let bytes = (self.chan.weigh)(&item);
        if self.is_closed() || !self.room.take(bytes).await {
            return Err(SendError(item));
        }
        self.put(bytes, item).map_err(SendError)
    }

    /// Queues `item` if it has room now: `Full` (with the item) when it hasn't, `Closed` when no one takes
    /// from it any more.
    pub fn try_send(&self, item: T) -> Result<(), TrySendError<T>> {
        let bytes = (self.chan.weigh)(&item);
        if self.is_closed() {
            return Err(TrySendError::Closed(item));
        }
        if !self.room.try_take(bytes) {
            return Err(TrySendError::Full(item));
        }
        self.put(bytes, item).map_err(TrySendError::Closed)
    }

    /// [`Sender::send`], blocking its thread: for a thread of its own, never the runtime's.
    pub fn send_blocking(&self, item: T) -> Result<(), SendError<T>> {
        let bytes = (self.chan.weigh)(&item);
        if self.is_closed() || !self.room.take_blocking(bytes, None) {
            return Err(SendError(item));
        }
        self.put(bytes, item).map_err(SendError)
    }

    /// Queues `item` whatever the queue holds: for a sender that must never wait, its item counted all the
    /// same (each use says why it can't wait).
    pub fn force_send(&self, item: T) -> Result<(), SendError<T>> {
        let bytes = (self.chan.weigh)(&item);
        if self.is_closed() {
            return Err(SendError(item));
        }
        self.room.force(bytes);
        self.put(bytes, item).map_err(SendError)
    }

    /// Into the queue, its bytes already taken, and whoever waits to take woken: given back if the queue
    /// closed meanwhile.
    fn put(&self, bytes: usize, item: T) -> Result<(), T> {
        match self.chan.items.push(Weighed { bytes, item }) {
            Ok(()) => {
                self.chan.takers.wake();
                Ok(())
            }
            Err(refused) => {
                self.room.give(bytes);
                Err(refused.into_inner().item)
            }
        }
    }
}

impl<T> Sender<T> {
    /// Whether no one takes from the queue any more.
    pub fn is_closed(&self) -> bool {
        self.chan.items.is_closed()
    }

    /// Closes the queue: what waits in it can still be taken, nothing more goes in, and whoever waits for
    /// room stops waiting.
    pub fn close(&self) -> bool {
        let closed = self.chan.close();
        self.room.close();
        closed
    }

    /// Items waiting now.
    pub fn len(&self) -> usize {
        self.chan.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chan.items.is_empty()
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
        Arc::ptr_eq(&self.chan, &other.chan)
    }

    /// A handle that doesn't keep the queue open ([`WeakSender::upgrade`]).
    pub fn downgrade(&self) -> WeakSender<T> {
        WeakSender { chan: Arc::downgrade(&self.chan), room: Arc::downgrade(&self.room) }
    }
}

/// A sending end that doesn't keep the queue open: upgraded while a [`Sender`] still is.
pub struct WeakSender<T> {
    chan: Weak<Chan<T>>,
    room: Weak<Room>,
}

impl<T> Clone for WeakSender<T> {
    fn clone(&self) -> Self {
        Self { chan: Weak::clone(&self.chan), room: Weak::clone(&self.room) }
    }
}

impl<T> std::fmt::Debug for WeakSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeakSender")
    }
}

impl<T> WeakSender<T> {
    /// A sender, while the queue is open and a sender still lives.
    pub fn upgrade(&self) -> Option<Sender<T>> {
        let (chan, room) = (self.chan.upgrade()?, self.room.upgrade()?);
        if chan.items.is_closed() {
            return None;
        }
        let mut now = chan.senders.load(Ordering::Acquire);
        loop {
            if now == 0 {
                return None;
            }
            match chan.senders.compare_exchange_weak(now, now + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(Sender { chan, room }),
                Err(seen) => now = seen,
            }
        }
    }
}

/// A queue's receiving end: what it takes lets its bytes go, and wakes whoever waits for room. Cheap to
/// clone; the last of its clones to go closes the queue.
pub struct Receiver<T> {
    inner: Arc<Taking<T>>,
    /// Where its waits wait (each clone its own).
    own: Own,
}

/// A receiver's own place among its queue's waiters, and whether a wait has it now: one wait at a time takes
/// it ([`Own::place`]), and gives it back as it ends or is dropped. A wait dropped while its place is listed
/// leaves it listed, and the next takes it up, listed still (goal 4, plan 01a1a59c2a9c step 5). A place a wait
/// made for itself would stay listed after the wait, until the queue's next item woke it, one more each turn:
/// a terminal's keystrokes waited for beside its output, a feed's links beside a timer, piled up so while the
/// other arm won.
struct Own {
    place: Arc<Place>,
    taken: AtomicBool,
}

impl Own {
    fn new() -> Self {
        Self { place: Place::new(), taken: AtomicBool::new(false) }
    }

    /// The place a wait waits in: this receiver's own while no other wait has it ([`Claim`], which gives it
    /// back), else one of the wait's own (two waits on one receiver at once, through its `&`).
    fn place(&self) -> (Arc<Place>, Option<Claim<'_>>) {
        if self.taken.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            (Arc::clone(&self.place), Some(Claim(&self.taken)))
        } else {
            (Place::new(), None)
        }
    }
}

/// A wait's hold on its receiver's own place: given back as it goes, the wait over or dropped.
struct Claim<'a>(&'a AtomicBool);

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The receiving end itself, one for all of a queue's [`Receiver`]s: dropped with the last, it closes the
/// queue, so a sender waiting for room it would never get stops waiting.
struct Taking<T> {
    chan: Arc<Chan<T>>,
    room: Arc<Room>,
}

impl<T> Taking<T> {
    /// [`Receiver::look`].
    fn look(&self) -> Option<Result<T, RecvError>> {
        match self.chan.items.pop() {
            Ok(w) => Some(Ok(self.taken(w))),
            Err(PopError::Closed) => Some(Err(RecvError)),
            Err(PopError::Empty) => None,
        }
    }

    /// An item taken: its bytes let go, which wakes whoever waits for room.
    fn taken(&self, w: Weighed<T>) -> T {
        self.room.give(w.bytes);
        w.item
    }
}

/// A poll for `taking`'s next item, its wait's place `place` listed with `cx`'s waker before it looks again
/// (the queue's handshake, [`Waiters`]).
fn poll_taking<T>(taking: &Taking<T>, cx: &Context<'_>, place: &Arc<Place>) -> Poll<Result<T, RecvError>> {
    if let Some(taken) = taking.look() {
        return Poll::Ready(taken);
    }
    taking.chan.takers.wait(place, cx.waker());
    match taking.look() {
        Some(taken) => Poll::Ready(taken),
        None => Poll::Pending,
    }
}

/// The items as they come, until the queue is closed and empty (goal 4, plan 01a1a59c2a9c step 5): an HTTP
/// body's chunks. It waits in the receiver's own place, which no other wait has while it's polled (its `&mut`).
impl<T> futures::Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let Self { inner, own } = self.get_mut();
        poll_taking(inner, cx, &own.place).map(Result::ok)
    }
}

impl<T> Drop for Taking<T> {
    fn drop(&mut self) {
        self.chan.close();
        self.room.close();
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner), own: Own::new() }
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver").field("held", &self.inner.room.held()).field("bound", &self.inner.room.bound).finish()
    }
}

impl<T> Receiver<T> {
    /// The next item, waiting for one: an error once the queue is empty and no one sends to it any more.
    /// Cancel-safe: an item is taken and its bytes let go in the same poll. It waits in the receiver's own
    /// place ([`Own`]), so one dropped before it ends (a select's arm) leaves nothing more listed. An item there
    /// is taken without claiming that place (goal 9, plan 01a1a87b2cec step 5).
    pub async fn recv(&self) -> Result<T, RecvError> {
        if let Some(taken) = self.look() {
            return taken;
        }
        let (place, _claim) = self.own.place();
        std::future::poll_fn(|cx| poll_taking(&self.inner, cx, &place)).await
    }

    /// Whether `other` takes from the same queue.
    pub fn same_channel(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner.chan, &other.inner.chan)
    }

    /// The next item if one waits.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.inner.chan.items.pop() {
            Ok(w) => Ok(self.taken(w)),
            Err(PopError::Empty) => Err(TryRecvError::Empty),
            Err(PopError::Closed) => Err(TryRecvError::Closed),
        }
    }

    /// [`Receiver::recv`], blocking its thread: for a thread of its own, never the runtime's. An item there is
    /// taken with no place or waker made for a wait (goal 9, plan 01a1a87b2cec step 5: each take made both).
    pub fn recv_blocking(&self) -> Result<T, RecvError> {
        if let Some(taken) = self.look() {
            return taken;
        }
        let (place, waker) = (Place::new(), thread_waker());
        loop {
            if let Some(taken) = self.look() {
                return taken;
            }
            self.inner.chan.takers.wait(&place, &waker);
            if let Some(taken) = self.look() {
                return taken;
            }
            park(None);
        }
    }

    /// What a wait for an item finds now: an item, or the end once the queue is closed and empty; none while
    /// it waits on.
    fn look(&self) -> Option<Result<T, RecvError>> {
        self.inner.look()
    }

    fn taken(&self, w: Weighed<T>) -> T {
        self.inner.taken(w)
    }

    /// Items waiting now.
    pub fn len(&self) -> usize {
        self.inner.chan.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.chan.items.is_empty()
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
        self.inner.chan.items.is_closed()
    }

    /// Closes the queue: what waits in it can still be taken; whoever waits for room stops waiting.
    pub fn close(&self) -> bool {
        let closed = self.inner.chan.close();
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

    /// What a source file's code says outside its tests, its comments left out (CRLF read as LF).
    fn code_of(src: &str) -> String {
        let src = src.replace("\r\n", "\n");
        let code = src.split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default();
        code.lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n")
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): a queue for one answer holds one, as the async-channel bounded at
    /// one it replaces did: a second `try_send` (a clone's too) is told it's full, the answer is taken once, waited
    /// for on another thread than its sender's, and a sender gone with nothing sent ends the wait.
    #[test]
    fn g5_an_answer_holds_one_and_its_wait_ends_with_it_or_without() -> anyhow::Result<()> {
        let (tx, rx) = answer::<u32>();
        let again = tx.clone();
        anyhow::ensure!(tx.try_send(7).is_ok(), "the answer refused");
        anyhow::ensure!(matches!(again.try_send(8), Err(TrySendError::Full(8))), "a second answer went in");
        anyhow::ensure!(rx.try_recv() == Ok(7), "the answer");
        anyhow::ensure!(rx.try_recv() == Err(TryRecvError::Empty), "an answer twice");
        let sender = std::thread::spawn(move || tx.try_send(9).is_ok());
        anyhow::ensure!(rx.recv_blocking() == Ok(9), "the answer sent from another thread");
        anyhow::ensure!(sender.join().unwrap_or(false), "the other thread's answer refused");
        drop(again);
        anyhow::ensure!(rx.recv_blocking() == Err(RecvError), "a wait whose senders went without answering didn't end");
        Ok(())
    }

    /// Goal 4, plan 01a1a59c2a9c step 5 (goal 5's rule), run by kiln's CI with every test: the queue takes no
    /// lock, and waits on nothing that takes one (event-listener's waiters sit behind a `Mutex`, and so do
    /// an async-channel's).
    #[test]
    fn g4_s5_the_queue_takes_no_lock() {
        let code = code_of(include_str!("queue.rs"));
        for lock in ["Mutex", "RwLock", "Condvar", "parking_lot", "async_lock", "event_listener", "Event::new", "async_channel::unbounded", "async_channel::bounded"] {
            assert!(!code.contains(lock), "memory/queue.rs takes a lock, or waits on something that does: {lock}");
        }
    }

    /// Goal 4, plan 01a1a59c2a9c step 5 (goal 5's rule), run by kiln's CI with every test: the queues on these
    /// runner and websocket paths are this module's or hyper-rt's, none an async-channel's, whose waits take
    /// event-listener's `Mutex` on the shards on both sides: the sealed chunks' packer (`harness/copies.rs`),
    /// a terminal's keystrokes and its relay (`termsess.rs`), a conversation feed's links, parts and answers
    /// (`convo/feed.rs`), a live job's writes (`logs.rs`), the CI dispatcher's answer to a cancel (`hub.rs`),
    /// and a response's pieces read ahead and a socket's audit rows (`api.rs`).
    #[test]
    fn g4_s5_runner_and_websocket_paths_make_no_async_channel() {
        let files = [
            ("harness/copies.rs", include_str!("../harness/copies.rs")),
            ("termsess.rs", include_str!("../termsess.rs")),
            ("convo/feed.rs", include_str!("../convo/feed.rs")),
            ("logs.rs", include_str!("../logs.rs")),
            ("hub.rs", include_str!("../hub.rs")),
            ("api.rs", include_str!("../api.rs")),
        ];
        for (file, src) in files {
            let code = code_of(src);
            for made in ["async_channel::bounded", "async_channel::unbounded"] {
                assert!(!code.contains(made), "{file} makes an async-channel, whose waits take a lock ({made})");
            }
        }
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal), run by kiln's CI with every test: these server paths' queues are
    /// this module's, none an async-channel's: every live command to a runner, a terminal's keystrokes among them
    /// (`wire/commands.rs`), each frame reach's relay passes between runners (`reach/relay.rs`), and WebRTC's
    /// loop and what it tells its viewers' sockets (`harness/webrtc.rs`).
    #[test]
    fn g5_the_servers_command_relay_and_webrtc_paths_make_no_async_channel() {
        let files = [("wire/commands.rs", include_str!("../wire/commands.rs")), ("reach/relay.rs", include_str!("../reach/relay.rs")), ("harness/webrtc.rs", include_str!("../harness/webrtc.rs"))];
        for (file, src) in files {
            let code = code_of(src);
            for made in ["async_channel::bounded", "async_channel::unbounded"] {
                assert!(!code.contains(made), "{file} makes an async-channel, whose waits take a lock ({made})");
            }
        }
    }

    /// An item of a test's: its sender, its number, and `n` bytes counted.
    struct Tagged(usize, usize, usize);

    impl Weigh for Tagged {
        fn weight(&self) -> usize {
            self.2
        }
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: senders on every shard at once, a bound that holds a few of their
    /// items so they wait for room, and two receivers taking on theirs: every item is taken once, each
    /// sender's in the order it sent them, and the queue never holds past its bound (an item past it alone).
    #[test]
    fn g4_s5_senders_on_every_shard_keep_their_order_and_none_is_lost_while_they_wait_for_room() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_queue_shards");
        crate::rt::block_on(async {
            let (senders, per, each) = (8usize, 300usize, 100usize);
            let bound = 4 * (each + PLACE);
            let (tx, rx) = bounded::<Tagged>(&OWNER, bound);
            let most = Arc::new(AtomicUsize::new(0));
            let mut takers = Vec::new();
            for _ in 0..2 {
                let (rx, most) = (rx.clone(), Arc::clone(&most));
                takers.push(crate::rt::spawn_spread_joined(async move {
                    let mut got: Vec<(usize, usize)> = Vec::new();
                    while let Ok(item) = rx.recv().await {
                        most.fetch_max(rx.held(), Ordering::AcqRel);
                        got.push((item.0, item.1));
                    }
                    got
                }));
            }
            drop(rx);
            let mut sent = Vec::new();
            for s in 0..senders {
                let tx = tx.clone();
                sent.push(crate::rt::spawn_spread_joined(async move {
                    for n in 0..per {
                        if tx.send(Tagged(s, n, each)).await.is_err() {
                            return false;
                        }
                    }
                    true
                }));
            }
            drop(tx);
            for s in sent {
                anyhow::ensure!(s.await.unwrap_or(false), "a sender was refused");
            }
            let mut all: Vec<(usize, usize)> = Vec::new();
            for t in takers {
                let got = crate::rt::timeout(std::time::Duration::from_secs(30), t).await.map_err(|_| anyhow::anyhow!("a receiver never saw the end"))?.map_err(|_| anyhow::anyhow!("a receiver ended"))?;
                // Each receiver has each sender's in order, among what it took.
                for s in 0..senders {
                    let theirs: Vec<usize> = got.iter().filter(|(g, _)| *g == s).map(|(_, n)| *n).collect();
                    anyhow::ensure!(theirs.windows(2).all(|w| w.first() < w.get(1)), "sender {s}'s out of order: {theirs:?}");
                }
                all.extend(got);
            }
            all.sort_unstable();
            let want: Vec<(usize, usize)> = (0..senders).flat_map(|s| (0..per).map(move |n| (s, n))).collect();
            anyhow::ensure!(all == want, "{} items taken of {}: some lost or doubled", all.len(), want.len());
            anyhow::ensure!(most.load(Ordering::Acquire) <= bound, "held {} past its bound {bound}", most.load(Ordering::Acquire));
            anyhow::ensure!(OWNER.bytes() == 0, "{} bytes still counted", OWNER.bytes());
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a receiver waiting on a thread of its own is woken by a send from a
    /// task, a weak sender upgrades while a sender lives and the queue is open (not after), and the queue ends
    /// for its receiver once its last sender goes.
    #[test]
    fn g4_s5_waits_are_woken_and_the_queue_ends_with_its_senders() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_queue_wake");
        let (tx, rx) = bounded::<Item>(&OWNER, 1 << 10);
        let taker = std::thread::spawn(move || {
            let mut got = Vec::new();
            while let Ok(item) = rx.recv_blocking() {
                got.push(item.0);
            }
            got
        });
        let weak = tx.downgrade();
        crate::rt::block_on(async move {
            crate::rt::sleep(std::time::Duration::from_millis(30)).await;
            for n in 0..3 {
                tx.send(Item(PLACE + n)).await.map_err(|_| anyhow::anyhow!("refused"))?;
                crate::rt::sleep(std::time::Duration::from_millis(5)).await;
            }
            anyhow::ensure!(weak.upgrade().is_some(), "a sender lives");
            drop(tx);
            anyhow::ensure!(weak.upgrade().is_none(), "the last sender went");
            Ok(())
        })?;
        let got = taker.join().map_err(|_| anyhow::anyhow!("the taker panicked"))?;
        anyhow::ensure!(got == vec![PLACE, PLACE + 1, PLACE + 2], "{got:?}");
        anyhow::ensure!(OWNER.bytes() == 0);
        Ok(())
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a queue of places holds that many items whatever each carries, as the
    /// count-bounded channel it stands for did, each counted at its place alone; the one past them is told it's
    /// full, and they come out in the order they went in.
    #[test]
    fn g4_s5_a_queue_of_places_holds_that_many_whatever_each_carries() {
        static OWNER: Owner = Owner::new("test_queue_places");
        let (tx, rx) = places::<Vec<u8>>(&OWNER, 3);
        assert_eq!(OWNER.bound(), Some(3 * place_of::<Vec<u8>>()));
        for n in 0u8..3 {
            assert!(tx.try_send(vec![n; if n == 1 { 1 << 20 } else { 1 }]).is_ok(), "item {n} fits, a megabyte or a byte");
        }
        assert!(matches!(tx.try_send(Vec::new()), Err(TrySendError::Full(_))), "a fourth, however small, doesn't");
        assert_eq!((OWNER.bytes(), tx.len()), (3 * place_of::<Vec<u8>>(), 3), "each counted at its place alone");
        for n in 0u8..3 {
            assert!(rx.try_recv().is_ok_and(|v| v.first() == Some(&n)), "item {n} in its turn");
        }
        assert!(tx.try_send(Vec::new()).is_ok(), "room again once taken");
        drop((tx, rx));
        assert_eq!((OWNER.bytes(), OWNER.bound()), (0, None), "its bytes and bound go with it");
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: an unbounded queue takes every send at once (none waits, none is told
    /// it's full), as the unbounded channel it stands for did, adds nothing to its owner's bound, and gives
    /// each item once, in order, then its end once its sender is gone.
    #[test]
    fn g4_s5_an_unbounded_queue_takes_every_send_at_once() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_queue_unbounded");
        let (tx, rx) = unbounded::<usize>(&OWNER);
        let n = 100_000usize;
        for i in 0..n {
            anyhow::ensure!(tx.try_send(i).is_ok(), "send {i} refused");
        }
        anyhow::ensure!(OWNER.bound().is_none(), "an unbounded queue adds no bound: {:?}", OWNER.bound());
        anyhow::ensure!(OWNER.bytes() == n * place_of::<usize>(), "{} bytes counted", OWNER.bytes());
        drop(tx);
        let got: Vec<usize> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        anyhow::ensure!(got.iter().copied().eq(0..n), "{} of {n}, in order: each once", got.len());
        anyhow::ensure!(matches!(rx.try_recv(), Err(TryRecvError::Closed)), "its end, once its sender went");
        anyhow::ensure!(OWNER.bytes() == 0, "{} bytes still counted", OWNER.bytes());
        Ok(())
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a loop selecting over a quiet queue beside a busy one (a terminal's
    /// keystrokes beside its output) drops its wait on the quiet one each turn: the receiver's waits list its
    /// one place however many turns, and the quiet queue's next item still wakes the wait then. Two waits on one
    /// receiver at once (the second in a place of its own) are each woken.
    #[test]
    fn g4_s5_waits_dropped_turn_after_turn_list_one_place() -> anyhow::Result<()> {
        use futures::FutureExt as _;
        static OWNER: Owner = Owner::new("test_queue_dropped_waits");
        crate::rt::block_on(async {
            let (quiet_tx, quiet) = places::<usize>(&OWNER, 4);
            let (busy_tx, busy) = places::<usize>(&OWNER, 4);
            let turns = 10_000usize;
            for n in 0..turns {
                busy_tx.try_send(n).map_err(|_| anyhow::anyhow!("the busy queue was full"))?;
                // The quiet arm first: it waits (its place listed), then the busy one wins.
                let won = futures::select_biased! {
                    q = quiet.recv().fuse() => q.ok().map(|_| usize::MAX),
                    b = busy.recv().fuse() => b.ok(),
                };
                anyhow::ensure!(won == Some(n), "turn {n}: {won:?}");
            }
            let listed = quiet.inner.chan.takers.list.len();
            anyhow::ensure!(listed <= 1, "{listed} places listed after {turns} dropped waits");
            // The quiet queue's item, sent from another shard, wakes a wait in that same place.
            let sender = crate::rt::spawn_spread_joined({
                let tx = quiet_tx.clone();
                async move {
                    crate::rt::sleep(std::time::Duration::from_millis(20)).await;
                    tx.send(7).await.is_ok()
                }
            });
            let got = crate::rt::timeout(std::time::Duration::from_secs(5), quiet.recv()).await.map_err(|_| anyhow::anyhow!("the wait was never woken"))?;
            anyhow::ensure!(matches!(got, Ok(7)) && sender.await.unwrap_or(false), "{got:?}");
            // Two waits at once on one receiver: the second waits in a place of its own, and both are woken.
            let both = crate::rt::spawn_spread_joined({
                let tx = quiet_tx.clone();
                async move {
                    crate::rt::sleep(std::time::Duration::from_millis(20)).await;
                    tx.send(1).await.is_ok() && tx.send(2).await.is_ok()
                }
            });
            let (a, b) = crate::rt::timeout(std::time::Duration::from_secs(5), futures::future::join(quiet.recv(), quiet.recv())).await.map_err(|_| anyhow::anyhow!("a wait was never woken"))?;
            let mut got = [a.unwrap_or(0), b.unwrap_or(0)];
            got.sort_unstable();
            anyhow::ensure!(got == [1, 2] && both.await.unwrap_or(false), "{got:?}");
            drop((quiet_tx, busy_tx, quiet, busy));
            anyhow::ensure!(OWNER.bytes() == 0, "{} bytes still counted", OWNER.bytes());
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a receiver as a stream (an HTTP body's chunks), polled by a plain
    /// thread's executor, its waker no task's, while senders on every shard send: every item once, each
    /// sender's in the order sent, and the stream ends once the senders are gone.
    #[test]
    fn g4_s5_a_receiver_as_a_stream_takes_every_item_in_order_under_any_waker() -> anyhow::Result<()> {
        use futures::StreamExt as _;
        static OWNER: Owner = Owner::new("test_queue_stream");
        let (senders, per) = (8usize, 2_000usize);
        let (tx, rx) = places::<(usize, usize)>(&OWNER, 4);
        let taker = std::thread::spawn(move || futures::executor::block_on(rx.collect::<Vec<_>>()));
        crate::rt::block_on(async move {
            let mut sending = Vec::new();
            for s in 0..senders {
                let tx = tx.clone();
                sending.push(crate::rt::spawn_spread_joined(async move {
                    for n in 0..per {
                        if tx.send((s, n)).await.is_err() {
                            return false;
                        }
                    }
                    true
                }));
            }
            drop(tx);
            for s in sending {
                anyhow::ensure!(s.await.unwrap_or(false), "a sender was refused");
            }
            Ok(())
        })?;
        let got = taker.join().map_err(|_| anyhow::anyhow!("the taker panicked"))?;
        for s in 0..senders {
            let theirs: Vec<usize> = got.iter().filter(|(g, _)| *g == s).map(|(_, n)| *n).collect();
            anyhow::ensure!(theirs.iter().copied().eq(0..per), "sender {s}'s: {} of {per}, in order: {}", theirs.len(), theirs.windows(2).all(|w| w.first() < w.get(1)));
        }
        anyhow::ensure!(got.len() == senders * per, "{} items for {}: some doubled", got.len(), senders * per);
        anyhow::ensure!(OWNER.bytes() == 0, "{} bytes still counted", OWNER.bytes());
        Ok(())
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

    /// Goal 10, plan 01a1a87b2dae step 4: queues made in one room hold its bound together, as one queue of
    /// it would, each taking from its own channel; room one of them makes is room for the others.
    #[test]
    fn g10_s4_queues_in_one_room_hold_its_bound_together() {
        static OWNER: Owner = Owner::new("test_queue_room");
        let room = Arc::new(Room::new(&OWNER, 4 * PLACE));
        let (a, ra) = bounded_in::<Item>(&room);
        let (b, rb) = bounded_in::<Item>(&room);
        assert!(a.try_send(Item(2 * PLACE)).is_ok() && b.try_send(Item(2 * PLACE)).is_ok());
        assert!(a.try_send(Item(PLACE)).is_err() && b.try_send(Item(PLACE)).is_err(), "full together");
        assert_eq!((OWNER.bytes(), OWNER.bound()), (4 * PLACE, Some(4 * PLACE)), "one bound, the queues' bytes together");
        assert!(ra.try_recv().is_ok());
        assert!(b.try_send(Item(PLACE)).is_ok(), "what one took is room for the other");
        assert!(rb.try_recv().is_ok() && rb.try_recv().is_ok() && ra.try_recv().is_err());
        drop((a, b, ra, rb, room));
        assert_eq!((OWNER.bytes(), OWNER.bound()), (0, None));
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

    /// Goal 9, plan 01a1a87b2cec step 5: a send that finds room and a take that finds an item make no place for a wait
    /// (every send made one, and every blocking take one and a waker: allocations and frees on the hot path), and a take
    /// that does wait is still woken with its item.
    #[test]
    fn g9_s5_a_send_that_finds_room_or_a_take_that_finds_an_item_makes_no_place() -> anyhow::Result<()> {
        static OWNER: Owner = Owner::new("test_queue_no_place");
        const SENT: usize = 64;
        let (tx, rx) = bounded::<Item>(&OWNER, SENT * PLACE);
        let made = || PLACES_MADE.with(std::cell::Cell::get);
        let before = made();
        for _ in 0..SENT / 2 {
            anyhow::ensure!(futures::executor::block_on(tx.send(Item(PLACE))).is_ok(), "a send with room was refused");
            anyhow::ensure!(tx.send_blocking(Item(PLACE)).is_ok(), "a blocking send with room was refused");
        }
        for _ in 0..SENT / 2 {
            anyhow::ensure!(futures::executor::block_on(rx.recv()).is_ok(), "an item there wasn't taken");
            anyhow::ensure!(rx.recv_blocking().is_ok(), "an item there wasn't taken");
        }
        let places = made().saturating_sub(before);
        anyhow::ensure!(places == 0, "{places} places made for {SENT} sends and {SENT} takes that never waited");
        let taking = std::thread::spawn(move || rx.recv_blocking().map(|i| i.0).ok());
        std::thread::sleep(std::time::Duration::from_millis(50));
        anyhow::ensure!(tx.try_send(Item(PLACE + 1)).is_ok(), "a send to an empty queue was refused");
        anyhow::ensure!(taking.join().ok().flatten() == Some(PLACE + 1), "the waiting take wasn't woken with its item");
        Ok(())
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

    /// Goal 5, plan 01a1a59c2aae (its top goal), the runner's side, run by kiln's CI with every test: the runner
    /// process's queues on its paths are this module's, none an async-channel's, whose waits take
    /// event-listener's `Mutex` on the sender's shard (or thread) and the taker's: its loop's uplink, its own
    /// words, its jobs' results, its terminals' and sessions' output and its blue/green words (`agent.rs`,
    /// `exec.rs`), its sessions' lanes and hand-overs (`harness/host.rs`, `harness/handing.rs`), its
    /// harnesses' and steps' pipes on every platform (`os/proc_unix.rs`, `os/proc_windows.rs`), its terminals
    /// on every platform (`terminal.rs`, `os/pty_unix.rs`, `os/pty_windows.rs`, `runner/kept.rs`), its ports'
    /// bytes (`browser/ports.rs`), its sessions' control and outputs (`harness/session.rs`), and reach's side of
    /// it: every frame between runners into the node, what the node tells the server, a run's output, stdin
    /// and events, syncs, and `kiln run`'s and MCP `run`'s requests and events (`reach/node.rs`,
    /// `reach/serve.rs`, `reach/local.rs`, `reach/reuse.rs`).
    #[test]
    fn g5_the_runners_paths_make_no_async_channel() {
        let files = [
            ("agent.rs", include_str!("../agent.rs")),
            ("exec.rs", include_str!("../exec.rs")),
            ("harness/host.rs", include_str!("../harness/host.rs")),
            ("harness/session.rs", include_str!("../harness/session.rs")),
            ("harness/handing.rs", include_str!("../harness/handing.rs")),
            ("reach/node.rs", include_str!("../reach/node.rs")),
            ("reach/serve.rs", include_str!("../reach/serve.rs")),
            ("reach/local.rs", include_str!("../reach/local.rs")),
            ("reach/reuse.rs", include_str!("../reach/reuse.rs")),
            ("os/proc_unix.rs", include_str!("../os/proc_unix.rs")),
            ("os/proc_windows.rs", include_str!("../os/proc_windows.rs")),
            ("terminal.rs", include_str!("../terminal.rs")),
            ("os/pty_unix.rs", include_str!("../os/pty_unix.rs")),
            ("os/pty_windows.rs", include_str!("../os/pty_windows.rs")),
            ("runner/kept.rs", include_str!("../runner/kept.rs")),
            ("browser/ports.rs", include_str!("../browser/ports.rs")),
        ];
        for (file, src) in files {
            let code = code_of(src);
            for made in ["async_channel::bounded", "async_channel::unbounded"] {
                assert!(!code.contains(made), "{file} makes an async-channel, whose waits take a lock ({made})");
            }
        }
    }
}
