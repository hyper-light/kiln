//! Learned wait weights against the papers (docs/lb.md "Learned wait weights"; LB08, W05-R23):
//! the kernel's integers decide as Lattanzi, Lavastida, Moseley and Vassilvitskii's Algorithm 1
//! (SODA 2020), with Aspnes, Azar, Fiat, Plotkin and Waarts's doubling for T (J. ACM 1997) and the
//! switch to the robust draw, written here as the papers state them, in floats. Each rule has its
//! planted mistake caught.

use super::model::Rng;
use super::weights::Weights;
use super::*;

/// Runs `f` and says whether it fails with mutant `m` planted (caught).
fn caught(m: Mutant, f: impl Fn() -> Result<(), String>) -> Result<(), String> {
    f()?;
    match with_mutant(Some(m), &f) {
        Err(_) => Ok(()),
        Ok(()) => Err(format!("{m:?} was not caught")),
    }
}

/// Algorithm 1 as the paper writes it, with T by Aspnes et al.'s doubling and the switch, in
/// floats. `near`: a comparison came within 10^-5 of its threshold, where the kernel's integers
/// (a unit a load's add, 2^-32 of T's start's) and these floats may round apart.
struct Reference {
    w: Vec<f64>,
    phase: Vec<f64>,
    epoch: Vec<f64>,
    kinds: Vec<(f64, f64)>,
    fleet: (f64, f64),
    t: f64,
    doubled: u32,
    halvings: u32,
    fell_back: Option<usize>,
    near: bool,
}

impl Reference {
    fn new(w: Vec<f64>) -> Reference {
        let n = w.len();
        Reference { w, phase: vec![0.0; n], epoch: vec![0.0; n], kinds: Vec::new(), fleet: (0.0, 0.0), t: 0.0, doubled: 0, halvings: 0, fell_back: None, near: false }
    }

    fn close(&mut self, a: f64, b: f64) {
        if (a - b).abs() <= 1e-5 * b.abs() {
            self.near = true;
        }
    }

    /// A job's lower bound on the optimum: its kind's sizes over the kind's CPUs, and the fleet's
    /// over its CPUs, in this epoch.
    fn shown(&mut self, kind: usize, size: f64, cpu: f64, fleet_cpu: f64) -> f64 {
        if self.kinds.len() <= kind {
            self.kinds.resize(kind + 1, (0.0, 0.0));
        }
        let of = |b: &mut (f64, f64), c: f64| {
            if b.1 != c {
                *b = (0.0, c);
            }
            b.0 += size;
            b.0 / c
        };
        let k = self.kinds.get_mut(kind).map_or(0.0, |b| of(b, cpu));
        let f = of(&mut self.fleet, fleet_cpu);
        k.max(f)
    }

    fn job(&mut self, kind: usize, size: f64, members: &[(usize, f64)], fleet_cpu: f64, m: u32) {
        let cpu: f64 = members.iter().map(|(_, c)| c).sum();
        let fleet_cpu = fleet_cpu.max(cpu);
        loop {
            let shown = self.shown(kind, size, cpu, fleet_cpu);
            if self.t == 0.0 {
                self.t = shown;
                break;
            }
            self.close(shown, self.t);
            if shown <= self.t {
                break;
            }
            // Aspnes et al.: the epoch failed, T doubles, the next begins with this job.
            self.t *= 2.0;
            self.doubled += 1;
            self.kinds.clear();
            self.fleet = (0.0, 0.0);
            self.fell_back = None;
            self.phase.iter_mut().for_each(|p| *p = 0.0);
            self.epoch.iter_mut().for_each(|e| *e = 0.0);
        }
        if self.fell_back.is_some() {
            return;
        }
        let total: f64 = members.iter().map(|(r, _)| self.w.get(*r).copied().unwrap_or(0.0)).sum();
        let log = f64::from(m.max(2)).log2().ceil();
        let (two_t, bound) = (2.0 * self.t, 2.0 * log * self.t);
        for &(r, c) in members {
            let w = self.w.get(r).copied().unwrap_or(0.0);
            let add = size * (w / total) / c;
            let (p, e) = (self.phase.get(r).copied().unwrap_or(0.0) + add, self.epoch.get(r).copied().unwrap_or(0.0) + add);
            self.close(e, bound);
            if self.fell_back.is_none() && e > bound {
                self.fell_back = Some(r);
            }
            self.close(p, two_t);
            let halve = p > two_t;
            if let Some(x) = self.epoch.get_mut(r) {
                *x = e;
            }
            if let Some(x) = self.phase.get_mut(r) {
                *x = if halve { 0.0 } else { p };
            }
            if halve && let Some(x) = self.w.get_mut(r) {
                *x = (*x / 2.0).floor().max(1.0);
                self.halvings += 1;
            }
        }
    }
}

/// How many instances each kind of decision was seen in, so a run that never halves, doubles or
/// falls back can't pass for one that agrees.
#[derive(Default)]
struct Seen {
    halved: u32,
    doubled: u32,
    fell_back: u32,
    skipped: u32,
}

/// One random instance: runners' CPUs, kinds (sets of runners), predictions, a run of jobs; the
/// kernel's weights and the reference's compared after every job.
fn instance(rng: &mut Rng, seen: &mut Seen) -> Result<(), String> {
    let m = 2 + rng.below(8) as usize;
    let cpus: Vec<u64> = (0..m).map(|_| 250 * (1 + rng.below(64))).collect();
    let kinds: Vec<Vec<usize>> = (0..1 + rng.below(4))
        .map(|_| {
            let k: Vec<usize> = (0..m).filter(|_| rng.below(3) != 0).collect();
            if k.is_empty() { vec![rng.below(m as u64) as usize] } else { k }
        })
        .collect();
    let mut kernel = Weights::new(true);
    let mut start = Vec::new();
    for (r, c) in cpus.iter().enumerate() {
        let w = if rng.below(2) == 0 { *c << rng.below(13) } else { *c };
        if w != *c {
            kernel.set(r as u32, w);
        }
        start.push(w as f64);
    }
    let mut reference = Reference::new(start);
    let fleet: u64 = cpus.iter().sum();
    let mut fell = false;
    for j in 0..1 + rng.below(80) {
        let kind = rng.below(kinds.len() as u64) as usize;
        let size = 1000 + rng.below(15_001);
        let members: Vec<(u32, u64)> = kinds.get(kind).ok_or("kind")?.iter().map(|r| (*r as u32, cpus.get(*r).copied().unwrap_or(1))).collect();
        let floats: Vec<(usize, f64)> = members.iter().map(|(r, c)| (*r as usize, *c as f64)).collect();
        kernel.waited(kind as u32, size, &members, fleet, m as u32);
        reference.job(kind, size as f64, &floats, fleet as f64, m as u32);
        if reference.near {
            seen.skipped += 1;
            return Ok(());
        }
        for (r, c) in cpus.iter().enumerate() {
            let (got, want) = (kernel.weight(r as u32, *c), reference.w.get(r).copied().unwrap_or(0.0) as u64);
            if got != want {
                return Err(format!("job {j}: runner {r}'s weight is {got}, Algorithm 1's {want}"));
            }
        }
        let got = kernel.fell_back.map(|f| f.runner.0 as usize);
        if got != reference.fell_back {
            return Err(format!("job {j}: fell back at {got:?}, Algorithm 1's switch at {:?}", reference.fell_back));
        }
        fell |= got.is_some();
        if kernel.t().1 != reference.doubled {
            return Err(format!("job {j}: T doubled {} times, Aspnes et al.'s {}", kernel.t().1, reference.doubled));
        }
    }
    seen.halved += u32::from(reference.halvings > 0);
    seen.fell_back += u32::from(fell);
    seen.doubled += u32::from(reference.doubled > 0);
    Ok(())
}

/// On 400 random instances (two to nine runners of a quarter to 16 CPUs, one to four kinds of
/// random members, predictions up to 4,096 times a runner's CPUs, up to 80 jobs of one to 16
/// CPUs), the kernel's weights, its switch and T's doublings are the papers', job by job.
fn as_the_papers() -> Result<(), String> {
    let mut rng = Rng(2020);
    let mut seen = Seen::default();
    let n = 400;
    for i in 0..n {
        instance(&mut rng, &mut seen).map_err(|e| format!("instance {i}: {e}"))?;
    }
    if seen.skipped * 10 > n || seen.halved < 40 || seen.doubled < 40 || seen.fell_back < 10 {
        return Err(format!("too few decisions to compare: {} skipped near a threshold, {} halved, {} doubled, {} fell back", seen.skipped, seen.halved, seen.doubled, seen.fell_back));
    }
    if !any_planted() {
        println!("{n} instances: {} stopped near a threshold; of the rest, {} halved a weight, {} doubled T, {} fell back", seen.skipped, seen.halved, seen.doubled, seen.fell_back);
    }
    Ok(())
}

#[test]
fn lb08_w05_r23_learned_weights_decide_as_algorithm_1_with_t_doubled() -> Result<(), String> {
    for m in [Mutant::NoLearn, Mutant::NoFallback, Mutant::DrawnOnly, Mutant::KindBlind, Mutant::NoDoubling, Mutant::NoRestart] {
        caught(m, as_the_papers)?;
    }
    Ok(())
}

/// The case the earlier rule got wrong (T the fleet's balanced load): runner 0 alone runs one kind,
/// which gets three runs for each one of a kind all eight runners run. Its load is the kind's,
/// which no assignment avoids, and what the jobs show of the optimum says so: its weight stays
/// and the predictions aren't dropped. Blind to kinds, T is a quarter of that load and runner 0
/// looks overloaded past the switch.
fn forced() -> Result<(), String> {
    let cpus = [1000u64; 8];
    let all: Vec<(u32, u64)> = (0..8).map(|r| (r, 1000)).collect();
    let mut w = Weights::new(true);
    for _ in 0..200 {
        for _ in 0..3 {
            w.waited(0, 1000, &[(0, 1000)], cpus.iter().sum(), 8);
        }
        w.waited(1, 1000, &all, cpus.iter().sum(), 8);
    }
    if w.weight(0, 1000) != 1000 || w.fell_back.is_some() {
        return Err(format!("runner 0's weight is {}, fell back {:?}", w.weight(0, 1000), w.fell_back));
    }
    Ok(())
}

#[test]
fn lb08_a_runner_that_alone_runs_a_kind_keeps_its_weight_and_the_predictions() -> Result<(), String> {
    caught(Mutant::KindBlind, forced)
}

/// Algorithm 1 on a case small enough to follow: sixteen runners of a CPU, runner 0 predicted at
/// four. The first job's T is 1/16 (one CPU over sixteen); runner 0's share, 4/19, is 3.4 T, past
/// 2T, so its weight halves to two; under the switch's 8T. From the second job each epoch is
/// about as long as all before it, and runner 0's share, 2/17 of each job, never passes 2T in
/// one: its weight stays at two.
fn halves() -> Result<(), String> {
    let all: Vec<(u32, u64)> = (0..16).map(|r| (r, 1000)).collect();
    let mut w = Weights::new(true);
    w.set(0, 4000);
    w.waited(0, 1000, &all, 16_000, 16);
    if w.weight(0, 1000) != 2000 || w.fell_back.is_some() {
        return Err(format!("after one job runner 0 weighs {}, fell back {:?}", w.weight(0, 1000), w.fell_back));
    }
    for _ in 0..200 {
        w.waited(0, 1000, &all, 16_000, 16);
    }
    if w.weight(0, 1000) != 2000 || w.t().1 != 7 {
        return Err(format!("after 201 jobs runner 0 weighs {}, T doubled {} times", w.weight(0, 1000), w.t().1));
    }
    Ok(())
}

#[test]
fn lb08_algorithm_1_halves_a_weight_whose_phase_load_passed_2t() -> Result<(), String> {
    caught(Mutant::NoLearn, halves)
}

/// Predictions that aren't helping (one runner of sixteen weighted 2^20 times the rest) put more
/// than 2⌈log2 m⌉T on it with the first job: the switch, saying which runner and why. A doubling
/// begins an epoch: the weights as learned are tried again.
fn falls_back() -> Result<(), String> {
    let all: Vec<(u32, u64)> = (0..16).map(|r| (r, 1000)).collect();
    let mut w = Weights::new(true);
    w.set(0, 1000 << 20);
    w.waited(0, 1000, &all, 16_000, 16);
    match w.fell_back {
        Some(f) if f.runner == RunnerId(0) && f.load > f.bound && f.bound == 8 * w.t().0 => {}
        other => return Err(format!("the predictions weren't dropped: {other:?}")),
    }
    w.waited(0, 1000, &all, 16_000, 16);
    if w.t().1 != 1 || w.fell_back.is_some_and(|f| f.runner != RunnerId(0)) {
        return Err(format!("after a doubling: T doubled {} times, fell back {:?}", w.t().1, w.fell_back));
    }
    Ok(())
}

#[test]
fn lb08_predictions_that_dont_help_fall_back_and_say_why() -> Result<(), String> {
    caught(Mutant::NoFallback, falls_back)
}

/// The owner draws its places as it did before the weights once they fell back (tokens off: the
/// members nearest to fitting), and in proportion to them before.
#[test]
fn lb08_once_the_weights_fall_back_places_are_the_nearest_to_fitting() -> Result<(), String> {
    let places = |weights: bool| -> Result<(Vec<Vec<u32>>, bool), String> {
        let mut o = Owner::new(OwnerId(0));
        if weights {
            o.set_weights(true, 11);
        }
        let mut out = Out::default();
        let none = Word { epoch: 1, whole: false, cpu: 0, rest: [0; 3], ..Word::default() };
        for r in 0..16 {
            o.join(RunnerId(r), [CPU, 0, 0, 0], none, &mut out);
        }
        let s = o.sig();
        for r in 0..16 {
            o.add(s, RunnerId(r), &mut out);
        }
        if weights {
            o.set_weight(RunnerId(1), CPU << 20);
        }
        let mut got = Vec::new();
        let mut fell = false;
        for _ in 0..2 {
            let mut out = Out::default();
            o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
            got.push(out.to.iter().filter(|(_, m)| matches!(m, ToRunner::Wait(_))).map(|(r, _)| r.0).collect());
            fell |= o.fell_back().is_some();
        }
        Ok((got, fell))
    };
    let ((with, fell), (without, _)) = (places(true)?, places(false)?);
    // The first ask is drawn by the weights (runner 1 first), and fell back; the second waits
    // where it would without them.
    if !fell || with.get(1) != without.get(1) || with.first().and_then(|p| p.first()) != Some(&1) {
        return Err(format!("with weights that fell back {with:?}, without {without:?}"));
    }
    Ok(())
}
