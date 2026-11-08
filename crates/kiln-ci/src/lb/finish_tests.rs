//! Expected finish (docs/lb.md "Rank", "Orders", "Per-ask terms"), W05-R23 and W24-R18: the
//! kernel's choices against a brute force over small fleets with every term, each term's planted
//! bug caught; narrow asks' order; a run waiting for a warm runner alone; cordon; measures
//! reaching the owner.

use super::model::Rng;
use super::order::class_of;
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

fn key_of(view: u64, speed: u32) -> u64 {
    let scale = if speed == 0 { u64::from(SCALE) } else { (u64::from(SCALE) * u64::from(speed) / u64::from(SPEED_ONE)).min(u64::from(u32::MAX)) };
    ((u128::from(view) * u128::from(scale)) >> 40).min(8) as u64
}

fn per_core(speed: u32, slowness: u32) -> u64 {
    let s = if speed == 0 { SPEED_ONE } else { speed };
    let w = if slowness == 0 { SPEED_ONE } else { slowness };
    (u64::from(s) << 32) / u64::from(w)
}

/// A rate: unmeasured (0) for at most one runner a fleet, the rest in classes of their own.
fn rate(rng: &mut Rng, lo: u64, hi: u64, zero_ok: &mut bool, taken: &mut Vec<u32>) -> u64 {
    if *zero_ok && rng.below(4) == 0 {
        *zero_ok = false;
        return 0;
    }
    loop {
        let r = lo + rng.below(hi - lo);
        let c = class_of(r);
        if !taken.contains(&c) {
            taken.push(c);
            return r;
        }
    }
}

/// A fleet whose keys, and classes in every order, are each a runner's own (so each order's
/// best is one runner, whatever the turns), measured or not as the case draws.
fn shapes(rng: &mut Rng, n: usize) -> Vec<Shape> {
    loop {
        let cpu_measured = rng.below(4) != 0;
        let (mut z_disk, mut z_net, mut z_link) = (true, true, true);
        let (mut t_disk, mut t_net, mut t_link, mut t_core) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut out = Vec::new();
        let mut keys = Vec::new();
        let mut ok = true;
        for _ in 0..n {
            let (speed, slowness) = if cpu_measured { (512 + rng.below(1537) as u32, 1024 + rng.below(3073) as u32) } else { (0, 0) };
            let c = class_of(per_core(speed, slowness));
            if cpu_measured && t_core.contains(&c) {
                ok = false;
            }
            t_core.push(c);
            let used = (rng.below(9) * CPU).saturating_sub(if rng.below(3) == 0 { rng.below(CPU) } else { 0 });
            let k = key_of(8 * CPU - used, speed);
            if keys.contains(&k) {
                ok = false;
            }
            keys.push(k);
            let disk = rate(rng, 50 << 20, 4 << 30, &mut z_disk, &mut t_disk);
            let net = rate(rng, 10 << 20, 2 << 30, &mut z_net, &mut t_net);
            let lr = rate(rng, 1 << 20, 1 << 30, &mut z_link, &mut t_link);
            let link = Link { rate: lr, latency_ns: rng.below(200_000_000), between_drops_ns: if rng.below(3) == 0 { 0 } else { 1 + rng.below(3_600_000_000_000) }, outage_ns: rng.below(60_000_000_000) };
            out.push(Shape { speed, slowness, disk, net, link, used });
        }
        if ok {
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
    let q = Rates::of(x.speed, x.slowness, x.disk, x.net, &x.link);
    let k = if wide { p.min(view.max(cpu)) } else { p }.max(1);
    let per_core = u128::from(t.cpu_ns.saturating_sub(saved)) * u128::from(CPU) / u128::from(k);
    let t_cpu = mul_q32(per_core, q.core);
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

/// The brute force: every runner's place in every order worked out from its shape, the
/// candidates each relevant order's best gives, every candidate's expected finish, the soonest.
/// Also the soonest over every runner that fits (for the single-term cases).
fn oracle(sh: &[Shape], need: Amounts, t: &Terms) -> (Choice, u128, u128) {
    let cpu = need[0];
    let p = if t.parallel == 0 { cpu } else { t.parallel };
    let wide = p > cpu;
    let ordered = sh.iter().any(|x| x.speed != 0 || x.slowness != 0);
    let orders_on = ordered || sh.iter().any(|x| x.disk != 0 || x.net != 0 || x.link.rate != 0);
    let view = |x: &Shape| 8 * CPU - x.used;
    let best_by = |f: &dyn Fn(&Shape) -> Option<u64>, room: u64| -> Option<usize> {
        let mut best: Option<(u64, usize)> = None;
        for (i, x) in sh.iter().enumerate() {
            if view(x) >= room
                && let Some(v) = f(x)
                && best.is_none_or(|b| v > b.0)
            {
                best = Some((v, i));
            }
        }
        best.map(|b| b.1)
    };
    let wide_top = || best_by(&|x| Some(key_of(view(x), x.speed)), cpu);
    let mut cands: Vec<(usize, u64, u64, u64)> = Vec::new();
    if wide || !ordered {
        cands.extend(wide_top().map(|i| (i, 0, 0, t.bytes)));
    }
    if ordered {
        let narrow = best_by(&|x| (key_of(view(x), x.speed) != 0).then(|| u64::from(class_of(per_core(x.speed, x.slowness)))), p.max(cpu));
        match narrow {
            Some(i) => cands.push((i, 0, 0, t.bytes)),
            None if !wide => cands.extend(wide_top().map(|i| (i, 0, 0, t.bytes))),
            None => {}
        }
    }
    if orders_on {
        if t.io_bytes != 0 {
            cands.extend(best_by(&|x| (key_of(view(x), x.speed) != 0).then(|| u64::from(class_of(x.disk))), cpu).map(|i| (i, 0, 0, t.bytes)));
        }
        if t.bytes != 0 {
            cands.extend(best_by(&|x| (key_of(view(x), x.speed) != 0).then(|| u64::from(class_of(x.link.rate))), cpu).map(|i| (i, 0, 0, t.bytes)));
        }
        if t.net_bytes != 0 {
            cands.extend(best_by(&|x| (key_of(view(x), x.speed) != 0).then(|| u64::from(class_of(x.net))), cpu).map(|i| (i, 0, 0, t.bytes)));
        }
    }
    for w in t.warm {
        let i = w.runner.0 as usize;
        let Some(x) = sh.get(i) else { continue };
        let wait = if view(x) >= cpu { 0 } else if w.wait_ns != 0 { w.wait_ns } else { continue };
        cands.push((i, wait, w.saved_ns, w.missing));
    }
    if cands.is_empty() {
        return (Choice::Waits, 0, 0);
    }
    let mut on = (true, true, true);
    for c in &cands {
        if let Some(x) = sh.get(c.0) {
            on = (on.0 && x.disk != 0, on.1 && x.net != 0, on.2 && x.link.rate != 0);
        }
    }
    let mut best = (u128::MAX, Choice::Waits);
    for &(i, wait, saved, missing) in &cands {
        let Some(x) = sh.get(i) else { continue };
        let e = finish(x, view(x), t, cpu, p, wide, on, wait, saved, missing);
        if e < best.0 {
            best = (e, if wait != 0 { Choice::WaitAt(i as u32) } else { Choice::Start(i as u32) });
        }
    }
    let global = sh.iter().filter(|x| view(x) >= cpu).map(|x| finish(x, view(x), t, cpu, p, wide, on, 0, 0, t.bytes)).min().unwrap_or(u128::MAX);
    (best.1, best.0, global)
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

/// `cases` random fleets of 2 to 6 runners, one ask each, every term drawn on or off: the
/// kernel chooses what the brute force does. Where one term alone bears on the finish and every
/// runner has it measured, that choice is the soonest of every runner with room.
fn oracle_cases(seed: u64, cases: u32) -> Result<Vec<f64>, String> {
    let mut rng = Rng(seed);
    let mut warm = Vec::new();
    let mut ratios = Vec::new();
    for case in 0..cases {
        let n = 2 + rng.below(5) as usize;
        let sh = shapes(&mut rng, n);
        let (mut o, _rs, s) = build(&sh);
        let (need, t0, on) = terms(&mut rng, n as u64, &mut warm);
        let t = Terms { warm: &warm, ..t0 };
        let (want, e, global) = oracle(&sh, need, &t);
        let got = decide(&mut o, s, need, &t)?;
        if got != want {
            return Err(format!("case {case}: kernel {got:?}, brute force {want:?}\nshapes {sh:?}\nneed {need:?} terms {t:?}"));
        }
        if want == Choice::Waits {
            continue;
        }
        let single = [on[0], on[1], on[2], on[3]].iter().filter(|b| **b).count() == 1 && !on[4] && warm.is_empty();
        let measured = sh.iter().all(|x| x.disk != 0 && x.net != 0 && x.link.rate != 0);
        if single && measured && !on[0] && e != global {
            return Err(format!("case {case}: one term, its order's best finishes at {e}, the soonest at {global}\nshapes {sh:?}\nterms {t:?}"));
        }
        if !single && measured && warm.is_empty() && global > 0 {
            ratios.push(e as f64 / global as f64);
        }
    }
    Ok(ratios)
}

#[test]
fn w05_r23_expected_finish_chooses_as_the_brute_force() -> Result<(), String> {
    let mut r = oracle_cases(0x5eed, 20_000)?;
    r.sort_by(f64::total_cmp);
    let at = |q: usize| r.get(q * r.len().saturating_sub(1) / 100).copied().unwrap_or(1.0);
    let optimal = r.iter().filter(|x| **x <= 1.0).count();
    println!("several terms, every rate measured, no list: the candidates' best is the soonest of all in {optimal} of {}; its finish over the soonest: median {:.3}, p90 {:.3}, p99 {:.3}, worst {:.3}", r.len(), at(50), at(90), at(99), at(100));
    Ok(())
}

#[test]
fn w24_r18_each_terms_planted_bug_is_caught() -> Result<(), String> {
    for m in [Mutant::NoNarrow, Mutant::NoSlowness, Mutant::NoWarm, Mutant::NoTransfer, Mutant::NoDisk, Mutant::NoNet, Mutant::NoLatency, Mutant::NoStability, Mutant::NoQueueWait, Mutant::OneCandidate] {
        caught(m, || oracle_cases(0xb0b, 1500).map(|_| ()))?;
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
