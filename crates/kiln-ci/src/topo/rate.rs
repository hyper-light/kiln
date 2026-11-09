//! How fast bytes move between two nodes (LB08), learned from kiln's own transfers (a sync's
//! pieces, a cache's or an artifact's object), never from a probe.
//!
//! **The model** is Hockney's latency and bandwidth (Hockney, "The communication challenge for
//! MPP: Intel Paragon and Meiko CS-2", Parallel Computing 20(3), 1994): moving `n` bytes takes `t =
//! a + n/r`, `a` what a transfer costs whatever its size (its round trips, its setup) and `r` the
//! rate. Fitted by least squares on the pair's recent transfers, so small transfers say what `a` is
//! and large ones what `r` is, with no size taken as "large enough". A pair seen at one size only
//! has its `a` taken as one round trip of the path (a transfer is at least a request and its
//! answer), and `r` from what's left.
//!
//! **What's recent** is He, Dovrolis and Ammar's history-based prediction ("On the
//! Predictability of Large Transfer TCP Throughput", SIGCOMM 2005; tech report GIT-CERCS-05-06,
//! §V and §VI.A): the last `n = 10` transfers, with their level-shift and outlier rules: a sample
//! more than `ψ = 0.4` off (relative) from what the rest predict is an outlier and left out; when
//! the latest samples all lie on one side of the earlier ones, their medians more than `χ = 0.3`
//! apart (relative) and at least two of them (`k + 2 ≤ n`), the path shifted, and only the samples
//! since the shift count. They apply the rules to the throughput of transfers alike in size;
//! kiln's vary, so here they apply to each transfer's time against what the fit of the others
//! predicts for its size.
//!
//! **A pair never seen** is bounded by its two ends ([`Access`]): bytes leave the sender no faster
//! than its uplink and reach the receiver no faster than its downlink, so the path's rate is at
//! most the smaller. That bound is often the rate: broadband hosts' bottleneck is taken to be "at
//! or very close to the last-hop" (Lakshminarayanan and Padmanabhan, "Some Findings on the Network
//! Performance of Broadband Hosts", IMC 2003, §4.2), but not always: "about 50% of the Internet
//! paths explored have a non-access bottleneck" (Akella, Seshan and Shaikh, "An Empirical
//! Evaluation of Wide-Area Internet Bottlenecks", IMC 2003), and "up to 40% of the bottleneck
//! locations are within an AS" (Hu et al., "Locating Internet Bottlenecks", SIGCOMM 2004, §1). So
//! the bound is an upper bound, and its estimate is said to be one ([`Source::Edges`]); a measured
//! pair between the same two sites bounds it further ([`Source::Site`]), since such pairs share
//! the path between the sites.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::fixed::{self, ONE};

/// The transfers a pair's estimate rests on (He et al. 2005, §VI.A's MA-LSO: `n = 10`).
pub const HISTORY: usize = 10;
/// The relative difference past which a sample is an outlier, Q32 (ibid. §V.B: `ψ = 0.4`).
pub const OUTLIER: u64 = ONE / 5 * 2;
/// The relative difference of medians past which the path shifted, Q32 (ibid.: `χ = 0.3`).
pub const SHIFT: u64 = ONE / 10 * 3;

/// One transfer: its bytes and how long it took, µs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    pub bytes: u64,
    pub us: u64,
}

/// What a pair's transfers say: the fixed cost (µs), the rate (bytes a second), and how far a
/// prediction from them may be off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fit {
    pub fixed_us: u64,
    pub rate_bps: u64,
    /// The residuals' standard deviation, µs (0 with fewer than three samples: unknown).
    pub sd_us: u64,
    pub samples: u32,
}

impl Fit {
    /// How long `bytes` take on this pair, µs.
    pub fn predict_us(&self, bytes: u64) -> u64 {
        self.fixed_us.saturating_add(time_us(bytes, self.rate_bps))
    }
}

/// `bytes` at `rate_bps`, µs; none at rate 0 is never.
pub fn time_us(bytes: u64, rate_bps: u64) -> u64 {
    if rate_bps == 0 {
        return u64::MAX;
    }
    fixed::sat(u128::from(bytes) * 1_000_000 / u128::from(rate_bps))
}

/// A pair's recent transfers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pair {
    pub recent: VecDeque<Transfer>,
}

impl Pair {
    /// A transfer seen, at most [`HISTORY`] kept; a level shift drops what came before it.
    pub fn add(&mut self, t: Transfer, rtt_us: u64) {
        if t.us == 0 || t.bytes == 0 {
            return;
        }
        self.recent.push_back(t);
        while self.recent.len() > HISTORY {
            self.recent.pop_front();
        }
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::NoLevelShift) {
            return;
        }
        if let Some(k) = shifted(&self.recent, rtt_us) {
            self.recent.drain(..k);
        }
    }

    /// The fit of what's kept, outliers left out; `rtt_us` the path's round trip, the fixed cost
    /// of a pair seen at one size only.
    pub fn fit(&self, rtt_us: u64) -> Option<Fit> {
        let all: Vec<Transfer> = self.recent.iter().copied().collect();
        let first = fit_of(&all, rtt_us)?;
        let kept: Vec<Transfer> = all.iter().copied().filter(|t| !off(t, &first, OUTLIER)).collect();
        if kept.len() == all.len() || kept.is_empty() {
            return Some(first);
        }
        fit_of(&kept, rtt_us).or(Some(first))
    }
}

/// Whether `t` is more than `by` (relative) from what `f` predicts for it.
fn off(t: &Transfer, f: &Fit, by: u64) -> bool {
    let want = f.predict_us(t.bytes).max(1);
    fixed::ratio(t.us.abs_diff(want), want) > by
}

/// Least squares `t = a + n/r` over `ts`, `a ≥ 0` and `r > 0` (the two-term non-negative fit:
/// a negative intercept refits through the origin; a slope at or below zero, a fit by sizes alike).
fn fit_of(ts: &[Transfer], rtt_us: u64) -> Option<Fit> {
    let k = u64::try_from(ts.len()).ok().filter(|&k| k > 0)?;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0u128, 0u128, 0u128, 0u128);
    for t in ts {
        let (x, y) = (u128::from(t.bytes), u128::from(t.us));
        sx += x;
        sy += y;
        sxx = sxx.saturating_add(x * x);
        sxy = sxy.saturating_add(x * y);
    }
    let kk = i128::from(k);
    let den = kk.saturating_mul(i(sxx)).saturating_sub(i(sx).saturating_mul(i(sx)));
    let num = kk.saturating_mul(i(sxy)).saturating_sub(i(sx).saturating_mul(i(sy)));
    // The slope in µs a byte, Q32; the intercept in µs.
    let (mut slope, mut a): (i128, i128) = if den > 0 && num > 0 {
        let s = (num << 32) / den;
        (s, (i(sy) - ((s * i(sx)) >> 32)) / kk)
    } else {
        // One size (or a fit that falls): the fixed cost is the path's round trip.
        let a = i128::from(rtt_us);
        let left = (i(sy) - a * kk).max(0);
        (if sx > 0 { (left << 32) / i(sx) } else { 0 }, a)
    };
    if a < 0 {
        a = 0;
        slope = if sxx > 0 { (i(sxy) << 32) / i(sxx) } else { 0 };
    }
    if slope <= 0 {
        return None;
    }
    let rate = fixed::sat((1_000_000u128 << 32) / u128::try_from(slope).unwrap_or(1).max(1));
    let fit = Fit { fixed_us: u64::try_from(a).unwrap_or(0), rate_bps: rate.max(1), sd_us: 0, samples: u32::try_from(k).unwrap_or(u32::MAX) };
    let sd = if k >= 3 {
        let ss: u128 = ts.iter().map(|t| u128::from(t.us.abs_diff(fit.predict_us(t.bytes)))).map(|d| d * d).sum();
        fixed::sat((ss / u128::from(k - 2)).isqrt())
    } else {
        0
    };
    Some(Fit { sd_us: sd, ..fit })
}

fn i(v: u128) -> i128 {
    i128::try_from(v).unwrap_or(i128::MAX)
}

/// The first of the latest samples that all lie on one side of the earlier ones, by more than
/// [`SHIFT`] between the two groups' medians, the earlier fitted alone and at least two after it
/// (He et al. 2005, §V.B).
fn shifted(recent: &VecDeque<Transfer>, rtt_us: u64) -> Option<usize> {
    let n = recent.len();
    let all: Vec<Transfer> = recent.iter().copied().collect();
    for k in 1..n.saturating_sub(1) {
        let (before, after) = all.split_at(k);
        let fit = fit_of(before, rtt_us)?;
        let rel = |t: &Transfer| -> i128 {
            let want = i128::from(fit.predict_us(t.bytes).max(1));
            ((i128::from(t.us) - want) << 32) / want
        };
        let mut a: Vec<i128> = after.iter().map(rel).collect();
        let mut b: Vec<i128> = before.iter().map(rel).collect();
        let (lo_a, hi_a) = (a.iter().min().copied()?, a.iter().max().copied()?);
        let (lo_b, hi_b) = (b.iter().min().copied()?, b.iter().max().copied()?);
        if !(lo_a > hi_b || hi_a < lo_b) {
            continue;
        }
        a.sort_unstable();
        b.sort_unstable();
        let (ma, mb) = (a.get(a.len() / 2).copied()?, b.get(b.len() / 2).copied()?);
        if (ma - mb).unsigned_abs() > u128::from(SHIFT) {
            return Some(k);
        }
    }
    None
}

/// What one node's own interfaces show it can move, bytes a second, each way: the most it was
/// seen to move (a lower bound on what it can) and, where the system says, its link's speed (an
/// upper bound).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Access {
    pub peak_rx: u64,
    pub peak_tx: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<u64>,
}

impl Access {
    /// What it was seen to send: its uplink's least (each way on its own: access links are often
    /// asymmetric, Lakshminarayanan and Padmanabhan 2003 §4.2 measuring broadband's median at 212
    /// Kb/s up and 900 Kb/s down).
    pub fn up(&self) -> Option<u64> {
        nonzero(self.peak_tx)
    }

    /// What it was seen to receive: its downlink's least.
    pub fn down(&self) -> Option<u64> {
        nonzero(self.peak_rx)
    }
}

fn nonzero(v: u64) -> Option<u64> {
    (v > 0).then_some(v)
}

/// Where a rate comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// This pair's own transfers.
    Measured,
    /// Another pair's between the same two sites, bounded by this pair's ends.
    Site,
    /// The smaller of the two ends' rates: an upper bound.
    Edges,
}

/// A pair's rate: bytes a second, its range, and what it rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rate {
    pub bps: u64,
    /// The least and most it may be, bytes a second (`lo` 0: no lower bound known).
    pub lo: u64,
    pub hi: u64,
    pub source: Source,
    /// The fixed cost of a transfer, µs, where measured.
    pub fixed_us: u64,
}

impl Rate {
    /// How long `bytes` take, µs.
    pub fn time_us(&self, bytes: u64) -> u64 {
        self.fixed_us.saturating_add(time_us(bytes, self.bps))
    }
}

/// The rate of `bytes`-sized transfers from `from` to `to`: the pair's own fit if it has one;
/// else a fit between the same two sites (`site`), held to what the two ends allow; else the ends'
/// bound, `hops` the access rates of any relays in between (each a node bytes pass in and out of).
pub fn estimate(own: Option<&Fit>, site: Option<&Fit>, from: &Access, to: &Access, hops: &[Access]) -> Option<Rate> {
    let mut edge: Option<u64> = None;
    let mut bound = |v: Option<u64>| {
        if let Some(v) = v {
            edge = Some(edge.map_or(v, |e: u64| e.min(v)));
        }
    };
    bound(from.up());
    bound(to.down());
    for h in hops {
        bound(h.down());
        bound(h.up());
    }
    let ceiling = [from.link, to.link].into_iter().flatten().min();
    if let Some(f) = own {
        let spread = spread_bps(f);
        return Some(Rate { bps: f.rate_bps, lo: f.rate_bps.saturating_sub(spread), hi: f.rate_bps.saturating_add(spread), source: Source::Measured, fixed_us: f.fixed_us });
    }
    #[cfg(test)]
    let site = if super::mutant() == Some(super::Mutant::SiteIgnored) { None } else { site };
    if let Some(f) = site {
        let bps = edge.map_or(f.rate_bps, |e| e.min(f.rate_bps));
        return Some(Rate { bps, lo: 0, hi: bps, source: Source::Site, fixed_us: f.fixed_us });
    }
    let bps = edge?;
    Some(Rate { bps, lo: 0, hi: ceiling.unwrap_or(bps).max(bps), source: Source::Edges, fixed_us: 0 })
}

/// How far a fit's rate may be off, bytes a second: its residual spread read at the median size
/// it saw, as a rate.
fn spread_bps(f: &Fit) -> u64 {
    if f.sd_us == 0 {
        return 0;
    }
    // At a transfer of one second's bytes, `sd` µs off is `rate·sd/1e6` bytes a second off.
    fixed::sat(u128::from(f.rate_bps) * u128::from(f.sd_us) / 1_000_000)
}
