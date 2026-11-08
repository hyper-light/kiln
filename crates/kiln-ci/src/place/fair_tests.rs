//! Fair share across the fleet (W05-R32; docs/needs.md §5): placement's owners and runners, run in
//! process over links that delay each word, measured against an exact constrained max-min fair
//! oracle.
//!
//! - **The oracle** ([`cmmf`]): Choosy's constrained max-min fairness (Ghodsi, Zaharia, Shenker and
//!   Stoica, EuroSys 2013), weighted, for tenants that always have work waiting. Every allocation the
//!   runners can serve is a point of the polymatroid whose rank of a set of tenants is the capacity
//!   of the runners any of them may use (Hall's theorem); the max-min fair one is its
//!   lexicographically optimal base, found by Fujishige's decomposition ("Lexicographically optimal
//!   base of a polymatroid with respect to a weight vector", Math. of OR 5(2), 1980): the least
//!   rank-over-weight among the tenants not fixed yet, its largest set of tenants fixed at that level,
//!   the rest contracted by what they hold. Exact, in integers.
//! - **The fleet** ([`Fleet`]): owners and runners as placement has them; each word waits on its link
//!   (in order, as a reach link keeps it) up to the delay the test gives; a run's tree is copied in a
//!   link's time; each tenant keeps its backlog waiting, its runs coming through its owners in turn,
//!   each run waiting at two runners as owners place it. What each tenant held, over a window after
//!   a warm-up, is compared with the oracle's shares of what the runners delivered in that window.
//!
//! The simulation's numbers (runners, tenants, run times, delays, windows) are the test's inputs:
//! placement acts on none of them.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::ask::{Ask, Peer, Sample};
use super::keys::SplitMix;
use super::msg::{Down, Up};
use super::owner::{Cmd, Machine, Owner, Size};
use super::runner::{Admission, Init, Records, Reply, Settings, Start};
use crate::facts::RunnerFacts;
use crate::sched::needs::Needs;
use crate::sched::state::{Cpus, Fence, PerMille, Resources};
use crate::sched::units::Ms;

/// Constrained max-min fair shares, weighted (Choosy's CMMF), of runners with capacities `cap`
/// among tenants that may each use the runners `eligible` marks (bit `r` for runner `r`) and that
/// always have work waiting: each tenant's share as a fraction, numerator and denominator.
pub(crate) fn cmmf(cap: &[u128], eligible: &[u64], weights: &[u128]) -> Vec<(u128, u128)> {
    let n = eligible.len().min(16);
    let full = (1usize << n) - 1;
    let members = |s: usize| (0..n).filter(move |t| (s >> t) & 1 == 1);
    // Each set's rank: the capacity of the runners any of its tenants may use.
    let rank: Vec<u128> = (0..=full)
        .map(|s| {
            let runners = members(s).fold(0u64, |m, t| m | eligible.get(t).copied().unwrap_or(0));
            cap.iter().enumerate().filter(|(r, _)| (runners >> r) & 1 == 1).map(|(_, c)| *c).sum()
        })
        .collect();
    let weight = |s: usize| members(s).map(|t| weights.get(t).copied().unwrap_or(0)).sum::<u128>();
    let mut out = vec![(0u128, 1u128); n];
    let mut fixed = 0usize;
    while fixed != full {
        let free = full & !fixed;
        let base = rank.get(fixed).copied().unwrap_or(0);
        let mut least: Option<(u128, u128)> = None;
        let mut at = 0usize;
        // Every nonempty set of the free tenants, each submask the next by (s - 1) & free.
        let mut s = free;
        while s != 0 {
            let (num, den) = (rank.get(s | fixed).copied().unwrap_or(0).saturating_sub(base), weight(s));
            if den > 0 {
                match least {
                    Some((ln, ld)) if num * ld > ln * den => {}
                    Some((ln, ld)) if num * ld == ln * den => at |= s,
                    _ => {
                        least = Some((num, den));
                        at = s;
                    }
                }
            }
            s = (s - 1) & free;
        }
        let Some((num, den)) = least else { break };
        if at == 0 {
            break;
        }
        for t in members(at) {
            if let Some(o) = out.get_mut(t) {
                *o = (num * weights.get(t).copied().unwrap_or(0), den);
            }
        }
        fixed |= at;
    }
    out
}

/// A tenant of the simulated fleet: a repository, its weight, the runners it may use (none named:
/// any), how many runs it keeps outstanding, its runs' mean time, whether its asks carry that
/// median, and the owners its runs come through in turn.
#[derive(Debug, Clone)]
pub(crate) struct Tenant {
    pub repo: String,
    pub weight: PerMille,
    pub among: Vec<usize>,
    pub backlog: u32,
    pub mean: i64,
    pub median: bool,
    pub owners: Vec<usize>,
}

/// A simulated fleet: each runner's CPUs, the tenants, how many owners, the most a word waits on a
/// link (ms), and the window measured (from `warm` to `until`, ms).
#[derive(Debug, Clone)]
pub(crate) struct Spec {
    pub cpus: Vec<u64>,
    pub tenants: Vec<Tenant>,
    pub owners: usize,
    pub delay: i64,
    pub warm: i64,
    pub until: i64,
    pub seed: u64,
}

#[derive(Debug)]
enum Ev {
    Down { o: usize, r: usize, run: Arc<str>, msg: Down },
    Up { r: usize, o: usize, token: u64, run: Arc<str>, msg: Up },
    Go { o: usize, r: usize, run: Arc<str>, fence: Fence },
    End { r: usize, o: usize, run: Arc<str> },
    TickOwner(usize),
    TickRunner(usize),
}

/// What a simulated window measured: each tenant's mean CPUs held and its oracle share of what the
/// runners delivered (CPUs), and the fleet's use of its CPUs.
#[derive(Debug, Clone)]
pub(crate) struct Measured {
    pub held: Vec<f64>,
    pub share: Vec<f64>,
    pub used: f64,
}

impl Measured {
    /// The largest gap between a tenant's share and what it held, in CPUs (Choosy's machines).
    pub fn gap(&self) -> f64 {
        self.held.iter().zip(&self.share).map(|(h, s)| (h - s).abs()).fold(0.0, f64::max)
    }
}

pub(crate) struct Fleet {
    spec: Spec,
    owners: Vec<Owner>,
    runners: Vec<Admission>,
    onames: Vec<String>,
    /// Each owner's handle of each runner, and back.
    rid: Vec<Vec<u32>>,
    by_rid: Vec<HashMap<u32, usize>>,
    q: BTreeMap<(i64, u64), Ev>,
    seq: u64,
    /// When each link's last word (owner, runner, toward the runner) arrives: words keep their order.
    last: HashMap<(usize, usize, bool), i64>,
    rng: SplitMix,
    now: i64,
    next: u64,
    turn: Vec<usize>,
    /// Each run's tenant and time, and where and when it started.
    runs: HashMap<Arc<str>, (usize, i64)>,
    started: HashMap<Arc<str>, (usize, i64)>,
    /// CPU-milliseconds within the window: each tenant's held, each runner's delivered.
    held: Vec<u128>,
    delivered: Vec<u128>,
    ticks_o: Vec<Option<i64>>,
    ticks_r: Vec<Option<i64>>,
}

fn rname(r: usize) -> String {
    format!("r{r}")
}

impl Fleet {
    pub fn new(spec: Spec) -> Fleet {
        let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
        let weights: BTreeMap<String, PerMille> = spec.tenants.iter().map(|t| (t.repo.clone(), t.weight)).collect();
        let settings = Settings { weights, ..Settings::default() };
        let sizes: Vec<Resources> = spec.cpus.iter().map(|c| Resources { cpu: Cpus::whole(*c), mem: 64 << 30, ..Resources::default() }).collect();
        let runners = sizes
            .iter()
            .enumerate()
            .map(|(r, size)| Admission::new(Init { me: rname(r), epoch: 1, facts: facts.clone(), labels: Vec::new(), sample: Sample { size: *size, ..Sample::default() }, records: Records::new(), settings: settings.clone() }, 0))
            .collect();
        let machines: Vec<Machine> = sizes.iter().enumerate().map(|(r, size)| Machine { name: rname(r), facts: facts.clone(), labels: Vec::new(), size: Size::known(size), epoch: 1, connected: true, places: true, idle: None }).collect();
        let onames: Vec<String> = (0..spec.owners).map(|o| format!("o{o}")).collect();
        let mut owners = Vec::new();
        let (mut rid, mut by_rid) = (Vec::new(), Vec::new());
        for (o, name) in onames.iter().enumerate() {
            let mut owner = Owner::seeded(name, 1, spec.seed ^ (o as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let _ = owner.machines(machines.clone());
            let ids: Vec<u32> = (0..spec.cpus.len()).map(|r| owner.runner_id(&rname(r))).collect();
            by_rid.push(ids.iter().enumerate().map(|(r, id)| (*id, r)).collect());
            rid.push(ids);
            owners.push(owner);
        }
        let (n, m, k) = (spec.tenants.len(), spec.cpus.len(), spec.owners);
        Fleet { rng: SplitMix(spec.seed), spec, owners, runners, onames, rid, by_rid, q: BTreeMap::new(), seq: 0, last: HashMap::new(), now: 0, next: 0, turn: vec![0; n], runs: HashMap::new(), started: HashMap::new(), held: vec![0; n], delivered: vec![0; m], ticks_o: vec![None; k], ticks_r: vec![None; m] }
    }

    fn at(&mut self, t: i64, ev: Ev) {
        self.seq += 1;
        self.q.insert((t, self.seq), ev);
    }

    /// A word on the link between owner `o` and runner `r`, toward the runner or not: it arrives
    /// within the delay, never before the word said before it.
    fn send(&mut self, o: usize, r: usize, down: bool, ev: Ev) {
        let d = i64::from(self.rng.below(u32::try_from(self.spec.delay.max(0)).unwrap_or(0).saturating_add(1)));
        let last = self.last.get(&(o, r, down)).copied().unwrap_or(0);
        let t = (self.now + d).max(last);
        self.last.insert((o, r, down), t);
        self.at(t, ev);
    }

    /// Tenant `t` asks one more run, through its next owner.
    fn ask(&mut self, t: usize) {
        let Some(ten) = self.spec.tenants.get(t).cloned() else { return };
        let o = ten.owners.get(self.turn.get(t).copied().unwrap_or(0) % ten.owners.len().max(1)).copied().unwrap_or(0);
        if let Some(x) = self.turn.get_mut(t) {
            *x += 1;
        }
        self.next += 1;
        let run: Arc<str> = Arc::from(format!("{}-{}", ten.repo, self.next));
        let half = (ten.mean / 2).max(1);
        let dur = half + i64::from(self.rng.below(u32::try_from(ten.mean).unwrap_or(u32::MAX).saturating_add(1)));
        self.runs.insert(Arc::clone(&run), (t, dur));
        let ask = Ask {
            run: run.to_string(),
            needs: Needs { request: Resources { cpu: Cpus::whole(1), ..Resources::default() }, ..Needs::default() },
            repo: ten.repo.clone(),
            actor: "a".into(),
            median: ten.median.then(|| Ms::try_from(ten.mean).unwrap_or(0)),
            among: ten.among.iter().map(|r| rname(*r)).collect(),
            ..Ask::default()
        };
        let mut cmds = Vec::new();
        if let Some(owner) = self.owners.get_mut(o) {
            owner.ask_c(ask, self.now, &mut cmds);
        }
        self.cmds(o, cmds);
    }

    fn cmds(&mut self, o: usize, cmds: Vec<Cmd>) {
        for c in cmds {
            match c {
                Cmd::Tell { to, run, msg } => {
                    let Some(r) = self.by_rid.get(o).and_then(|m| m.get(&to)).copied() else { continue };
                    self.send(o, r, true, Ev::Down { o, r, run, msg });
                }
                Cmd::Go { run, to, fence } | Cmd::Restart { run, to, fence } => {
                    let Some(r) = self.by_rid.get(o).and_then(|m| m.get(&to)).copied() else { continue };
                    // Its tree copied in a link's time.
                    self.send(o, r, true, Ev::Go { o, r, run, fence });
                }
                Cmd::Answer { .. } | Cmd::Fallback { .. } => {}
            }
        }
        self.retick_owner(o);
    }

    fn replies(&mut self, r: usize, out: Vec<Reply>) {
        for m in out {
            let name = self.runners.get(r).map(|a| a.owner_name(m.owner).to_owned()).unwrap_or_default();
            let Some(o) = self.onames.iter().position(|n| *n == name) else { continue };
            self.send(o, r, false, Ev::Up { r, o, token: m.token, run: m.run, msg: m.msg });
        }
        self.retick_runner(r);
    }

    fn retick_owner(&mut self, o: usize) {
        let Some(t) = self.owners.get(o).and_then(Owner::timer) else { return };
        if self.ticks_o.get(o).copied().flatten().is_none_or(|x| t < x) {
            if let Some(x) = self.ticks_o.get_mut(o) {
                *x = Some(t);
            }
            self.at(t.max(self.now), Ev::TickOwner(o));
        }
    }

    fn retick_runner(&mut self, r: usize) {
        let Some(t) = self.runners.get(r).and_then(Admission::timer) else { return };
        if self.ticks_r.get(r).copied().flatten().is_none_or(|x| t < x) {
            if let Some(x) = self.ticks_r.get_mut(r) {
                *x = Some(t);
            }
            self.at(t.max(self.now), Ev::TickRunner(r));
        }
    }

    /// CPU-milliseconds of `[from, to)` within the window.
    fn within(&self, from: i64, to: i64) -> u128 {
        u128::try_from(to.min(self.spec.until) - from.max(self.spec.warm)).unwrap_or(0)
    }

    fn step(&mut self, ev: Ev) {
        match ev {
            Ev::Down { o, r, run, msg } => {
                let mut out = Vec::new();
                let peer = Peer { name: self.onames.get(o).map_or("", String::as_str), epoch: 1 };
                let Some(a) = self.runners.get_mut(r) else { return };
                match msg {
                    Down::Probe { ask, token, elsewhere } => {
                        let predicted = ask.median;
                        a.carried_r(peer, &ask.run, Some(&*ask), elsewhere, self.now, &mut out);
                        a.probe_r(peer, ask, token, predicted, self.now, &mut out);
                    }
                    Down::Withdraw => a.withdraw_r(peer, &run, self.now, &mut out),
                    Down::Take { fence } => a.take_r(peer, &run, fence, self.now, &mut out),
                    Down::Decline { fence } => a.decline_r(peer, &run, fence, self.now, &mut out),
                    Down::Start { fence, elsewhere } => self.start(o, r, &run, fence, elsewhere, &mut out),
                    Down::Ack => a.ack(peer, &run),
                }
                self.replies(r, out);
            }
            Ev::Go { o, r, run, fence } => {
                let to = self.rid.get(o).and_then(|v| v.get(r)).copied().unwrap_or(u32::MAX);
                if let Some(w) = self.owners.get_mut(o)
                    && w.starting_c(&run, to, fence)
                {
                    let elsewhere = w.start_carry(&run);
                    self.send(o, r, true, Ev::Down { o, r, run, msg: Down::Start { fence, elsewhere } });
                }
            }
            Ev::Up { r, o, token, run, msg } => {
                let to = self.rid.get(o).and_then(|v| v.get(r)).copied().unwrap_or(u32::MAX);
                let mut cmds = Vec::new();
                if let Some(w) = self.owners.get_mut(o) {
                    w.heard_c(to, token, &run, msg, self.now, &mut cmds);
                }
                self.cmds(o, cmds);
            }
            Ev::End { r, o, run } => {
                if let Some((_, from)) = self.started.remove(&run) {
                    let cpu = self.within(from, self.now);
                    if let Some(&(t, _)) = self.runs.get(&run)
                        && let Some(h) = self.held.get_mut(t)
                    {
                        *h += cpu;
                    }
                    if let Some(d) = self.delivered.get_mut(r) {
                        *d += cpu;
                    }
                }
                let mut out = Vec::new();
                let peer = Peer { name: self.onames.get(o).map_or("", String::as_str), epoch: 1 };
                if let Some(a) = self.runners.get_mut(r) {
                    a.ended_r(peer, &run, true, self.now, &mut out);
                }
                self.replies(r, out);
                // Its tenant keeps its backlog: one more in its place.
                if let Some((t, _)) = self.runs.remove(&run) {
                    self.ask(t);
                }
            }
            Ev::TickOwner(o) => {
                if let Some(x) = self.ticks_o.get_mut(o) {
                    *x = None;
                }
                let mut cmds = Vec::new();
                if let Some(w) = self.owners.get_mut(o) {
                    w.tick_c(self.now, &mut cmds);
                }
                self.cmds(o, cmds);
            }
            Ev::TickRunner(r) => {
                if let Some(x) = self.ticks_r.get_mut(r) {
                    *x = None;
                }
                let mut out = Vec::new();
                if let Some(a) = self.runners.get_mut(r) {
                    a.tick_r(self.now, &mut out);
                }
                self.replies(r, out);
            }
        }
    }

    /// The start of `run` reaches runner `r`: a new one runs for its time.
    fn start(&mut self, o: usize, r: usize, run: &Arc<str>, fence: Fence, elsewhere: i64, out: &mut Vec<Reply>) {
        let peer = Peer { name: self.onames.get(o).map_or("", String::as_str), epoch: 1 };
        let Some(a) = self.runners.get_mut(r) else { return };
        a.carried_r(peer, run, None, elsewhere, self.now, out);
        if a.start_r(peer, run, fence, self.now, out) == Some(Start::New) {
            // Its process is up: the runner's word that it started.
            a.spawned_r(peer, run, out);
            let dur = self.runs.get(run).map_or(1, |(_, d)| *d);
            self.started.insert(Arc::clone(run), (r, self.now));
            self.at(self.now + dur, Ev::End { r, o, run: Arc::clone(run) });
        }
    }

    /// Runs the fleet through its window, and what it measured.
    pub fn measure(mut self) -> Measured {
        for t in 0..self.spec.tenants.len() {
            for _ in 0..self.spec.tenants.get(t).map_or(0, |x| x.backlog) {
                self.ask(t);
            }
        }
        while let Some(((t, _), ev)) = self.q.pop_first() {
            if t > self.spec.until {
                break;
            }
            self.now = t;
            self.step(ev);
        }
        let until = self.spec.until;
        let open: Vec<(Arc<str>, (usize, i64))> = self.started.drain().collect();
        for (run, (r, from)) in open {
            let cpu = self.within(from, until);
            if let Some(&(t, _)) = self.runs.get(&run)
                && let Some(h) = self.held.get_mut(t)
            {
                *h += cpu;
            }
            if let Some(d) = self.delivered.get_mut(r) {
                *d += cpu;
            }
        }
        let all = (1u64 << self.spec.cpus.len().min(63)) - 1;
        let eligible: Vec<u64> = self.spec.tenants.iter().map(|t| if t.among.is_empty() { all } else { t.among.iter().fold(0, |m, r| m | (1 << r)) }).collect();
        let weights: Vec<u128> = self.spec.tenants.iter().map(|t| u128::from(t.weight)).collect();
        let shares = cmmf(&self.delivered, &eligible, &weights);
        let window = (self.spec.until - self.spec.warm) as f64;
        let cpus: u64 = self.spec.cpus.iter().sum();
        Measured {
            held: self.held.iter().map(|h| *h as f64 / window).collect(),
            share: shares.iter().map(|(n, d)| *n as f64 / (*d).max(1) as f64 / window).collect(),
            used: self.delivered.iter().sum::<u128>() as f64 / window / cpus.max(1) as f64,
        }
    }
}

/// A fleet of runners with `cpus`, its tenants each through one owner of their own (or `owners`
/// of them in turn), words delayed up to `delay` ms, measured from 20 s to 80 s.
pub(crate) fn fleet(cpus: &[u64], tenants: Vec<Tenant>, owners: usize, delay: i64) -> Spec {
    Spec { cpus: cpus.to_vec(), tenants, owners, delay, warm: 20_000, until: 80_000, seed: 7 }
}

/// A tenant that keeps enough runs waiting to wait at every runner it may use (two places each).
pub(crate) fn tenant(repo: &str, weight: PerMille, among: &[usize], mean: i64, median: bool, owners: &[usize]) -> Tenant {
    Tenant { repo: repo.into(), weight, among: among.to_vec(), backlog: 32, mean, median, owners: owners.to_vec() }
}

/// DRF's discrete bound (Ghodsi et al., NSDI 2011, Thm 7): giving room run by run to the tenant
/// with the least stays within one run of the continuous allocation. Each run here asks one CPU.
pub(crate) const ONE_RUN: f64 = 1.0;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::place::{Mutant, planted};

    fn shares(cap: &[u128], eligible: &[u64], weights: &[u128]) -> Vec<f64> {
        cmmf(cap, eligible, weights).iter().map(|(n, d)| *n as f64 / *d as f64).collect()
    }

    #[test]
    fn w05_r32_the_oracle_is_choosy_s_constrained_max_min_fair_share() {
        // One runner each, X held to the first: each gets one.
        assert_eq!(shares(&[1, 1], &[0b01, 0b11], &[1, 1]), vec![1.0, 1.0]);
        // X held to a runner of 4, Y anywhere among it and one of 1: they level at 2.5.
        assert_eq!(shares(&[4, 1], &[0b01, 0b11], &[1, 1]), vec![2.5, 2.5]);
        // X held to a runner of 1, Y with 3 more: X its 1, Y the rest.
        assert_eq!(shares(&[1, 3], &[0b01, 0b11], &[1, 1]), vec![1.0, 3.0]);
        // Weights: Y three times X, both on the same runner of 4.
        assert_eq!(shares(&[4], &[0b1, 0b1], &[1, 3]), vec![1.0, 3.0]);
        // A on 1 and 2, B on 2 and 3, C anywhere, runners of 2: 2 each.
        assert_eq!(shares(&[2, 2, 2], &[0b011, 0b110, 0b111], &[1, 1, 1]), vec![2.0, 2.0, 2.0]);
        // Two held to one runner of 2 and a third anywhere among it and two more: 1, 1 and 4.
        assert_eq!(shares(&[2, 2, 2], &[0b001, 0b001, 0b111], &[1, 1, 1]), vec![1.0, 1.0, 4.0]);
    }

    /// X can run only on the runner of 8; Y and Z anywhere, three more runners of 2 besides. The
    /// oracle levels all three at 14/3; a runner's own standing alone gives X a third of its runner.
    fn held_to_one() -> Spec {
        let t = |r, among: &[usize], o| tenant(r, 1000, among, 1000, true, &[o]);
        fleet(&[8, 2, 2, 2], vec![t("x", &[0], 0), t("y", &[], 1), t("z", &[], 2)], 3, 20)
    }

    fn check(name: &str, spec: Spec) -> Measured {
        let got = Fleet::new(spec).measure();
        println!("{name}: held {:?}, oracle {:?}, gap {:.2}, used {:.2}", got.held, got.share, got.gap(), got.used);
        got
    }

    #[test]
    fn w05_r32_a_tenant_held_to_one_runner_gets_its_share_there() {
        let got = check("held to one", held_to_one());
        assert!(got.gap() <= ONE_RUN, "within a run of the oracle: {got:?}");
        assert!(got.used > 0.9, "the fleet stays busy: {got:?}");
    }

    #[test]
    fn w05_r32_fair_share_holds_by_weight_through_several_owners_and_settled_usage() {
        // X (weight 1) only on the first runner of 4; Y (weight 3) anywhere, through two owners, its
        // asks without a median (charged as they end); runners of 4 and 4: X 2, Y 6.
        let spec = fleet(&[4, 4], vec![tenant("x", 1000, &[0], 800, true, &[0]), tenant("y", 3000, &[], 1200, false, &[0, 1])], 2, 20);
        let got = check("weights, owners, settled", spec);
        assert!(got.gap() <= ONE_RUN, "within a run of the oracle: {got:?}");
    }

    #[test]
    fn w05_r32_a_planted_mistake_in_carrying_or_using_usage_is_caught() {
        for m in [Mutant::LocalOnly, Mutant::DoubleDelta] {
            let got = planted(Some(m), || check(&format!("{m:?}"), held_to_one()));
            assert!(got.gap() > ONE_RUN, "{m:?} is caught: {got:?}");
        }
        // Without medians, usage elsewhere is known only as runs end: settling is all of it.
        let mut spec = held_to_one();
        for t in &mut spec.tenants {
            t.median = false;
        }
        let ok = check("no medians", spec.clone());
        assert!(ok.gap() <= ONE_RUN, "settled usage alone holds it: {ok:?}");
        let got = planted(Some(Mutant::NoSettle), || check("NoSettle", spec));
        assert!(got.gap() > ONE_RUN, "NoSettle is caught: {got:?}");
    }

    /// One runner of one CPU, `hold` on it: each `(run, repo, predicted)` asks a second after the
    /// one before; then each run ends `ran` ms after it starts, and the one offered next starts. The
    /// runs in the order they start.
    fn starts_in_order(gamma: Option<PerMille>, asks: &[(&str, &str, Ms)], ran: i64) -> Vec<String> {
        use super::super::msg::ToOwner;
        let size = Resources { cpu: Cpus::whole(1), mem: 1 << 30, ..Resources::default() };
        let settings = Settings { predicted_order: gamma, ..Settings::default() };
        let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
        let mut a = Admission::new(Init { me: "r".into(), epoch: 1, facts, labels: Vec::new(), sample: Sample { size, ..Sample::default() }, records: Records::new(), settings }, 0);
        let from = Peer { name: "o", epoch: 1 };
        let one = |run: &str, repo: &str| Ask { run: run.into(), needs: Needs { request: Resources { cpu: Cpus::whole(1), ..Resources::default() }, ..Needs::default() }, repo: repo.into(), actor: "a".into(), median: Some(1000), ..Ask::default() };
        let offered = |said: &[super::super::runner::Said]| said.iter().find_map(|(_, m)| if let ToOwner::Offer { run, fence, .. } = m { Some((run.clone(), *fence)) } else { None });
        let (mut said, mut now) = (Vec::new(), 0);
        let go = |a: &mut Admission, run: &str, fence: Fence, now: i64| {
            let mut s = Vec::new();
            a.take(from, run, fence, now, &mut s);
            let _ = a.start(from, run, fence, now, &mut s);
            a.spawned(from, run, &mut s);
        };
        a.probe(from, one("hold", "h"), Some(1000), now, &mut said);
        let Some((mut running, fence)) = offered(&said) else { return Vec::new() };
        go(&mut a, &running, fence, now);
        for (run, repo, p) in asks {
            now += 1000;
            a.probe(from, one(run, repo), Some(*p), now, &mut said);
        }
        let mut order = Vec::new();
        loop {
            now += ran;
            said.clear();
            a.ended(from, &running, true, now, &mut said);
            let Some((run, fence)) = offered(&said) else { break };
            go(&mut a, &run, fence, now);
            order.push(run.clone());
            running = run;
        }
        order
    }

    #[test]
    fn w05_r09_a_tenant_s_own_runs_go_shortest_predicted_first_and_none_is_passed_past_its_bound() {
        // A run predicted 10 s, then twelve predicted 1 s, a second apart. γ = 4: κ = (4 − 2)/2 = 1,
        // so a short one passes the long one only if it came within 1 × (10 − 1) s after it.
        let mut asks = vec![("long", "t", 10_000)];
        let names: Vec<String> = (1..=12).map(|i| format!("s{i}")).collect();
        asks.extend(names.iter().map(|n| (n.as_str(), "t", 1_000)));
        let arrival: Vec<String> = asks.iter().map(|(r, _, _)| (*r).to_owned()).collect();
        assert_eq!(starts_in_order(None, &asks, 500), arrival, "undeclared: arrival order, as before");
        let mut bounded: Vec<String> = names.iter().take(8).cloned().collect();
        bounded.push("long".into());
        bounded.extend(names.iter().skip(8).cloned());
        assert_eq!(starts_in_order(Some(4000), &asks, 500), bounded, "s1..s8 came within 9 s of it; s9, 9 s after, ties and goes after it");
        assert_eq!(starts_in_order(Some(2000), &asks, 500), arrival, "γ = 2: λ = 0, arrival order");
        let got = planted(Some(Mutant::Unbounded), || starts_in_order(Some(4000), &asks, 500));
        assert_ne!(got, bounded, "Unbounded is caught: {got:?}");
        assert_eq!(got.last().map(String::as_str), Some("long"), "by prediction alone, every short one passes it");
    }

    #[test]
    fn w05_r09_predicted_order_leaves_the_order_between_tenants_as_it_was() {
        // Tenants a and b, their runs predicted differently, charged alike (the same median, the
        // same time run): which tenant starts next is the same with predictions trusted or not; only
        // which of its own runs does changes.
        let p = [500, 4000, 1500, 3000, 800, 2500];
        let names: Vec<String> = (0..12).map(|i| format!("{}{i}", if i % 2 == 0 { "a" } else { "b" })).collect();
        let asks: Vec<(&str, &str, Ms)> = names.iter().enumerate().map(|(i, n)| (n.as_str(), if i % 2 == 0 { "a" } else { "b" }, p.get(i / 2).copied().unwrap_or(1000))).collect();
        let tenants = |order: &[String]| order.iter().map(|r| r.chars().next().unwrap_or(' ')).collect::<String>();
        let (before, after) = (starts_in_order(None, &asks, 700), starts_in_order(Some(6000), &asks, 700));
        assert_eq!(after.len(), names.len(), "{after:?}");
        assert_eq!(tenants(&after), tenants(&before), "between tenants, standing alone: {before:?} / {after:?}");
        assert_ne!(after, before, "within a tenant, its predictions: {after:?}");
    }
}
