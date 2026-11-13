//! CP-T7.4, B4: kiln's per-ask expected finish, adapted to a box's initialized class and image.
//! Source: hyper-light/kiln e2a330eb51ae7a197856e04f7e6ce8ab0431416c,
//! crates/kiln-ci/src/lb/owner/terms.rs (Terms, Warm, finish, Rates, Mean and arithmetic),
//! owner/orders.rs (Link), order.rs (class_of). CPU units retain Hex's ported lb constants.
//! The caller supplies learned work and measured rates; absent rates omit that term everywhere.
//! A box's complete create/start service, including staging, is measured by node and class.
//! Its population prior follows the source's empirical-Bayes mean. This combined service and
//! explicit modeled components are mutually exclusive: a component-bearing Terms omits the combined sample.

use crate::error::SimError;
use crate::lb::{CPU, SCALE, SPEED_ONE};
use crate::model::{Class, NodeId};

const NS: u128 = 1_000_000_000;
const ONE: u64 = 1 << 32;
/// Source order.rs: the six mantissa bits give 64 classes per octave and the 1/64 finish bound.
const ROUND: u32 = 6;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Link {
    pub rate: u64,
    pub latency_ns: u64,
    pub between_drops_ns: u64,
    pub outage_ns: u64,
}

/// Measured node rates. Byte rates are bytes/second, times nanoseconds. Speed and slowness are in
/// lb::SPEED_ONE units; zero means unmeasured, as in kiln's Word.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rates {
    pub speed: u32,
    pub slowness: u32,
    pub disk: u64,
    pub net: u64,
    pub link: Link,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Warm {
    pub node: NodeId,
    /// Reference-core CPU nanoseconds saved by this requested image's warm state.
    pub saved_ns: u64,
    /// Bytes of the requested image still missing here.
    pub missing: u64,
    pub wait_ns: u64,
}

impl Default for Warm {
    fn default() -> Self {
        Self {
            node: NodeId(0),
            saved_ns: 0,
            missing: 0,
            wait_ns: 0,
        }
    }
}

/// The request's learned launch components, selected by its class/image by the caller. cpu_ns is
/// reference-core CPU work; bytes/warm describe this image directly, with no lossy image key.
/// Any modeled component selects the component model instead of measured combined launch service.
/// No term introduces a weighting parameter.
#[derive(Debug, Clone, Copy, Default)]
pub struct Terms<'a> {
    pub parallel: u64,
    pub cpu_ns: u64,
    pub io_bytes: u64,
    pub net_bytes: u64,
    pub bytes: u64,
    pub interactive: bool,
    pub warm: &'a [Warm],
}

impl Terms<'_> {
    /// Exact source predicate: parallelism alone carries no time. A caller supplying any component
    /// chooses that model uniformly, even where its byte rates have not yet been measured.
    pub fn costs(&self) -> bool {
        (self.cpu_ns | self.io_bytes | self.net_bytes | self.bytes) != 0
            || self.interactive
            || !self.warm.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Mean {
    sum: u128,
    n: u32,
}

impl Mean {
    fn moved(&mut self, was: u64, now: u64) {
        self.replaced(
            if was == 0 { None } else { Some(was) },
            if now == 0 { None } else { Some(now) },
        );
    }

    fn replaced(&mut self, was: Option<u64>, now: Option<u64>) {
        if let Some(was) = was {
            self.sum = self.sum.saturating_sub(u128::from(was));
            self.n = self.n.saturating_sub(1);
        }
        if let Some(now) = now {
            self.sum = self.sum.saturating_add(u128::from(now));
            self.n = self.n.saturating_add(1);
        }
    }

    fn value(self) -> u64 {
        self.sum
            .checked_div(u128::from(self.n))
            .map_or(0, |m| u64::try_from(m).unwrap_or(u64::MAX))
    }

    fn observed(self) -> bool {
        self.n != 0
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Observation {
    sum: u128,
    n: u64,
}

impl Observation {
    fn value(self) -> u64 {
        self.sum
            .checked_div(u128::from(self.n))
            .map_or(0, |m| u64::try_from(m).unwrap_or(u64::MAX))
    }

    fn adding(self, ns: u64) -> Result<Self, SimError> {
        Ok(Self {
            sum: self
                .sum
                .checked_add(u128::from(ns))
                .ok_or(SimError::TimeOverflow)?,
            n: self.n.checked_add(1).ok_or(SimError::TimeOverflow)?,
        })
    }
}

/// CP-T7.4: the exact bounded aggregate persisted by the shell. A zero sum with a nonzero count
/// is a measured zero, distinct from a node/class with no observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartObservation {
    pub node: NodeId,
    pub class: Class,
    pub sum_ns: u128,
    pub count: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct Measured {
    raw: Rates,
    scale: u32,
    bytes: [u64; 3],
    drops: u128,
}

impl Measured {
    fn of(raw: Rates) -> Self {
        let speed = if raw.speed == 0 { SPEED_ONE } else { raw.speed };
        let slow = if raw.slowness == 0 {
            SPEED_ONE
        } else {
            raw.slowness
        };
        let scale = u64::from(SCALE)
            .saturating_mul(u64::from(speed))
            .checked_div(u64::from(slow))
            .map_or(SCALE, |n| u32::try_from(n).unwrap_or(u32::MAX))
            .max(1);
        Self {
            raw,
            scale,
            bytes: [
                per_byte(raw.disk),
                per_byte(raw.net),
                per_byte(raw.link.rate),
            ],
            drops: (1u128 << 96)
                .checked_div(u128::from(raw.link.between_drops_ns))
                .unwrap_or(0),
        }
    }
}

/// Fixed state per node and per class; no history queue or image cache. Every retained sum/count
/// is bounded by its integer representation and refuses overflow before changing a sample.
#[derive(Debug, Clone)]
pub(crate) struct Costs {
    nodes: Vec<Measured>,
    priors: [Mean; 3],
    starts: Vec<[Observation; 3]>,
    start_priors: [Mean; 3],
}

pub(crate) struct Prepared<'a> {
    terms: &'a Terms<'a>,
    warm: Vec<Warm>,
}

impl Costs {
    pub(crate) fn try_clone(&self) -> Result<Self, SimError> {
        fn copy<T: Copy>(values: &[T]) -> Result<Vec<T>, SimError> {
            let mut out = Vec::new();
            out.try_reserve_exact(values.len())
                .map_err(|_| SimError::Full {
                    what: "start-cost snapshot",
                    bound: values.len(),
                })?;
            out.extend_from_slice(values);
            Ok(out)
        }
        Ok(Self {
            nodes: copy(&self.nodes)?,
            priors: self.priors,
            starts: copy(&self.starts)?,
            start_priors: self.start_priors,
        })
    }

    pub(crate) fn new(nodes: usize) -> Self {
        Self {
            nodes: vec![Measured::of(Rates::default()); nodes],
            priors: [Mean::default(); 3],
            starts: vec![[Observation::default(); 3]; nodes],
            start_priors: [Mean::default(); 3],
        }
    }

    pub(crate) fn observed_class(&self, class: Class) -> bool {
        self.start_priors
            .get(class.index())
            .is_some_and(|p| p.observed())
    }

    pub(crate) fn set_rates(&mut self, node: NodeId, rates: Rates) -> Result<(), SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let old = self.nodes.get_mut(at).ok_or(SimError::UnknownNode(node))?;
        let now = Measured::of(rates);
        for ((prior, was), next) in self.priors.iter_mut().zip(old.bytes).zip(now.bytes) {
            prior.moved(was, next);
        }
        *old = now;
        Ok(())
    }

    pub(crate) fn observe(
        &mut self,
        node: NodeId,
        class: Class,
        elapsed_ns: u64,
    ) -> Result<(), SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let old = self
            .starts
            .get_mut(at)
            .and_then(|n| n.get_mut(class.index()))
            .ok_or(SimError::UnknownNode(node))?;
        let now = old.adding(elapsed_ns)?;
        if let Some(prior) = self.start_priors.get_mut(class.index()) {
            prior.replaced((old.n != 0).then(|| old.value()), Some(now.value()));
        }
        *old = now;
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> Vec<StartObservation> {
        let mut out = Vec::new();
        for (at, starts) in self.starts.iter().enumerate() {
            let Ok(node) = u32::try_from(at) else {
                continue;
            };
            for class in [Class::Oci, Class::Firecracker, Class::Shards] {
                if let Some(start) = starts.get(class.index()).filter(|s| s.n != 0) {
                    out.push(StartObservation {
                        node: NodeId(node),
                        class,
                        sum_ns: start.sum,
                        count: start.n,
                    });
                }
            }
        }
        out
    }

    pub(crate) fn restore(&mut self, rows: &[StartObservation]) -> Result<(), SimError> {
        let bound = self.nodes.len().saturating_mul(3);
        if rows.len() > bound {
            return Err(SimError::Full {
                what: "start observation aggregates",
                bound,
            });
        }
        let mut starts = vec![[Observation::default(); 3]; self.nodes.len()];
        let mut priors = [Mean::default(); 3];
        for row in rows {
            let at = usize::try_from(row.node.0).map_err(|_| SimError::UnknownNode(row.node))?;
            let slot = starts
                .get_mut(at)
                .and_then(|s| s.get_mut(row.class.index()))
                .ok_or(SimError::UnknownNode(row.node))?;
            if slot.n != 0 {
                return Err(SimError::Config(
                    "a start observation aggregate is duplicated",
                ));
            }
            if row.count == 0
                || row.sum_ns > u128::from(u64::MAX).saturating_mul(u128::from(row.count))
            {
                return Err(SimError::Config(
                    "a start observation aggregate is inconsistent",
                ));
            }
            *slot = Observation {
                sum: row.sum_ns,
                n: row.count,
            };
            if let Some(prior) = priors.get_mut(row.class.index()) {
                prior.replaced(None, Some(slot.value()));
            }
        }
        self.starts = starts;
        self.start_priors = priors;
        Ok(())
    }

    pub(crate) fn prepare<'a>(&self, terms: &'a Terms<'a>) -> Result<Prepared<'a>, SimError> {
        if terms.warm.len() > self.nodes.len() {
            return Err(SimError::Full {
                what: "an ask's warm nodes",
                bound: self.nodes.len(),
            });
        }
        let mut warm = terms.warm.to_vec();
        warm.sort_unstable_by_key(|w| w.node);
        let mut previous = None;
        for w in &warm {
            if usize::try_from(w.node.0)
                .ok()
                .is_none_or(|i| i >= self.nodes.len())
            {
                return Err(SimError::UnknownNode(w.node));
            }
            if previous == Some(w.node) {
                return Err(SimError::Config("a warm node is listed twice"));
            }
            previous = Some(w.node);
        }
        Ok(Prepared { terms, warm })
    }

    /// Source finish(): every component is nanoseconds. With no modeled components, use measured
    /// complete launch service and its empirical population prior. A modeled transfer, warm state,
    /// CPU, I/O, network or interactive component excludes that combined elapsed time entirely.
    pub(crate) fn finish(
        &self,
        node: NodeId,
        class: Class,
        cpu: u64,
        free_cpu: u64,
        t: &Prepared<'_>,
    ) -> u128 {
        let Some(at) = usize::try_from(node.0).ok() else {
            return u128::MAX;
        };
        let Some(rate) = self.nodes.get(at) else {
            return u128::MAX;
        };
        let terms = t.terms;
        let warm = t
            .warm
            .binary_search_by_key(&node, |w| w.node)
            .ok()
            .and_then(|i| t.warm.get(i));
        let (saved, missing, wait) =
            warm.map_or((0, terms.bytes, 0), |w| (w.saved_ns, w.missing, w.wait_ns));
        let parallel = if terms.parallel == 0 {
            cpu
        } else {
            terms.parallel
        };
        let cores = if parallel > cpu {
            parallel.min(free_cpu.max(cpu))
        } else {
            parallel
        };
        let compute = cpu_time(terms.cpu_ns.saturating_sub(saved), cores, rate.scale);
        let byte_time = |at: usize, bytes| {
            let own = rate.bytes.get(at).copied().unwrap_or(0);
            let multiplier = if own == 0 {
                self.priors.get(at).copied().unwrap_or_default().value()
            } else {
                own
            };
            mul_q32(u128::from(bytes), multiplier)
        };
        let run = compute
            .saturating_add(byte_time(0, terms.io_bytes))
            .saturating_add(byte_time(1, terms.net_bytes));
        let observation = self
            .starts
            .get(at)
            .and_then(|n| n.get(class.index()))
            .copied()
            .unwrap_or_default();
        let start = if terms.costs() {
            0
        } else if observation.n != 0 {
            observation.value()
        } else {
            self.start_priors
                .get(class.index())
                .copied()
                .unwrap_or_default()
                .value()
        };
        let mut finish = u128::from(wait)
            .saturating_add(u128::from(start))
            .saturating_add(byte_time(2, missing))
            .saturating_add(run);
        if terms.interactive && self.priors.get(2).is_some_and(|p| p.observed()) {
            finish = finish
                .saturating_add(u128::from(rate.raw.link.latency_ns))
                .saturating_add(stability(run, rate.drops, rate.raw.link.outage_ns));
        }
        finish
    }
}

/// Source within_bound after a complete scan: nodes inside the orders' resolution let packing decide.
pub(crate) fn within_bound(soonest: u128) -> u128 {
    soonest.saturating_add(soonest >> ROUND)
}

/// Source order.rs's octave and six mantissa bits, with checked arithmetic for Hex's production bar.
pub fn class_of(rate: u64) -> u32 {
    if rate == 0 {
        return 0;
    }
    let e = 63u32.saturating_sub(rate.leading_zeros());
    let mask = (1u64 << ROUND).saturating_sub(1);
    let m = if e >= ROUND {
        rate.checked_shr(e.saturating_sub(ROUND)).unwrap_or(0) & mask
    } else {
        rate.checked_shl(ROUND.saturating_sub(e)).unwrap_or(0) & mask
    };
    e.saturating_mul(1u32 << ROUND)
        .saturating_add(u32::try_from(m).unwrap_or(u32::MAX))
}

fn cpu_time(work: u64, cores: u64, scale: u32) -> u128 {
    u128::from(work)
        .saturating_mul(u128::from(CPU))
        .saturating_mul(u128::from(SCALE))
        .checked_div(u128::from(cores.max(1)).saturating_mul(u128::from(scale.max(1))))
        .unwrap_or(u128::MAX)
}

fn per_byte(rate: u64) -> u64 {
    (NS << 32)
        .checked_div(u128::from(rate))
        .map_or(0, |n| u64::try_from(n).unwrap_or(u64::MAX))
}

fn mul_q32(x: u128, q: u64) -> u128 {
    x.checked_mul(u128::from(q)).map_or(u128::MAX, |v| v >> 32)
}

fn stability(run: u128, drops: u128, outage: u64) -> u128 {
    if drops == 0 || outage == 0 {
        return 0;
    }
    let x = run.checked_mul(drops).map_or(u128::MAX, |v| v >> 64);
    let Some(x) = u64::try_from(x)
        .ok()
        .filter(|x| *x < 64u64.saturating_mul(ONE))
    else {
        return u128::from(outage);
    };
    let chance = ONE.saturating_sub(exp_neg_q32(x));
    u128::from(outage).saturating_mul(u128::from(chance)) >> 32
}

/// Source's Q32 e^-x: six-term Horner polynomial at x/16, four squarings, then e^-1 factors.
/// Constants are rounded mathematical reciprocals/exponential, with error below 2^-25 (source proof).
fn exp_neg_q32(x: u64) -> u64 {
    const E_INV: u64 = 1_580_030_169;
    const INV: [u64; 5] = [
        2_147_483_648,
        1_431_655_766,
        1_073_741_824,
        858_993_460,
        715_827_883,
    ];
    let (n, f) = (x >> 32, x & ONE.saturating_sub(1));
    let g = f >> 4;
    let mut h = ONE;
    for inv in INV.iter().rev() {
        h = ONE.saturating_sub((g.saturating_mul(*inv) >> 32).saturating_mul(h) >> 32);
    }
    h = ONE.saturating_sub(g.saturating_mul(h) >> 32);
    let mut e = u128::from(h);
    for _ in 0..4 {
        e = e.saturating_mul(e) >> 32;
    }
    let mut e = u64::try_from(e).unwrap_or(ONE);
    for _ in 0..n {
        e = e.saturating_mul(E_INV) >> 32;
        if e == 0 {
            break;
        }
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CP-T7.4/B4: the source finish formula adds physical nanoseconds, with warm CPU savings,
    /// missing image bytes, wide parallelism and an interactive-only link latency.
    #[test]
    fn start_cost_terms_retain_kilns_units_and_warm_transfer() {
        let mut costs = Costs::new(2);
        costs
            .set_rates(
                NodeId(1),
                Rates {
                    speed: 2 * SPEED_ONE,
                    disk: 1_000_000_000,
                    net: 1_000_000_000,
                    link: Link {
                        rate: 1_000_000_000,
                        latency_ns: 10,
                        ..Link::default()
                    },
                    ..Rates::default()
                },
            )
            .unwrap();
        let warm = [Warm {
            node: NodeId(1),
            saved_ns: 200,
            missing: 40,
            wait_ns: 20,
        }];
        let terms = Terms {
            parallel: 2 * CPU,
            cpu_ns: 1000,
            io_bytes: 30,
            net_bytes: 20,
            bytes: 100,
            warm: &warm,
            ..Terms::default()
        };
        let p = costs.prepare(&terms).unwrap();
        assert_eq!(costs.finish(NodeId(1), Class::Oci, CPU, 4 * CPU, &p), 310);
        assert_eq!(costs.finish(NodeId(0), Class::Oci, CPU, 4 * CPU, &p), 650);
        assert_eq!(costs.finish(NodeId(1), Class::Oci, CPU, CPU, &p), 510);
        let interactive = Terms {
            interactive: true,
            ..terms
        };
        assert_eq!(
            costs.finish(
                NodeId(1),
                Class::Oci,
                CPU,
                4 * CPU,
                &costs.prepare(&interactive).unwrap()
            ),
            320
        );
    }

    /// CP-T7.4: observed combined launch service counts; means are by node/class and unknown
    /// nodes take that class's measured population prior rather than a chosen cold-start value.
    #[test]
    fn start_cost_observations_are_by_class_with_empirical_prior() {
        let mut costs = Costs::new(2);
        costs.observe(NodeId(0), Class::Oci, 100).unwrap();
        costs.observe(NodeId(0), Class::Oci, 300).unwrap();
        let terms = Terms::default();
        let p = costs.prepare(&terms).unwrap();
        assert_eq!(costs.finish(NodeId(0), Class::Oci, CPU, CPU, &p), 200);
        assert_eq!(costs.finish(NodeId(1), Class::Oci, CPU, CPU, &p), 200);
        costs.observe(NodeId(1), Class::Oci, 400).unwrap();
        assert_eq!(costs.finish(NodeId(1), Class::Oci, CPU, CPU, &p), 400);
        assert_eq!(costs.finish(NodeId(1), Class::Shards, CPU, CPU, &p), 0);
        assert!(costs.observe(NodeId(2), Class::Oci, 100).is_err());
        assert_eq!(costs.finish(NodeId(0), Class::Oci, CPU, CPU, &p), 200);
    }

    /// CP-T7.4: opaque Runtime.create includes staging. Its complete elapsed time is charged once
    /// by default; a real per-image component model uniformly replaces it rather than counting both.
    #[test]
    fn start_cost_combined_launch_and_modeled_components_are_exclusive() {
        let mut costs = Costs::new(1);
        costs.observe(NodeId(0), Class::Oci, 1000).unwrap();
        costs
            .set_rates(
                NodeId(0),
                Rates {
                    disk: 1_000_000_000,
                    link: Link {
                        rate: 1_000_000_000,
                        latency_ns: 10,
                        ..Link::default()
                    },
                    ..Rates::default()
                },
            )
            .unwrap();
        let predict = |terms: &Terms<'_>| {
            costs.finish(
                NodeId(0),
                Class::Oci,
                CPU,
                CPU,
                &costs.prepare(terms).unwrap(),
            )
        };
        assert_eq!(predict(&Terms::default()), 1000);
        assert_eq!(
            predict(&Terms {
                bytes: 100,
                ..Terms::default()
            }),
            100
        );
        assert_eq!(
            predict(&Terms {
                io_bytes: 200,
                ..Terms::default()
            }),
            200
        );
        assert_eq!(
            predict(&Terms {
                cpu_ns: 300,
                ..Terms::default()
            }),
            300
        );
        assert_eq!(
            predict(&Terms {
                interactive: true,
                ..Terms::default()
            }),
            10
        );
        let warm = [Warm {
            node: NodeId(0),
            wait_ns: 20,
            ..Warm::default()
        }];
        assert_eq!(
            predict(&Terms {
                warm: &warm,
                ..Terms::default()
            }),
            20
        );
        assert_eq!(
            predict(&Terms {
                parallel: 2 * CPU,
                ..Terms::default()
            }),
            1000
        );
    }

    /// CP-T7.4: bounded aggregates survive restart without replaying means as observations;
    /// measured zero remains zero beside a nonzero prior. Invalid restore leaves the model intact.
    #[test]
    fn start_cost_aggregates_restore_exactly_and_accept_measured_zero() {
        let mut costs = Costs::new(3);
        costs.observe(NodeId(0), Class::Oci, 0).unwrap();
        costs.observe(NodeId(1), Class::Oci, 100).unwrap();
        costs.observe(NodeId(1), Class::Oci, 300).unwrap();
        let rows = costs.snapshot();
        let mut restored = Costs::new(3);
        restored.restore(&rows).unwrap();
        assert_eq!(restored.snapshot(), rows);
        let terms = Terms::default();
        let p = restored.prepare(&terms).unwrap();
        assert_eq!(restored.finish(NodeId(0), Class::Oci, CPU, CPU, &p), 0);
        assert_eq!(restored.finish(NodeId(1), Class::Oci, CPU, CPU, &p), 200);
        assert_eq!(restored.finish(NodeId(2), Class::Oci, CPU, CPU, &p), 100);
        restored.observe(NodeId(1), Class::Oci, 200).unwrap();
        assert_eq!(restored.snapshot().get(1).unwrap().count, 3);
        let was = restored.snapshot();
        let mut invalid = was.clone();
        invalid.push(invalid[0]);
        assert!(restored.restore(&invalid).is_err());
        assert_eq!(restored.snapshot(), was);
    }

    /// CP-T7.4/B4: E[ns/byte] is the mean, not mean bytes/second; an unmeasured byte term is
    /// disabled uniformly, and a later measurement updates that prior without retaining history.
    #[test]
    fn start_cost_byte_priors_are_harmonic_and_unmeasured_terms_are_absent() {
        let mut costs = Costs::new(3);
        let terms = Terms {
            io_bytes: 100,
            ..Terms::default()
        };
        let p = costs.prepare(&terms).unwrap();
        assert_eq!(costs.finish(NodeId(0), Class::Oci, CPU, CPU, &p), 0);
        costs
            .set_rates(
                NodeId(0),
                Rates {
                    disk: 1_000_000_000,
                    ..Rates::default()
                },
            )
            .unwrap();
        costs
            .set_rates(
                NodeId(1),
                Rates {
                    disk: 500_000_000,
                    ..Rates::default()
                },
            )
            .unwrap();
        assert_eq!(costs.finish(NodeId(0), Class::Oci, CPU, CPU, &p), 100);
        assert_eq!(costs.finish(NodeId(1), Class::Oci, CPU, CPU, &p), 200);
        assert_eq!(costs.finish(NodeId(2), Class::Oci, CPU, CPU, &p), 150);
        costs.set_rates(NodeId(1), Rates::default()).unwrap();
        assert_eq!(costs.finish(NodeId(2), Class::Oci, CPU, CPU, &p), 100);
    }

    /// CP-T7.4: the original integer order resolution and exponential accuracy are preserved.
    #[test]
    fn start_cost_resolution_and_stability_match_the_source() {
        assert_eq!(
            (
                class_of(0),
                class_of(1),
                class_of(64),
                class_of(65),
                class_of(u64::MAX)
            ),
            (0, 0, 384, 385, 4095)
        );
        assert_eq!(within_bound(64), 65);
        for x in [0, ONE / 2, ONE, 2 * ONE, 32 * ONE, 63 * ONE] {
            let got = exp_neg_q32(x) as f64 / ONE as f64;
            let exact = (-(x as f64) / ONE as f64).exp();
            assert!(
                (got - exact).abs() < 2f64.powi(-25),
                "{x}: {got} vs {exact}"
            );
        }
        assert_eq!(stability(u128::MAX, u128::MAX, 100), 100);
    }

    /// CP-T7.4: the warm input is bounded by the actual node set; ambiguous or unknown nodes
    /// are refused before they can affect a pick.
    #[test]
    fn start_cost_warm_inputs_are_bounded_and_unambiguous() {
        let costs = Costs::new(2);
        let duplicate = [Warm::default(), Warm::default()];
        assert!(
            costs
                .prepare(&Terms {
                    warm: &duplicate,
                    ..Terms::default()
                })
                .is_err()
        );
        let unknown = [Warm {
            node: NodeId(2),
            ..Warm::default()
        }];
        assert!(
            costs
                .prepare(&Terms {
                    warm: &unknown,
                    ..Terms::default()
                })
                .is_err()
        );
        let excessive = [Warm::default(); 3];
        assert!(
            costs
                .prepare(&Terms {
                    warm: &excessive,
                    ..Terms::default()
                })
                .is_err()
        );
    }
}
