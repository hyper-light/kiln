//! A connection's writes within its poll's turn on its shard (kiln's goal 9, plan 01a1a87b2cec step 3: no poll past the
//! shard's step quantum). What a connection writes in one poll is what fits what's left of that poll's quantum, at what
//! its writes are learned to cost a byte ([`Writes`]); the rest goes in its next poll, the shard given to its other tasks
//! between. An answer whose handler spent most of a poll (a list joined, a page's texts gathered) was written in that poll
//! all the same: in kiln's lab the agent sessions' reads held their shard 0.94 ms on the CPU at the longest calm, 1.62 ms
//! beside CPU hogs (g9p35, train 35), their writes 35 to 180 µs of it on a Mac.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// What a write to a transport is expected to take, by its bytes: moving averages of each write's bytes and of its time,
/// nanoseconds (a new sample weighing 1/8, as RFC 6298 smooths a round trip, §2.3), and of how far each fell from what was
/// expected of it (weighing 1/4, as RFC 6298 smooths a round trip's variation), learned from the writes themselves. Every
/// connection's writes teach it, each with loads and stores: a sample one loses to another at the same moment is no harm
/// to an average.
#[derive(Debug)]
pub struct Writes {
    bytes: AtomicU64,
    ns: AtomicU64,
    dev: AtomicU64,
}

impl Writes {
    /// A [`Writes`] with nothing learned.
    pub const fn nothing_yet() -> Self {
        Self { bytes: AtomicU64::new(0), ns: AtomicU64::new(0), dev: AtomicU64::new(0) }
    }

    /// Nanoseconds a write of `len` bytes is expected to take: it grows with its length at the rate the writes so far took
    /// (their fixed cost in it, so a long one is expected to take longer than it does, not shorter); none until one was
    /// timed.
    fn expected(&self, len: u64) -> Option<u64> {
        len.saturating_mul(self.ns.load(Relaxed)).checked_div(self.bytes.load(Relaxed))
    }

    /// The most bytes a write may carry and still be expected to end within `room_ns`: at the rate the writes so far took,
    /// after four times how far they fall from it (RFC 6298's bound on a round trip, K = 4, §2.3); none until one was
    /// timed (the first write goes whole, and teaches).
    pub fn fitting(&self, room_ns: u64) -> Option<usize> {
        let (bytes, ns) = (self.bytes.load(Relaxed), self.ns.load(Relaxed));
        if bytes == 0 || ns == 0 {
            return None;
        }
        let left = room_ns.saturating_sub(self.dev.load(Relaxed).saturating_mul(4));
        Some(usize::try_from(left.saturating_mul(bytes) / ns).unwrap_or(usize::MAX))
    }

    /// A write of `len` bytes took `took` nanoseconds.
    pub fn learn(&self, len: u64, took: u64) {
        if len == 0 {
            return;
        }
        // How far it fell from what was expected of it, taken before what it teaches, as RFC 6298 (§2.3) takes a round
        // trip's variation before its mean: x ← x + (|sample − expected| − x)/4.
        if let Some(expected) = self.expected(len) {
            let was = self.dev.load(Relaxed);
            self.dev.store((was - was / 4).saturating_add(took.abs_diff(expected) / 4), Relaxed);
        }
        // x ← x + (sample − x)/8, in whole bytes and nanoseconds.
        let smooth = |avg: u64, sample: u64| if avg == 0 { sample } else { (avg - avg / 8).saturating_add(sample / 8) };
        self.bytes.store(smooth(self.bytes.load(Relaxed), len), Relaxed);
        self.ns.store(smooth(self.ns.load(Relaxed), took.max(1)), Relaxed);
    }
}

/// What every connection's writes are learned to cost ([`Writes`]).
pub(crate) static WRITES: Writes = Writes::nothing_yet();

/// What's left of the shard's step quantum after what the poll a write is in has held its shard for, nanoseconds
/// (`hyper_rt::futures::poll_clock`). None off a shard (a thread of its own, a test's), where a write is never held back.
pub fn room() -> Option<u64> {
    let poll = hyper_rt::futures::poll_clock()?;
    Some(quantum()?.saturating_sub(poll.held_ns()))
}

/// The quantum a whole poll has: the shard's step quantum (`hyper_rt::futures::step_budget_ns`).
pub fn quantum() -> Option<u64> {
    hyper_rt::futures::step_budget_ns()
}
