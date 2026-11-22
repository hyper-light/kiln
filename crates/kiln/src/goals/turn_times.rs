//! G9 step 3's trace of where an edit's time goes (goal a2a44298, T-T1.5.1; tests only): how long
//! the stacks' turns (`super::Changing`) were waited for and every stack's turn held, and how long
//! each of the driver's rounds took, while the edit-to-action measure runs (`latency`). The shared
//! turn's wait is what a change to one stack waits for a change that spans stacks (the driver's
//! words said again, its holds said, a subagent's goal); the own turn's, what it waits for another
//! change to the same stack (the driver's step on it); every stack's turn held is what each of
//! those barriers cost every other change.
//!
//! Each series is counts in buckets of powers of two of microseconds (bucket `k` holds `[2^(k-1),
//! 2^k)`, bucket 0 holds 0: the format is a u64's bits, 65 buckets), with the count and the sum: a
//! quantile is the upper end of its bucket, within a factor of two, which says which wait it is.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Buckets of a series: one for 0 and one per bit of a u64 (the format, not a choice).
const BUCKETS: usize = 65;

pub(crate) struct Series {
    buckets: [AtomicU64; BUCKETS],
    sum_us: AtomicU64,
}

impl Series {
    const fn new() -> Self {
        Self { buckets: [const { AtomicU64::new(0) }; BUCKETS], sum_us: AtomicU64::new(0) }
    }

    /// The time since `since`, counted, when `counted`.
    pub(crate) fn since(&self, since: Instant, counted: bool) {
        if counted {
            self.add(since.elapsed());
        }
    }

    pub(crate) fn add(&self, d: Duration) {
        let us = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
        let k = usize::try_from(u64::BITS - us.leading_zeros()).unwrap_or(BUCKETS - 1);
        if let Some(b) = self.buckets.get(k) {
            b.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_us.fetch_add(us, Ordering::Relaxed);
    }

    fn reset(&self) {
        for b in &self.buckets {
            b.store(0, Ordering::Relaxed);
        }
        self.sum_us.store(0, Ordering::Relaxed);
    }

    /// Its count, sum, and the upper ends of the buckets holding its p50, p99 and p99.9 and its
    /// largest, in microseconds.
    fn summary(&self) -> Value {
        let counts: Vec<u64> = self.buckets.iter().map(|b| b.load(Ordering::Relaxed)).collect();
        let n: u64 = counts.iter().sum();
        // The upper end of bucket k: 2^k - 1 microseconds (bucket 0: 0).
        let top = |k: usize| u32::try_from(k).ok().and_then(|k| 1u64.checked_shl(k)).map_or(u64::MAX, |v| v - 1);
        let at = |q: f64| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss)]
            let want = ((q * n as f64).ceil() as u64).clamp(1, n.max(1));
            let mut seen = 0u64;
            for (k, c) in counts.iter().enumerate() {
                seen += c;
                if seen >= want {
                    return top(k);
                }
            }
            0
        };
        let max = counts.iter().rposition(|c| *c > 0).map_or(0, top);
        json!({"n": n, "sum_us": self.sum_us.load(Ordering::Relaxed), "p50_us_under": at(0.50), "p99_us_under": at(0.99), "p999_us_under": at(0.999), "max_us_under": max})
    }
}

/// The wait for the shared turn, as a reader, by a change to one stack (`super::stack_turn!`).
pub(crate) static SHARED: Series = Series::new();
/// The wait for one stack's own turn (`super::stack_turn!`).
pub(crate) static OWN: Series = Series::new();
/// The wait for every stack's turn (`super::Changing::every`).
pub(crate) static EVERY_WAIT: Series = Series::new();
/// How long every stack's turn was held, each time (`super::Every`).
pub(crate) static EVERY_HELD: Series = Series::new();
/// Each of the driver's rounds with what rides on it (`Driver::follow`: its round, `tend` and its
/// deadline), from its start to its wait.
pub(crate) static ROUNDS: Series = Series::new();

fn all() -> [(&'static str, &'static Series); 5] {
    [("shared_wait", &SHARED), ("own_wait", &OWN), ("every_wait", &EVERY_WAIT), ("every_held", &EVERY_HELD), ("rounds", &ROUNDS)]
}

/// Every series to zero: the measure's start.
pub(crate) fn reset() {
    for (_, s) in all() {
        s.reset();
    }
}

/// Every series as it stands.
pub(crate) fn summary() -> serde_json::Map<String, Value> {
    all().into_iter().map(|(name, s)| (name.to_owned(), s.summary())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buckets: 0 alone, then each power of two to the next; quantiles are a bucket's upper end.
    #[test]
    fn g9_s3_turn_times_bucket_by_powers_of_two() {
        let s = Series::new();
        for us in [0u64, 1, 3, 1000, 1000] {
            s.add(Duration::from_micros(us));
        }
        let v = s.summary();
        assert_eq!(v.get("n").and_then(Value::as_u64), Some(5));
        assert_eq!(v.get("sum_us").and_then(Value::as_u64), Some(2004));
        // 1000 µs is in [512, 1024): its bucket's upper end is 1023.
        assert_eq!(v.get("max_us_under").and_then(Value::as_u64), Some(1023));
        // The third of five (p50) is 3 µs, in [2, 4).
        assert_eq!(v.get("p50_us_under").and_then(Value::as_u64), Some(3));
        s.reset();
        assert_eq!(s.summary().get("n").and_then(Value::as_u64), Some(0));
    }
}
