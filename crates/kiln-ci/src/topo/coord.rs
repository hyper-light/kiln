//! Network coordinates: each node places itself in a small space whose distances predict round
//! trips, from round trips it measures on traffic that already flows (LB08). Vivaldi (Dabek, Cox,
//! Kaashoek and Morris, "Vivaldi: A Decentralized Network Coordinate System", SIGCOMM 2004), with
//! two refinements of Ledlie, Gardner and Seltzer ("Network Coordinates in the Wild", NSDI 2007):
//! a latency filter and neighbor decay. HashiCorp serf's `coordinate` package is a reference
//! implementation of the same (github.com/hashicorp/serf, `coordinate/`, at
//! de0966873b49fb021ba110ad0d840e7cac6d51d6); where this differs from it, it says so.
//!
//! **The space** is Dabek's height vectors (§5.4): a point in the plane and a height, "the time it
//! takes packets to travel the access link from the node to the core". `[x, xh] − [y, yh] = [x −
//! y, xh + yh]`, `‖[x, xh]‖ = ‖x‖ + xh`: the predicted round trip between two nodes is the
//! distance between their points plus both heights. Two dimensions, which with a height predict
//! better than two or three without (Fig. 15). Serf uses eight, citing nothing for it.
//!
//! **The update** is Dabek's Fig. 3, a sample from node `j` of round trip `rtt`:
//! `w = e_i/(e_i + e_j)`; `e_s = |‖x_i − x_j‖ − rtt|/rtt`; `e_i = e_s·c_e·w + e_i·(1 − c_e·w)`;
//! `δ = c_c·w`; `x_i = x_i + δ·(rtt − ‖x_i − x_j‖)·u(x_i − x_j)`, with
//! - `c_c` = 0.25: "Empirically, a cc value of 0.25 yields both quick error reduction and low
//!   oscillation" (§4.1, Fig. 5(b));
//! - `c_e`, which neither paper gives a value for (serf's 0.25 cites none), adaptive: the error
//!   estimate is the node's mean relative error over its links (§2.6), and it hears its `m` live
//!   neighbors in turn, so its moving average remembers `m` samples: `c_e = 2/(m + 1)`, the
//!   exponentially weighted average whose variance is an `m`-sample mean's (NIST/SEMATECH
//!   e-Handbook of Statistical Methods §6.3.2.4: `σ²·λ/(2 − λ) = σ²/m`);
//! - a new node's error 1 (100 %): it knows nothing, and predicting nothing is wrong by all of the
//!   round trip (Dabek's "if its error is large (say, ±100%), then it will eagerly move all the way").
//!
//! **The latency filter** (Ledlie, Pietzuch and Seltzer, "Stable and Accurate Network
//! Coordinates", ICDCS 2006 §4, which the NSDI paper's §4.1 builds on): a link's samples become its
//! expected round trip as the 25th percentile of its last four ("short histories, e.g., only four
//! observations, achieve the best performance"; "p = 25 resulted in slightly lower error than p =
//! 50"), the percentile interpolated linearly between order statistics (Hyndman and Fan,
//! "Sample Quantiles in Statistical Packages", 1996, definition 7). Serf keeps a median of three,
//! citing the NSDI paper.
//!
//! **Neighbor decay** (NSDI 2007 §4.2): a sample moves the node by the forces of all its recent
//! neighbors, each weighted by how recently it was heard, `(a_max − a_j)/Σ(a_max − a)`, `a_j` the
//! age of what it knows of `j` and `a_max` the oldest's, neighbors equally recent weighing alike;
//! "nodes expired from the recent neighbor set after 30 minutes". Serf has none.
//!
//! **Ageing.** A coordinate not refreshed grows less sure of itself as the network under it
//! changes. Each node measures that change on its own links: the relative change of a link's
//! filtered round trip between two times, squared, against the time between them, fitted by least
//! squares as `noise + slope·gap` (a linear variogram: noise is its nugget, change its slope;
//! Cressie, "Statistics for Spatial Data", 1993, §2.3). A coordinate `a` old is then taken as wrong
//! by `√(e² + slope·a)`: what it was wrong by and what the network changed by since, independent.
//! No growth is picked.
//!
//! **Fixed point**: coordinates in nanoseconds (`i64`), errors and weights Q32 ([`super::fixed`]),
//! so a sample's update is the same bit for bit everywhere. Two nodes at one point with no height
//! meet Dabek's `u(0)`, "a unit-length vector in a randomly chosen direction" (§2.3): drawn from the
//! node's own stream (SplitMix64, seeded by its id), so a replay draws the same.

use serde::{Deserialize, Serialize};

use super::fixed::{self, ONE};

/// The plane's dimensions besides the height (Dabek 2004 §5.4, Fig. 15).
pub const DIMS: usize = 2;
/// `c_c`, Q32 (Dabek 2004 §4.1, Fig. 5(b)).
pub const CC: u64 = ONE / 4;
/// The latency filter's history (Ledlie, Pietzuch and Seltzer 2006 §4).
pub const FILTER_SAMPLES: usize = 4;
/// The latency filter's percentile, Q32 (ibid.).
pub const FILTER_PERCENTILE: u64 = ONE / 4;
/// How long a neighbor stays in the recent set, ms (Ledlie, Gardner and Seltzer 2007 §4.2).
pub const NEIGHBOR_EXPIRY_MS: i64 = 30 * 60 * 1000;
/// A new node's relative error, Q32: all of the round trip.
pub const FRESH_ERROR: u64 = ONE;
/// An hour in ms: drift is said per hour.
const HOUR_MS: u64 = 3_600_000;

/// A node's coordinate: its point, its height and how wrong it takes itself to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coord {
    /// The point in the plane, ns.
    pub x: [i64; DIMS],
    /// The height, ns, never negative.
    pub h: i64,
    /// Its relative error, Q32 (`ONE` = 100 %).
    pub e: u64,
}

impl Default for Coord {
    fn default() -> Self {
        Coord { x: [0; DIMS], h: 0, e: FRESH_ERROR }
    }
}

impl Coord {
    /// The distance between the two points in the plane, ns.
    pub fn plane(&self, o: &Coord) -> u64 {
        let mut s: u128 = 0;
        for (a, b) in self.x.iter().zip(o.x.iter()) {
            let d = (i128::from(*a) - i128::from(*b)).unsigned_abs();
            s = s.saturating_add(d.saturating_mul(d));
        }
        fixed::sat(s.isqrt())
    }

    /// The round trip the two coordinates predict, ns: the plane's distance and both heights.
    pub fn rtt(&self, o: &Coord) -> u64 {
        self.plane(o).saturating_add(self.h.max(0).unsigned_abs()).saturating_add(o.h.max(0).unsigned_abs())
    }

    /// Its error once `age_ms` old, with the network drifting by `drift` (Q32 of squared relative
    /// error an hour): `√(e² + drift·age)`.
    pub fn aged(&self, drift: u64, age_ms: u64) -> u64 {
        let e2 = fixed::mul(self.e, self.e);
        fixed::sqrt(e2.saturating_add(variance(drift, age_ms)))
    }
}

/// The squared relative change links drifting by `drift` (Q32 an hour) are expected to show over
/// `age_ms`, Q32.
fn variance(drift: u64, age_ms: u64) -> u64 {
    fixed::sat(u128::from(drift) * u128::from(age_ms) / u128::from(HOUR_MS))
}

/// The relative change links drifting by `drift` are expected to show over `age_ms`, Q32: the
/// square root of [`variance`].
pub fn grown(drift: u64, age_ms: u64) -> u64 {
    fixed::sqrt(variance(drift, age_ms))
}

/// What a node says of its coordinate (in its word and the directory): the coordinate and how
/// fast its links drift, so whoever holds it can age it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announce {
    pub c: Coord,
    /// Squared relative change an hour, Q32 ([`Drift::per_hour`]).
    #[serde(default)]
    pub d: u64,
    /// What it vouches for of links it never measured, Q32: its error, once it has more live
    /// neighbors than its coordinate has unknowns ([`DIMS`] and a height), so that its links
    /// over-determine where it is and its error over them speaks for the space around it. None
    /// before: a node placed by fewer links (today's runners, by their one link to the server)
    /// fits them whatever it predicts of the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v: Option<u64>,
}

/// The latency filter: a link's last [`FILTER_SAMPLES`] round trips and their 25th percentile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filter {
    samples: [u64; FILTER_SAMPLES],
    len: usize,
    next: usize,
}

impl Filter {
    pub fn push(&mut self, rtt: u64) {
        if let Some(s) = self.samples.get_mut(self.next) {
            *s = rtt;
        }
        self.next = (self.next + 1) % FILTER_SAMPLES;
        self.len = (self.len + 1).min(FILTER_SAMPLES);
    }

    fn sorted(&self) -> ([u64; FILTER_SAMPLES], usize) {
        let mut s = self.samples;
        let n = self.len;
        if let Some(head) = s.get_mut(..n) {
            head.sort_unstable();
        }
        (s, n)
    }

    /// The expected round trip: the samples' 25th percentile (Hyndman and Fan's definition 7).
    pub fn value(&self) -> Option<u64> {
        let (s, n) = self.sorted();
        let pos = fixed::mul(u64::try_from(n.checked_sub(1)?).unwrap_or(0) << 32, FILTER_PERCENTILE);
        let lo = usize::try_from(pos >> 32).unwrap_or(0);
        let frac = pos & (ONE - 1);
        let a = *s.get(lo)?;
        let b = s.get(lo + 1).filter(|_| lo + 1 < n).copied().unwrap_or(a);
        Some(a.saturating_add(fixed::mul(b.saturating_sub(a), frac)))
    }

    /// How far its samples spread, ns: the largest less the smallest.
    pub fn spread(&self) -> u64 {
        let (s, n) = self.sorted();
        let head = s.get(..n).unwrap_or(&[]);
        head.last().copied().unwrap_or(0).saturating_sub(head.first().copied().unwrap_or(0))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The drift of a node's links ([`Coord::aged`]): `(gap, r²)` pairs, `r` a link's relative change
/// over `gap`, fitted as `noise + slope·gap` by least squares from their sums.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drift {
    pub n: u64,
    pub sx: u128,
    pub sy: u128,
    pub sxx: u128,
    pub sxy: u128,
}

impl Drift {
    /// A link's expected round trip went from `from` to `to` over `gap_ms`.
    pub fn add(&mut self, gap_ms: u64, from: u64, to: u64) {
        if gap_ms == 0 || from == 0 {
            return;
        }
        let r = fixed::ratio(from.abs_diff(to), from);
        let y = u128::from(fixed::mul(r, r));
        let x = u128::from(gap_ms);
        self.n = self.n.saturating_add(1);
        self.sx = self.sx.saturating_add(x);
        self.sy = self.sy.saturating_add(y);
        self.sxx = self.sxx.saturating_add(x.saturating_mul(x));
        self.sxy = self.sxy.saturating_add(x.saturating_mul(y));
    }

    /// The fitted slope: squared relative change an hour, Q32. With gaps all alike the slope
    /// can't be told from the noise, and all the change is taken as drift (an upper bound: a
    /// coordinate is never taken as surer than its links allow); a fit that falls is no drift.
    pub fn per_hour(&self) -> u64 {
        if self.n == 0 || self.sx == 0 {
            return 0;
        }
        let n = i128::from(self.n);
        let (sx, sy) = (i(self.sx), i(self.sy));
        let den = n.saturating_mul(i(self.sxx)).saturating_sub(sx.saturating_mul(sx));
        let num = n.saturating_mul(i(self.sxy)).saturating_sub(sx.saturating_mul(sy));
        let hour = i128::from(HOUR_MS);
        let slope = if den > 0 { num.saturating_mul(hour) / den } else { sy.saturating_mul(hour) / sx.max(1) };
        u64::try_from(slope.max(0)).unwrap_or(u64::MAX)
    }

    /// Both together: one node's drift pooled with another's.
    pub fn merge(&mut self, o: &Drift) {
        self.n = self.n.saturating_add(o.n);
        self.sx = self.sx.saturating_add(o.sx);
        self.sy = self.sy.saturating_add(o.sy);
        self.sxx = self.sxx.saturating_add(o.sxx);
        self.sxy = self.sxy.saturating_add(o.sxy);
    }
}

fn i(v: u128) -> i128 {
    i128::try_from(v).unwrap_or(i128::MAX)
}

/// What a node knows of one neighbor: its coordinate, its link's filter, when it was heard, and the
/// filtered round trip it measures drift from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Neighbor {
    pub id: u64,
    pub coord: Announce,
    pub heard: i64,
    pub filter: Filter,
    anchor: Option<(i64, u64)>,
}

/// A node's own coordinate and what it learns it from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: u64,
    pub coord: Coord,
    pub drift: Drift,
    stream: u64,
    neighbors: Vec<Neighbor>,
}

impl Node {
    /// A new node, at the origin with no height, wrong by all of every round trip.
    pub fn new(id: u64) -> Node {
        Node { id, coord: Coord::default(), drift: Drift::default(), stream: id, neighbors: Vec::new() }
    }

    /// What it says of itself.
    pub fn announce(&self) -> Announce {
        let determined = self.neighbors.len() > DIMS + 1;
        Announce { c: self.coord, d: self.drift.per_hour(), v: determined.then_some(self.coord.e) }
    }

    /// Its neighbors, by id.
    pub fn neighbors(&self) -> &[Neighbor] {
        &self.neighbors
    }

    /// What it measured of `peer`'s link: its filtered round trip and its spread, ns.
    pub fn link(&self, peer: u64) -> Option<(u64, u64)> {
        let n = self.neighbors.binary_search_by_key(&peer, |n| n.id).ok().and_then(|i| self.neighbors.get(i))?;
        Some((n.filter.value()?, n.filter.spread()))
    }

    /// Forgets a neighbor (gone from the fleet).
    pub fn forget(&mut self, peer: u64) {
        self.neighbors.retain(|n| n.id != peer);
    }

    /// A round trip of `rtt_ns` measured to `peer`, which says `theirs`, at `now` (ms, this node's
    /// clock): the latency filter, then Dabek's update with every recent neighbor's force.
    /// Returns whether the coordinate moved.
    pub fn sample(&mut self, peer: u64, theirs: Announce, rtt_ns: u64, now: i64) -> bool {
        if rtt_ns == 0 || peer == self.id {
            return false;
        }
        let at = match self.neighbors.binary_search_by_key(&peer, |n| n.id) {
            Ok(i) => i,
            Err(i) => {
                self.neighbors.insert(i, Neighbor { id: peer, coord: theirs, heard: now, filter: Filter::default(), anchor: None });
                i
            }
        };
        let Some(n) = self.neighbors.get_mut(at) else { return false };
        n.coord = theirs;
        n.heard = now;
        n.filter.push(rtt_ns);
        let Some(rtt) = n.filter.value() else { return false };
        // Drift, from the filtered round trip against the oldest still recent.
        match n.anchor {
            Some((t, from)) => {
                let gap = u64::try_from(now.saturating_sub(t)).unwrap_or(0);
                #[cfg(test)]
                let gap = if super::mutant() == Some(super::Mutant::DriftIgnored) { 0 } else { gap };
                self.drift.add(gap, from, rtt);
                if now.saturating_sub(t) >= NEIGHBOR_EXPIRY_MS {
                    n.anchor = Some((now, rtt));
                }
            }
            None => n.anchor = Some((now, rtt)),
        }
        self.neighbors.retain(|n| now.saturating_sub(n.heard) <= NEIGHBOR_EXPIRY_MS);
        let drift = self.drift.per_hour();
        let m = u64::try_from(self.neighbors.len()).unwrap_or(u64::MAX).max(1);
        // Dabek Fig. 3 for the sampled link: the weight, the sample's error, the node's error.
        let e_i = self.coord.e.max(1);
        let e_j = theirs.c.e.max(1);
        let w = fixed::ratio(e_i, e_i.saturating_add(e_j));
        let predicted = self.coord.rtt(&theirs.c);
        // An error below a nanosecond can't be measured: at least 1 ns over the round trip.
        let e_s = fixed::ratio(predicted.abs_diff(rtt).max(1), rtt);
        let c_e = fixed::ratio(2, m.saturating_add(1));
        #[cfg(test)]
        let c_e = if super::mutant() == Some(super::Mutant::ErrorFrozen) { 0 } else { c_e };
        let step = fixed::mul(c_e, w);
        self.coord.e = fixed::mul(e_s, step).saturating_add(fixed::mul(e_i, ONE.saturating_sub(step))).max(1);
        // Every recent neighbor's force, weighted by recency (neighbor decay).
        let oldest = self.neighbors.iter().map(|n| now.saturating_sub(n.heard)).max().unwrap_or(0);
        let total: u64 = self.neighbors.iter().map(|n| u64::try_from(oldest.saturating_sub(now.saturating_sub(n.heard))).unwrap_or(0)).sum();
        let mut force = [0i64; DIMS];
        let mut lift = 0i64;
        let me = self.coord;
        let mut draws = Vec::new();
        for n in &self.neighbors {
            let Some(rtt_k) = n.filter.value() else { continue };
            let age = now.saturating_sub(n.heard);
            let weight = if total == 0 {
                fixed::ratio(1, m)
            } else {
                fixed::ratio(u64::try_from(oldest.saturating_sub(age)).unwrap_or(0), total)
            };
            #[cfg(test)]
            let weight = if super::mutant() == Some(super::Mutant::OnlyLatest) { if n.id == peer { ONE } else { 0 } } else { weight };
            if weight == 0 {
                continue;
            }
            let e_k = n.coord.c.aged(n.coord.d.max(drift), u64::try_from(age).unwrap_or(0)).max(1);
            let delta = fixed::mul(CC, fixed::ratio(e_i, e_i.saturating_add(e_k)));
            let pred_k = me.rtt(&n.coord.c);
            let err = i64::try_from(rtt_k).unwrap_or(i64::MAX).saturating_sub(i64::try_from(pred_k).unwrap_or(i64::MAX));
            let push = fixed::mul_i(fixed::mul_i(err, delta), weight);
            if pred_k == 0 {
                draws.push(push);
                continue;
            }
            // u(x_i − x_k) in height vectors: [x_i − x_k, h_i + h_k] / ‖·‖.
            let len = i128::from(pred_k);
            for (f, (a, b)) in force.iter_mut().zip(me.x.iter().zip(n.coord.c.x.iter())) {
                let part = i128::from(push) * (i128::from(*a) - i128::from(*b)) / len;
                *f = f.saturating_add(i64::try_from(part).unwrap_or(0));
            }
            let hh = i128::from(me.h.max(0)) + i128::from(n.coord.c.h.max(0));
            lift = lift.saturating_add(i64::try_from(i128::from(push) * hh / len).unwrap_or(0));
        }
        for push in draws {
            let (dir, share) = self.direction();
            for (f, d) in force.iter_mut().zip(dir.iter()) {
                *f = f.saturating_add(fixed::mul_s(push, *d));
            }
            lift = lift.saturating_add(fixed::mul_i(push, share));
        }
        let before = self.coord;
        for (x, f) in self.coord.x.iter_mut().zip(force.iter()) {
            *x = x.saturating_add(*f);
        }
        // A height stays positive (§5.4), at least the 1 ns a coordinate resolves.
        self.coord.h = self.coord.h.saturating_add(lift).max(1);
        self.coord.x != before.x || self.coord.h != before.h
    }

    /// Dabek's `u(0)`: a direction uniform on the height vectors' unit sphere `‖v‖ + h = 1`, as
    /// its plane part (Q32, signed) and its height part (Q32): the angle uniform (a point drawn
    /// uniformly in the disk, by rejection), the height's share of density `2(1 − h)`.
    fn direction(&mut self) -> ([i64; DIMS], u64) {
        for _ in 0..64 {
            let a = i64::try_from(fixed::splitmix(&mut self.stream) >> 32).unwrap_or(0) - (1 << 31);
            let b = i64::try_from(fixed::splitmix(&mut self.stream) >> 32).unwrap_or(0) - (1 << 31);
            let r2 = (i128::from(a) * i128::from(a) + i128::from(b) * i128::from(b)).unsigned_abs();
            if r2 == 0 || r2 > (1u128 << 62) {
                continue;
            }
            let r = i128::try_from(r2.isqrt()).unwrap_or(1).max(1);
            let u = fixed::splitmix(&mut self.stream) >> 32;
            let share = ONE.saturating_sub(fixed::sqrt(ONE.saturating_sub(u)));
            let plane = i128::from(ONE.saturating_sub(share));
            let part = |c: i64| i64::try_from(i128::from(c) * plane / r).unwrap_or(0);
            return ([part(a), part(b)], share);
        }
        ([i64::try_from(ONE).unwrap_or(i64::MAX), 0], 0)
    }
}
