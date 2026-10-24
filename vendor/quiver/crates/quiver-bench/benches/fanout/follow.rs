//! What every system's run shares: the open-loop schedule, the stamp each message carries, and
//! each subscriber's check of the numbers it takes.

use std::sync::atomic::{AtomicU64, Ordering};

use hyper_measure::open_loop::Arrivals;

use crate::Args;
use crate::histogram::Histogram;

/// The number a priming message carries: one a publisher sends before its schedule, until every
/// subscriber has taken one, so every subscription stands at its node before the first message.
pub const PRIMING: u64 = u64::MAX;

/// Writes a message's scheduled time (nanoseconds from the run's base) and its number into its
/// first 16 bytes.
pub fn stamp(payload: &mut [u8], scheduled: u64, number: u64) {
    payload[..8].copy_from_slice(&scheduled.to_le_bytes());
    payload[8..16].copy_from_slice(&number.to_le_bytes());
}

/// A message's scheduled time and number, as [`stamp`] wrote them.
pub fn read(payload: &[u8]) -> Option<(u64, u64)> {
    let scheduled = u64::from_le_bytes(payload.get(..8)?.try_into().ok()?);
    let number = u64::from_le_bytes(payload.get(8..16)?.try_into().ok()?);
    Some((scheduled, number))
}

/// The measured window: a delivery of a message scheduled in `[from, until)` nanoseconds from the
/// run's base is timed. Set by the publisher as its schedule starts.
pub struct Window {
    from: AtomicU64,
    until: AtomicU64,
}

impl Window {
    pub fn new() -> Self {
        Self {
            from: AtomicU64::new(u64::MAX),
            until: AtomicU64::new(u64::MAX),
        }
    }

    fn contains(&self, scheduled: u64) -> bool {
        scheduled >= self.from.load(Ordering::Relaxed)
            && scheduled < self.until.load(Ordering::Relaxed)
    }
}

/// The publisher's open loop: messages arrive at `--rate` a second on a Poisson schedule from its
/// start, `--warmup` seconds before the window, `--seconds` in it and half a second past it, so
/// every measured message has one after it. Each is numbered from 0. Once the schedule's time is
/// over, what it still holds is never offered (as Focal's comparison rules): a publisher its
/// system held back stops there, and the window's messages it never got to count as undelivered.
pub struct Schedule {
    arrivals: Arrivals,
    from: u64,
    until: u64,
    end: u64,
    /// Messages scheduled so far.
    pub issued: u64,
    /// Messages the schedule held when its time was over.
    pub unoffered: u64,
    /// The numbers of the messages scheduled in the window: `[first, last)`.
    pub first: u64,
    pub last: u64,
}

impl Schedule {
    /// The schedule from `start` nanoseconds after the run's base; sets `window` to its window.
    pub fn new(a: &Args, seed: u64, start: u64, window: &Window) -> Self {
        let ns = |s: u64| s * 1_000_000_000;
        let from = start + ns(a.warmup);
        let until = from + ns(a.seconds);
        window.from.store(from, Ordering::Relaxed);
        window.until.store(until, Ordering::Relaxed);
        Self {
            arrivals: Arrivals::poisson(a.rate, seed, start).unwrap(),
            from,
            until,
            end: until + 500_000_000,
            issued: 0,
            unoffered: 0,
            first: 0,
            last: 0,
        }
    }

    /// The next message's scheduled time and number, or `None` once the schedule is over.
    pub fn next(&mut self) -> Option<(u64, u64)> {
        let at = self.arrivals.next_arrival().unwrap();
        if at >= self.end {
            return None;
        }
        let number = self.issued;
        self.issued += 1;
        if at < self.from {
            self.first = self.issued;
        }
        if at < self.until {
            self.last = self.issued;
        }
        Some((at, number))
    }

    /// The publisher is at `now` nanoseconds from the run's base: if the schedule's time is over,
    /// it numbers what it still holds as never offered and says so.
    pub fn over(&mut self, now: u64) -> bool {
        if now < self.end {
            return false;
        }
        while self.next().is_some() {
            self.unoffered += 1;
        }
        true
    }

    /// The measured window, nanoseconds from the run's base: `[from, until)`.
    pub fn window(&self) -> (u64, u64) {
        (self.from, self.until)
    }

    /// The messages published: those scheduled, less those never offered.
    pub fn offered(&self) -> u64 {
        self.issued - self.unoffered
    }

    /// Which minute of the run `at` is in.
    pub fn minute(at: u64) -> u64 {
        at / 60_000_000_000
    }
}

/// Gaps a subscriber's record holds before it grows: a subscriber that fell behind and was told so
/// records a gap, and its record growing then would count as its thread's allocation, which the
/// profile counts as the take's (a run that lags this often is no measure of its takes anyway).
const GAPS: usize = 256;

/// One subscriber's place in the numbers it takes.
pub struct Follow {
    /// The number it takes next.
    next: u64,
    /// The numbers it skipped, `[from, to)`, and whether a lag notice came before them.
    missed: Vec<(u64, u64, bool)>,
    /// A lag notice came, and no message since.
    lagged: bool,
    /// Numbers taken again, or out of order.
    pub doubled: u64,
    pub lags: u64,
    /// Its stream ended before the run did (its broker disconnected it).
    pub ended: bool,
}

/// What a subscriber missed of the window's messages.
#[derive(Default)]
pub struct Missed {
    /// Never taken.
    pub undelivered: u64,
    /// Never taken, with no lag notice before them: frames lost.
    pub silent: u64,
}

impl Follow {
    pub fn new() -> Self {
        Self {
            next: 0,
            missed: Vec::with_capacity(GAPS),
            lagged: false,
            doubled: 0,
            lags: 0,
            ended: false,
        }
    }

    pub fn next(&self) -> u64 {
        self.next
    }

    pub fn lag(&mut self) {
        self.lags += 1;
        self.lagged = true;
    }

    /// Took message `number`: whether it was new (a number taken again, or out of order, is
    /// counted doubled and timed no more).
    fn took(&mut self, number: u64) -> bool {
        if number < self.next {
            self.doubled += 1;
            return false;
        }
        if number > self.next {
            self.missed.push((self.next, number, self.lagged));
        }
        self.lagged = false;
        self.next = number + 1;
        true
    }

    /// What it never took of the window's numbers, `[first, last)`, of which the first `offered`
    /// were published: what it skipped and what was published that it hadn't taken when the run
    /// ended, each lost unless a lag notice came before it; and what was never offered (its
    /// system held the publisher back), undelivered but no frame lost.
    pub fn missed(&self, first: u64, last: u64, offered: u64) -> Missed {
        let overlap = |a: u64, b: u64| b.min(last).saturating_sub(a.max(first));
        let mut m = Missed::default();
        let published = offered.max(self.next);
        let tail = (self.next, published, self.lagged);
        for &(a, b, told) in self.missed.iter().chain(std::iter::once(&tail)) {
            let n = overlap(a, b);
            m.undelivered += n;
            if !told {
                m.silent += n;
            }
        }
        m.undelivered += overlap(published, last);
        m
    }

    /// It took every message published, in order, once, and was never told it fell behind.
    pub fn kept_up(&self, published: u64) -> bool {
        self.missed.is_empty() && self.next >= published && self.doubled == 0 && self.lags == 0
    }
}

/// What one thread's subscribers took.
pub struct Seen {
    pub latency: Histogram,
    pub deliveries: u64,
    pub measured: u64,
    /// Messages too short to carry a stamp: none should be.
    pub unreadable: u64,
}

/// What a take was.
#[derive(PartialEq, Eq)]
pub enum Taken {
    Message,
    Priming,
    Unreadable,
}

impl Seen {
    pub fn new() -> Self {
        Self {
            latency: Histogram::new(),
            deliveries: 0,
            measured: 0,
            unreadable: 0,
        }
    }

    /// `follow`'s subscriber took `payload` at `at` nanoseconds from the run's base.
    pub fn take(&mut self, follow: &mut Follow, payload: &[u8], at: u64, window: &Window) -> Taken {
        let Some((scheduled, number)) = read(payload) else {
            self.unreadable += 1;
            return Taken::Unreadable;
        };
        if number == PRIMING {
            return Taken::Priming;
        }
        if !follow.took(number) {
            return Taken::Message;
        }
        self.deliveries += 1;
        crate::profile::delivered();
        // A test build's planted allocation, one a delivery (bench/plant.sh): the profile must
        // show it, whichever thread the delivery is on.
        #[cfg(feature = "plant")]
        drop(std::hint::black_box(Box::new(number)));
        if window.contains(scheduled) {
            self.latency.record(at.saturating_sub(scheduled));
            self.measured += 1;
        }
        Taken::Message
    }

    pub fn merge(&mut self, other: &Self) {
        self.latency.merge(&other.latency);
        self.deliveries += other.deliveries;
        self.measured += other.measured;
        self.unreadable += other.unreadable;
    }
}
