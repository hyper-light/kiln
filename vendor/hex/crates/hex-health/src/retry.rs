//! The retry budget (S8: retries budgeted, because sustained retries drove more than half of the
//! metastable outages studied, PF[20]). Google's two rules (SRE book ch. 21, "Deciding to Retry"): a
//! request is tried at most [`MAX_ATTEMPTS`] times, and a client retries only while its retries are
//! under [`RETRY_PER_MILLE`] / 1000 of its requests, which "reduces the growth to just 1.1x". The ratio
//! is over a sliding window of [`BUCKETS`] buckets of [`BUCKET_NS`], so an old burst of requests can't
//! fund a retry storm now.

use crate::HealthError;

/// "If a request has already failed three times, we let the failure bubble up" (SRE ch. 21).
pub const MAX_ATTEMPTS: u32 = 3;
/// "retried as long as this ratio is below 10%" (SRE ch. 21).
pub const RETRY_PER_MILLE: u64 = 100;
/// The window: 12 buckets of 10 s, two minutes, the history SRE ch. 21's adaptive throttling keeps.
pub const BUCKET_NS: u64 = 10_000_000_000;
pub const BUCKETS: usize = 12;

/// Why a retry was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The request has had its [`MAX_ATTEMPTS`].
    Attempts,
    /// Retries in the window are at the budget: `retries` of `requests`.
    Budget { retries: u64, requests: u64 },
}

#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    start_ns: u64,
    requests: u64,
    retries: u64,
}

#[derive(Debug, Clone)]
pub struct RetryBudget {
    buckets: [Bucket; BUCKETS],
    last_ns: u64,
}

impl RetryBudget {
    pub fn new(now_ns: u64) -> Self {
        Self {
            buckets: [Bucket::default(); BUCKETS],
            last_ns: now_ns,
        }
    }

    /// A new request's first attempt at `now_ns`.
    pub fn request(&mut self, now_ns: u64) -> Result<(), HealthError> {
        let b = self.bucket(now_ns)?;
        b.requests = b.requests.saturating_add(1);
        Ok(())
    }

    /// May a request whose `attempts` so far all failed be tried again at `now_ns`? A yes is counted.
    pub fn retry(
        &mut self,
        now_ns: u64,
        attempts: u32,
    ) -> Result<Result<(), Refusal>, HealthError> {
        if attempts >= MAX_ATTEMPTS {
            return Ok(Err(Refusal::Attempts));
        }
        self.bucket(now_ns)?;
        let span = BUCKET_NS.saturating_mul(BUCKETS as u64);
        let (requests, retries) = self
            .buckets
            .iter()
            .filter(|b| b.start_ns.saturating_add(span) > now_ns)
            .fold((0u64, 0u64), |(q, r), b| {
                (q.saturating_add(b.requests), r.saturating_add(b.retries))
            });
        // retries + 1 <= requests × 10%.
        let allowed = retries.saturating_add(1).saturating_mul(1000)
            <= requests.saturating_mul(RETRY_PER_MILLE);
        if !allowed {
            return Ok(Err(Refusal::Budget { retries, requests }));
        }
        let b = self.bucket(now_ns)?;
        b.retries = b.retries.saturating_add(1);
        Ok(Ok(()))
    }

    fn bucket(&mut self, now_ns: u64) -> Result<&mut Bucket, HealthError> {
        if now_ns < self.last_ns {
            return Err(HealthError::TimeBackwards {
                what: "retry budget",
                now_ns,
                last_ns: self.last_ns,
            });
        }
        self.last_ns = now_ns;
        let overflow = HealthError::Overflow("retry bucket");
        let slot_no = now_ns.checked_div(BUCKET_NS).ok_or(overflow.clone())?;
        let start = slot_no.saturating_mul(BUCKET_NS);
        let index = slot_no
            .checked_rem(BUCKETS as u64)
            .and_then(|i| usize::try_from(i).ok())
            .ok_or(overflow)?;
        let b = self
            .buckets
            .get_mut(index)
            .ok_or(HealthError::Overflow("retry bucket"))?;
        if b.start_ns != start {
            *b = Bucket {
                start_ns: start,
                requests: 0,
                retries: 0,
            };
        }
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// S8 retry budget: against a backend that fails every attempt, 100,000 requests over time make
    /// at most 1.1x their number in attempts, and no request is tried more than three times.
    #[test]
    fn retries_against_a_dead_backend_grow_load_at_most_1_1x() {
        let mut b = RetryBudget::new(0);
        let mut attempts_total: u64 = 0;
        let requests: u64 = 100_000;
        for i in 0..requests {
            let t = i * MS;
            b.request(t).unwrap();
            attempts_total += 1;
            let mut attempts = 1;
            while b.retry(t, attempts).unwrap().is_ok() {
                attempts += 1;
                attempts_total += 1;
            }
            assert!(attempts <= MAX_ATTEMPTS);
        }
        let growth_milli = attempts_total * 1000 / requests;
        assert!(growth_milli <= 1100, "{growth_milli}");
        // Non-vacuity: the budget did let retries through.
        assert!(growth_milli > 1000, "{growth_milli}");
    }

    /// The per-request rule: a fourth attempt is refused even with budget to spare.
    #[test]
    fn a_fourth_attempt_is_refused() {
        let mut b = RetryBudget::new(0);
        for _ in 0..1000 {
            b.request(0).unwrap();
        }
        assert_eq!(b.retry(0, 1).unwrap(), Ok(()));
        assert_eq!(b.retry(0, 2).unwrap(), Ok(()));
        assert_eq!(b.retry(0, 3).unwrap(), Err(Refusal::Attempts));
    }

    /// The window slides: requests older than two minutes fund no retry now.
    #[test]
    fn old_requests_fund_no_retries() {
        let mut b = RetryBudget::new(0);
        for _ in 0..1000 {
            b.request(0).unwrap();
        }
        let later = 3 * 60 * 1_000_000_000;
        b.request(later).unwrap();
        assert_eq!(
            b.retry(later, 1).unwrap(),
            Err(Refusal::Budget {
                retries: 0,
                requests: 1
            })
        );
        assert!(matches!(
            b.request(0),
            Err(HealthError::TimeBackwards { .. })
        ));
    }
}
