//! A connection's writes within its poll's turn on its shard (kiln's goal 9, plan 01a1a87b2cec step 3: no poll past the
//! shard's step quantum). What a connection writes in one poll is what fits what's left of that poll's quantum by what its
//! writes are learned to cost ([`Writes`]); the rest goes in its next poll, the shard given to its other tasks between. An
//! answer whose handler spent most of a poll (a list joined, a page's texts gathered) was written in that poll all the
//! same: in kiln's lab the agent sessions' reads held their shard 0.94 ms on the CPU at the longest calm, 1.62 ms beside
//! CPU hogs (g9p35, train 35), their writes 35 to 180 µs of it on a Mac.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The powers of two a [`Costs`] keeps apart, from 2^[`LOWEST`] nanoseconds a unit: a byte written (fractions of a
/// nanosecond) to a job (seconds). A cost past either end goes in that end's bucket, which keeps its sum and its dearest,
/// so it is still reckoned as it was.
const BUCKETS: usize = 48;
const LOWEST: i32 = -16;

/// What a kind of work costs a unit (nanoseconds a byte written, a job done), a relative-error sketch by powers of two
/// (DDSketch: Masson, Rim and Lee, VLDB 2019): each power keeps the units that cost that much, their nanoseconds and the
/// dearest and cheapest of them. No gain is picked: every `window` samples each bucket is halved, so the last window weighs as
/// much as all before it, and the window is as many samples as a mean of these costs' own spread needs to come within the
/// runtime's precision (`hyper_rt::machine::wake::sample_window`, which sizes the shard's own quantum estimate), the spread
/// the most its buckets can hold at each halving. Its callers
/// learn into it with loads and stores: a sample lost to another's at the same moment is one of many (goal 9, plan
/// 01a1a87b2cec step 3).
/// Laid out so a sample stores to two cache lines, its totals' and its bucket's.
#[derive(Debug)]
#[repr(C, align(64))]
pub struct Costs {
    units: AtomicU64,
    ns: AtomicU64,
    /// Samples since the buckets were last halved, and how many they're halved at.
    taken: AtomicU64,
    window: AtomicU64,
    buckets: [Bucket; BUCKETS],
}

#[derive(Debug)]
#[repr(C, align(32))]
struct Bucket {
    units: AtomicU64,
    ns: AtomicU64,
    /// The dearest a unit of it cost, and the cheapest, f64 bits: the cheapest set by the first unit after it held none.
    dearest: AtomicU64,
    cheapest: AtomicU64,
}

impl Costs {
    /// A [`Costs`] with nothing learned, its first window the fewest samples the runtime's probes keep
    /// (`hyper_rt::machine::bench::MIN_SAMPLES`).
    pub const fn nothing_yet() -> Self {
        Self {
            units: AtomicU64::new(0),
            ns: AtomicU64::new(0),
            taken: AtomicU64::new(0),
            window: AtomicU64::new(hyper_rt::machine::bench::MIN_SAMPLES as u64),
            buckets: [const { Bucket { units: AtomicU64::new(0), ns: AtomicU64::new(0), dearest: AtomicU64::new(0), cheapest: AtomicU64::new(0) } }; BUCKETS],
        }
    }

    /// `units` units took `ns` nanoseconds.
    pub fn learn(&self, units: u64, ns: u64) {
        if units == 0 {
            return;
        }
        let cost = ns as f64 / units as f64;
        if let Some(b) = self.buckets.get(bucket(cost)) {
            let held = b.units.load(Relaxed);
            b.units.store(held.saturating_add(units), Relaxed);
            add(&b.ns, ns);
            if cost > f64::from_bits(b.dearest.load(Relaxed)) {
                b.dearest.store(cost.to_bits(), Relaxed);
            }
            if held == 0 || cost < f64::from_bits(b.cheapest.load(Relaxed)) {
                b.cheapest.store(cost.to_bits(), Relaxed);
            }
        }
        add(&self.units, units);
        add(&self.ns, ns);
        let taken = self.taken.load(Relaxed).saturating_add(1);
        self.taken.store(taken, Relaxed);
        if taken >= self.window.load(Relaxed) {
            self.halve();
        }
    }

    /// Every bucket halved, and the next window what's left of them asks for: their spread taken from the buckets alone, so
    /// a sample a halving let go takes its share of the spread with it (summed apart from the buckets, one outlier's square
    /// outlived its bucket and held the window at hundreds of thousands of samples: goal 9, plan 01a1a87b2cec step 3).
    fn halve(&self) {
        let (mut units, mut ns, mut squares) = (0u64, 0u64, 0.0f64);
        for b in &self.buckets {
            let all = b.units.load(Relaxed);
            let u = all / 2;
            // Half its units, with their nanoseconds at its mean, which stays what it was; emptied, its dearest goes too.
            let n = u64::try_from(u128::from(b.ns.load(Relaxed)) * u128::from(u) / u128::from(all.max(1))).unwrap_or(u64::MAX);
            b.units.store(u, Relaxed);
            b.ns.store(n, Relaxed);
            if u == 0 {
                b.dearest.store(0, Relaxed);
                continue;
            }
            // The most its units' squares can sum to, a mean m between its cheapest c and dearest d: u (m (c + d) − c d), the
            // widest spread those allow (Bhatia and Davis, Amer. Math. Monthly 107(4), 2000), and never under u m².
            let (m, c, d) = (n as f64 / u as f64, f64::from_bits(b.cheapest.load(Relaxed)), f64::from_bits(b.dearest.load(Relaxed)));
            squares += u as f64 * (m * (c + d) - c * d).max(m * m);
            units = units.saturating_add(u);
            ns = ns.saturating_add(n);
        }
        self.units.store(units, Relaxed);
        self.ns.store(ns, Relaxed);
        self.taken.store(0, Relaxed);
        // The spread and the mean, each times the units: the window takes only their ratio.
        let spread = (squares * units as f64 - (ns as f64).powi(2)).max(0.0).sqrt();
        self.window.store(hyper_rt::machine::wake::sample_window(ns, spread as u64), Relaxed);
    }

    /// What a unit is expected to cost, nanoseconds: none until one was learned.
    pub fn mean(&self) -> Option<f64> {
        let units = self.units.load(Relaxed);
        (units > 0).then(|| self.ns.load(Relaxed) as f64 / units as f64)
    }

    /// The dearest a unit cost of those its window holds, nanoseconds: what [`Costs::fit`] holds a unit to with nothing to
    /// put off (`fit(free, 0)` is at least one where it's within `free`); none until one was learned.
    pub fn dearest(&self) -> Option<f64> {
        self.buckets.iter().rev().find(|b| b.units.load(Relaxed) > 0).map(|b| f64::from_bits(b.dearest.load(Relaxed)))
    }

    /// The cheapest and the dearest a unit cost of those its window holds, nanoseconds: none until one was learned.
    pub fn range(&self) -> Option<(f64, f64)> {
        let low = self.buckets.iter().find(|b| b.units.load(Relaxed) > 0)?;
        Some((f64::from_bits(low.cheapest.load(Relaxed)), self.dearest()?))
    }

    /// The rule work is weighed by: the most units whose cost past `free` nanoseconds (negative: already past by that) is
    /// expected to be no more than `defer`, what putting the rest off costs; none until a cost was learned. Each bucket's
    /// units are reckoned at its dearest and at its floor, in the shares that keep their nanoseconds: the widest spread they
    /// could have, so what's past `free` is never reckoned short.
    pub fn fit(&self, free: i64, defer: u64) -> Option<u64> {
        let units = self.units.load(Relaxed);
        if units == 0 {
            return None;
        }
        let (free, budget) = (free as f64, defer as f64 * units as f64);
        let mut points = self.buckets.iter().enumerate().rev().filter_map(|(i, b)| b.points(i)).flatten().peekable();
        // The units of the dearest points so far, and their nanoseconds: at n units, those past `free` cost
        // n·past_ns − free·past past it, and the n that makes that the budget holds where the next point stays within.
        let (mut past, mut past_ns) = (0.0, 0.0);
        while let Some((cost, u)) = points.next() {
            past += u;
            past_ns += u * cost;
            let n = (budget + free * past) / past_ns;
            if points.peek().is_none_or(|p| n * p.0 <= free) {
                return Some(n as u64);
            }
        }
        None
    }
}

impl Default for Costs {
    fn default() -> Self {
        Self::nothing_yet()
    }
}

impl Bucket {
    /// Its units as two points, (cost, units), the dearest first: at its dearest and at its floor (the `i`th's), in the
    /// shares that keep its nanoseconds. None while it holds none.
    fn points(&self, i: usize) -> Option<[(f64, f64); 2]> {
        let units = self.units.load(Relaxed);
        if units == 0 {
            return None;
        }
        let (floor, units, ns, dearest) = (floor(i), units as f64, self.ns.load(Relaxed) as f64, f64::from_bits(self.dearest.load(Relaxed)));
        // Clamped: a sample lost to another's can leave its mean past its floor or its dearest.
        let share = if dearest > floor { ((ns / units - floor) / (dearest - floor)).clamp(0.0, 1.0) } else { 1.0 };
        Some([(dearest, units * share), (floor, units * (1.0 - share))])
    }
}

/// The bucket a unit's cost falls in: its power of two from [`LOWEST`], each end taking what's past it (zero and
/// subnormals, whose binary exponent is none, the bottom).
fn bucket(cost: f64) -> usize {
    let power = ((cost.to_bits() >> 52) & 0x7ff) as i32 - 1023;
    usize::try_from(power - LOWEST).map_or(0, |i| i.min(BUCKETS - 1))
}

/// The least cost bucket `i` holds: its power of two (an f64 of that exponent), but none for the bottom, which holds every
/// cost under its next.
fn floor(i: usize) -> f64 {
    if i == 0 { 0.0 } else { f64::from_bits(u64::try_from(1023 + LOWEST + i as i32).unwrap_or(0) << 52) }
}

fn add(a: &AtomicU64, v: u64) {
    a.store(a.load(Relaxed).saturating_add(v), Relaxed);
}

/// Format: the sizes [`Writes`] keeps apart, by the power of two a write's bytes fall in: 1 byte to 2 GiB, a longer write the
/// last's.
const CLASSES: usize = 32;

/// What a connection's writes cost a byte, by their size: what a write's bytes are held to within what's left of its poll's
/// quantum ([`Writes::fitting`]). Each power of two a write's bytes fall in keeps the dearest a byte of its writes cost, this
/// window and the last (a window: as many writes of that size as the runtime's probes keep, `MIN_SAMPLES`), so what a write
/// costs whatever it carries (its call) is in each size's as it is in each write: within a size a write's bytes are at most
/// twice another's, its call shared by half as many at the most. Goal 9, plan 01a1a87b2cec step 3: kept as one cost a byte,
/// every byte reckoned at the dearest any byte cost, a write's call spread over a few bytes held every write to the bytes
/// that call allowed: about a hundred in a poll's whole quantum, written a hundred at a time (kiln's lab on Linux after
/// train 65: 900 polls a second writing to their room's end, a write of 125 bytes with 225 µs left).
#[derive(Debug)]
pub struct Writes {
    sizes: [Size; CLASSES],
}

/// One size's writes: the dearest a byte cost this window and the last (f64 bits; 0: none), and its writes this window.
#[derive(Debug)]
struct Size {
    now: AtomicU64,
    last: AtomicU64,
    writes: AtomicU64,
}

impl Writes {
    /// A [`Writes`] with nothing learned.
    pub const fn nothing_yet() -> Self {
        Self { sizes: [const { Size { now: AtomicU64::new(0), last: AtomicU64::new(0), writes: AtomicU64::new(0) } }; CLASSES] }
    }

    /// A write of `len` bytes took `took` nanoseconds.
    pub fn learn(&self, len: u64, took: u64) {
        let Some(s) = self.sizes.get(size_of_write(len)) else { return };
        let rate = took.max(1) as f64 / len.max(1) as f64;
        if rate > f64::from_bits(s.now.load(Relaxed)) {
            s.now.store(rate.to_bits(), Relaxed);
        }
        let writes = s.writes.load(Relaxed).saturating_add(1);
        if writes >= hyper_rt::machine::bench::MIN_SAMPLES as u64 {
            s.last.store(s.now.load(Relaxed), Relaxed);
            s.now.store(0, Relaxed);
            s.writes.store(0, Relaxed);
        } else {
            s.writes.store(writes, Relaxed);
        }
    }

    /// The dearest a byte of size `class`'s writes cost, this window and the last: none where none was written.
    fn dearest(&self, class: usize) -> Option<f64> {
        let s = self.sizes.get(class)?;
        Some(f64::from_bits(s.now.load(Relaxed)).max(f64::from_bits(s.last.load(Relaxed)))).filter(|r| *r > 0.0)
    }

    /// The most bytes a write may carry in `room` nanoseconds, what's left of its poll's quantum (negative: the poll is past
    /// it by that, and none fits): the most whose cost a byte at the dearest a write of that size has cost ends within it,
    /// the biggest size that fits. Where a size wasn't written, at the dearest of the nearest smaller size that was (a byte of
    /// a longer write costs no more), else of the smallest that was: a shorter write than any written yet is reckoned as
    /// those are, short by at most its call, and teaches its size. None until a write was timed: the first goes whole, and
    /// teaches. A write isn't let run past its poll's quantum by what coming back to write the rest is expected to cost (goal
    /// 9, plan 01a1a87b2cec step 3: probe 17 on the M3, its clocks kept up, saw 256 KiB written in 99 µs with 4 µs of its
    /// poll's quantum left, held to 18.7 MB by that).
    pub fn fitting(&self, room: i64) -> Option<usize> {
        let smallest = (0..CLASSES).find_map(|c| self.dearest(c))?;
        if room <= 0 {
            return Some(0);
        }
        let mut rate = smallest;
        let mut fits = 0usize;
        for class in 0..CLASSES {
            rate = self.dearest(class).unwrap_or(rate);
            // The bytes that fit at this size's rate, where they're as many as a write of this size carries at least.
            let n = (room as f64 / rate) as u64;
            if n >= 1u64 << class {
                let most = if class + 1 == CLASSES { u64::MAX } else { (2u64 << class) - 1 };
                fits = usize::try_from(n.min(most)).unwrap_or(usize::MAX);
            }
        }
        Some(fits)
    }
}

impl Writes {
    /// What a write of `len` bytes is expected to take at the dearest a byte of writes of its size has cost (an unwritten
    /// size's as [`Writes::fitting`] reckons it: the nearest smaller written size's, else the smallest's), nanoseconds: none
    /// until a write was timed.
    pub fn cost_ns(&self, len: u64) -> Option<u64> {
        let size = size_of_write(len);
        let rate = (0..=size).rev().find_map(|c| self.dearest(c)).or_else(|| (size..CLASSES).find_map(|c| self.dearest(c)))?;
        Some((rate * len as f64) as u64)
    }
}

/// What a write of `len` bytes is expected to take on this shard's thread, at what its writes of that size have cost
/// ([`Writes::cost_ns`], [`WRITES`]), nanoseconds: none before a write was timed here, or off a shard's thread. What a
/// writer outside an answer (a WebSocket's frames) weighs a write by against what's left of its turn's quantum.
pub fn write_cost_ns(len: usize) -> Option<u64> {
    WRITES.try_with(|w| w.cost_ns(u64::try_from(len).unwrap_or(u64::MAX))).ok().flatten()
}

/// The size a write of `len` bytes is kept as: the power of two its bytes fall in, the last size taking what's past it.
fn size_of_write(len: u64) -> usize {
    usize::try_from(63 - len.max(1).leading_zeros()).map_or(CLASSES - 1, |p| p.min(CLASSES - 1))
}

thread_local! {
    /// What a shard's connections' writes are learned to cost ([`Writes`]): each shard's own, a shard being a thread, so a
    /// write stores only to cache lines its own core holds (one shared by every shard had each write dirty two lines every
    /// other shard writes too), and a fresh shard learns from nothing, as a fresh process does (goal 9, plan 01a1a87b2cec
    /// step 3). Made in place with the thread, nothing to drop: there, and allocating nothing, for the thread's whole life.
    pub(crate) static WRITES: Writes = const { Writes::nothing_yet() };
}

/// What's left of the shard's step quantum after what the poll a write is in has held its shard for, nanoseconds, negative
/// once the poll is past it (`hyper_rt::futures::poll_clock`). None off a shard (a thread of its own, a test's), where a
/// write is never held back.
pub fn room() -> Option<i64> {
    let poll = hyper_rt::futures::poll_clock()?;
    Some(signed(quantum()?).saturating_sub(signed(poll.held_ns())))
}

/// `ns` as a signed count of nanoseconds.
pub fn signed(ns: u64) -> i64 {
    i64::try_from(ns).unwrap_or(i64::MAX)
}

/// The quantum a whole poll has: the shard's step quantum (`hyper_rt::futures::step_budget_ns`).
pub fn quantum() -> Option<u64> {
    hyper_rt::futures::step_budget_ns()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 9, plan 01a1a87b2cec step 3: a write is sized by the dearest its writes have cost, not by their average. Do:
    /// 100 writes of 64 KiB at 1 ns a byte, two of them (the 11th and the 41st) at 10. Expect: what fits 250 µs at 10 ns a
    /// byte. RFC 6298's smoothing forgot both dear ones within a few dozen writes and sized the next write at 1 ns a byte,
    /// ten times what a dear one takes in the room.
    #[test]
    fn g9_s3_a_write_fits_its_room_at_the_dearest_its_writes_cost() {
        let w = Writes::nothing_yet();
        for k in 0..100 {
            w.learn(64 << 10, if k == 10 || k == 40 { 655_360 } else { 65_536 });
        }
        let fit = w.fitting(250_000);
        assert!(fit.is_some_and(|n| n.saturating_mul(10) <= 250_000), "{fit:?} bytes: past 250 µs at 10 ns a byte");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a write is held to what's left of its poll's quantum, its bytes at the dearest they
    /// cost, and none fits where nothing's left or the poll is past it. Do: writes at 2 ns a byte. Expect: 1,500 bytes fit
    /// 3 µs; none fits no room, nor 2 µs past it.
    #[test]
    fn g9_s3_a_write_is_held_to_what_is_left_of_its_polls_quantum() {
        let w = Writes::nothing_yet();
        w.learn(64 << 10, 131_072);
        assert_eq!((w.fitting(3_000), w.fitting(0), w.fitting(-2_000)), (Some(1_500), Some(0), Some(0)));
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what a write costs whatever it carries (its call) doesn't hold every write to the
    /// bytes it allows: a long write is sized by what long writes cost a byte. Do: writes of 100 bytes at 5 µs (their call:
    /// 50 ns a byte), and of 256 KiB at 50 µs (0.19 ns a byte). Expect: a 200 µs room fits a megabyte (at 0.19 ns a byte),
    /// where one cost a byte for every write held it to 4,000 bytes; 3 µs fits no 100-byte write. And a size's dearest is
    /// forgotten two windows after its last: a slow write (preempted) holds its size back that long.
    #[test]
    fn g9_s3_a_writes_call_doesnt_hold_long_writes_to_what_short_ones_cost_a_byte() {
        let w = Writes::nothing_yet();
        for _ in 0..10 {
            w.learn(100, 5_000);
            w.learn(256 << 10, 50_000);
        }
        assert_eq!(w.fitting(200_000), Some(1 << 20), "200 µs at 0.19 ns a byte");
        assert!(w.fitting(3_000).is_some_and(|n| n < 100), "a 100-byte write takes 5 µs: {:?}", w.fitting(3_000));
        w.learn(256 << 10, 5_000_000);
        assert!(w.fitting(200_000).is_some_and(|n| n < 256 << 10), "a write of 256 KiB took 5 ms: {:?}", w.fitting(200_000));
        for _ in 0..2 * hyper_rt::machine::bench::MIN_SAMPLES {
            w.learn(256 << 10, 50_000);
        }
        assert_eq!(w.fitting(200_000), Some(1 << 20), "the slow one forgotten");
        // What a write is expected to take: at its size's rate, or the nearest smaller written size's.
        assert_eq!(w.cost_ns(256 << 10), Some(50_000), "256 KiB at its size's rate");
        assert_eq!(w.cost_ns(1 << 20), Some(200_000), "a megabyte at 256 KiB's rate, the nearest smaller written");
        assert_eq!(w.cost_ns(100), Some(5_000), "100 bytes at their size's");
        assert_eq!(Writes::nothing_yet().cost_ns(100), None, "nothing learned");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: no gain is picked; the sketch forgets by halving each window. Do: a window of jobs
    /// at 10 µs, then jobs at 1 µs. Expect: the dear ones weigh less each window and are gone in the end; 10 jobs fit
    /// 10 µs once they are. The mean is exact: what was learned, over the units.
    #[test]
    fn g9_s3_costs_forget_by_halving_each_window() {
        let c = Costs::nothing_yet();
        assert_eq!((c.fit(10_000, 0), c.mean()), (None, None), "nothing learned");
        for _ in 0..hyper_rt::machine::bench::MIN_SAMPLES {
            c.learn(1, 10_000);
        }
        assert_eq!(c.fit(10_000, 0), Some(1), "at 10 µs a job");
        assert_eq!(c.mean(), Some(10_000.0));
        let mut cheap = 0u64;
        while c.fit(10_000, 0) != Some(10) && cheap < 1 << 24 {
            c.learn(1, 1_000);
            cheap += 1;
        }
        assert_eq!(c.fit(10_000, 0), Some(10), "the dear ones forgotten after {cheap} cheap ones");
        assert!(cheap > hyper_rt::machine::bench::MIN_SAMPLES as u64, "not before a window of cheap ones: {cheap}");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: one outlier doesn't stop the costs forgetting. Do: units at 3, 5 and 900 ns, then
    /// units at 5 ns. Expect: the window holds 3 to 900 at first; within two of the floor's windows (59 samples each) it holds
    /// 5 alone, and asks no longer a window. The spread summed apart from the buckets kept the 900 ns unit's square after its
    /// bucket emptied: a window of 853,776 samples, and the 3 ns unit never let go.
    #[test]
    fn g9_s3_one_outlier_doesnt_stop_the_costs_forgetting() {
        let c = Costs::nothing_yet();
        assert_eq!((c.range(), c.dearest()), (None, None), "nothing learned");
        c.learn(1, 5);
        c.learn(2, 6);
        c.learn(1, 900);
        assert_eq!((c.range(), c.dearest()), (Some((3.0, 900.0)), Some(900.0)), "the cheapest and the dearest the window holds");
        let mut fives = 0u64;
        while c.range() != Some((5.0, 5.0)) && fives < 1 << 20 {
            c.learn(1, 5);
            fives += 1;
        }
        let floor = hyper_rt::machine::bench::MIN_SAMPLES as u64;
        assert!(fives <= 2 * floor, "the 3 and 900 ns units let go after {fives} at 5 ns, the window {}", c.window.load(Relaxed));
        assert_eq!(c.window.load(Relaxed), floor, "5 ns units alone ask no more than the floor's window");
    }

    /// Goal 9, plan 01a1a87b2cec step 3: a cost falls in the bucket of its power of two, whose floor is that power.
    #[test]
    fn g9_s3_a_cost_falls_in_its_power_of_twos_bucket() {
        for i in 1..BUCKETS {
            assert_eq!((bucket(floor(i)), bucket(floor(i) * 1.999)), (i, i), "bucket {i}, floor {}", floor(i));
        }
        assert_eq!((bucket(0.0), bucket(f64::MIN_POSITIVE), bucket(1.0), bucket(1e300)), (0, 0, usize::try_from(-LOWEST).unwrap_or(0), BUCKETS - 1));
    }

    /// Goal 9, plan 01a1a87b2cec step 3: with nothing free (work that can go elsewhere), the rule is the expected cost
    /// against what going elsewhere costs: `defer` over the mean, exactly.
    #[test]
    fn g9_s3_with_nothing_free_the_rule_is_the_mean() {
        let c = Costs::nothing_yet();
        for _ in 0..9 {
            c.learn(1, 20_000);
        }
        c.learn(1, 400_000);
        assert_eq!(c.mean(), Some(58_000.0));
        assert_eq!(c.fit(0, 60_000), Some(1), "58 µs on average within 60");
        assert_eq!(c.fit(0, 57_000), Some(0), "not within 57");
    }
}
