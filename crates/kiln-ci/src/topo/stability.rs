//! How steady a link is, from its own history of drops (LB08): its drop rate and its outages'
//! lengths, kept as the sums a fit needs, from the connects and disconnects the link's ends see
//! anyway. Nothing is probed.
//!
//! **The model** is an alternating renewal process: the link is up for a while, drops, is out for
//! a while, comes back. Drops are taken as a Poisson process over the time the link is up, whose
//! rate's maximum-likelihood estimate is drops over time up, with standard error √drops over time
//! up. With no drop seen in `U` of up time, the rate is 0 and its 95 % upper bound `3/U`: the rule
//! of three (Hanley and Lippman-Hand, "If Nothing Goes Wrong, Is Everything All Right?
//! Interpreting Zero Numerators", JAMA 249(13), 1983; −ln 0.05 = 2.996).
//!
//! A run that takes `T` is cut short by a drop with chance `1 − e^(−λT)`, and then waits an outage
//! out: its expected stall is that chance times the mean outage ([`History::stall_ms`]), what the
//! placement kernel's stability term multiplies out.
//!
//! A path through a relay is up only while both its legs are: its drops are the legs' together
//! (competing risks, rates adding), and its outage the mean of the legs' weighted by how often each
//! drops ([`Path::of`]).

use serde::{Deserialize, Serialize};

use super::fixed::{self, ONE};

const HOUR_MS: u64 = 3_600_000;

/// One link's history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    /// Whether the link is up, and since when (ms): up or down since `at`.
    pub up: bool,
    pub at: i64,
    /// Whether the span since `at` is one nobody watched (the watching end was away): it counts
    /// neither as time up nor as an outage.
    pub away: bool,
    /// Time up in spans that ended, ms.
    pub up_ms: u64,
    /// Times it dropped while up.
    pub drops: u64,
    /// Outages that ended: how many, their total and their total of squares, ms.
    pub outages: u64,
    pub out_ms: u64,
    pub out_sq: u128,
}

impl History {
    /// The link came up at `at`.
    pub fn up(&mut self, at: i64) {
        if self.up {
            return;
        }
        if !self.away && self.drops > 0 {
            let out = u64::try_from(at.saturating_sub(self.at)).unwrap_or(0);
            self.outages = self.outages.saturating_add(1);
            self.out_ms = self.out_ms.saturating_add(out);
            self.out_sq = self.out_sq.saturating_add(u128::from(out) * u128::from(out));
        }
        *self = History { up: true, at, away: false, ..*self };
    }

    /// The link dropped at `at`.
    pub fn down(&mut self, at: i64) {
        if !self.up {
            return;
        }
        if !self.away {
            self.up_ms = self.up_ms.saturating_add(u64::try_from(at.saturating_sub(self.at)).unwrap_or(0));
            self.drops = self.drops.saturating_add(1);
        }
        *self = History { up: false, at, away: false, ..*self };
    }

    /// The end that watches went away at `at` (it stopped, or restarts): what follows until the
    /// link is seen again is neither up time nor an outage. Its last span up is credited only to
    /// `at`.
    pub fn away(&mut self, at: i64) {
        if self.up && !self.away {
            self.up_ms = self.up_ms.saturating_add(u64::try_from(at.saturating_sub(self.at)).unwrap_or(0));
        }
        *self = History { up: false, at, away: true, ..*self };
    }

    /// Time up as of `now`, ms, the span under way included.
    pub fn up_ms_at(&self, now: i64) -> u64 {
        let open = if self.up && !self.away { u64::try_from(now.saturating_sub(self.at)).unwrap_or(0) } else { 0 };
        self.up_ms.saturating_add(open)
    }

    /// The drop rate, drops an hour of up time, Q32: its estimate and its standard error; with no
    /// drop seen, 0 and the rule of three's 95 % bound instead. None before any time up.
    pub fn rate(&self, now: i64) -> Option<(u64, u64)> {
        let up = self.up_ms_at(now);
        if up == 0 {
            return None;
        }
        let per_hour = |n: u128| fixed::sat((n << 32) * u128::from(HOUR_MS) / u128::from(up));
        if self.drops == 0 {
            return Some((0, per_hour(3)));
        }
        let rate = per_hour(u128::from(self.drops));
        // √n/U, Q32: √(n·2^64)/U.
        let se = fixed::sat((u128::from(self.drops) << 64).isqrt() * u128::from(HOUR_MS) / u128::from(up));
        Some((rate, se))
    }

    /// The mean outage, ms, once one has ended.
    pub fn mean_outage_ms(&self) -> Option<u64> {
        (self.outages > 0).then(|| self.out_ms / self.outages)
    }

    /// The chance it drops within `span_ms`, Q32: `1 − e^(−λ·span)`.
    pub fn p_drop(&self, span_ms: u64, now: i64) -> u64 {
        let up = self.up_ms_at(now);
        if up == 0 || self.drops == 0 {
            return 0;
        }
        let x = fixed::sat((u128::from(self.drops) << 32) * u128::from(span_ms) / u128::from(up));
        fixed::one_minus_exp_neg(x)
    }

    /// A run of `span_ms` here waits, expected, this long on drops: the chance one comes times the
    /// mean outage (`pooled`, the fleet's, while this link's own outages are unmeasured).
    pub fn stall_ms(&self, span_ms: u64, now: i64, pooled: Option<u64>) -> u64 {
        #[cfg(test)]
        if super::mutant() == Some(super::Mutant::StallIgnoresDrops) {
            return 0;
        }
        let out = self.mean_outage_ms().or(pooled).unwrap_or(0);
        fixed::mul(out, self.p_drop(span_ms, now))
    }
}

/// A path's stability: its drop rate (an hour, Q32) and mean outage (ms), measured on one link or
/// composed from a relayed path's legs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Path {
    pub per_hour: u64,
    pub outage_ms: u64,
}

impl Path {
    /// One link's, as of `now`, its outage the fleet's `pooled` while its own are unmeasured.
    pub fn link(h: &History, now: i64, pooled: Option<u64>) -> Path {
        Path { per_hour: h.rate(now).map_or(0, |(r, _)| r), outage_ms: h.mean_outage_ms().or(pooled).unwrap_or(0) }
    }

    /// A path up only while all of `legs` are: their rates add, and an outage is the leg's that
    /// dropped, weighted by how often each does.
    pub fn of(legs: &[Path]) -> Path {
        let per_hour = legs.iter().fold(0u64, |a, l| a.saturating_add(l.per_hour));
        if per_hour == 0 {
            return Path { per_hour: 0, outage_ms: legs.iter().map(|l| l.outage_ms).max().unwrap_or(0) };
        }
        let weighted: u128 = legs.iter().map(|l| u128::from(l.per_hour) * u128::from(l.outage_ms)).sum();
        Path { per_hour, outage_ms: fixed::sat(weighted / u128::from(per_hour)) }
    }

    /// The chance it drops within `span_ms`, Q32.
    pub fn p_drop(&self, span_ms: u64) -> u64 {
        let x = fixed::sat(u128::from(self.per_hour) * u128::from(span_ms) / u128::from(HOUR_MS));
        fixed::one_minus_exp_neg(x)
    }

    /// The expected stall of a run of `span_ms` on it, ms.
    pub fn stall_ms(&self, span_ms: u64) -> u64 {
        fixed::mul(self.outage_ms, self.p_drop(span_ms))
    }
}

/// The fleet's mean outage, ms, over every link's outages that ended: what a link with none of its
/// own is taken to suffer.
pub fn pooled_outage<'a>(links: impl IntoIterator<Item = &'a History>) -> Option<u64> {
    let (n, ms) = links.into_iter().fold((0u64, 0u64), |(n, ms), h| (n.saturating_add(h.outages), ms.saturating_add(h.out_ms)));
    (n > 0).then(|| ms / n)
}

/// 1 in Q32, for callers composing chances.
pub const CERTAIN: u64 = ONE;
