//! Learned weights for where runs wait (docs/lb.md "Learned wait weights"; LB08, W05-R23): which
//! members a run that finds no room waits at, when the owner is told to learn them
//! ([`super::Owner::set_weights`]; off until then).
//!
//! Lattanzi, Lavastida, Moseley and Vassilvitskii, "Online Scheduling via Learned Weights", SODA
//! 2020, pp. 1859-1877: restricted assignment (a job may run only on its eligible machines N(j), as
//! a run only on its kind's members), one weight per machine, each job split over its eligible
//! machines in proportion to their weights (their (3.1): x_ij = w_i / Σ_{i' ∈ N(j)} w_i'). Good
//! weights exist for any instance; predicted ones within a factor η of good ones give an
//! O(min{log η, log m})-competitive fractional makespan (Theorem 3.1) with their Algorithm 1, which
//! this follows step for step:
//!
//! - Each job j adds p_j x_ij(ŵ) to the phase load L_i of every i ∈ N(j); then each of them whose
//!   L_i passed 2T has its weight halved and starts a new phase (L_i ← 0). If 1/2 ≤ ŵ_i/w_i ≤ 1 for
//!   all i then x_ij(ŵ) ≤ 2 x_ij(w), so a machine goes through at most ⌈log2 μ_i⌉ + 1 phases of at
//!   most 2T each (μ_i = ŵ_i/w_i).
//! - Once a job makes the algorithm's makespan (a machine's load over all its phases) pass
//!   2 log m T, the predictions aren't helping, and they switch to a robust algorithm (§3.1, after
//!   Theorem 3.1): the owner draws its places as it did before the weights, and says which runner
//!   and loads ([`FellBack`]).
//! - T is the optimal fractional makespan, which Algorithm 1 is given. Their §2 ("Technical
//!   Assumptions") and Lemma D.1 remove that by a standard online doubling argument, which is
//!   Aspnes, Azar, Fiat, Plotkin and Waarts's (their [3]; "On-line routing of virtual circuits with
//!   applications to load balancing and machine scheduling", J. ACM 44(3), 1997, §2.1 and §3.2):
//!   T starts at what the first job alone shows of the optimum, and an epoch (their phase) runs
//!   the algorithm on its own jobs, ignoring the jobs of the epochs before; once its jobs show the
//!   optimum is past T, T doubles and a new epoch begins with the job that showed it. Each
//!   doubling is shown needed, so T stays below twice the optimum, and the epochs together cost at
//!   most four times one epoch's guarantee. What the jobs show is a lower bound on the optimum:
//!   every job of a kind runs on the kind's members, so one of them carries at least the kind's
//!   sizes over the members' CPUs; and one of the fleet carries the fleet's sizes over its CPUs.
//!   A new epoch begins from the weights as learned (Theorem 3.1 holds for any predictions, η
//!   theirs), every load at 0 and nothing fallen back.
//!
//! For kiln's runners:
//! - A runner of C CPUs is C unit machines weighted alike (related machines as Algorithm 1's
//!   identical ones): its load is per unit of its CPU, its units pass 2T together, and halving the
//!   runner's weight halves theirs. Its weight starts at its CPU budget (allocation in proportion
//!   to capacity: what predicts with no history), or at what the shell predicts ([`Weights::set`]).
//! - m, in the switch, counts the runners: the machines the places are drawn among, as the draw
//!   the owner falls back to draws among them.
//! - A job's size p_j is the caller's: the kernel's, a wait's CPU need; the place core's, a run's
//!   CPUs times its median.
//!
//! A job costs O(|N(j)|): Algorithm 1 adds to and tests every eligible machine. Integer only: loads
//! and T in 2^-32 of the caller's size unit per CPU thousandth, saturating.

use super::RunnerId;

/// One, in loads and T.
pub const ONE: u64 = 1 << 32;

/// Why an owner stopped drawing by its weights: in this epoch `runner`'s load passed `bound`
/// (2⌈log2 m⌉ times T), so the predictions weren't helping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FellBack {
    pub runner: RunnerId,
    pub load: u64,
    pub bound: u64,
}

/// What one kind's jobs (or the fleet's) in this epoch show of the optimum: their sizes over the
/// CPUs that may run them, counted afresh when those CPUs change.
#[derive(Debug, Clone, Copy, Default)]
struct Bound {
    work: u128,
    cpu: u64,
}

impl Bound {
    /// A job of `size` over `cpu` CPUs (thousandths): the bound now, in 2^-32 per CPU thousandth.
    fn add(&mut self, size: u64, cpu: u64) -> u64 {
        if self.cpu != cpu {
            *self = Bound { work: 0, cpu };
        }
        self.work = self.work.saturating_add(u128::from(size));
        u64::try_from(self.work.saturating_mul(u128::from(ONE)) / u128::from(cpu.max(1))).unwrap_or(u64::MAX)
    }
}

/// The weights an owner learns, one per runner.
#[derive(Debug, Clone, Default)]
pub struct Weights {
    pub(crate) on: bool,
    /// Each runner's weight (0: none set, its CPU budget is taken).
    w: Vec<u64>,
    /// Each runner's load in its phase (L_i), and in this epoch over all its phases.
    phase: Vec<u64>,
    epoch: Vec<u64>,
    /// What this epoch's jobs show of the optimum, by kind and for the fleet.
    kinds: Vec<Bound>,
    fleet: Bound,
    /// T (0: no job yet), and how many times it doubled.
    t: u64,
    doubled: u32,
    pub(crate) fell_back: Option<FellBack>,
}

/// ⌈log2 m⌉, at least 1.
fn log2_up(m: u32) -> u64 {
    u64::from(32 - m.saturating_sub(1).leading_zeros()).max(1)
}

impl Weights {
    /// Weights learned (`on`) or not, nothing learned yet.
    pub fn new(on: bool) -> Weights {
        Weights { on, ..Weights::default() }
    }

    /// Runner `r`'s weight, its CPU budget `cpu` when none is set.
    pub fn weight(&self, r: u32, cpu: u64) -> u64 {
        match self.w.get(r as usize).copied() {
            Some(w) if w > 0 => w,
            _ => cpu.max(1),
        }
    }

    /// The heaviest of `members` (each with its CPU): what a draw in proportion to the weights
    /// rejects against.
    pub fn heaviest(&self, members: &[(u32, u64)]) -> u64 {
        members.iter().map(|(r, c)| self.weight(*r, *c)).max().unwrap_or(1)
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
            *p = 0;
        }
    }

    /// Learning starts again from the weights as they are: T not known, no load, nothing fallen
    /// back.
    pub fn begin(&mut self) {
        self.t = 0;
        self.doubled = 0;
        self.fell_back = None;
        self.phase.clear();
        self.epoch.clear();
        self.kinds.clear();
        self.fleet = Bound::default();
    }

    /// T now (0: no job yet), in 2^-32 of a size unit per CPU thousandth, and how many times it
    /// doubled since learning began.
    pub fn t(&self) -> (u64, u32) {
        (self.t, self.doubled)
    }

    /// Whether the owner draws by its weights: they're on and haven't fallen back this epoch.
    pub fn drawing(&self) -> bool {
        self.on && self.fell_back.is_none()
    }

    /// A job of size `size` of kind `kind`, whose eligible machines are `members` (each with its
    /// CPU, in thousandths), in a fleet of `m` runners and `fleet_cpu` CPUs: Algorithm 1's step,
    /// in this epoch or the next one it shows is due.
    pub fn waited(&mut self, kind: u32, size: u64, members: &[(u32, u64)], fleet_cpu: u64, m: u32) {
        if size == 0 || members.is_empty() {
            return;
        }
        let cpu = members.iter().fold(0u64, |a, (_, c)| a.saturating_add((*c).max(1)));
        let fleet_cpu = fleet_cpu.max(cpu);
        // T: what this epoch's jobs show of the optimum, this one among them, doubling T and
        // beginning an epoch with this job while it shows T is short (Aspnes et al.'s "fail").
        loop {
            let shown = self.shown(kind, size, cpu, fleet_cpu);
            if self.t == 0 {
                self.t = shown.max(1);
                break;
            }
            #[cfg(test)]
            if super::planted(super::Mutant::NoDoubling) {
                break;
            }
            if shown <= self.t || self.t == u64::MAX {
                break;
            }
            self.t = self.t.saturating_mul(2);
            self.doubled = self.doubled.saturating_add(1);
            self.epoch_begins();
        }
        if self.fell_back.is_some() {
            return;
        }
        let total = members.iter().fold(0u128, |a, (r, c)| a.saturating_add(u128::from(self.weight(*r, *c))));
        let (two_t, bound) = (self.t.saturating_mul(2), self.t.saturating_mul(2 * log2_up(m)));
        #[cfg(test)]
        let members = if super::planted(super::Mutant::DrawnOnly) { members.get(..2).unwrap_or(members) } else { members };
        for &(r, c) in members {
            let w = self.weight(r, c);
            // p_j x_ij(ŵ) per CPU, with the weights as the job found them: one divide, exact to
            // the unit while size times weight is below 2^96, else through x_ij in 2^-32.
            let den = total.max(1).saturating_mul(u128::from(c.max(1)));
            let add = match u128::from(size).checked_mul(u128::from(w)).and_then(|x| x.checked_mul(u128::from(ONE))) {
                Some(num) => num / den,
                None => u128::from(size).saturating_mul((u128::from(w) << 32) / total.max(1)) / u128::from(c.max(1)),
            };
            let add = u64::try_from(add).unwrap_or(u64::MAX);
            let i = r as usize;
            if self.phase.len() <= i {
                self.phase.resize(i + 1, 0);
                self.epoch.resize(i + 1, 0);
            }
            let (Some(p), Some(e)) = (self.phase.get(i).copied(), self.epoch.get(i).copied()) else { continue };
            let (p, e) = (p.saturating_add(add), e.saturating_add(add));
            #[cfg(test)]
            let robust = !super::planted(super::Mutant::NoFallback);
            #[cfg(not(test))]
            let robust = true;
            if robust && self.fell_back.is_none() && e > bound {
                self.fell_back = Some(FellBack { runner: RunnerId(r), load: e, bound });
            }
            #[cfg(test)]
            let learns = !super::planted(super::Mutant::NoLearn);
            #[cfg(not(test))]
            let learns = true;
            let halve = learns && p > two_t;
            if let Some(x) = self.epoch.get_mut(i) {
                *x = e;
            }
            if let Some(x) = self.phase.get_mut(i) {
                *x = if halve { 0 } else { p };
            }
            if halve {
                if self.w.len() <= i {
                    self.w.resize(i + 1, 0);
                }
                if let Some(x) = self.w.get_mut(i) {
                    *x = (w / 2).max(1);
                }
            }
        }
    }

    /// What this epoch's jobs, with one more of `size` of kind `kind` (over `cpu` CPUs, in a fleet
    /// of `fleet_cpu`), show of the optimum: the more of the kind's bound and the fleet's.
    fn shown(&mut self, kind: u32, size: u64, cpu: u64, fleet_cpu: u64) -> u64 {
        let k = kind as usize;
        if self.kinds.len() <= k {
            self.kinds.resize(k + 1, Bound::default());
        }
        let of_kind = self.kinds.get_mut(k).map_or(0, |b| b.add(size, cpu));
        let of_fleet = self.fleet.add(size, fleet_cpu);
        #[cfg(test)]
        if super::planted(super::Mutant::KindBlind) {
            return of_fleet;
        }
        of_kind.max(of_fleet)
    }

    /// An epoch begins: its jobs alone are counted from here, every load at 0, nothing fallen
    /// back, the weights as learned.
    fn epoch_begins(&mut self) {
        self.kinds.clear();
        self.fleet = Bound::default();
        self.fell_back = None;
        #[cfg(test)]
        if super::planted(super::Mutant::NoRestart) {
            return;
        }
        self.phase.iter_mut().for_each(|p| *p = 0);
        self.epoch.iter_mut().for_each(|e| *e = 0);
    }
}
