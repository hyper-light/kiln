//! Expected finish (docs/lb.md "Rank", "Orders", "Per-ask terms"), W05-R23 and W24-R18: the
//! kernel's choices against a brute force over small fleets with every term, each term's planted
//! bug caught; narrow asks' order; a run waiting for a warm runner alone; cordon; measures
//! reaching the owner.

use super::model::Rng;
use super::order::{class_hi, class_of};
use super::owner::terms::{Rates, exp_neg_q32, mul_q32, stability};
use super::tests::{caught, fleet, settle};
use super::*;

const C8: Amounts = [8 * CPU, 32 << 30, 400 << 30, 8];

/// One runner of a fleet as a case draws it.
#[derive(Debug, Clone, Copy, Default)]
struct Shape {
    speed: u32,
    slowness: u32,
    disk: u64,
    net: u64,
    link: Link,
    /// CPU in use outside the ledger (its room is the rest).
    used: u64,
}

/// A runner's key multiplier, from the spec: SCALE times speed over slowness (1024 each when not
/// measured), at most what a u32 holds, at least 1.
fn scale(x: &Shape) -> u64 {
    let s = u64::from(if x.speed == 0 { SPEED_ONE } else { x.speed });
    let w = u64::from(if x.slowness == 0 { SPEED_ONE } else { x.slowness });
    (u64::from(SCALE) * s / w).clamp(1, u64::from(u32::MAX))
}

/// The highest bucket: the most core-equivalents any runner's whole budget has.
fn last_of(sh: &[Shape]) -> u64 {
    sh.iter().map(|x| ((u128::from(C8[0]) * u128::from(scale(x))) >> 40) as u64).max().unwrap_or(0).max(C8[0] / CPU)
}

/// A key, from the spec: 0 without a whole core free, else core-equivalents under contention, at
/// least 1 and at most `last`.
fn key_of(view: u64, x: &Shape, last: u64) -> u64 {
    if view < CPU { 0 } else { (((u128::from(view) * u128::from(scale(x))) >> 40) as u64).clamp(1, last) }
}

/// A rate: unmeasured (0) for at most one runner a fleet, the rest in classes of their own (or,
/// `shared`, one of three values a fleet, so classes hold several runners).
fn rate(rng: &mut Rng, lo: u64, hi: u64, zero_ok: &mut bool, taken: &mut Vec<u64>, shared: bool) -> u64 {
    if *zero_ok && rng.below(4) == 0 {
        *zero_ok = false;
        return 0;
    }
    if shared {
        while taken.len() < 3 {
            taken.push(lo + rng.below(hi - lo));
        }
        return taken.get(rng.below(3) as usize).copied().unwrap_or(lo);
    }
    loop {
        let r = lo + rng.below(hi - lo);
        let c = u64::from(class_of(r));
        if !taken.contains(&c) {
            taken.push(c);
            return r;
        }
    }
}

/// A fleet measured or not as the case draws: with `shared` false, every key and every class in
/// every order a runner's own (so each order's best is one runner, whatever the turns); with it,
/// rates and room from a few values, so classes and buckets hold several runners.
fn shapes(rng: &mut Rng, n: usize, shared: bool) -> Vec<Shape> {
    loop {
        let cpu_measured = rng.below(4) != 0;
        let (mut z_disk, mut z_net, mut z_link) = (true, true, true);
        let (mut t_disk, mut t_net, mut t_link) = (Vec::new(), Vec::new(), Vec::new());
        let mut out = Vec::new();
        for _ in 0..n {
            let (speed, slowness) = match (cpu_measured, shared) {
                (false, _) => (0, 0),
                (true, false) => (512 + rng.below(1537) as u32, 1024 + rng.below(3073) as u32),
                (true, true) => (1024 + 512 * rng.below(3) as u32, 1024 + 1024 * rng.below(2) as u32),
            };
            let used = if shared { 2 * rng.below(5) * CPU } else { (rng.below(9) * CPU).saturating_sub(if rng.below(3) == 0 { rng.below(CPU) } else { 0 }) };
            let disk = rate(rng, 50 << 20, 4 << 30, &mut z_disk, &mut t_disk, shared);
            let net = rate(rng, 10 << 20, 2 << 30, &mut z_net, &mut t_net, shared);
            let lr = rate(rng, 1 << 20, 1 << 30, &mut z_link, &mut t_link, shared);
            let link = Link { rate: lr, latency_ns: rng.below(200_000_000), between_drops_ns: if rng.below(3) == 0 { 0 } else { 1 + rng.below(3_600_000_000_000) }, outage_ns: rng.below(60_000_000_000) };
            out.push(Shape { speed, slowness, disk, net, link, used });
        }
        if shared {
            return out;
        }
        let last = last_of(&out);
        let keys: Vec<u64> = out.iter().map(|x| key_of(8 * CPU - x.used, x, last)).collect();
        let classes: Vec<u64> = out.iter().map(|x| u64::from(class_of(scale(x)))).collect();
        let unique = |v: &[u64]| v.iter().enumerate().all(|(i, a)| v.iter().skip(i + 1).all(|b| a != b));
        if unique(&keys) && (!cpu_measured || unique(&classes)) {
            return out;
        }
    }
}

/// The fleet built and every measure heard by its owner.
fn build(sh: &[Shape]) -> (Owner, Vec<Runner>, SigId) {
    let (mut o, mut rs, s) = fleet(u32::try_from(sh.len()).unwrap_or(0), C8);
    let mut out = Out::default();
    let mut rout = RunnerOut::new();
    for (i, (r, x)) in rs.iter_mut().zip(sh).enumerate() {
        r.measure(x.speed, x.slowness);
        r.measure_rates(x.disk, x.net);
        r.outside([x.used, 0, 0, 0], &mut rout);
        o.set_link(RunnerId(u32::try_from(i).unwrap_or(0)), x.link);
    }
    for r in rs.iter_mut() {
        r.drain(&mut rout);
    }
    for (_, m) in rout {
        o.hear(m, &mut out);
    }
    settle(&mut o, &mut rs, &mut out);
    (o, rs, s)
}

/// What the brute force says an ask does: start on a runner, wait there alone, or wait as any
/// run that finds no room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Start(u32),
    WaitAt(u32),
    Waits,
}

/// The expected finish, from docs/lb.md's terms (the spec, written again, on the kernel's
/// multipliers: [`Rates`]).
#[allow(clippy::too_many_arguments)]
fn finish(x: &Shape, view: u64, t: &Terms, cpu: u64, p: u64, wide: bool, on: (bool, bool, bool), wait: u64, saved: u64, missing: u64) -> u128 {
    let q = Rates::of(x.disk, x.net, &x.link);
    let k = if wide { p.min(view.max(cpu)) } else { p }.max(1);
    // W / (cores x speed / slowness), in the key multiplier's units.
    let t_cpu = u128::from(t.cpu_ns.saturating_sub(saved)) * u128::from(CPU) * u128::from(SCALE) / (u128::from(k) * u128::from(scale(x)));
    let t_io = if on.0 { mul_q32(u128::from(t.io_bytes), q.disk) } else { 0 };
    let t_net = if on.1 { mul_q32(u128::from(t.net_bytes), q.net) } else { 0 };
    let t_move = if on.2 { mul_q32(u128::from(missing), q.link) } else { 0 };
    let run = t_cpu + t_io + t_net;
    let mut e = u128::from(wait) + t_move + run;
    if t.interactive && on.2 {
        e += u128::from(x.link.latency_ns) + stability(run, q.drops, x.link.outage_ns);
    }
    e
}

/// The brute force: every option's expected finish from the terms as written above, each runner
/// with a whole core free and room as any runner is, each listed runner with room (its saving, its
/// missing bytes) or, without room and with a wait, waiting for it; a term compared only where
/// every runner with a core free and every listed option has it measured. Each runner's soonest
/// start and wait, and the soonest of all (`None`: no option).
struct Options {
    start: Vec<Option<u128>>,
    wait: Vec<Option<u128>>,
    best: Option<u128>,
}

fn options(sh: &[Shape], need: Amounts, t: &Terms) -> Options {
    let cpu = need[0];
    let p = if t.parallel == 0 { cpu } else { t.parallel };
    let wide = p > cpu;
    let view = |x: &Shape| 8 * CPU - x.used;
    let measured_any = sh.iter().any(|x| scale(x) != u64::from(SCALE) || x.disk != 0 || x.net != 0 || x.link.rate != 0);
    let listed: Vec<(usize, u64, u64, u64)> = t
        .warm
        .iter()
        .filter_map(|w| {
            let i = w.runner.0 as usize;
            let x = sh.get(i)?;
            let wait = if view(x) >= cpu { 0 } else if w.wait_ns != 0 { w.wait_ns } else { return None };
            Some((i, wait, w.saved_ns, w.missing))
        })
        .collect();
    let all = |f: &dyn Fn(&Shape) -> u64| {
        measured_any && sh.iter().filter(|x| view(x) >= CPU).all(|x| class_of(f(x)) != 0) && listed.iter().all(|l| sh.get(l.0).is_some_and(|x| class_of(f(x)) != 0))
    };
    let on = (all(&|x| x.disk), all(&|x| x.net), all(&|x| x.link.rate));
    let mut o = Options { start: vec![None; sh.len()], wait: vec![None; sh.len()], best: None };
    let put = |v: &mut Vec<Option<u128>>, i: usize, e: u128| {
        if let Some(x) = v.get_mut(i) {
            *x = Some(x.map_or(e, |y| y.min(e)));
        }
    };
    for (i, x) in sh.iter().enumerate() {
        if view(x) >= CPU && view(x) >= cpu {
            put(&mut o.start, i, finish(x, view(x), t, cpu, p, wide, on, 0, 0, t.bytes));
        }
    }
    for &(i, wait, saved, missing) in &listed {
        let Some(x) = sh.get(i) else { continue };
        let e = finish(x, view(x), t, cpu, p, wide, on, wait, saved, missing);
        if wait == 0 { put(&mut o.start, i, e) } else { put(&mut o.wait, i, e) }
    }
    o.best = o.start.iter().chain(&o.wait).flatten().copied().min();
    o
}

/// Whether `e` is within the bound of the soonest, `best`: e <= best x (1 + 1/64).
fn within(e: u128, best: u128) -> bool {
    e.saturating_mul(64) <= best.saturating_mul(65)
}

/// The kernel's choice for an ask, as the brute force says it.
fn decide(o: &mut Owner, s: SigId, need: Amounts, t: &Terms) -> Result<Choice, String> {
    let mut out = Out::default();
    let a = o.ask_with(s, need, t, &mut out).map_err(|e| format!("refused: {e:?}"))?;
    Ok(match a.send {
        Some((r, ToRunner::Start(_))) => Choice::Start(r.0),
        Some(other) => return Err(format!("sent {other:?}")),
        None => match out.to.as_slice() {
            [(r, ToRunner::Wait(_))] => Choice::WaitAt(r.0),
            _ => Choice::Waits,
        },
    })
}

/// Draws an ask's terms: each term on or off, a warm list of up to four.
fn terms(rng: &mut Rng, n: u64, warm: &mut Vec<Warm>) -> (Amounts, Terms<'static>, [bool; 6]) {
    let cpu = (1 + rng.below(3)) * CPU;
    let on: [bool; 6] = std::array::from_fn(|_| rng.below(2) == 0);
    let [cpu_t, io, net, bytes, inter, par] = on;
    warm.clear();
    if rng.below(2) == 0 {
        for _ in 0..rng.below(5) {
            let wait = if rng.below(2) == 0 { 1 + rng.below(600_000_000_000) } else { 0 };
            warm.push(Warm { runner: RunnerId(rng.below(n) as u32), saved_ns: rng.below(4_000_000_000_000), missing: rng.below(8 << 30), wait_ns: wait });
        }
    }
    let t = Terms {
        parallel: if par { cpu + rng.below(8 * CPU) } else { 0 },
        cpu_ns: if cpu_t { rng.below(8_000_000_000_000) } else { 0 },
        io_bytes: if io { rng.below(64 << 30) } else { 0 },
        net_bytes: if net { rng.below(16 << 30) } else { 0 },
        bytes: if bytes { rng.below(16 << 30) } else { 0 },
        interactive: inter,
        warm: &[],
    };
    ([cpu, 0, 0, 0], t, on)
}

/// `cases` random fleets of 2 to 6 runners (`shared`: 2 to 12, classes and buckets shared), one
/// ask each, every term drawn on or off: the kernel's choice is an option the brute force has,
/// and its finish is within 1 + 1/64 of the soonest; none only when no option is. Where one term
/// alone bears, every rate measured and the classes each a runner's own, it is the soonest. The
/// kernel's finish over the soonest for the asks with several terms, and the members the
/// threshold read over the fleet's size.
fn oracle_cases(seed: u64, cases: u32, shared: bool) -> Result<(Vec<f64>, Vec<f64>), String> {
    let mut rng = Rng(seed);
    let mut warm = Vec::new();
    let (mut ratios, mut reads) = (Vec::new(), Vec::new());
    for case in 0..cases {
        let n = 2 + rng.below(if shared { 11 } else { 5 }) as usize;
        let sh = shapes(&mut rng, n, shared);
        let (mut o, _rs, s) = build(&sh);
        let (need, t0, on) = terms(&mut rng, n as u64, &mut warm);
        let t = Terms { warm: &warm, ..t0 };
        let opts = options(&sh, need, &t);
        let got = decide(&mut o, s, need, &t)?;
        let e = match got {
            Choice::Start(i) => opts.start.get(i as usize).copied().flatten(),
            Choice::WaitAt(i) => opts.wait.get(i as usize).copied().flatten(),
            Choice::Waits => None,
        };
        let fail = |what: &str| Err(format!("case {case}: {what}: kernel {got:?} ({e:?}), the soonest {:?}\nshapes {sh:?}\nneed {need:?} terms {t:?}", opts.best));
        let Some(best) = opts.best else {
            if got == Choice::Waits {
                continue;
            }
            return fail("no option, and the kernel chose one");
        };
        let Some(e) = e else { return fail("not an option") };
        if !within(e, best) {
            return fail("past the bound");
        }
        let single = [on[0], on[1], on[2], on[3]].iter().filter(|b| **b).count() == 1 && !on[4] && warm.is_empty();
        let measured = sh.iter().all(|x| x.disk != 0 && x.net != 0 && x.link.rate != 0);
        if !shared && single && measured && !on[0] && e != best {
            return fail("one term, not the soonest");
        }
        if !single && best > 0 {
            ratios.push(e as f64 / best as f64);
            reads.push(o.counts.read as f64 / n as f64);
        }
    }
    Ok((ratios, reads))
}

/// W24-R18, W05-R23: the kernel's choice with several terms is within 1 + 1/64 of the soonest
/// finish of every option, on fleets whose classes are each a runner's own and on fleets whose
/// classes hold several (docs/lb.md "Per-ask terms": the threshold, Fagin et al. 2003).
#[test]
fn w24_r18_expected_finish_is_within_1_64_of_the_soonest() -> Result<(), String> {
    for (shared, seed, cases) in [(false, 0x5eed, 20_000), (true, 0x5ead, 20_000)] {
        let (mut r, reads) = oracle_cases(seed, cases, shared)?;
        r.sort_by(f64::total_cmp);
        let at = |q: usize| r.get(q * r.len().saturating_sub(1) / 100).copied().unwrap_or(1.0);
        let optimal = r.iter().filter(|x| **x <= 1.0).count();
        let mean = reads.iter().sum::<f64>() / reads.len().max(1) as f64;
        println!("several terms ({}): the choice is the soonest of all in {optimal} of {}; its finish over the soonest: median {:.4}, p99 {:.4}, worst {:.4} (bound {:.4}); members read over the fleet's size: mean {mean:.2}", if shared { "classes shared" } else { "classes each a runner's own" }, r.len(), at(50), at(99), at(100), 65.0 / 64.0);
    }
    Ok(())
}

/// A shared owner's draw ([`Owner::set_spread`]) keeps the bound: owners with the same fleet and
/// view drawing with other seeds choose differently, and each choice is within the bound.
#[test]
fn w24_r18_a_shared_owners_draw_keeps_the_bound() -> Result<(), String> {
    let mut rng = Rng(0x5b4e);
    let mut warm = Vec::new();
    let (mut asks, mut differ) = (0u32, 0u32);
    for case in 0..2000 {
        let n = 4 + rng.below(9) as usize;
        let sh = shapes(&mut rng, n, true);
        let (need, t0, _) = terms(&mut rng, n as u64, &mut warm);
        let t = Terms { warm: &warm, ..t0 };
        let opts = options(&sh, need, &t);
        let Some(best) = opts.best else { continue };
        let mut picks = Vec::new();
        for seed in [1u64, 2] {
            let (mut o, _rs, s) = build(&sh);
            // Seven others deciding within a word's second: eight candidates.
            o.set_spread(7, 1_000_000_000, seed);
            let got = decide(&mut o, s, need, &t)?;
            let e = match got {
                Choice::Start(i) => opts.start.get(i as usize).copied().flatten(),
                Choice::WaitAt(i) => opts.wait.get(i as usize).copied().flatten(),
                Choice::Waits => None,
            };
            if !e.is_some_and(|e| within(e, best)) {
                return Err(format!("case {case}: a drawn choice {got:?} ({e:?}) past the bound of {best}\nshapes {sh:?}\nterms {t:?}"));
            }
            picks.push(got);
        }
        asks += 1;
        differ += u32::from(picks.first() != picks.get(1));
    }
    println!("drawn: {differ} of {asks} asks chose differently by seed");
    if differ == 0 { Err("two seeds never chose differently".into()) } else { Ok(()) }
}

/// The threshold's reads on the bench's `--terms` fleet (benches/lb.rs, `kiln_terms_impl`): disk
/// and network rates in 7 classes, link rates in 5, latencies in 3, drops in 4, and the bench's
/// ask (300 s of CPU, 8 GiB of I/O, 1 GiB fetched, 2 GiB to move, interactive), with and without
/// its list of four warm runners, runs ending so the fleet stays half full: members read an ask.
#[test]
fn w24_r18_threshold_reads_on_the_bench_fleet() -> Result<(), String> {
    for n in [100u32, 1000] {
        for listed in [false, true] {
            let (mut o, mut rs, s) = fleet(n, C8);
            let mut out = Out::default();
            let mut rout = RunnerOut::new();
            for (i, r) in (0u64..).zip(rs.iter_mut()) {
                r.measure_rates((i % 7 + 1) * (100 << 20), (i % 7 + 1) * (20 << 20));
                r.drain(&mut rout);
                o.set_link(RunnerId(i as u32), Link { rate: (i % 5 + 1) * (50 << 20), latency_ns: (i % 3 + 1) * 1_000_000, between_drops_ns: (i % 4 + 1) * 3_600_000_000_000, outage_ns: 20_000_000_000 });
            }
            for (_, m) in std::mem::take(&mut rout) {
                o.hear(m, &mut out);
            }
            settle(&mut o, &mut rs, &mut out);
            let mut rng = Rng(u64::from(n) + u64::from(listed));
            let (mut reads, mut running) = (Vec::new(), 0u64);
            for _ in 0..2000 {
                let warm: [Warm; 4] = std::array::from_fn(|j| Warm { runner: RunnerId(rng.below(u64::from(n)) as u32), saved_ns: 20_000_000_000 * (j as u64 + 1), missing: 1 << 28, wait_ns: 0 });
                let t = Terms { cpu_ns: 300_000_000_000, io_bytes: 8 << 30, net_bytes: 1 << 30, bytes: 2 << 30, interactive: true, warm: if listed { &warm } else { &[] }, ..Terms::default() };
                let before = o.counts.read;
                let a = o.ask_with(s, [CPU, 0, 0, 0], &t, &mut out).map_err(|e| format!("{e:?}"))?;
                reads.push(o.counts.read - before);
                if let Some(m) = a.send {
                    out.to.push(m);
                    running += 1;
                }
                settle(&mut o, &mut rs, &mut out);
                // Half full: a run ends for each past half the fleet's cores.
                while running > u64::from(n) * 4 {
                    let i = rng.below(u64::from(n)) as usize;
                    let Some(r) = rs.get_mut(i) else { break };
                    let Some(&(_, _, at, _)) = r.admitted().first() else { continue };
                    if let Some((_, m)) = r.end(at, &mut rout) {
                        o.hear(m, &mut out);
                    }
                    running -= 1;
                    settle(&mut o, &mut rs, &mut out);
                }
            }
            reads.sort_unstable();
            let at = |q: usize| reads.get(q * reads.len().saturating_sub(1) / 100).copied().unwrap_or(0);
            println!("bench fleet, {n} runners, {}: members the threshold read an ask: median {}, p99 {}, most {}", if listed { "four listed" } else { "no list" }, at(50), at(99), at(100));
        }
    }
    Ok(())
}

#[test]
fn w24_r18_each_terms_planted_bug_is_caught() -> Result<(), String> {
    for m in [Mutant::NoSlowness, Mutant::NoWarm, Mutant::NoTransfer, Mutant::NoDisk, Mutant::NoNet, Mutant::NoLatency, Mutant::NoStability, Mutant::NoQueueWait, Mutant::OneCandidate] {
        caught(m, || oracle_cases(0xb0b, 1500, false).map(|_| ()))?;
    }
    Ok(())
}

/// The plain path's choice by the spec (docs/lb.md "Rank", "Orders"), as the rank the kernel's
/// choice must have (members of one class, or one bucket, are equals the turns order): a narrow
/// ask on measured runners goes to the fastest key multiplier's class among the members with a
/// whole core free and room, else (a need below a core) to the most core-equivalents with room; a
/// wide one, or any on an unmeasured fleet, to the most core-equivalents with room. `views`: each
/// runner's CPU free for a new run. `None`: no member has room.
fn plain_rank(sh: &[Shape], views: &[u64], need: u64, wide: bool) -> Option<(bool, u64)> {
    let last = last_of(sh);
    let ordered = sh.iter().any(|x| scale(x) != u64::from(SCALE));
    let room: Vec<(&Shape, u64)> = sh.iter().zip(views.iter().copied()).filter(|(_, v)| *v >= need).collect();
    if ordered && !wide {
        let class = room.iter().filter(|(_, v)| *v >= CPU).map(|(x, _)| u64::from(class_of(scale(x)))).max();
        if let Some(c) = class {
            return Some((true, c));
        }
    }
    room.iter().map(|(x, v)| key_of(*v, x, last)).max().map(|k| (false, k))
}

/// `cases` random fleets, measured as the case draws (per-core speed and slowness, measured once
/// and then again, so every key is folded anew; disk, network and link rates, which must not
/// matter to a plain ask), each a run of plain asks, narrow or wide, of a whole CPU or more or of
/// half a core, with runs ending between (keys go to and from 0): each choice is the brute
/// force's, and with `fast`, a need of at most a core while a member has one free is decided by
/// the fast entry ([`Owner::ask_fast`]) with nothing walked.
fn plain_cases(seed: u64, cases: u32, fast: bool) -> Result<(), String> {
    let mut rng = Rng(seed);
    let (mut ordered, mut asks, mut by_fast, mut narrow) = (0u32, 0u32, 0u32, 0u32);
    for case in 0..cases {
        let n = 2 + rng.below(7) as usize;
        let cpu_measured = rng.below(4) != 0;
        let sh: Vec<Shape> = (0..n)
            .map(|_| {
                let measured = cpu_measured && rng.below(5) != 0;
                let (speed, slowness) = if measured { (256 + rng.below(3841) as u32, if rng.below(4) == 0 { 0 } else { 1024 + rng.below(7169) as u32 }) } else { (0, 0) };
                let used = (rng.below(9) * CPU).saturating_sub(if rng.below(3) == 0 { rng.below(CPU) } else { 0 });
                let rate = |rng: &mut Rng, hi: u64| if rng.below(2) == 0 { 0 } else { 1 + rng.below(hi) };
                let link = Link { rate: rate(&mut rng, 1 << 30), ..Link::default() };
                Shape { speed, slowness, disk: rate(&mut rng, 4 << 30), net: rate(&mut rng, 2 << 30), link, used }
            })
            .collect();
        ordered += u32::from(sh.iter().any(|x| scale(x) != u64::from(SCALE)));
        // Built on other rates first, then measured again with the case's own.
        let first: Vec<Shape> = sh.iter().map(|x| Shape { speed: x.speed.saturating_add(u32::from(x.speed != 0) * 300), slowness: x.slowness / 2, ..*x }).collect();
        let (mut o, mut rs, s) = build(&first);
        let mut out = Out::default();
        let mut rout = RunnerOut::new();
        for (r, x) in rs.iter_mut().zip(&sh) {
            r.measure(x.speed, x.slowness);
            r.drain(&mut rout);
        }
        for (_, m) in std::mem::take(&mut rout) {
            o.hear(m, &mut out);
        }
        settle(&mut o, &mut rs, &mut out);
        for step in 0..1 + rng.below(16) {
            // A run ends now and then: its runner's key rises, from 0 too.
            if rng.below(3) == 0
                && let Some(r) = rs.get_mut(rng.below(n as u64) as usize)
                && let Some(&(_, _, at, _)) = r.admitted().first()
            {
                if let Some((_, m)) = r.end(at, &mut rout) {
                    o.hear(m, &mut out);
                }
                settle(&mut o, &mut rs, &mut out);
            }
            let views: Vec<u64> = rs.iter().map(|r| r.word().cpu).collect();
            let cpu = match rng.below(5) {
                0 => CPU / 2,
                k => k.min(3) * CPU,
            };
            let wide = rng.below(3) == 0;
            let Some((by_class, rank)) = plain_rank(&sh, &views, cpu, wide) else { continue };
            asks += 1;
            narrow += u32::from(!wide);
            let t = Terms { parallel: if wide { cpu + CPU + rng.below(8 * CPU) } else { 0 }, ..Terms::default() };
            let walked = o.counts.walked;
            let got = if wide {
                o.ask_with(s, [cpu, 0, 0, 0], &t, &mut out).map_err(|e| format!("case {case}: refused {e:?}"))?.send
            } else {
                match o.ask_fast(s, u32::try_from(cpu).unwrap_or(u32::MAX)) {
                    Some((r, run, epoch)) => {
                        if o.counts.walked != walked {
                            return Err(format!("case {case} step {step}: the fast entry walked"));
                        }
                        by_fast += 1;
                        Some((RunnerId(r), ToRunner::Start(Start { owner: OwnerId(0), run, epoch, need: [cpu, 0, 0, 0] })))
                    }
                    None => {
                        if fast && cpu <= CPU && views.iter().any(|v| *v >= CPU) {
                            return Err(format!("case {case} step {step}: a plain ask of {cpu} with a whole core free went off the fast path\nshapes {sh:?}\nviews {views:?}"));
                        }
                        o.ask(s, [cpu, 0, 0, 0], &mut out).map_err(|e| format!("case {case}: refused {e:?}"))?.send
                    }
                }
            };
            let Some((r, m)) = got else { return Err(format!("case {case} step {step}: a plain ask of {cpu} waits, with room at {views:?}")) };
            let x = sh.get(r.0 as usize).ok_or("runner")?;
            let v = views.get(r.0 as usize).copied().unwrap_or(0);
            let have = if by_class { u64::from(class_of(scale(x))) } else { key_of(v, x, last_of(&sh)) };
            if v < cpu || have != rank {
                return Err(format!("case {case} step {step}: {} ask of {cpu} went to {} ({have}), the brute force's rank is {rank} (by class: {by_class})\nshapes {sh:?}\nviews {views:?}", if wide { "a wide" } else { "a narrow" }, r.0));
            }
            out.to.push((r, m));
            settle(&mut o, &mut rs, &mut out);
        }
    }
    if fast {
        println!("plain asks: {cases} fleets, {ordered} with keys folded from measured rates; {asks} asks, {narrow} narrow, {by_fast} decided by the fast entry");
    }
    Ok(())
}

/// W05-R23, W05-R26: a plain ask on measured runners is decided in O(1) again, as the brute force
/// decides it: speed and slowness folded into each runner's key when its word says them, the
/// narrow order one lookup, the index (core-equivalents under contention) one lookup; disk,
/// network and link rates measured don't take plain asks off the fast path.
#[test]
fn w05_r26_plain_asks_on_measured_runners_take_the_fast_path() -> Result<(), String> {
    plain_cases(0xfa57, 4000, true)
}

/// The folded key keeping stale rates, and the narrow order not following a key to or from 0, are
/// caught by the plain path's brute force.
#[test]
fn w05_r26_a_stale_key_or_gate_is_caught() -> Result<(), String> {
    for m in [Mutant::StaleKey, Mutant::NoGate] {
        caught(m, || plain_cases(0x57a1e, 1000, false))?;
    }
    Ok(())
}

/// A plain ask (its parallelism its CPU need: narrow) on measured runners: the fastest core with
/// room, not the most core-equivalents free; with nothing measured, the most free.
fn narrow_ranks_by_core() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(3, C8);
    let mut rout = RunnerOut::new();
    let mut out = Out::default();
    // 0: twice as fast a core, two free; 1: the reference, eight free; 2: as fast as 0, slowed
    // to half by contention, eight free.
    for (r, (speed, slowness, used)) in rs.iter_mut().zip([(2048, 0, 6 * CPU), (0, 0, 0), (2048, 2048, 0)]) {
        r.measure(speed, slowness);
        r.outside([used, 0, 0, 0], &mut rout);
        r.drain(&mut rout);
    }
    for (_, m) in rout {
        o.hear(m, &mut out);
    }
    settle(&mut o, &mut rs, &mut out);
    let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
    match a.send {
        Some((RunnerId(0), _)) => Ok(()),
        other => Err(format!("a narrow ask went to {other:?}, not the fastest core with room")),
    }
}

#[test]
fn w05_r23_narrow_asks_rank_by_per_core_speed_under_contention() -> Result<(), String> {
    caught(Mutant::NoNarrow, narrow_ranks_by_core)
}

/// A wide ask (learned parallelism past its CPU need) ranks by core-equivalents free.
#[test]
fn w05_r23_wide_asks_rank_by_core_equivalents() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(2, C8);
    let mut rout = RunnerOut::new();
    let mut out = Out::default();
    for (r, (speed, used)) in rs.iter_mut().zip([(2048, 6 * CPU), (1024, 0)]) {
        r.measure(speed, 0);
        r.outside([used, 0, 0, 0], &mut rout);
        r.drain(&mut rout);
    }
    for (_, m) in rout {
        o.hear(m, &mut out);
    }
    settle(&mut o, &mut rs, &mut out);
    let t = Terms { parallel: 8 * CPU, ..Terms::default() };
    match o.ask_with(s, [CPU, 0, 0, 0], &t, &mut out).map_err(|e| format!("{e:?}"))?.send {
        Some((RunnerId(1), _)) => Ok(()),
        other => Err(format!("a wide ask went to {other:?}, not the most core-equivalents")),
    }
}

/// W24-R18: a run whose soonest finish is a busy warm runner waits there alone: room rising
/// elsewhere doesn't take it, and it starts there when that runner has room.
#[test]
fn w24_r18_a_run_waits_for_its_warm_runner_alone() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(2, C8);
    let mut rout = RunnerOut::new();
    let mut out = Out::default();
    if let Some(r) = rs.get_mut(0) {
        r.outside([8 * CPU, 0, 0, 0], &mut rout);
        r.drain(&mut rout);
    }
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    let warm = [Warm { runner: RunnerId(0), saved_ns: 600_000_000_000, missing: 0, wait_ns: 1_000_000_000 }];
    let t = Terms { cpu_ns: 660_000_000_000, warm: &warm, ..Terms::default() };
    let a = o.ask_with(s, [CPU, 0, 0, 0], &t, &mut out).map_err(|e| format!("{e:?}"))?;
    if a.send.is_some() || !matches!(out.to.as_slice(), [(RunnerId(0), ToRunner::Wait(_))]) {
        return Err(format!("it didn't wait at the warm runner alone: {:?} {:?}", a.send, out.to));
    }
    settle(&mut o, &mut rs, &mut out);
    // Room rises on the other runner: still waiting.
    if let Some(r) = rs.get_mut(1) {
        r.outside([CPU, 0, 0, 0], &mut rout);
        r.outside([0; 4], &mut rout);
        r.drain(&mut rout);
    }
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    settle(&mut o, &mut rs, &mut out);
    if !matches!(o.state(a.run), Some(State::Waiting { .. })) {
        return Err(format!("room elsewhere took it: {:?}", o.state(a.run)));
    }
    if let Some(r) = rs.get_mut(0) {
        r.outside([0; 4], &mut rout);
        r.drain(&mut rout);
    }
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    settle(&mut o, &mut rs, &mut out);
    match o.state(a.run) {
        Some(State::Started { runner: RunnerId(0), .. }) => Ok(()),
        other => Err(format!("it didn't start at its warm runner: {other:?}")),
    }
}

/// A runner cordoned (a laptop on battery) takes no new run and says it has no room; its runs
/// run on; opened again, it takes runs.
#[test]
fn a_cordoned_runner_takes_no_new_run() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, C8);
    let mut out = Out::default();
    let first = super::tests::ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("no first start")?;
    let mut rout = RunnerOut::new();
    if let Some(r) = rs.get_mut(0) {
        r.cordon(true, &mut rout);
        r.drain(&mut rout);
        if r.word().cpu != 0 || r.admitted().len() != 1 {
            return Err(format!("cordoned, its word says {} free and it holds {}", r.word().cpu, r.admitted().len()));
        }
    }
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
    settle(&mut o, &mut rs, &mut out);
    if !matches!(o.state(a.run), Some(State::Waiting { .. })) {
        return Err(format!("a cordoned runner's room was taken: {:?}", o.state(a.run)));
    }
    if let Some(r) = rs.get_mut(0) {
        r.cordon(false, &mut rout);
        r.drain(&mut rout);
    }
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    settle(&mut o, &mut rs, &mut out);
    match (o.state(a.run), o.state(first.1)) {
        (Some(State::Started { .. }), Some(State::Started { .. })) => Ok(()),
        other => Err(format!("opened again, it didn't take the run: {other:?}")),
    }
}

/// A runner's slowness changing alone (its speed as it was) reaches the owner: its word's stamp
/// carries the measure count, so the owner reads it whole once.
#[test]
fn a_new_slowness_alone_reaches_the_owner() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(2, C8);
    let mut out = Out::default();
    let mut rout = RunnerOut::new();
    for (r, w) in rs.iter_mut().zip([1024, 1024]) {
        r.measure(1024, w);
        r.drain(&mut rout);
    }
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    if let Some(r) = rs.get_mut(0) {
        r.measure(1024, 4096);
        r.drain(&mut rout);
    }
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    settle(&mut o, &mut rs, &mut out);
    for _ in 0..4 {
        let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
        if let Some((RunnerId(0), _)) = a.send {
            return Err("a narrow ask went to the runner its slowness slowed to a quarter".into());
        }
    }
    Ok(())
}

/// Every rate is at most its class's bound (what the threshold assumes of a member it hasn't
/// read), and from 64 up the bound is its class's highest rate: the next is in a class above.
#[test]
fn a_class_bounds_its_rates() -> Result<(), String> {
    let mut rng = Rng(0xc1a55);
    for _ in 0..200_000 {
        let r = rng.next() >> rng.below(64);
        let c = class_of(r);
        let hi = class_hi(c);
        if r > hi || (r >= 64 && (class_of(hi) != c || hi.checked_add(1).is_some_and(|h| class_of(h) <= c))) {
            return Err(format!("rate {r}, class {c}, its bound {hi}"));
        }
    }
    Ok(())
}

/// The integer e^-x the stability term uses, against the float one.
#[test]
fn exp_neg_q32_follows_the_exponential() -> Result<(), String> {
    for i in 0..2000u64 {
        let x = i * (40u64 << 32) / 2000;
        let want = (-(x as f64) / 4_294_967_296.0).exp() * 4_294_967_296.0;
        let got = exp_neg_q32(x) as f64;
        if (got - want).abs() > 128.0 {
            return Err(format!("e^-{} is {got}, not {want}", x as f64 / 4_294_967_296.0));
        }
    }
    let s = stability(3_600_000_000_000, (1u128 << 96) / 3_600_000_000_000, 10_000_000_000) as f64;
    let want = (1.0 - (-1.0f64).exp()) * 10_000_000_000.0;
    if (s - want).abs() > want / 33_554_432.0 {
        return Err(format!("an hour's run on a link that drops hourly loses {s} ns, not {want}"));
    }
    Ok(())
}
