//! A modelled WAN, and hyper-swim's coordinates learned over it (`hex-sim.md` H20, scheduler design
//! S4 and A2).
//!
//! **Regions.** Eight Azure regions, their round trips Azure's published monthly P50s between
//! regions, measured by its backbone probes in 1-minute intervals ("Azure network round-trip latency
//! statistics", learn.microsoft.com/azure/networking/azure-network-latency, 2026-07-30). The table
//! gives each direction; the two differ by at most 2 ms, and the larger is kept, so the matrix is
//! symmetric. The table is real data: a few of its triples violate the triangle inequality, as
//! Internet latencies do (Ledlie, Gardner and Seltzer, NSDI 2007).
//!
//! **Zones and nodes.** A region has three zones; Azure's stated bound between zones is "less than
//! approximately 2 milliseconds" ("What are Azure availability zones?", Inter-zone networking), and
//! only that bound is published. The record's WAN ([`Access::Bound`]) takes the bound itself as every
//! pair of zones' round trip, the worst case Azure allows, and gives nodes no access height and probes
//! no jitter: nothing measured states either, so the record claims inter-region and inter-zone
//! structure only, and two nodes of one zone are 0 apart. [`Access::Modelled`] is for a labelled
//! sensitivity run, never the record: zone terms drawn below the bound, heights drawn from
//! [`HEIGHT_NS`] and probes stretched by up to [`JITTER_PER_MILLE`], each modelled, not measured.
//!
//! **Coordinates.** Every node runs [`CoordinateEngine`], hyper-swim's update, on hyper-swim's own
//! probe schedule: one probe a protocol period, each round a fresh shuffled permutation of the other
//! members, so every member is probed once a round (its detector's `RandomizedOrder`, SWIM §4.3).
//! Each probe moves the prober by its sample against the peer's coordinate as it stood. Rounds run
//! with scale-free patience (the lead, 2026-11-06): the lowest median relative error over every pair
//! so far is tracked, and the convergence stops once as many rounds have passed without a new low as
//! it took to reach that low (best at round 4, stop at round 8 if nothing lower), so a noisy uptick
//! doesn't stop it and no round count is chosen ([`Coordinates::history`] holds the error after each
//! round; [`MAX_ROUNDS`] only bounds it). The coordinates are the last round's, as a running
//! cluster's are: choosing the lowest round would read the true RTTs, which no node knows. A seed
//! names the result.

use std::time::Duration;

use crate::error::SimError;
use crate::model::NodeId;
use crate::rng::Rng;
use crate::trace::TraceHash;
use crate::vivaldi::{CoordinateEngine, NetworkCoordinate};

/// The regions, in the order of [`INTER_MS`].
pub const REGIONS: [&str; 8] = [
    "East US",
    "West US",
    "North Europe",
    "West Europe",
    "Southeast Asia",
    "Japan East",
    "Brazil South",
    "Australia East",
];

/// Azure's P50 round trip between regions, milliseconds, the larger of its two directions (module
/// documentation). Row and column follow [`REGIONS`]; the diagonal is the zones' term instead.
pub const INTER_MS: [[u64; 8]; 8] = [
    [0, 69, 73, 85, 224, 162, 118, 202],
    [69, 0, 133, 146, 170, 108, 169, 140],
    [73, 133, 0, 17, 176, 227, 172, 263],
    [85, 146, 17, 0, 169, 234, 185, 265],
    [224, 170, 176, 169, 0, 72, 331, 95],
    [162, 108, 227, 234, 72, 0, 262, 104],
    [118, 169, 172, 185, 331, 262, 0, 295],
    [202, 140, 263, 265, 95, 104, 295, 0],
];

/// Zones per region: each of the eight has three (Azure's list of regions with availability zones).
pub const ZONES: usize = 3;
/// Azure's stated bound on a round trip between zones of one region, nanoseconds.
pub const ZONE_BOUND_NS: u64 = 2_000_000;
/// Sensitivity only ([`Access::Modelled`]), not of record: a node's access height, nanoseconds,
/// modelled at 25 µs to 250 µs.
pub const HEIGHT_NS: (u64, u64) = (25_000, 250_000);
/// Sensitivity only ([`Access::Modelled`]), not of record: the most a probe's round trip exceeds the
/// true one, per mille, modelled.
pub const JITTER_PER_MILLE: u64 = 100;
/// The input-bound rule's margin (the lead, 2026-11-06): a bound is the largest value in the census
/// of inputs times this.
pub const INPUT_MARGIN: usize = 2;
/// The census's largest node model: `bench/sched/huawei-r5-day30.fleet.trace`'s 50 node rows (Hex's
/// fleet's hosts, five of each); the m5d.metal trace has 10.
pub const CENSUS_NODE_ROWS: usize = 50;
/// The most nodes a WAN holds, by the input-bound rule.
pub const MAX_NODES: usize = CENSUS_NODE_ROWS * INPUT_MARGIN;
/// The census's longest convergence: the most rounds any convergence of the record and its
/// sensitivity runs took before patience stopped it, 6,350 (`bench/sched/proximity.md`).
pub const CENSUS_ROUNDS: usize = 6_350;
/// The most rounds a convergence runs, by the input-bound rule; past it, a typed refusal.
pub const MAX_ROUNDS: usize = CENSUS_ROUNDS * INPUT_MARGIN;
const NS_PER_MS: u64 = 1_000_000;
const NS_PER_SEC: f64 = 1e9;

/// What lies between a node and its zone's network, and between zones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The record's: no heights, no jitter, every pair of zones at Azure's stated bound.
    Bound,
    /// A labelled sensitivity run's, never the record's: modelled heights, jitter and zone terms.
    Modelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WanConfig {
    pub nodes: usize,
    /// How many of [`REGIONS`] the nodes spread over, round-robin, from the first.
    pub regions: usize,
    pub access: Access,
    pub seed: u64,
}

/// Where a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Site {
    pub region: usize,
    pub zone: usize,
    pub height_ns: u64,
}

/// The modelled network: each node's site, and each region's zone-pair terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wan {
    sites: Vec<Site>,
    /// Per region, the term between each pair of its zones, nanoseconds; zero within one zone.
    zone_terms: Vec<[[u64; ZONES]; ZONES]>,
    jitter_per_mille: u64,
}

impl Wan {
    /// A WAN of `config.nodes` nodes, drawn from `rng`.
    pub fn generate(config: &WanConfig, rng: &mut Rng) -> Result<Self, SimError> {
        if config.nodes == 0 || config.nodes > MAX_NODES {
            return Err(SimError::Config("a WAN holds 1 to MAX_NODES nodes"));
        }
        if config.regions == 0 || config.regions > REGIONS.len() {
            return Err(SimError::Config("a WAN spans 1 to 8 regions"));
        }
        let modelled = config.access == Access::Modelled;
        let mut zone_terms = Vec::with_capacity(config.regions);
        for _ in 0..config.regions {
            let mut terms = [[0u64; ZONES]; ZONES];
            for a in 0..ZONES {
                for b in a.saturating_add(1)..ZONES {
                    let term = if modelled {
                        rng.range(1, ZONE_BOUND_NS)
                    } else {
                        ZONE_BOUND_NS
                    };
                    set(&mut terms, a, b, term)?;
                    set(&mut terms, b, a, term)?;
                }
            }
            zone_terms.push(terms);
        }
        let mut sites = Vec::with_capacity(config.nodes);
        for node in 0..config.nodes {
            let region = node
                .checked_rem(config.regions)
                .ok_or(SimError::Config("regions"))?;
            let zone = node
                .checked_div(config.regions)
                .and_then(|n| n.checked_rem(ZONES))
                .ok_or(SimError::Config("zones"))?;
            let height_ns = if modelled {
                rng.range(HEIGHT_NS.0, HEIGHT_NS.1)
            } else {
                0
            };
            sites.push(Site {
                region,
                zone,
                height_ns,
            });
        }
        Ok(Self {
            sites,
            zone_terms,
            jitter_per_mille: if modelled { JITTER_PER_MILLE } else { 0 },
        })
    }

    pub fn len(&self) -> usize {
        self.sites.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    pub fn site(&self, node: NodeId) -> Result<Site, SimError> {
        usize::try_from(node.0)
            .ok()
            .and_then(|i| self.sites.get(i))
            .copied()
            .ok_or(SimError::UnknownNode(node))
    }

    /// The true round trip between two nodes, nanoseconds: zero to itself.
    pub fn rtt_ns(&self, a: NodeId, b: NodeId) -> Result<u64, SimError> {
        if a == b {
            return Ok(0);
        }
        let (x, y) = (self.site(a)?, self.site(b)?);
        let path = if x.region == y.region {
            self.zone_terms
                .get(x.region)
                .and_then(|t| t.get(x.zone))
                .and_then(|row| row.get(y.zone))
                .copied()
                .ok_or(SimError::UnknownNode(b))?
        } else {
            INTER_MS
                .get(x.region)
                .and_then(|row| row.get(y.region))
                .and_then(|ms| ms.checked_mul(NS_PER_MS))
                .ok_or(SimError::UnknownNode(b))?
        };
        x.height_ns
            .checked_add(y.height_ns)
            .and_then(|h| h.checked_add(path))
            .ok_or(SimError::TimeOverflow)
    }

    /// One probe's round trip: the true one stretched by a draw of up to the jitter.
    pub fn sample_ns(&self, a: NodeId, b: NodeId, rng: &mut Rng) -> Result<u64, SimError> {
        let rtt = self.rtt_ns(a, b)?;
        let stretch = rtt
            .checked_mul(rng.range(0, self.jitter_per_mille))
            .and_then(|s| s.checked_div(1_000))
            .ok_or(SimError::TimeOverflow)?;
        rtt.checked_add(stretch).ok_or(SimError::TimeOverflow)
    }
}

fn set(terms: &mut [[u64; ZONES]; ZONES], a: usize, b: usize, term: u64) -> Result<(), SimError> {
    let cell = terms
        .get_mut(a)
        .and_then(|row| row.get_mut(b))
        .ok_or(SimError::Config("zones"))?;
    *cell = term;
    Ok(())
}

/// Every node's coordinate engine after a convergence.
pub struct Coordinates {
    engines: Vec<CoordinateEngine>,
    /// Samples taken (the engine's `update` said so), and offered.
    pub taken: u64,
    pub offered: u64,
    /// The median relative error over every pair after each round run, the first entry before any.
    pub history: Vec<f64>,
}

impl Coordinates {
    /// Each node at the origin, its engine's stream seeded by the run's seed and its id.
    fn fresh(n: usize, seed: u64) -> Result<Self, SimError> {
        let mut engines = Vec::with_capacity(n);
        for node in 0..n {
            let id = u64::try_from(node).map_err(|_| SimError::Config("node ids"))?;
            engines.push(CoordinateEngine::new(seed ^ id.rotate_left(32)));
        }
        Ok(Self {
            engines,
            taken: 0,
            offered: 0,
            history: Vec::new(),
        })
    }

    /// Runs hyper-swim's schedule from the origin, round after round, until as many rounds have
    /// passed without a new lowest median error as it took to reach the lowest; refused past
    /// [`MAX_ROUNDS`]. The coordinates are the last round's, as a running cluster's are.
    pub fn converge(wan: &Wan, seed: u64, rng: &mut Rng) -> Result<Self, SimError> {
        let n = wan.len();
        let mut coordinates = Self::fresh(n, seed)?;
        let Some(origin) = median_error(wan, &coordinates)? else {
            // No pair is apart: nothing to learn.
            return Ok(coordinates);
        };
        coordinates.history.push(origin);
        // The lowest median error so far, and the round it came at.
        let (mut best_round, mut best) = (0usize, origin);
        let probes = n.saturating_sub(1);
        let mut order: Vec<NodeId> = Vec::with_capacity(probes);
        for round in 1..=MAX_ROUNDS {
            for node in 0..n {
                let me = node_id(node)?;
                order.clear();
                for peer in (0..n).filter(|&p| p != node) {
                    order.push(node_id(peer)?);
                }
                shuffle(&mut order, rng)?;
                for &peer in &order {
                    let rtt = Duration::from_nanos(wan.sample_ns(me, peer, rng)?);
                    let theirs = *coordinates.coordinate(peer)?;
                    let engine = coordinates
                        .engines
                        .get_mut(node)
                        .ok_or(SimError::UnknownNode(me))?;
                    let took = engine.update(&theirs, rtt, probes);
                    coordinates.offered = coordinates.offered.saturating_add(1);
                    if took {
                        coordinates.taken = coordinates.taken.saturating_add(1);
                    }
                }
            }
            let now = median_error(wan, &coordinates)?.ok_or(SimError::Config("pairs"))?;
            coordinates.history.push(now);
            if now < best {
                (best_round, best) = (round, now);
            }
            let since = round
                .checked_sub(best_round)
                .ok_or(SimError::Config("rounds"))?;
            if since >= best_round {
                return Ok(coordinates);
            }
        }
        Err(SimError::StepBudget(
            u64::try_from(MAX_ROUNDS).unwrap_or(u64::MAX),
        ))
    }

    /// Rounds run.
    pub fn rounds(&self) -> usize {
        self.history.len().saturating_sub(1)
    }

    pub fn coordinate(&self, node: NodeId) -> Result<&NetworkCoordinate, SimError> {
        usize::try_from(node.0)
            .ok()
            .and_then(|i| self.engines.get(i))
            .map(CoordinateEngine::coordinate)
            .ok_or(SimError::UnknownNode(node))
    }

    /// The predicted round trip between two nodes, seconds.
    pub fn predict(&self, a: NodeId, b: NodeId) -> Result<f64, SimError> {
        Ok(CoordinateEngine::estimate_rtt(
            self.coordinate(a)?,
            self.coordinate(b)?,
        ))
    }

    /// A hash of every coordinate's bits, in node order.
    pub fn hash(&self) -> u64 {
        let mut h = TraceHash::default();
        for engine in &self.engines {
            let c = engine.coordinate();
            for x in c.position {
                h.word(x.to_bits());
            }
            h.word(c.height.to_bits());
            h.word(c.error.to_bits());
        }
        h.finish()
    }
}

/// A Fisher–Yates pass, as hyper-swim's `RandomizedOrder` shuffles its probe order.
fn shuffle(items: &mut [NodeId], rng: &mut Rng) -> Result<(), SimError> {
    for i in (1..items.len()).rev() {
        let j =
            usize::try_from(rng.range(0, u64::try_from(i).map_err(|_| SimError::Config("ids"))?))
                .map_err(|_| SimError::Config("ids"))?;
        if j < items.len() && i < items.len() {
            items.swap(i, j);
        }
    }
    Ok(())
}

/// Every pair of distinct nodes apart (a true round trip above zero): `(a, b, truth seconds)`.
fn pairs_apart(wan: &Wan) -> Result<Vec<(NodeId, NodeId, f64)>, SimError> {
    let n = wan.len();
    let mut out = Vec::with_capacity(
        n.saturating_mul(n.saturating_sub(1))
            .checked_div(2)
            .unwrap_or(0),
    );
    for a in 0..n {
        for b in a.saturating_add(1)..n {
            let (x, y) = (node_id(a)?, node_id(b)?);
            let truth = wan.rtt_ns(x, y)?;
            if truth > 0 {
                out.push((x, y, seconds(truth)));
            }
        }
    }
    Ok(out)
}

/// The median of `|predicted − true| / true` over every pair apart, or none if no pair is.
fn median_error(wan: &Wan, coordinates: &Coordinates) -> Result<Option<f64>, SimError> {
    let mut errors = Vec::new();
    for (a, b, truth) in pairs_apart(wan)? {
        errors.push((coordinates.predict(a, b)? - truth).abs() / truth);
    }
    errors.sort_by(f64::total_cmp);
    Ok(errors
        .get(errors.len().checked_div(2).unwrap_or(0))
        .copied())
}

/// How well the coordinates predict the WAN, over every pair.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accuracy {
    /// The median of `|predicted − true| / true` over every pair apart.
    pub median_relative_error: f64,
    /// Of every trial of a node, a peer apart from it in its region and a peer outside it, how many
    /// predicted the one in its region nearer; of how many.
    pub region_ranked_first: usize,
    pub region_trials: usize,
}

impl Accuracy {
    pub fn measure(wan: &Wan, coordinates: &Coordinates) -> Result<Self, SimError> {
        let median_relative_error =
            median_error(wan, coordinates)?.ok_or(SimError::Config("no pair of nodes is apart"))?;
        let n = wan.len();
        let mut region_ranked_first = 0usize;
        let mut region_trials = 0usize;
        for a in 0..n {
            let me = node_id(a)?;
            let home = wan.site(me)?.region;
            for near in 0..n {
                let near = node_id(near)?;
                if near == me || wan.site(near)?.region != home || wan.rtt_ns(me, near)? == 0 {
                    continue;
                }
                for far in 0..n {
                    let far = node_id(far)?;
                    if wan.site(far)?.region == home {
                        continue;
                    }
                    region_trials = region_trials.saturating_add(1);
                    if coordinates.predict(me, near)? < coordinates.predict(me, far)? {
                        region_ranked_first = region_ranked_first.saturating_add(1);
                    }
                }
            }
        }
        Ok(Self {
            median_relative_error,
            region_ranked_first,
            region_trials,
        })
    }
}

fn seconds(ns: u64) -> f64 {
    ns as f64 / NS_PER_SEC
}

pub(crate) fn node_id(node: usize) -> Result<NodeId, SimError> {
    u32::try_from(node)
        .map(NodeId)
        .map_err(|_| SimError::Config("node ids fit u32"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vivaldi's median relative error over 1,740 Internet hosts (PF[1]).
    const VIVALDI_MEDIAN_ERROR: f64 = 0.11;

    /// The record's WANs: the two node models' sizes (10 m5d.metal hosts, 50 fleet hosts).
    fn record(nodes: usize, seed: u64) -> WanConfig {
        WanConfig {
            nodes,
            regions: REGIONS.len(),
            access: Access::Bound,
            seed,
        }
    }

    fn converged(config: &WanConfig) -> (Wan, Coordinates) {
        let mut rng = Rng::new(config.seed);
        let wan = Wan::generate(config, &mut rng).unwrap();
        let coordinates = Coordinates::converge(&wan, config.seed, &mut rng).unwrap();
        (wan, coordinates)
    }

    /// The census constants are the traces' own (a doc-truth test): the fleet trace's node rows.
    #[test]
    fn the_census_is_the_traces() {
        let rows = |text: &str| text.lines().filter(|l| l.starts_with("node,")).count();
        let fleet = rows(include_str!(
            "../../../bench/sched/huawei-r5-day30.fleet.trace"
        ));
        let m5d = rows(include_str!("../../../bench/sched/huawei-r5-day30.trace"));
        assert_eq!(CENSUS_NODE_ROWS, fleet.max(m5d));
    }

    /// The matrix is Azure's table as the module says: symmetric, zero on the diagonal, and every
    /// cross-region round trip far above the zones' bound.
    #[test]
    fn the_region_matrix_is_symmetric_and_wide_area() {
        for (i, row) in INTER_MS.iter().enumerate() {
            for (j, &ms) in row.iter().enumerate() {
                assert_eq!(ms, INTER_MS[j][i], "{} / {}", REGIONS[i], REGIONS[j]);
                if i == j {
                    assert_eq!(ms, 0);
                } else {
                    assert!(ms * NS_PER_MS > 8 * ZONE_BOUND_NS, "{i} {j}");
                }
            }
        }
    }

    /// Do: build the record's WAN at the fleet's size. Expect every true round trip symmetric, 0 within
    /// a zone, Azure's zone bound between zones of a region, the table's between regions, and every
    /// probe the truth. The sensitivity WAN keeps zone terms below the bound and probes within the
    /// jitter of the truth.
    #[test]
    fn true_round_trips_follow_the_model() {
        let mut rng = Rng::new(3);
        let wan = Wan::generate(&record(CENSUS_NODE_ROWS, 3), &mut rng).unwrap();
        let modelled = Wan::generate(
            &WanConfig {
                access: Access::Modelled,
                ..record(CENSUS_NODE_ROWS, 3)
            },
            &mut rng,
        )
        .unwrap();
        let n = u32::try_from(CENSUS_NODE_ROWS).unwrap();
        for a in 0..n {
            for b in 0..n {
                let (x, y) = (NodeId(a), NodeId(b));
                let rtt = wan.rtt_ns(x, y).unwrap();
                assert_eq!(rtt, wan.rtt_ns(y, x).unwrap());
                let (sx, sy) = (wan.site(x).unwrap(), wan.site(y).unwrap());
                let want = if a == b || (sx.region == sy.region && sx.zone == sy.zone) {
                    0
                } else if sx.region == sy.region {
                    ZONE_BOUND_NS
                } else {
                    INTER_MS[sx.region][sy.region] * NS_PER_MS
                };
                assert_eq!(rtt, want, "{a} {b}");
                assert_eq!(wan.sample_ns(x, y, &mut rng).unwrap(), rtt);
                let truth = modelled.rtt_ns(x, y).unwrap();
                let sample = modelled.sample_ns(x, y, &mut rng).unwrap();
                assert!(sample >= truth && sample <= truth + truth * JITTER_PER_MILLE / 1_000);
                if a != b && sx.region == sy.region {
                    assert!(truth < ZONE_BOUND_NS + 2 * HEIGHT_NS.1);
                }
            }
        }
        assert_eq!(
            wan.rtt_ns(NodeId(0), NodeId(n)),
            Err(SimError::UnknownNode(NodeId(n)))
        );
    }

    /// S4, A2 (G3 step 1): one seed names one set of coordinates, bit for bit; another seed another.
    /// Each round every node probes every other once: offered = rounds × n × (n − 1), and every probe
    /// of a peer apart is a sample.
    #[test]
    fn a_seed_names_one_set_of_coordinates() {
        let config = record(10, 1);
        let (wan, a) = converged(&config);
        let (_, b) = converged(&config);
        let (_, c) = converged(&WanConfig { seed: 2, ..config });
        assert_eq!(a.hash(), b.hash());
        assert_ne!(a.hash(), c.hash());
        let n = u64::try_from(wan.len()).unwrap();
        assert_eq!(a.offered, u64::try_from(a.rounds()).unwrap() * n * (n - 1));
        let apart = u64::try_from(pairs_apart(&wan).unwrap().len()).unwrap();
        assert_eq!(a.taken, u64::try_from(a.rounds()).unwrap() * apart * 2);
    }

    /// S4, A2 (G3 step 1): do converge on each node model's WAN; expect the patience rule to have
    /// stopped it at twice its best round with nothing lower after the best, the error to end no worse than
    /// Vivaldi's own evaluation over 1,740 Internet hosts, 11% (Dabek et al., SIGCOMM 2004;
    /// `proximity-and-failure-containment.md` §1, PF[1]), and a node's own region predicted nearer
    /// than another region's in more of the trials than chance, which is half.
    #[test]
    fn coordinates_learn_the_wan() {
        for nodes in [10, CENSUS_NODE_ROWS] {
            let (wan, coordinates) = converged(&record(nodes, 1));
            let h = &coordinates.history;
            let (best_round, best) = h
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(b.1))
                .unwrap();
            assert!(best_round > 0, "{nodes}: {h:?}");
            assert_eq!(coordinates.rounds(), 2 * best_round, "{nodes}: {h:?}");
            assert!(
                h[best_round + 1..].iter().all(|e| e >= best),
                "{nodes}: {h:?}"
            );
            let accuracy = Accuracy::measure(&wan, &coordinates).unwrap();
            eprintln!(
                "G3 coordinates {nodes} nodes, {} rounds: {accuracy:?}",
                coordinates.rounds()
            );
            assert!(
                accuracy.median_relative_error <= VIVALDI_MEDIAN_ERROR,
                "{nodes}: median relative error {:.3}",
                accuracy.median_relative_error
            );
            assert!(accuracy.region_trials > 0);
            assert!(
                accuracy.region_ranked_first * 2 > accuracy.region_trials,
                "{nodes}: {} of {}",
                accuracy.region_ranked_first,
                accuracy.region_trials
            );
        }
    }

    /// S4, A2 (G3 step 1), the stopping rule: do converge the record's AllKinds WAN of seed 15, whose
    /// median error rises once by noise (25.4% at round 3, 26.4% at round 4); expect the convergence to
    /// run on past that uptick, by patience: it stops once as many rounds have passed without a new
    /// low as it took to reach the low, so it ends at twice the best round, with nothing after the
    /// best lower than it.
    #[test]
    fn patience_runs_past_a_noise_uptick() {
        let (_, coordinates) = converged(&record(10, 15));
        let h = &coordinates.history;
        eprintln!("G3 seed 15: {h:?}");
        assert!(h.len() > 5 && h[4] > h[3], "the uptick: {h:?}");
        let (best_round, best) = h
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        assert_eq!(coordinates.rounds(), 2 * best_round, "{h:?}");
        assert!(h[best_round + 1..].iter().all(|e| e >= best), "{h:?}");
    }

    /// S4, A2 (G3 step 1): do converge seed 15's WAN, whose last round is not its best; expect the
    /// coordinates held to be the last round's, as a running cluster's are: choosing the best round
    /// reads the true RTTs, which no node knows, and would make proximity look better than
    /// production can be (the lead, 2026-11-06).
    #[test]
    fn the_coordinates_held_are_the_last_rounds() {
        let config = record(10, 15);
        let (wan, coordinates) = converged(&config);
        let h = &coordinates.history;
        let last = h[h.len() - 1];
        let best = h.iter().copied().fold(f64::INFINITY, f64::min);
        assert!(last > best, "seed 15's last round is not its best: {h:?}");
        let held = Accuracy::measure(&wan, &coordinates).unwrap();
        assert_eq!(held.median_relative_error, last, "{h:?}");
    }

    /// Non-vacuity for the test above: coordinates still at the origin fail both of its bounds.
    #[test]
    fn unconverged_coordinates_fail_the_same_measure() {
        let config = record(CENSUS_NODE_ROWS, 1);
        let wan = Wan::generate(&config, &mut Rng::new(1)).unwrap();
        let origin = Coordinates::fresh(wan.len(), 1).unwrap();
        let accuracy = Accuracy::measure(&wan, &origin).unwrap();
        assert!(accuracy.median_relative_error > VIVALDI_MEDIAN_ERROR);
        assert!(accuracy.region_ranked_first * 2 <= accuracy.region_trials);
    }

    /// Hostile configurations are refused, typed.
    #[test]
    fn out_of_bounds_configurations_are_refused() {
        let mut rng = Rng::new(1);
        for bad in [
            record(0, 1),
            record(MAX_NODES + 1, 1),
            WanConfig {
                regions: 0,
                ..record(10, 1)
            },
            WanConfig {
                regions: REGIONS.len() + 1,
                ..record(10, 1)
            },
        ] {
            assert!(matches!(
                Wan::generate(&bad, &mut rng),
                Err(SimError::Config(_))
            ));
        }
    }
}
