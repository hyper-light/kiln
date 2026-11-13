//! Penalties that escalate, forgive and are capped, so no detector's verdict, right or wrong, can take
//! a cell down (S8: repeat offenders, mass loss).
//!
//! - [`Ejector`]: a node taken out of placement stays out for the base time multiplied by its
//!   consecutive ejections, up to a maximum (Envoy's outlier ejection, PF[18]), and is taken out only
//!   when the cell can re-place its work: the scheduler measures that at the moment of ejection
//!   (re-placing the node's workloads on what the rest of the cell has free), so a correlated fault
//!   drains no cell, with no copied share of nodes.
//! - [`EvictionLimiter`]: work moved off failed nodes at a rate that drops when most of the cell has
//!   failed at once, since a mass failure looks like a partition (Borg PF[37] §3.5; Kubernetes'
//!   node lifecycle controller).
//! - [`CrashBackoff`]: a workload that keeps failing restarts after a doubling delay, forgiven once
//!   it has run long enough (Kubernetes' CrashLoopBackOff, PF[17]).

use std::collections::BTreeMap;

use crate::HealthError;

const S: u64 = 1_000_000_000;

/// Envoy `base_ejection_time` default: "Defaults to 30000ms or 30s" (outlier_detection.proto:42, v1.36.0).
pub const BASE_EJECTION_NS: u64 = 30 * S;
/// Envoy `max_ejection_time` default: 300 s (outlier_detection.proto:157, v1.36.0).
pub const MAX_EJECTION_NS: u64 = 300 * S;
/// Kubernetes `--node-eviction-rate` default 0.1 nodes/s, in thousandths of a node per second.
pub const EVICTION_MILLI_PER_S: u64 = 100;
/// Kubernetes `--secondary-node-eviction-rate` default 0.01 nodes/s, once the cell is unhealthy.
pub const SECONDARY_EVICTION_MILLI_PER_S: u64 = 10;
/// Kubernetes `--unhealthy-zone-threshold` default 0.55, "minimum 3" nodes not ready.
pub const UNHEALTHY_PER_MILLE: u64 = 550;
pub const UNHEALTHY_MIN_NODES: u64 = 3;
/// Kubernetes `--large-cluster-size-threshold` default 50: at or below it the secondary rate is 0.
pub const LARGE_CELL_NODES: u64 = 50;
/// CrashLoopBackOff: "10s, 20s, 40s, …, capped at 300 seconds", reset after "10 minutes without any
/// problems" (Kubernetes Pod Lifecycle, PF[17]).
pub const BACKOFF_BASE_NS: u64 = 10 * S;
pub const BACKOFF_CAP_NS: u64 = 300 * S;
pub const BACKOFF_RESET_NS: u64 = 600 * S;

/// Why a penalty was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The rest of the cell can't re-place the node's work, as the scheduler measured it.
    NoRoom,
    /// No eviction token until `next_ns`; at a rate of 0, never (`None`).
    RateLimited { next_ns: Option<u64> },
}

#[derive(Debug, Clone, Copy)]
struct Ejection {
    /// Consecutive ejections, decremented once per base time healthy (Envoy's multiplier decay).
    count: u64,
    until_ns: u64,
    healthy_since_ns: u64,
}

#[derive(Debug, Clone)]
pub struct Ejector {
    max_tracked: usize,
    nodes: BTreeMap<u64, Ejection>,
}

impl Ejector {
    pub fn new(max_tracked: usize) -> Self {
        Self {
            max_tracked,
            nodes: BTreeMap::new(),
        }
    }

    pub fn ejected_at(&self, now_ns: u64) -> u64 {
        let n = self.nodes.values().filter(|e| e.until_ns > now_ns).count();
        u64::try_from(n).unwrap_or(u64::MAX)
    }

    pub fn is_ejected(&self, node: u64, now_ns: u64) -> bool {
        self.nodes.get(&node).is_some_and(|e| e.until_ns > now_ns)
    }

    /// Eject `node` at `now_ns`, where `re_placeable` is the scheduler's measure of whether the rest
    /// of the cell can re-place its work now; returns until when, or [`Refusal::NoRoom`]. A node
    /// already ejected stays as it is.
    pub fn eject(
        &mut self,
        node: u64,
        now_ns: u64,
        re_placeable: bool,
    ) -> Result<Result<u64, Refusal>, HealthError> {
        if let Some(e) = self.nodes.get(&node)
            && e.until_ns > now_ns
        {
            return Ok(Ok(e.until_ns));
        }
        if !re_placeable {
            return Ok(Err(Refusal::NoRoom));
        }
        if !self.nodes.contains_key(&node) && self.nodes.len() >= self.max_tracked {
            return Err(HealthError::Full {
                what: "ejection records",
                bound: self.max_tracked,
            });
        }
        let e = self.nodes.entry(node).or_insert(Ejection {
            count: 0,
            until_ns: 0,
            healthy_since_ns: now_ns,
        });
        e.count = e.count.saturating_add(1);
        let span = BASE_EJECTION_NS
            .saturating_mul(e.count)
            .min(MAX_EJECTION_NS.max(BASE_EJECTION_NS));
        e.until_ns = now_ns.saturating_add(span);
        e.healthy_since_ns = e.until_ns;
        Ok(Ok(e.until_ns))
    }

    /// `node` was seen healthy at `now_ns`: each base time healthy since its return takes one off its
    /// multiplier, and a node back to zero is forgotten.
    pub fn healthy(&mut self, node: u64, now_ns: u64) {
        let Some(e) = self.nodes.get_mut(&node) else {
            return;
        };
        if e.until_ns > now_ns {
            return;
        }
        let periods = now_ns.saturating_sub(e.healthy_since_ns) / BASE_EJECTION_NS;
        if periods > 0 {
            e.count = e.count.saturating_sub(periods);
            e.healthy_since_ns = e
                .healthy_since_ns
                .saturating_add(periods.saturating_mul(BASE_EJECTION_NS));
        }
        if e.count == 0 {
            self.nodes.remove(&node);
        }
    }
}

/// A token bucket of one eviction (a burst of one), refilled at the rate the cell's health allows.
#[derive(Debug, Clone)]
pub struct EvictionLimiter {
    /// Thousandths of an eviction token.
    milli: u64,
    last_ns: u64,
}

impl EvictionLimiter {
    pub fn new(now_ns: u64) -> Self {
        Self {
            milli: 1000,
            last_ns: now_ns,
        }
    }

    /// The rate for a cell of `nodes` with `unhealthy` of them not ready, in thousandths per second.
    pub fn rate(nodes: u64, unhealthy: u64) -> u64 {
        let unhealthy_cell = unhealthy >= UNHEALTHY_MIN_NODES
            && unhealthy.saturating_mul(1000) >= nodes.saturating_mul(UNHEALTHY_PER_MILLE);
        if !unhealthy_cell {
            EVICTION_MILLI_PER_S
        } else if nodes > LARGE_CELL_NODES {
            SECONDARY_EVICTION_MILLI_PER_S
        } else {
            0
        }
    }

    /// Take one eviction at `now_ns` in a cell of `nodes` with `unhealthy` not ready.
    pub fn take(
        &mut self,
        now_ns: u64,
        nodes: u64,
        unhealthy: u64,
    ) -> Result<Result<(), Refusal>, HealthError> {
        if now_ns < self.last_ns {
            return Err(HealthError::TimeBackwards {
                what: "eviction limiter",
                now_ns,
                last_ns: self.last_ns,
            });
        }
        let rate = Self::rate(nodes, unhealthy);
        let elapsed = now_ns.saturating_sub(self.last_ns);
        // rate thousandths/s × elapsed ns / 1e9, in u128 so it can't overflow.
        let gained = u128::from(rate)
            .saturating_mul(u128::from(elapsed))
            .checked_div(u128::from(S))
            .unwrap_or(0);
        let gained = u64::try_from(gained).unwrap_or(u64::MAX);
        self.milli = self.milli.saturating_add(gained).min(1000);
        self.last_ns = now_ns;
        if self.milli >= 1000 {
            self.milli = 0;
            return Ok(Ok(()));
        }
        let next_ns = if rate == 0 {
            None
        } else {
            let missing = 1000u64.saturating_sub(self.milli);
            let wait = u128::from(missing)
                .saturating_mul(u128::from(S))
                .div_ceil(u128::from(rate));
            u64::try_from(wait).ok().map(|w| now_ns.saturating_add(w))
        };
        Ok(Err(Refusal::RateLimited { next_ns }))
    }
}

/// A workload's crash-loop backoff.
#[derive(Debug, Clone, Default)]
pub struct CrashBackoff {
    failures: u32,
}

impl CrashBackoff {
    pub fn new() -> Self {
        Self::default()
    }

    /// The workload ended in failure after running `ran_ns`: the delay before its next start. A run
    /// of [`BACKOFF_RESET_NS`] or more forgives what came before.
    pub fn failed(&mut self, ran_ns: u64) -> u64 {
        if ran_ns >= BACKOFF_RESET_NS {
            self.failures = 0;
        }
        let delay = 1u64
            .checked_shl(self.failures)
            .and_then(|m| BACKOFF_BASE_NS.checked_mul(m))
            .unwrap_or(BACKOFF_CAP_NS)
            .min(BACKOFF_CAP_NS);
        self.failures = self.failures.saturating_add(1).min(64);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S8 repeat offenders: each consecutive ejection lasts the base time times its count, capped at
    /// the maximum.
    #[test]
    fn ejection_escalates_by_count_and_caps() {
        let mut e = Ejector::new(1000);
        let mut t = 0;
        let mut spans = Vec::new();
        for _ in 0..12 {
            let until = e.eject(7, t, true).unwrap().unwrap();
            spans.push(until - t);
            t = until;
        }
        assert_eq!(spans[0], 30 * S);
        assert_eq!(spans[1], 60 * S);
        assert_eq!(spans[9], 300 * S);
        assert_eq!(spans[11], 300 * S);
    }

    /// S8: a healthy node is forgiven one multiple per base time, and forgotten at zero.
    #[test]
    fn healthy_time_forgives() {
        let mut e = Ejector::new(1000);
        let u1 = e.eject(7, 0, true).unwrap().unwrap();
        let u2 = e.eject(7, u1, true).unwrap().unwrap();
        assert_eq!(u2 - u1, 60 * S);
        e.healthy(7, u2 + 30 * S);
        let u3 = e.eject(7, u2 + 30 * S, true).unwrap().unwrap();
        assert_eq!(u3 - (u2 + 30 * S), 60 * S);
        e.healthy(7, u3 + 600 * S);
        let u4 = e.eject(7, u3 + 600 * S, true).unwrap().unwrap();
        assert_eq!(u4 - (u3 + 600 * S), 30 * S);
    }

    /// S8 ejection (F6, the criterion restated 2026-11-06): do eject nodes while the scheduler says
    /// the cell can re-place their work, then one it says can't; expect the first ejected and the last
    /// refused, typed, leaving it in placement; a node already out stays out either way.
    #[test]
    fn a_node_is_ejected_only_when_its_work_can_be_re_placed() {
        let mut e = Ejector::new(1000);
        for n in 0..5 {
            assert!(e.eject(n, S, true).unwrap().is_ok());
        }
        assert_eq!(e.eject(5, S, false).unwrap(), Err(Refusal::NoRoom));
        assert!(!e.is_ejected(5, S));
        assert_eq!(e.ejected_at(S), 5);
        assert!(e.eject(0, 2 * S, false).unwrap().is_ok());
    }

    /// S8 mass loss: 0.1 evictions/s in a healthy cell, 0.01/s once 55% (and at least 3) are down in
    /// a cell over 50 nodes, and none in a smaller one.
    #[test]
    fn evictions_are_rate_limited_and_slow_when_most_of_the_cell_is_down() {
        assert_eq!(EvictionLimiter::rate(100, 10), 100);
        assert_eq!(EvictionLimiter::rate(100, 55), 10);
        assert_eq!(EvictionLimiter::rate(40, 30), 0);
        assert_eq!(EvictionLimiter::rate(4, 2), 100);
        let mut l = EvictionLimiter::new(0);
        let mut granted = 0;
        let mut t = 0;
        while t <= 100 * S {
            if l.take(t, 100, 10).unwrap().is_ok() {
                granted += 1;
            }
            t += S / 10;
        }
        assert_eq!(granted, 11);
        let mut l = EvictionLimiter::new(0);
        assert!(l.take(0, 100, 60).unwrap().is_ok());
        assert_eq!(
            l.take(S, 100, 60).unwrap(),
            Err(Refusal::RateLimited {
                next_ns: Some(100 * S)
            })
        );
        assert_eq!(
            l.take(2 * S, 40, 30).unwrap(),
            Err(Refusal::RateLimited { next_ns: None })
        );
        assert!(matches!(
            l.take(0, 100, 0),
            Err(HealthError::TimeBackwards { .. })
        ));
    }

    /// S8 crash loops: 10, 20, 40 s ... capped at 300 s, and a 10-minute run forgives.
    #[test]
    fn crash_loop_backoff_doubles_caps_and_forgives() {
        let mut b = CrashBackoff::new();
        let delays: Vec<u64> = (0..8).map(|_| b.failed(S)).collect();
        assert_eq!(
            delays[..6],
            [10 * S, 20 * S, 40 * S, 80 * S, 160 * S, 300 * S]
        );
        assert_eq!(delays[7], 300 * S);
        for _ in 0..100 {
            assert_eq!(b.failed(S), 300 * S);
        }
        assert_eq!(b.failed(600 * S), 10 * S);
    }
}
