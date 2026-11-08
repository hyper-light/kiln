//! Learned weights for where runs wait (docs/lb.md "Learned wait weights"; LB08, W05-R23): which
//! members a run that finds no room waits at, when the owner is told to learn them
//! ([`super::Owner::set_weights`]; off until then).
//!
//! Lattanzi, Lavastida, Moseley and Vassilvitskii, "Online Scheduling via Learned Weights", SODA
//! 2020, pp. 1859-1877: restricted assignment (a job may run only on its eligible machines, as a run
//! only on its signature's members), one weight per machine, each job split over its eligible
//! machines in proportion to their weights (their (3.1): x_ij = w_i / Σ_{i' ∈ N(j)} w_i'). Good
//! weights exist for any instance; predicted ones within a factor η give an O(min{log η, log m})-
//! competitive fractional makespan (Theorem 3.1) with their Algorithm 1: a machine whose load in its
//! phase passes 2T has its weight halved and starts a new phase (if 1/2 ≤ ŵ_i/w_i ≤ 1 for all i,
//! then x_ij(ŵ) ≤ 2 x_ij(w), so each halving closes a factor of two of the error), and once the
//! makespan passes 2 log m T the predictions aren't helping and a robust algorithm takes over.
//!
//! Here, for kiln's runners:
//! - A runner's weight starts at its CPU budget (allocation in proportion to capacity: what
//!   predicts with no history), or at what the shell predicts ([`Weights::set`]).
//! - A run with no room waits at members drawn in proportion to their weights: the fractional
//!   assignment rounded by sampling, as their randomized rounding does.
//! - Loads are the fractional assignment's, per unit of each runner's CPU, kept lazily: a
//!   signature's waits add need / Σ weights to its accumulator, and a runner's phase load is its
//!   weight times what its signatures' accumulators gained since its phase began, over its CPU.
//! - T is the balanced load, each wait's need over the owner's whole CPU: a lower bound on the
//!   waits' fractional makespan so far, grown as they arrive.
//! - A drawn runner whose phase load passes 2T (gained since its phase began) has its weight
//!   halved and starts a phase; one whose load since the weights were set passes 2⌈log2 m⌉T makes
//!   the owner draw uniformly again (the robust choice, as with tokens), and say why ([`FellBack`]).
//!
//! Integer only: loads and T in 1/2^32 of a runner's CPU, wrapping (differences within a phase).

use super::RunnerId;

/// One, in loads and T.
pub const ONE: u64 = 1 << 32;

/// Why an owner stopped drawing by its weights: `runner`'s load since they were set passed
/// `bound` (2⌈log2 m⌉ times the balanced load), so the predictions weren't helping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FellBack {
    pub runner: RunnerId,
    pub load: u64,
    pub bound: u64,
}

/// A runner's phase.
#[derive(Debug, Clone, Default)]
struct Phase {
    /// T when it began.
    t0: u64,
    /// Each of its signatures' accumulator when it began.
    from: Vec<(u32, u64)>,
    /// Its load in phases closed since the weights were set.
    done: u64,
    begun: bool,
}

/// A signature's accumulator and its weights' sum and largest, worked out again when its members
/// or any weight changed.
#[derive(Debug, Clone, Copy, Default)]
struct Acc {
    a: u64,
    sum: u64,
    max: u64,
    len: u32,
    stamp: u64,
}

/// The weights an owner learns, one per runner.
#[derive(Debug, Clone, Default)]
pub struct Weights {
    pub(crate) on: bool,
    /// Each runner's weight (0: none set, its CPU budget is taken).
    w: Vec<u64>,
    phase: Vec<Phase>,
    acc: Vec<Acc>,
    /// T, the balanced load.
    t: u64,
    /// T when the weights were set.
    start: u64,
    /// Grown on every weight change: signatures' sums are stale.
    stamp: u64,
    /// The owner's whole CPU, worked out again with a signature's sums.
    whole: u64,
    pub(crate) fell_back: Option<FellBack>,
}

/// ⌈log2 m⌉, at least 1.
fn log2_up(m: u32) -> u64 {
    u64::from(32 - m.saturating_sub(1).leading_zeros()).max(1)
}

impl Weights {
    /// Runner `r`'s weight, its CPU budget `cpu` when none is set.
    pub fn weight(&self, r: u32, cpu: u64) -> u64 {
        match self.w.get(r as usize).copied() {
            Some(w) if w > 0 => w,
            _ => cpu.max(1),
        }
    }

    /// Runner `r`'s weight is predicted to be `w` (at least 1): its phase begins again.
    pub fn set(&mut self, r: u32, w: u64) {
        let i = r as usize;
        if self.w.len() <= i {
            self.w.resize(i + 1, 0);
        }
        if let Some(x) = self.w.get_mut(i) {
            *x = w.max(1);
        }
        if let Some(p) = self.phase.get_mut(i) {
            p.begun = false;
        }
        self.stamp = self.stamp.wrapping_add(1);
    }

    /// Learning starts (again) from the weights as they are: nothing has fallen back.
    pub fn begin(&mut self) {
        self.start = self.t;
        self.fell_back = None;
        for p in &mut self.phase {
            *p = Phase::default();
        }
    }

    /// Signature `sig`'s weights, summed and the largest, over `members` (each with its CPU),
    /// worked out again when its members or a weight changed; with them, the owner's whole CPU
    /// (`whole`), T's denominator.
    pub fn sums(&mut self, sig: u32, len: u32, members: impl Iterator<Item = (u32, u64)>, whole: impl FnOnce() -> u64) -> (u64, u64) {
        let i = sig as usize;
        if self.acc.len() <= i {
            self.acc.resize(i + 1, Acc::default());
        }
        let stamp = self.stamp;
        let stale = self.acc.get(i).is_none_or(|a| a.len != len || a.stamp != stamp || a.sum == 0);
        if stale {
            let (mut sum, mut max) = (0u64, 0u64);
            for (r, cpu) in members {
                let w = self.weight(r, cpu);
                sum = sum.saturating_add(w);
                max = max.max(w);
            }
            if let Some(a) = self.acc.get_mut(i) {
                (a.sum, a.max, a.len, a.stamp) = (sum, max, len, stamp);
            }
            self.whole = whole();
        }
        self.acc.get(i).map_or((0, 0), |a| (a.sum, a.max))
    }

    /// A run of `need` CPU waits for signature `sig`: the fractional assignment's loads and T
    /// grow. Call [`Weights::sums`] for `sig` first.
    pub fn waited(&mut self, sig: u32, need: u64) {
        let cap = self.whole;
        let n = u128::from(need) * u128::from(ONE);
        if let Some(a) = self.acc.get_mut(sig as usize)
            && a.sum > 0
        {
            a.a = a.a.wrapping_add(u64::try_from(n / u128::from(a.sum)).unwrap_or(u64::MAX));
        }
        if cap > 0 {
            self.t = self.t.wrapping_add(u64::try_from(n / u128::from(cap)).unwrap_or(u64::MAX));
        }
    }

    /// Runner `r` (of CPU `cpu`, a member of `sigs`, one of `m` runners) was drawn: Algorithm 1's
    /// test. Its weight halves when its phase load passed 2T, and the owner falls back when its
    /// load since the weights were set passed 2⌈log2 m⌉T.
    pub fn check(&mut self, r: u32, cpu: u64, sigs: &[u32], m: u32) {
        let i = r as usize;
        if self.phase.len() <= i {
            self.phase.resize(i + 1, Phase::default());
        }
        let (t, w) = (self.t, self.weight(r, cpu));
        let now: Vec<(u32, u64)> = sigs.iter().map(|s| (*s, self.acc.get(*s as usize).map_or(0, |a| a.a))).collect();
        let Some(p) = self.phase.get_mut(i) else { return };
        if !p.begun {
            *p = Phase { t0: t, from: now, done: p.done, begun: true };
            return;
        }
        let gained: u128 = now.iter().map(|(s, a)| u128::from(a.wrapping_sub(p.from.iter().find(|(x, _)| x == s).map_or(*a, |(_, f)| *f)))).sum();
        let load = u64::try_from(u128::from(w) * gained / u128::from(cpu.max(1))).unwrap_or(u64::MAX);
        let dt = t.wrapping_sub(p.t0);
        let total = p.done.saturating_add(load);
        let bound = t.wrapping_sub(self.start).saturating_mul(2 * log2_up(m));
        #[cfg(test)]
        let robust = !super::planted(super::Mutant::NoFallback);
        #[cfg(not(test))]
        let robust = true;
        if robust && self.fell_back.is_none() && total > bound {
            self.fell_back = Some(FellBack { runner: RunnerId(r), load: total, bound });
        }
        #[cfg(test)]
        if super::planted(super::Mutant::NoLearn) {
            return;
        }
        if load > dt.saturating_mul(2) {
            *p = Phase { t0: t, from: now, done: total, begun: true };
            if self.w.len() <= i {
                self.w.resize(i + 1, 0);
            }
            if let Some(x) = self.w.get_mut(i) {
                *x = (w / 2).max(1);
            }
            self.stamp = self.stamp.wrapping_add(1);
        }
    }

    /// Whether the owner draws by its weights: they're on and haven't fallen back.
    pub fn drawing(&self) -> bool {
        self.on && self.fell_back.is_none()
    }
}
