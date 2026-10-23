//! The ephemeral class on one node (Focal's doc 33, §4.2): a topic's messages in this process's
//! memory, lost with it, every gap a reader meets announced.
//!
//! A topic is a log of segments, each a run of up to [`SEGMENT_SLOTS`] messages filled in order and
//! never changed after. A publish frames its message, takes the topic's lock, gives it the next
//! offset and fills the next slot. A subscription reads without it; a read from an offset and a
//! look at a topic's bounds take it only to find their segment. A subscription holds the one
//! segment it reads (an `Arc`) and reaches the next by a weak link, so what it reads is shared,
//! never copied, and one that stalls keeps alive its own segment and none after it. Each message
//! is kept as a Focal batch of one record (`focal_bus::batch`: the bytes Focal's bus appends,
//! replicates and delivers), its CRC computed once as it's published, and its bytes are charged to
//! the node's memory budget (`focal_memory`) until the last reader lets its segment go.
//!
//! The topic keeps its newest messages, up to its retention in messages and in bytes; past it the
//! oldest are let go, and a segment whose messages are all let go leaves the log. A reader behind
//! what's kept is told so: a read is refused [`Refusal::Behind`], a subscription takes a lag notice
//! ([`Next::Lag`]) and goes on from the oldest kept. A publish waits for no reader, so a stalled
//! one holds up no one but itself.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::task::{Context, Poll};

use event_listener::{Event, EventListener};
use focal_bus::batch::{Batch as FocalBatch, BatchBuilder, Producing};
use focal_memory::{Allocation, BudgetKind, BudgetLane, MemoryBudget};
use quiver_api::{Batch, MessageRef, Next, Offset, Refusal, Start, Subscription, Topic};

/// Messages a segment holds at most. A subscription moves to the next segment, the one step that
/// touches a count other readers share (the next segment's `Arc`), once each this many messages.
pub const SEGMENT_SLOTS: usize = 256;
/// The bytes after which a segment takes no more messages: a stalled subscription keeps alive at
/// most this and one largest message ([`quiver_api::PAYLOAD_MAX`]) and its batch's framing.
pub const SEGMENT_BYTES: usize = 64 * 1024;

/// A message as kept: its Focal batch, where its key and payload are in it, and its charge.
struct Stored {
    /// A Focal batch of one record, the record Quiver's: the key's length plus one as a LEB128
    /// (0: no key), the key, then the payload.
    batch: Box<[u8]>,
    /// Where the key is in `batch` (its start and length), when there is one.
    key: Option<(usize, usize)>,
    /// Where the payload starts in `batch`: it runs to the end.
    payload: usize,
    at_us: u64,
    /// What it counts against its topic's retention: its key's and payload's bytes.
    size: u64,
    /// Its bytes, charged to the node's budget while any reader can still reach it.
    _charge: Allocation,
}

impl Stored {
    /// `key` and `payload` framed as a Focal batch of one record for `stream`, charged to `budget`.
    fn frame(
        budget: &MemoryBudget,
        stream: u64,
        at_us: u64,
        key: Option<&[u8]>,
        payload: &[u8],
        size: u64,
    ) -> Result<Self, Refusal> {
        let key_len = key.map_or(0, <[u8]>::len);
        let head = key.map_or(0, |k| {
            u64::try_from(k.len()).unwrap_or(u64::MAX).saturating_add(1)
        });
        let mut record = Vec::with_capacity(
            key_len
                .saturating_add(payload.len())
                .saturating_add(VARINT_MAX),
        );
        push_varint(&mut record, head);
        let head_len = record.len();
        record.extend_from_slice(key.unwrap_or_default());
        record.extend_from_slice(payload);
        let mut builder = BatchBuilder::new();
        builder.begin(&Producing {
            stream,
            producer: 0,
            sequence: 0,
            flags: 0,
            timestamp: at_us,
        });
        let built = builder
            .push(at_us, &record)
            .and_then(|()| builder.finish())
            .map_err(|e| framing(&format!("as a Focal batch: {e}")))?;
        let charge = budget
            .reserve(BudgetKind::Payload, BudgetLane::Ordinary, built.len())
            .map_err(|_| Refusal::Full {
                what: "the node's memory",
                limit: u64::try_from(budget.limit()).unwrap_or(u64::MAX),
            })?
            .commit();
        let batch: Box<[u8]> = built.into();
        // The record ends the batch: its head, its key, then its payload.
        let record_at = batch
            .len()
            .checked_sub(record.len())
            .ok_or_else(|| framing("the record is longer than its batch"))?;
        let key_at = record_at
            .checked_add(head_len)
            .ok_or_else(|| framing("the key is past the batch"))?;
        let payload_at = key_at
            .checked_add(key_len)
            .ok_or_else(|| framing("the payload is past the batch"))?;
        let stored = Self {
            batch,
            key: key.map(|_| (key_at, key_len)),
            payload: payload_at,
            at_us,
            size,
            _charge: charge,
        };
        // What a reader takes is what was published: checked once, here, against the bytes kept.
        if stored.key_bytes() != key
            || stored.payload_bytes() != Some(payload)
            || stored.focal().is_none()
        {
            return Err(framing("the batch doesn't hold the message as published"));
        }
        Ok(stored)
    }

    fn key_bytes(&self) -> Option<&[u8]> {
        self.key
            .and_then(|(at, len)| self.batch.get(at..at.checked_add(len)?))
    }

    fn payload_bytes(&self) -> Option<&[u8]> {
        self.batch.get(self.payload..)
    }

    fn message_ref(&self, offset: Offset) -> MessageRef<'_> {
        // `frame` checked both ranges against the bytes they index, which never change after.
        MessageRef {
            offset,
            key: self.key_bytes(),
            payload: self.payload_bytes().unwrap_or_default(),
            at_us: self.at_us,
        }
    }

    /// The Focal batch it's kept as.
    fn focal(&self) -> Option<FocalBatch<'_>> {
        FocalBatch::parse_verified(&self.batch, self.batch.len()).ok()
    }
}

/// The most bytes a LEB128 of a u64 takes.
const VARINT_MAX: usize = 10;

fn framing(why: &str) -> Refusal {
    Refusal::Invalid {
        what: "a message's framing",
        why: why.to_owned(),
    }
}

fn push_varint(out: &mut Vec<u8>, mut value: u64) {
    for _ in 0..VARINT_MAX {
        let low = u8::try_from(value & 0x7f).unwrap_or(0);
        value >>= 7;
        if value == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// A run of messages, filled in order and never changed after.
struct Segment {
    /// The offset of its first slot.
    base: Offset,
    slots: Box<[OnceLock<Stored>]>,
    /// The segment after it, set once that one's first message is in: weak, so a reader holding
    /// this one keeps no later one alive.
    next: OnceLock<Weak<Segment>>,
    /// The first of its offsets the topic still keeps: a reader below it is behind. Once the topic
    /// lets the whole segment go, the next segment's base.
    kept_from: AtomicU64,
}

impl Segment {
    fn new(base: Offset) -> Self {
        Self {
            base,
            slots: (0..SEGMENT_SLOTS).map(|_| OnceLock::new()).collect(),
            next: OnceLock::new(),
            kept_from: AtomicU64::new(base),
        }
    }

    fn slot(&self, offset: Offset) -> Option<&OnceLock<Stored>> {
        let index = usize::try_from(offset.checked_sub(self.base)?).ok()?;
        self.slots.get(index)
    }

    fn stored(&self, offset: Offset) -> Option<&Stored> {
        self.slot(offset).and_then(OnceLock::get)
    }
}

/// What a topic keeps, under its lock.
struct Tail {
    /// Kept segments, oldest first; the last takes the next message. Never empty.
    segments: VecDeque<Arc<Segment>>,
    /// Messages in the last segment, and their batches' bytes.
    filled: usize,
    filled_bytes: usize,
    /// The first offset kept.
    first: Offset,
    /// The offset the next message takes.
    end: Offset,
    /// Bytes the kept messages count against the retention.
    bytes: u64,
    /// The last message's stamp.
    last_at_us: u64,
}

/// One topic of the ephemeral class.
pub(crate) struct Log {
    pub(crate) topic: Topic,
    /// Its Focal stream: the number its batches carry.
    stream: u64,
    tail: Mutex<Tail>,
    /// The offset the next message takes, as last published: read without the lock.
    end: AtomicU64,
    /// Rung at each publish and at the topic's close.
    bell: Event,
    closed: AtomicBool,
    budget: MemoryBudget,
    /// Messages published, and lag notices its subscriptions took.
    pub(crate) published: AtomicU64,
    pub(crate) lags: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// No segment where one is always kept: the node's own fault, said as such.
fn no_segment() -> Refusal {
    Refusal::Unavailable("a topic kept no segment (Quiver's fault)".to_owned())
}

impl Log {
    pub(crate) fn new(topic: Topic, stream: u64, budget: MemoryBudget) -> Self {
        let tail = Tail {
            segments: VecDeque::from([Arc::new(Segment::new(0))]),
            filled: 0,
            filled_bytes: 0,
            first: 0,
            end: 0,
            bytes: 0,
            last_at_us: 0,
        };
        Self {
            topic,
            stream,
            tail: Mutex::new(tail),
            end: AtomicU64::new(0),
            bell: Event::new(),
            closed: AtomicBool::new(false),
            budget,
            published: AtomicU64::new(0),
            lags: AtomicU64::new(0),
        }
    }

    /// The first offset kept and the next to be given.
    pub(crate) fn bounds(&self) -> (Offset, Offset) {
        let t = lock(&self.tail);
        (t.first, t.end)
    }

    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Keeps a message, returning its offset: never waits for a reader. It's stamped `now_us`, or
    /// the topic's last stamp if that's later, so stamps rise with offsets whatever the clock does.
    pub(crate) fn publish(
        &self,
        name: &str,
        key: Option<&[u8]>,
        payload: &[u8],
        now_us: u64,
    ) -> Result<Offset, Refusal> {
        let size = quiver_api::check_message(&self.topic, key, payload)?;
        let offset = {
            let mut t = lock(&self.tail);
            if self.closed() {
                return Err(Refusal::NoTopic(name.to_owned()));
            }
            let at_us = now_us.max(t.last_at_us);
            let mut framed = Stored::frame(&self.budget, self.stream, at_us, key, payload, size);
            // The node's memory is full: this topic's oldest segments go, one at a time (its readers
            // are told what they missed), until the budget takes the message or the topic keeps
            // nothing. Each turn lets a segment go, so there are at most as many as it kept.
            while matches!(framed, Err(Refusal::Full { .. })) && self.let_go_oldest(&mut t) {
                framed = Stored::frame(&self.budget, self.stream, at_us, key, payload, size);
            }
            let offset = self.keep(&mut t, framed?)?;
            t.last_at_us = at_us;
            offset
        };
        self.published.fetch_add(1, Ordering::Relaxed);
        self.bell.notify(usize::MAX);
        Ok(offset)
    }

    /// Puts `stored` in the next slot, then lets the oldest go past the retention.
    fn keep(&self, t: &mut Tail, stored: Stored) -> Result<Offset, Refusal> {
        let offset = t.end;
        let end = offset.checked_add(1).ok_or(Refusal::Full {
            what: "the topic's offsets",
            limit: Offset::MAX,
        })?;
        let stored_bytes = stored.batch.len();
        let size = stored.size;
        let fits = t.filled < SEGMENT_SLOTS
            && (t.filled == 0 || t.filled_bytes.saturating_add(stored_bytes) <= SEGMENT_BYTES);
        if fits {
            let last = t.segments.back().ok_or_else(no_segment)?;
            if last
                .slot(offset)
                .is_none_or(|slot| slot.set(stored).is_err())
            {
                return Err(Refusal::Unavailable(
                    "a message's slot was taken (Quiver's fault)".to_owned(),
                ));
            }
            t.filled = t.filled.saturating_add(1);
            t.filled_bytes = t.filled_bytes.saturating_add(stored_bytes);
        } else {
            // A new segment, its first message in before any reader can reach it.
            let segment = Arc::new(Segment::new(offset));
            if segment
                .slot(offset)
                .is_none_or(|slot| slot.set(stored).is_err())
            {
                return Err(Refusal::Unavailable(
                    "a new segment's first slot was taken (Quiver's fault)".to_owned(),
                ));
            }
            if let Some(last) = t.segments.back() {
                let _ = last.next.set(Arc::downgrade(&segment));
            }
            t.segments.push_back(segment);
            t.filled = 1;
            t.filled_bytes = stored_bytes;
        }
        t.end = end;
        t.bytes = t.bytes.saturating_add(size);
        self.let_go_past_retention(t);
        self.end.store(t.end, Ordering::Release);
        Ok(offset)
    }

    /// Lets the oldest messages go while the topic keeps more than its retention, in messages or
    /// bytes. The newest stays (`check_message` refused one the retention can't keep alone), and
    /// each message goes once, so the work is at most what was kept.
    fn let_go_past_retention(&self, t: &mut Tail) {
        loop {
            let kept = t.end.saturating_sub(t.first);
            if kept <= 1
                || (kept <= self.topic.retain_messages && t.bytes <= self.topic.retain_bytes)
            {
                return;
            }
            let Some(front) = t.segments.front().cloned() else {
                return;
            };
            t.bytes = t
                .bytes
                .saturating_sub(front.stored(t.first).map_or(0, |s| s.size));
            t.first = t.first.saturating_add(1);
            front.kept_from.store(t.first, Ordering::Release);
            // The front segment leaves once none of its messages is kept (another takes the next).
            if let Some(second) = t.segments.get(1).map(|s| s.base)
                && t.first >= second
                && let Some(gone) = t.segments.pop_front()
            {
                gone.kept_from.store(second, Ordering::Release);
            }
        }
    }

    /// With the node's memory full: the oldest segment's messages are let go, and it leaves the
    /// log (what it held comes back as its last reader lets it go). The last segment goes too when
    /// it's the one left, a new one taking the next message, linked from it so a reader at the end
    /// goes on. False when the topic keeps nothing to let go.
    fn let_go_oldest(&self, t: &mut Tail) -> bool {
        if t.first >= t.end {
            return false;
        }
        let Some(front) = t.segments.front().cloned() else {
            return false;
        };
        let until = match t.segments.get(1) {
            Some(second) => second.base,
            None => t.end,
        };
        // The bytes its kept messages counted: at most a segment's slots of them.
        let mut at = t.first;
        while at < until {
            t.bytes = t
                .bytes
                .saturating_sub(front.stored(at).map_or(0, |s| s.size));
            at = at.saturating_add(1);
        }
        front.kept_from.store(until, Ordering::Release);
        t.first = until;
        if t.segments.len() > 1 {
            t.segments.pop_front();
        } else {
            let fresh = Arc::new(Segment::new(t.end));
            let _ = front.next.set(Arc::downgrade(&fresh));
            t.segments.clear();
            t.segments.push_back(fresh);
            t.filled = 0;
            t.filled_bytes = 0;
            t.bytes = 0;
        }
        true
    }

    /// Closes the topic: its subscriptions take the end, its waits are refused, and its segments
    /// leave (one a reader holds goes when it lets it go).
    pub(crate) fn close(&self) {
        {
            let mut t = lock(&self.tail);
            self.closed.store(true, Ordering::Release);
            let end = t.end;
            t.segments.clear();
            t.segments.push_back(Arc::new(Segment::new(end)));
            t.filled = 0;
            t.filled_bytes = 0;
            t.first = t.end;
            t.bytes = 0;
        }
        self.bell.notify(usize::MAX);
    }

    /// Where a reader at `offset` starts: the offset (the first kept, if it's gone), the segment
    /// holding it (the last, at the end), and the end.
    fn locate(&self, offset: Offset) -> Result<(Offset, Arc<Segment>, Offset), Refusal> {
        let t = lock(&self.tail);
        let start = offset.max(t.first).min(t.end);
        let at = t
            .segments
            .partition_point(|s| s.base <= start)
            .saturating_sub(1);
        let segment = t.segments.get(at).cloned().ok_or_else(no_segment)?;
        Ok((start, segment, t.end))
    }

    /// The messages from `from`, at most `limit`: copied out of the segments without the lock.
    pub(crate) fn read(&self, from: Offset, limit: usize) -> Result<Batch, Refusal> {
        let limit = quiver_api::check_limit(limit)?;
        let (start, mut segment, end) = self.locate(from)?;
        if start > from {
            return Err(Refusal::Behind {
                asked: from,
                first: start,
            });
        }
        let mut messages = Vec::new();
        let mut at = start;
        // At most `limit` messages, and a step onto each segment between.
        while messages.len() < limit && at < end {
            if let Some(stored) = segment.stored(at) {
                messages.push(stored.message_ref(at).to_message());
                at = at.saturating_add(1);
                continue;
            }
            match segment.next.get().and_then(Weak::upgrade) {
                Some(next) if next.base <= at => segment = next,
                _ => break,
            }
        }
        Ok(Batch { messages, next: at })
    }

    /// Completes once a message at `from` or later is there; refused once the topic is closed.
    pub(crate) async fn wait(&self, name: &str, from: Offset) -> Result<(), Refusal> {
        loop {
            // Listening first, then looking: a publish between the two is still heard.
            let listener = self.bell.listen();
            if self.closed() {
                return Err(Refusal::NoTopic(name.to_owned()));
            }
            if self.end.load(Ordering::Acquire) > from {
                return Ok(());
            }
            listener.await;
        }
    }

    /// A subscription from `start`, counted in `live` (given back as it's dropped).
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        start: Start,
        live: Arc<AtomicUsize>,
    ) -> Result<EphemeralSubscription, Refusal> {
        let (first, end) = self.bounds();
        let next = start.at(first, end);
        let (_, segment, _) = self.locate(next)?;
        Ok(EphemeralSubscription {
            log: Arc::clone(self),
            segment,
            next,
            listener: None,
            live,
        })
    }
}

/// A subscription to an ephemeral topic: it reads the segments without the topic's lock.
pub(crate) struct EphemeralSubscription {
    log: Arc<Log>,
    /// The segment holding `next`, or the last one before it while `next` isn't published.
    segment: Arc<Segment>,
    next: Offset,
    listener: Option<EventListener>,
    live: Arc<AtomicUsize>,
}

/// What a subscription meets at its offset.
enum Found {
    Message,
    /// The segment after the one it holds, which holds its offset.
    Moved(Arc<Segment>),
    /// Its offset is gone, or the segment it would go on to.
    Behind,
    Nothing,
    Closed,
}

impl EphemeralSubscription {
    fn find(&self) -> Found {
        if self.log.closed() {
            return Found::Closed;
        }
        if self.next < self.segment.kept_from.load(Ordering::Acquire) {
            return Found::Behind;
        }
        if self.segment.stored(self.next).is_some() {
            return Found::Message;
        }
        // Not in this segment: in the next one, once there is one (its first message is in before
        // it's linked), unless the topic let it go.
        match self.segment.next.get().map(Weak::upgrade) {
            None => Found::Nothing,
            Some(Some(next)) if next.base <= self.next => Found::Moved(next),
            Some(_) => Found::Behind,
        }
    }
}

impl Drop for EphemeralSubscription {
    fn drop(&mut self) {
        // Its place, taken once as it was made (`Node::subscribe`), given back once.
        self.live.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Subscription for EphemeralSubscription {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let has = |s: &Self| !matches!(s.find(), Found::Nothing);
        if has(self) {
            self.listener = None;
            return Poll::Ready(());
        }
        let listener = self.listener.get_or_insert_with(|| self.log.bell.listen());
        match Pin::new(listener).poll(cx) {
            Poll::Ready(()) => {
                self.listener = None;
                if has(self) {
                    return Poll::Ready(());
                }
                // Nothing for it after all: listen again, and be polled again to wait on it.
                self.listener = Some(self.log.bell.listen());
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            // Listening now: a publish between the look above and the listen is caught here.
            Poll::Pending if has(self) => {
                self.listener = None;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn take(&mut self) -> Option<Next<'_>> {
        // At most three steps: found behind and placed again, onto the next segment, its message.
        for _ in 0..3 {
            match self.find() {
                Found::Closed => return Some(Next::Ended),
                Found::Nothing => return None,
                Found::Moved(next) => self.segment = next,
                Found::Behind => {
                    let from = self.next;
                    let Ok((start, segment, _)) = self.log.locate(from) else {
                        return Some(Next::Ended);
                    };
                    self.segment = segment;
                    self.next = start;
                    if start > from {
                        self.log.lags.fetch_add(1, Ordering::Relaxed);
                        return Some(Next::Lag { from, to: start });
                    }
                }
                Found::Message => {
                    let offset = self.next;
                    self.next = offset.checked_add(1)?;
                    return self
                        .segment
                        .stored(offset)
                        .map(|s| Next::Message(s.message_ref(offset)));
                }
            }
        }
        None
    }

    fn position(&self) -> Offset {
        self.next
    }
}

/// The stream, record count and record a message at `offset` of `log` is kept as: a test's look
/// at the Focal batch.
#[cfg(test)]
pub(crate) fn focal_batch_of(log: &Log, offset: Offset) -> Option<(u64, u16, Vec<u8>)> {
    let (_, segment, _) = log.locate(offset).ok()?;
    let stored = segment.stored(offset)?;
    let batch = stored.focal()?;
    let record = batch.records().next()?.ok()?;
    Some((batch.stream(), batch.count(), record.payload.to_vec()))
}
