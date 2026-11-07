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
/// dearest of them. No gain is picked: every `window` samples each bucket is halved, so the last window weighs as much as
/// all before it, and the window is as many samples as a mean of these costs' own spread needs to come within the
/// runtime's precision (`hyper_rt::machine::wake::sample_window`, which sizes the shard's own quantum estimate). Its callers
/// learn into it with loads and stores: a sample lost to another's at the same moment is one of many (goal 9, plan
/// 01a1a87b2cec step 3).
#[derive(Debug)]
pub struct Costs {
    buckets: [Bucket; BUCKETS],
    units: AtomicU64,
    ns: AtomicU64,
    /// Σ ns²/units over its samples, f64 bits: their spread, which sizes the window.
    squares: AtomicU64,
    /// Samples since the buckets were last halved, and how many they're halved at.
    taken: AtomicU64,
    window: AtomicU64,
}

#[derive(Debug)]
struct Bucket {
    units: AtomicU64,
    ns: AtomicU64,
    /// The dearest a unit of it cost, f64 bits.
    dearest: AtomicU64,
}

impl Costs {
    /// A [`Costs`] with nothing learned, its first window the fewest samples the runtime's probes keep
    /// (`hyper_rt::machine::bench::MIN_SAMPLES`).
    pub const fn nothing_yet() -> Self {
        Self {
            buckets: [const { Bucket { units: AtomicU64::new(0), ns: AtomicU64::new(0), dearest: AtomicU64::new(0) } }; BUCKETS],
            units: AtomicU64::new(0),
            ns: AtomicU64::new(0),
            squares: AtomicU64::new(0),
            taken: AtomicU64::new(0),
            window: AtomicU64::new(hyper_rt::machine::bench::MIN_SAMPLES as u64),
        }
    }

    /// `units` units took `ns` nanoseconds.
    pub fn learn(&self, units: u64, ns: u64) {
        if units == 0 {
            return;
        }
        let cost = ns as f64 / units as f64;
        if let Some(b) = self.buckets.get(bucket(cost)) {
            add(&b.units, units);
            add(&b.ns, ns);
            if cost > f64::from_bits(b.dearest.load(Relaxed)) {
                b.dearest.store(cost.to_bits(), Relaxed);
            }
        }
        add(&self.units, units);
        add(&self.ns, ns);
        self.squares.store((f64::from_bits(self.squares.load(Relaxed)) + cost * ns as f64).to_bits(), Relaxed);
        let taken = self.taken.load(Relaxed).saturating_add(1);
        self.taken.store(taken, Relaxed);
        if taken >= self.window.load(Relaxed) {
            self.halve();
        }
    }

    /// Every bucket halved, and the next window what's left of them asks for.
    fn halve(&self) {
        let (mut was, mut units, mut ns) = (0u64, 0u64, 0u64);
        for b in &self.buckets {
            let all = b.units.load(Relaxed);
            let u = all / 2;
            // Half its units, with their nanoseconds at its mean, which stays what it was; emptied, its dearest goes too.
            let n = u64::try_from(u128::from(b.ns.load(Relaxed)) * u128::from(u) / u128::from(all.max(1))).unwrap_or(u64::MAX);
            b.units.store(u, Relaxed);
            b.ns.store(n, Relaxed);
            if u == 0 {
                b.dearest.store(0, Relaxed);
            }
            was = was.saturating_add(all);
            units = units.saturating_add(u);
            ns = ns.saturating_add(n);
        }
        let squares = f64::from_bits(self.squares.load(Relaxed)) * units as f64 / was.max(1) as f64;
        self.units.store(units, Relaxed);
        self.ns.store(ns, Relaxed);
        self.squares.store(squares.to_bits(), Relaxed);
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

    /// The rule work is weighed by: the most units whose cost past `free` nanoseconds is expected to be no more than
    /// `defer`, what putting the rest off costs; none until a cost was learned. Each bucket's units are reckoned at its
    /// dearest and at its floor, in the shares that keep their nanoseconds: the widest spread they could have, so what's
    /// past `free` is never reckoned short.
    pub fn fit(&self, free: u64, defer: u64) -> Option<u64> {
        let units = self.units.load(Relaxed);
        if units == 0 {
            return None;
        }
        let (free, budget) = (free as f64, defer as f64 * units as f64);
        let mut points = self.buckets.iter().enumerate().rev().filter_map(|(i, b)| b.points(floor(i))).flatten().peekable();
        // The units of the dearest points so far, and their nanoseconds: at n units, those past `free` cost
        // n·past_ns − free·past past it, and the n that makes that the budget holds where the next point stays within.
        let (mut past, mut past_ns) = (0.0, 0.0);
        while let Some((cost, u)) = points.next() {
            past += u;
            past_ns += u * cost;
            let n = (budget + free * past) / past_ns;
            if n * points.peek().map_or(0.0, |p| p.0) <= free {
                return Some(n as u64);
            }
        }
        Some(u64::MAX)
    }
}

impl Bucket {
    /// Its units as two points, (cost, units), the dearest first: at its dearest and at its floor, in the shares that keep
    /// its nanoseconds. None while it holds none.
    fn points(&self, floor: f64) -> Option<[(f64, f64); 2]> {
        let units = self.units.load(Relaxed);
        if units == 0 {
            return None;
        }
        let (units, ns, dearest) = (units as f64, self.ns.load(Relaxed) as f64, f64::from_bits(self.dearest.load(Relaxed)));
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

/// The least cost bucket `i` holds: its power of two, but none for the bottom, which holds every cost under its next.
fn floor(i: usize) -> f64 {
    if i == 0 { 0.0 } else { 2f64.powi(LOWEST + i as i32) }
}

fn add(a: &AtomicU64, v: u64) {
    a.store(a.load(Relaxed).saturating_add(v), Relaxed);
}

/// What a connection's writes cost a byte, and what a poll that gave its shard up for them costs as it comes back to write
/// on: what a write's bytes past its room are weighed against ([`Writes::fitting`]).
#[derive(Debug)]
pub struct Writes {
    bytes: Costs,
    resumes: Costs,
}

impl Writes {
    /// A [`Writes`] with nothing learned.
    pub const fn nothing_yet() -> Self {
        Self { bytes: Costs::nothing_yet(), resumes: Costs::nothing_yet() }
    }

    /// A write of `len` bytes took `took` nanoseconds.
    pub fn learn(&self, len: u64, took: u64) {
        self.bytes.learn(len, took);
    }

    /// A poll that gave its shard up came back to write `held` nanoseconds into its next poll.
    pub fn resumed(&self, held: u64) {
        self.resumes.learn(1, held);
    }

    /// The most bytes a write may carry in `room` nanoseconds: those whose cost past it is expected to be no more than a
    /// poll's coming back to write the rest ([`Costs::fit`]). None until a write was timed: the first goes whole, and
    /// teaches. Until a poll came back, putting the rest off is taken to cost nothing: what fits at the dearest.
    pub fn fitting(&self, room: u64) -> Option<usize> {
        let defer = self.resumes.mean().map_or(0, |ns| ns as u64);
        self.bytes.fit(room, defer).map(|n| usize::try_from(n).unwrap_or(usize::MAX))
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

    /// Goal 9, plan 01a1a87b2cec step 3: once a poll that gave its shard up is known to cost something as it comes back,
    /// rare dear writes count for their share: a write may run past its room by what's expected to cost no more than coming
    /// back. Do: as above, then polls that came back 2 µs into their next. Expect: more than fits at the dearest, less than
    /// at the cheapest.
    #[test]
    fn g9_s3_coming_back_weighs_rare_dear_writes_by_their_share() {
        let w = Writes::nothing_yet();
        for k in 0..100 {
            w.learn(64 << 10, if k == 10 || k == 40 { 655_360 } else { 65_536 });
        }
        w.resumed(2_000);
        let fit = w.fitting(250_000);
        assert!(fit.is_some_and(|n| n > 25_000 && n < 250_000), "{fit:?}");
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
