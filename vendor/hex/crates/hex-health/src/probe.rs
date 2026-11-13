//! A node's own disk probe (`AC-T7.10.3`): the node agent writes and flushes a small record on its
//! state disk every period, and a stall is judged by the outstanding write's age, as hyper-raft trusts a
//! node only while its log device takes a write and flush within the period ("a node whose disk stalls
//! stops being trusted, as a crashed one does", PF[26]).
//!
//! Nothing in it is picked (Ada, 2026-11-06: "learned, data-derived, and adaptive"):
//!
//! - `Suspect` (no new placements) once the outstanding write is slower than any healthy answer seen:
//!   its host class's recorded answers (a named record, the shell's to give), then its own.
//! - `Drain` (retire the node, its work moved) at the age where waiting costs more than acting. A
//!   drain at age `T` costs a false drain when a healthy write outlives `T`, the node's work moved for
//!   nothing at the measured re-placement time `C` per workload; waiting costs, per workload, the time
//!   a real stall blocks it. The probe drains at the `T` minimizing the expected loss per probe,
//!   `(1 - pi) * S(T) * C + pi * T`, where `S(T)` is the share of healthy answers slower than `T` (the
//!   record's and the node's, with a Jeffreys half-count past the slowest, so never zero) and `pi` the
//!   Jeffreys posterior of a stall per probe, `1/2 / (n + 1)` over `n` stall-free probes: the
//!   record's, healthy probes of the host class, and then the node's own. A fleet record of stall
//!   incidents would seed its numerator; none exists yet. The candidate ages are the healthy answers'
//!   histogram buckets (25%); `bench/health/replay_probe.py`'s `cost_rule` is the exact form, replayed
//!   on the recorded traces.
//!
//! A write that outlived the drain age before it answered is no answer: the drain had given up on it,
//! and taking it would let one stall raise the age that judges the next (Karn's rule, RFC 6298 §3). Nor
//! is it counted a stall: whether it was one is not known, and counting the probe's own verdicts would
//! feed its drains back into its prior. A disk that answers ever more slowly raises its own ages; that
//! fail-slow case is the peer grade's (F2), which judges a node against its peers.

use crate::histogram::Histogram;
use crate::{HealthError, Level};

#[derive(Debug, Clone)]
pub struct DiskProbe {
    /// Healthy answers' latencies: the record's, then the node's own, nanoseconds.
    healthy: Histogram,
    /// The slowest healthy answer, exact.
    slowest_ns: u64,
    /// The measured re-placement time of one workload, nanoseconds: what a false drain costs.
    replace_ns: u64,
    /// Probes the prior counts: the record's healthy ones, stall-free evidence of the host class, then
    /// every one this node finishes.
    probes: u64,
    outstanding: Option<u64>,
    last_ns: u64,
}

impl DiskProbe {
    /// A probe seeded with its host class's recorded healthy answers, `(latency ns, count)` each, and
    /// the measured time to re-place one workload. No answer, or a free re-placement, is no record:
    /// refused.
    pub fn new(record: &[(u64, u32)], replace_ns: u64) -> Result<Self, HealthError> {
        if replace_ns == 0 {
            return Err(HealthError::Config(
                "a disk probe needs the measured time to re-place a workload",
            ));
        }
        let mut healthy = Histogram::new();
        let mut slowest_ns: u64 = 0;
        for &(ns, n) in record.iter().filter(|(ns, n)| *ns > 0 && *n > 0) {
            healthy.record_n(ns, n)?;
            slowest_ns = slowest_ns.max(ns);
        }
        if slowest_ns == 0 {
            return Err(HealthError::Config(
                "a disk probe needs its host class's recorded answers",
            ));
        }
        let probes = u64::from(healthy.total());
        Ok(Self {
            healthy,
            slowest_ns,
            replace_ns,
            probes,
            outstanding: None,
            last_ns: 0,
        })
    }

    /// The slowest healthy answer, the record's included: past it, `Suspect`.
    pub fn slowest_ns(&self) -> u64 {
        self.slowest_ns
    }

    /// The age past which an outstanding write drains the node: the candidate `T` (a healthy
    /// bucket's highest value) of least expected loss, the least such on a tie. Scaled by
    /// `4 (n + 1) (N + 1)` over `N` healthy answers, the loss is
    /// `(2n + 1)(2a + 1) C + 2 (N + 1) T` with `a` the answers above `T`: integers, compared exactly.
    pub fn drain_age_ns(&self) -> Result<u64, HealthError> {
        let overflow = HealthError::Overflow("probe drain age");
        let total = u128::from(self.healthy.total());
        let not_stalled = u128::from(self.probes)
            .checked_mul(2)
            .and_then(|x| x.checked_add(1))
            .ok_or(overflow.clone())?;
        let wait = total
            .checked_add(1)
            .and_then(|x| x.checked_mul(2))
            .ok_or(overflow.clone())?;
        let mut above = total;
        let mut best: Option<(u128, u64)> = None;
        for (t, count) in self.healthy.counted() {
            above = above
                .checked_sub(u128::from(count))
                .ok_or(overflow.clone())?;
            let false_drain = above
                .checked_mul(2)
                .and_then(|x| x.checked_add(1))
                .and_then(|x| x.checked_mul(not_stalled))
                .and_then(|x| x.checked_mul(u128::from(self.replace_ns)));
            let waiting = wait.checked_mul(u128::from(t));
            let loss = false_drain
                .zip(waiting)
                .and_then(|(f, w)| f.checked_add(w))
                .ok_or(overflow.clone())?;
            if best.is_none_or(|(b, _)| loss < b) {
                best = Some((loss, t));
            }
        }
        best.map(|(_, t)| t).ok_or(HealthError::Config(
            "a disk probe needs its host class's recorded answers",
        ))
    }

    /// A write and flush began at `now_ns`.
    pub fn start(&mut self, now_ns: u64) -> Result<(), HealthError> {
        self.advance(now_ns)?;
        if self.outstanding.is_some() {
            return Err(HealthError::ProbeOutstanding);
        }
        self.outstanding = Some(now_ns);
        Ok(())
    }

    /// The outstanding write and flush finished at `now_ns`; returns its latency. Within the drain
    /// age it is a healthy answer, and raises the slowest if it is slower.
    pub fn finish(&mut self, now_ns: u64) -> Result<u64, HealthError> {
        self.advance(now_ns)?;
        let began = self.outstanding.ok_or(HealthError::NoProbe)?;
        let latency = now_ns
            .checked_sub(began)
            .ok_or(HealthError::Overflow("probe latency"))?;
        let drain_age = self.drain_age_ns()?;
        self.probes = self
            .probes
            .checked_add(1)
            .ok_or(HealthError::Overflow("probe count"))?;
        self.outstanding = None;
        if latency <= drain_age {
            self.healthy.record(latency)?;
            self.slowest_ns = self.slowest_ns.max(latency);
        }
        Ok(latency)
    }

    /// The node's grade at `now_ns`.
    pub fn level(&self, now_ns: u64) -> Result<Level, HealthError> {
        if now_ns < self.last_ns {
            return Err(HealthError::TimeBackwards {
                what: "disk probe",
                now_ns,
                last_ns: self.last_ns,
            });
        }
        let age = match self.outstanding {
            Some(began) => now_ns
                .checked_sub(began)
                .ok_or(HealthError::Overflow("probe age"))?,
            None => 0,
        };
        Ok(if age > self.drain_age_ns()? {
            Level::Drain
        } else if age > self.slowest_ns {
            Level::Suspect
        } else {
            Level::Healthy
        })
    }

    /// When the outstanding write, if it stays outstanding, next changes the grade: the instant a
    /// driver should look again. The ages don't move while a write is outstanding.
    pub fn next_change(&self) -> Result<Option<u64>, HealthError> {
        let Some(began) = self.outstanding else {
            return Ok(None);
        };
        let overflow = HealthError::Overflow("probe deadline");
        let suspect = began.checked_add(self.slowest_ns).ok_or(overflow.clone())?;
        let drain = began.checked_add(self.drain_age_ns()?).ok_or(overflow)?;
        let (first, last) = (suspect.min(drain), suspect.max(drain));
        let next = if self.last_ns <= first { first } else { last };
        Ok(Some(next.saturating_add(1)))
    }

    fn advance(&mut self, now_ns: u64) -> Result<(), HealthError> {
        if now_ns < self.last_ns {
            return Err(HealthError::TimeBackwards {
                what: "disk probe",
                now_ns,
                last_ns: self.last_ns,
            });
        }
        self.last_ns = now_ns;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;
    const S: u64 = 1_000_000_000;
    /// Re-placement times the replay priced (`bench/health/records/2026-11-06-probe-floor-apfs.md`):
    /// an OCI start, and Shards' measured boot p99.
    const OCI: u64 = 2 * MS;
    const SHARDS: u64 = 194 * MS;

    /// A record shaped as the APFS record's tail: most answers a few ms, a thin tail to 147 ms.
    fn record() -> Vec<(u64, u32)> {
        vec![
            (4 * MS, 5_000),
            (8 * MS, 3_000),
            (16 * MS, 600),
            (32 * MS, 60),
            (64 * MS, 6),
            (147 * MS, 1),
        ]
    }

    /// The oracle: the expected loss of draining at `t`, in floating point, straight from the
    /// formula, over the healthy answers `answers` (latency, count) after `n` probes.
    fn loss(answers: &[(u64, u32)], n: u64, replace: u64, t: u64) -> f64 {
        let f = |x: u64| f64::from(u32::try_from(x).unwrap());
        let total: f64 = answers.iter().map(|a| f64::from(a.1)).sum();
        let above: f64 = answers
            .iter()
            .filter(|a| a.0 > t)
            .map(|a| f64::from(a.1))
            .sum();
        let pi = 0.5 / (f(n) + 1.0);
        let s = (above + 0.5) / (total + 1.0);
        (1.0 - pi) * s * f(replace) + pi * f(t)
    }

    /// A probe that has answered `n` writes of `latency` each, 100 ms apart.
    fn answered(replace: u64, latency: u64, n: u32) -> (DiskProbe, u64) {
        let mut p = DiskProbe::new(&record(), replace).unwrap();
        let mut t = 0;
        for _ in 0..n {
            p.start(t).unwrap();
            t += latency;
            p.finish(t).unwrap();
            t += 100 * MS;
        }
        (p, t)
    }

    /// AC-T7.10.3, model-based: do seed probes with random records, re-placement times and probe
    /// counts; expect the drain age to be a healthy bucket's highest value whose loss, by the oracle
    /// over the probe's own buckets, is the least of all of them.
    #[test]
    fn the_drain_age_is_the_age_of_least_expected_loss() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        for _ in 0..500 {
            let rec: Vec<(u64, u32)> = (0..1 + next(12))
                .map(|_| (1 + next(500 * MS), u32::try_from(1 + next(5_000)).unwrap()))
                .collect();
            let replace = 1 + next(500 * MS);
            let mut p = DiskProbe::new(&rec, replace).unwrap();
            let probes = next(50);
            let mut t = 0;
            for _ in 0..probes {
                p.start(t).unwrap();
                t += 1 + next(5 * MS);
                p.finish(t).unwrap();
            }
            let buckets: Vec<_> = p.healthy.counted().collect();
            let age = p.drain_age_ns().unwrap();
            assert!(buckets.iter().any(|b| b.0 == age));
            let n = probes + rec.iter().map(|r| u64::from(r.1)).sum::<u64>();
            let at = loss(&buckets, n, replace, age);
            for b in &buckets {
                let other = loss(&buckets, n, replace, b.0);
                assert!(
                    at <= other * (1.0 + 1e-12),
                    "{age}: {at} > {}: {other}",
                    b.0
                );
            }
        }
    }

    /// AC-T7.10.3, before any sample: do stall the very first write; expect `Suspect` past the
    /// record's slowest and `Drain` past the drain age, armed and bounded from the record alone.
    #[test]
    fn the_first_wait_is_judged_by_the_record() {
        let mut p = DiskProbe::new(&record(), SHARDS).unwrap();
        let drain = p.drain_age_ns().unwrap();
        let slowest = p.slowest_ns();
        assert_eq!(slowest, 147 * MS);
        p.start(0).unwrap();
        assert_eq!(p.level(drain.min(slowest)).unwrap(), Level::Healthy);
        assert_eq!(p.level(drain.max(slowest) + 1).unwrap(), Level::Drain);
        assert_eq!(p.level(drain + 1).unwrap(), Level::Drain);
    }

    /// AC-T7.10.3, the drill's false drain (`bench/health/records/2026-11-06-dm-delay-drill-cost-rule.md`):
    /// do seed a probe with a record shaped as fedora's loaded baseline (10,000 answers, p99.9 175 ms,
    /// slowest 384 ms), answer 100 warm-up probes, then hold one write as long as the record's p99.9;
    /// expect no drain: the record's healthy probes are stall-free evidence the prior counts.
    #[test]
    fn a_healthy_answer_inside_the_record_is_no_drain_after_warm_up() {
        let rec = vec![
            (5 * MS, 9_000),
            (10 * MS, 900),
            (65 * MS, 90),
            (175 * MS, 9),
            (384 * MS, 1),
        ];
        let mut p = DiskProbe::new(&rec, SHARDS).unwrap();
        let mut t = 0;
        for _ in 0..100 {
            p.start(t).unwrap();
            t += 5 * MS;
            p.finish(t).unwrap();
            t += 100 * MS;
        }
        p.start(t).unwrap();
        assert_ne!(p.level(t + 175 * MS).unwrap(), Level::Drain);
    }

    /// Ada's rule, the price of a false drain: do judge one record with a cheap re-placement (an OCI
    /// start) and a dear one (a Shards boot); expect the dear one to wait longer before draining.
    #[test]
    fn a_dear_re_placement_waits_longer() {
        let cheap = DiskProbe::new(&record(), OCI).unwrap();
        let dear = DiskProbe::new(&record(), SHARDS).unwrap();
        assert!(cheap.drain_age_ns().unwrap() < dear.drain_age_ns().unwrap());
    }

    /// Ada's rule, the learned stall prior: every healthy answer is stall-free evidence and a point of
    /// the healthy tail at once, so do answer many fast probes; expect the drain age to stay where the
    /// record put it (the chance of a stall and of a healthy write past it fall together), at either
    /// price; and a dear re-placement to wait longer.
    #[test]
    fn fast_answers_leave_the_drain_age_to_the_tail() {
        let thin = [(4 * MS, 10), (8 * MS, 5), (16 * MS, 1), (64 * MS, 1)];
        for replace in [OCI, SHARDS] {
            let ages = [0u32, 100, 10_000].map(|n| {
                let mut p = DiskProbe::new(&thin, replace).unwrap();
                let mut t = 0;
                for _ in 0..n {
                    p.start(t).unwrap();
                    t += 4 * MS;
                    p.finish(t).unwrap();
                    t += 100 * MS;
                }
                p.drain_age_ns().unwrap()
            });
            assert!(ages.iter().all(|a| *a == ages[0]), "{ages:?}");
        }
        let at = |replace| {
            DiskProbe::new(&thin, replace)
                .unwrap()
                .drain_age_ns()
                .unwrap()
        };
        assert!(at(OCI) < at(SHARDS));
    }

    /// AC-T7.10.3, Karn: a write that answers only after outliving the drain age (a stall that
    /// ended) is no healthy answer: the slowest and the healthy count don't move.
    #[test]
    fn a_stall_that_ends_is_no_answer() {
        let (mut p, t) = answered(SHARDS, 4 * MS, 10);
        let (slowest, total) = (p.slowest_ns(), p.healthy.total());
        p.start(t).unwrap();
        p.finish(t + 60 * S).unwrap();
        assert_eq!(p.slowest_ns(), slowest);
        assert_eq!(p.healthy.total(), total);
    }

    /// A healthy answer slower than the record but within the drain age raises the slowest, and
    /// faster ones never lower it. One bucket of record, so the drain age is that bucket's top.
    #[test]
    fn the_slowest_rises_and_never_falls() {
        let mut p = DiskProbe::new(&[(4 * MS, 10)], SHARDS).unwrap();
        let slow = p.drain_age_ns().unwrap();
        assert!(slow > p.slowest_ns());
        let mut t = 0;
        p.start(t).unwrap();
        t += slow;
        p.finish(t).unwrap();
        assert_eq!(p.slowest_ns(), slow);
        for _ in 0..100 {
            p.start(t).unwrap();
            t += MS;
            p.finish(t).unwrap();
        }
        assert_eq!(p.slowest_ns(), slow);
    }

    /// Hostile input: no record, a free re-placement, time going backwards, a second start, a finish
    /// with none outstanding, and a loss past u128 are typed refusals, never a panic.
    #[test]
    fn hostile_input_is_refused_typed() {
        assert!(matches!(
            DiskProbe::new(&[], OCI),
            Err(HealthError::Config(_))
        ));
        assert!(matches!(
            DiskProbe::new(&[(0, 9), (5, 0)], OCI),
            Err(HealthError::Config(_))
        ));
        assert!(matches!(
            DiskProbe::new(&record(), 0),
            Err(HealthError::Config(_))
        ));
        let mut p = DiskProbe::new(&record(), OCI).unwrap();
        p.start(5 * S).unwrap();
        assert_eq!(p.start(6 * S), Err(HealthError::ProbeOutstanding));
        assert!(matches!(
            p.finish(S),
            Err(HealthError::TimeBackwards { .. })
        ));
        assert!(matches!(p.level(S), Err(HealthError::TimeBackwards { .. })));
        p.finish(6 * S).unwrap();
        assert_eq!(p.finish(7 * S), Err(HealthError::NoProbe));
        let huge = DiskProbe::new(&[(u64::MAX, u32::MAX)], u64::MAX).unwrap();
        let mut huge = huge;
        huge.probes = u64::MAX;
        assert_eq!(
            huge.drain_age_ns(),
            Err(HealthError::Overflow("probe drain age"))
        );
    }

    /// The driver's wake-up: one past the earlier deadline, then one past the later.
    #[test]
    fn next_change_names_each_deadline() {
        let (mut p, t) = answered(SHARDS, 4 * MS, 10);
        assert_eq!(p.next_change().unwrap(), None);
        p.start(t).unwrap();
        let (s, d) = (p.slowest_ns(), p.drain_age_ns().unwrap());
        assert_eq!(p.next_change().unwrap(), Some(t + s.min(d) + 1));
        p.level(t).unwrap();
        p.finish(t + 1).unwrap();
    }
}
