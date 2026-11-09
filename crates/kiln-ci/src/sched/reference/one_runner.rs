//! One engine (docs/needs.md E5; W05-R03, R09, R10, R30, R31, R32): a runner's own admission
//! (`crate::place::runner`, the core of kiln's balancer) decides as the engine does, held to the
//! reference scheduler as the engine is ([`super`]). Over [`CASES`] small random cases on one
//! runner in resource mode (at most 12 runs and 40 inputs: runs queued in every class, tenants by
//! repository, actor and lane, priorities aged or not, floors and bounds drawn, exclusive runs
//! whole or as shares, runs ending, the runner draining, the time passing), both are fed the same
//! inputs, and after every one of them what runs and what waits must be the same in both, with the
//! admission's own books checked. A case where they part is cut down to the fewest inputs that
//! still part and printed with its seed; `KILN_PLACE_TWIN_SEED=<seed> cargo test -p kiln-ci --lib
//! one_runner -- --nocapture` replays one.
//!
//! What the reference does that a runner's admission has no part in is left out of the cases:
//! other runners (where a run goes is its owner's to choose, `place::owner`), grants as leases,
//! refusals, runners lost and their legs retried, gangs, and room kept (backfilling: the runner
//! keeps room for its first in line, the reference for each class's first across the fleet).

use std::collections::{BTreeMap, BTreeSet};

use super::{Draw, MINUTE, Reference, Step, default_classes, ms};
use crate::facts::RunnerFacts;
use crate::place::ask::{Ask, Peer, Sample};
use crate::place::msg::ToOwner;
use crate::place::runner::{Admission, Init, Records, Settings};
use crate::sched::needs::Needs;
use crate::sched::retry::{Failed, FailureClass};
use crate::sched::routing::{LegRoute, RunnerView};
use crate::sched::state::{Class, ClassConfig, Config, Cpus, FairBy, LegIn, Outcome, Resources, RunnerIn};

const CASES: u64 = 10_000;
const GIB: u64 = 1 << 30;
const OWNER: Peer<'static> = Peer { name: "owner", epoch: 1 };

/// A case: its settings, its runner, and its inputs at their times.
fn case(seed: u64) -> (Config, RunnerIn, Vec<(Step, i64)>) {
    let mut d = Draw(seed);
    let fair_by = match d.below(3) {
        0 => vec![FairBy::Repo],
        1 => vec![FairBy::Repo, FairBy::Actor],
        _ => vec![FairBy::Repo, FairBy::Actor, FairBy::Lane],
    };
    let classes = match d.below(3) {
        0 => default_classes(),
        _ => Class::ALL.into_iter().map(|c| (c, ClassConfig { floor: [0, 0, 100, 250, 500].get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or(0), bound: (d.below(2) == 0).then(|| 60_000 * (1 + d.below(20))) })).collect(),
    };
    let aging_per_minute = [0, 1000, 10_000].get(usize::try_from(d.below(3)).unwrap_or(0)).copied().unwrap_or(1000);
    let cfg = Config { resources: true, fair_by, classes, aging_per_minute, ..Config::default() };
    let capacity = Resources { cpu: Cpus::whole(2 + d.below(7)), mem: (4 + d.below(29)) * GIB, gpu: if d.below(3) == 0 { 1 + u32::try_from(d.below(2)).unwrap_or(0) } else { 0 }, ..Resources::default() };
    let view = RunnerView { name: "r0".into(), label: None, labels: vec!["x".into()], facts: RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }, caps: 0, pool: None };
    let runner = RunnerIn { id: "r0".into(), view, slots: 1 + u32::try_from(d.below(3)).unwrap_or(0), capacity: Some(capacity), executor: None, draining: false };
    let (mut r, mut twin) = (Reference::new(cfg.clone(), 0), admission(&cfg, &runner));
    let mut steps = vec![(Step::Up(runner.clone()), 0)];
    r.handle(&Step::Up(runner.clone()), 0);
    let (most, mut queued, mut now, mut draining) = (1 + d.below(12), 0u64, 0i64, false);
    for _ in 0..40 {
        let before = now;
        now += ms(&mut d, 3 * MINUTE);
        let running: Vec<String> = r.placement().0.keys().cloned().collect();
        let step = match d.below(12) {
            0..=4 if queued < most => {
                queued += 1;
                Some(Step::Queued(Box::new(leg(&mut d, queued, &capacity))))
            }
            5..=8 => d.pick(&running).cloned().map(|leg| {
                let outcome = if d.below(3) == 0 { Outcome::Failed(Failed { class: FailureClass::Failure, exit_code: Some(1), log_hits: Vec::new() }) } else { Outcome::Success };
                Step::Ended { leg, attempt: 1, outcome }
            }),
            9 => {
                draining = !draining;
                Some(if draining { Step::Drain("r0".into()) } else { Step::Undrain(runner.clone()) })
            }
            // A tick, half the time at a moment the admission's timer would ask for one: a look at
            // a promotion due since the step before.
            _ => {
                if d.below(2) == 0
                    && let Some(at) = twin.timer().filter(|at| (before..=now).contains(at))
                {
                    now = at;
                }
                Some(Step::Tick)
            }
        };
        if let Some(step) = step.filter(|s| r.applies(s)) {
            r.handle(&step, now);
            feed(&mut twin, &step, now);
            steps.push((step, now));
        }
    }
    (cfg, runner, steps)
}

/// A run as the reference's cases make them, on the runner's labels and never more than it is,
/// so that every run could start on it idle (a run it could never take is answered never by the
/// admission, and queued for good by the reference).
fn leg(d: &mut Draw, n: u64, cap: &Resources) -> LegIn {
    let mut request = match d.below(4) {
        0 => Resources::default(),
        1 => Resources { cpu: Cpus::milli(500 * (1 + d.below(8))), mem: (1 + d.below(8)) * GIB, ..Resources::default() },
        2 => Resources { cpu: Cpus::whole(1 + d.below(4)), mem: (1 + d.below(16)) * GIB, ..Resources::default() },
        _ => Resources { cpu: Cpus::whole(4 + d.below(5)), mem: (8 + d.below(25)) * GIB, ..Resources::default() },
    };
    request.cpu = request.cpu.min(cap.cpu);
    request.mem = request.mem.min(cap.mem);
    let exclusive = d.below(5) == 0;
    if !exclusive && !request.cpu.is_zero() && cap.gpu > 0 && d.below(4) == 0 {
        request.gpu = 1;
    }
    LegIn {
        id: format!("l{n}"),
        run: format!("run{}", n / 3),
        repo: format!("acme/r{}", d.below(3)),
        route: LegRoute { labels: vec!["x".into()], ..LegRoute::default() },
        priority: [0u8, 20, 60, 80, 100].get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or(60),
        exclusive,
        request,
        cost: (d.below(2) == 0).then(|| 10_000 * (1 + d.below(60))),
        p90: (d.below(2) == 0).then(|| 5_000 * (1 + d.below(36))),
        timeout: (d.below(3) == 0).then(|| 60_000 * (2 + d.below(20))),
        actor: format!("a{}", d.below(2)),
        lane: format!("l{}", d.below(2)),
        class: Class::ALL.get(usize::try_from(d.below(5)).unwrap_or(0)).copied().unwrap_or_default(),
        ..LegIn::default()
    }
}

/// The runner's admission for a case: its settings the engine's, its sample the runner's size,
/// nothing else using it.
fn admission(cfg: &Config, r: &RunnerIn) -> Admission {
    let size = r.capacity.unwrap_or_default();
    let settings = Settings {
        budget: None,
        overcommit_cpu: cfg.overcommit_cpu,
        backfill: false,
        weights: BTreeMap::new(),
        classes: cfg.classes.clone(),
        aging_per_minute: cfg.aging_per_minute,
        fair_by: cfg.fair_by.clone(),
        slots: r.slots,
        default_cost: Some(cfg.default_cost),
        predicted_order: None,
        learned_weights: false,
        auction: false,
        tokens: false,
    };
    let init = Init { me: r.id.clone(), epoch: 1, since: None, facts: r.view.facts.clone(), labels: r.view.labels.clone(), sample: Sample { size, ..Sample::default() }, records: Records::new(), settings };
    Admission::new(init, 0)
}

fn ask(l: &LegIn) -> Ask {
    Ask {
        restartable: false,
        asker: None,
        run: l.id.clone(),
        needs: Needs { request: l.request, exclusive: l.exclusive, ..Needs::default() },
        class: l.class,
        repo: l.repo.clone(),
        actor: l.actor.clone(),
        lane: l.lane.clone(),
        priority: l.priority,
        median: l.cost,
        p90: l.p90,
        timeout: l.timeout,
        among: Vec::new(),
        key: None,
    }
}

/// Its owner takes and starts each run the runner offers, as soon as it's offered: a run offered
/// is one placed.
fn own(a: &mut Admission, mut said: Vec<(String, ToOwner)>, now: i64) {
    while let Some((_, m)) = said.pop() {
        if let ToOwner::Offer { run, fence, .. } = m {
            let mut out = Vec::new();
            a.take(OWNER, &run, fence, now, &mut out);
            a.start(OWNER, &run, fence, now, &mut out);
            a.spawned(OWNER, &run, &mut out);
            said.extend(out);
        }
    }
}

/// One input to the admission.
fn feed(a: &mut Admission, step: &Step, now: i64) {
    let mut out = Vec::new();
    match step {
        Step::Queued(l) => a.probe(OWNER, ask(l), None, now, &mut out),
        Step::Ended { leg, outcome, .. } => {
            a.ended(OWNER, leg, matches!(outcome, Outcome::Success), now, &mut out);
            a.ack(OWNER, leg);
        }
        Step::Drain(_) => a.drain(true, now, &mut out),
        Step::Undrain(_) => a.drain(false, now, &mut out),
        Step::Tick => a.tick(now, &mut out),
        _ => {}
    }
    own(a, out, now);
}

/// What runs and what waits on the admission.
fn placed(a: &Admission) -> (BTreeSet<String>, BTreeSet<String>) {
    let (mut on, mut queued) = (BTreeSet::new(), BTreeSet::new());
    for p in a.probes() {
        match p.state {
            "waiting" | "aside" => queued.insert(p.run),
            _ => on.insert(p.run),
        };
    }
    (on, queued)
}

/// A case's steps through both: `Err` where the admission's books are wrong or the two part, with
/// the steps that applied up to there.
fn run(cfg: &Config, runner: &RunnerIn, steps: &[(Step, i64)], mut tell: impl FnMut(&str)) -> Result<(), (Vec<(Step, i64)>, String)> {
    let (mut r, mut a, mut applied) = (Reference::new(cfg.clone(), 0), admission(cfg, runner), Vec::new());
    for (step, now) in steps {
        if !r.applies(step) {
            continue;
        }
        applied.push((step.clone(), *now));
        r.handle(step, *now);
        feed(&mut a, step, *now);
        let theirs = r.placement();
        let theirs = (theirs.0.keys().cloned().collect::<BTreeSet<String>>(), theirs.1);
        let mine = placed(&a);
        tell(&format!("{:>4}:{:02} {}\n      running {:?}, queued {:?}", now / MINUTE, now % MINUTE / 1000, step.words(), theirs.0, theirs.1));
        a.check().map_err(|why| (applied.clone(), format!("the admission's books: {why}")))?;
        if mine != theirs {
            return Err((applied, format!("they part:\n  admission: running {:?}, queued {:?}\n  reference: running {:?}, queued {:?}", mine.0, mine.1, theirs.0, theirs.1)));
        }
    }
    Ok(())
}

fn shrink(cfg: &Config, runner: &RunnerIn, steps: Vec<(Step, i64)>, why: String) -> (Vec<(Step, i64)>, String) {
    let (mut steps, mut why) = (steps, why);
    'smaller: loop {
        // Never the runner's coming up, which every case starts with.
        for i in 1..steps.len() {
            let mut fewer = steps.clone();
            fewer.remove(i);
            if let Err((applied, w)) = run(cfg, runner, &fewer, |_| {}) {
                (steps, why) = (applied, w);
                continue 'smaller;
            }
        }
        return (steps, why);
    }
}

fn report(seed: u64, cfg: &Config, runner: &RunnerIn, steps: &[(Step, i64)], why: &str) -> String {
    let mut s = format!("seed {seed}: {why}\n  fair by {:?}, aging {}‰ a minute, classes {:?}, runner {:?} with {} slots\n", cfg.fair_by, cfg.aging_per_minute, cfg.classes, runner.capacity, runner.slots);
    for (step, now) in steps {
        s.push_str(&format!("  {:>4}:{:02} {}\n", now / MINUTE, now % MINUTE / 1000, step.words()));
    }
    s
}

/// docs/needs.md E5's one engine: over 10,000 cases, a one-runner admission makes the same
/// decisions as the reference, with zero disagreements; and the cases make it use what it's meant
/// to (W05-R31's floors going ahead and over, promotion by bound and into `land`; W05-R03's
/// whole runs and shares; fair share over two and three levels).
#[test]
fn w05_r31_e5_a_runners_admission_decides_as_the_reference() {
    if let Ok(seed) = std::env::var("KILN_PLACE_TWIN_SEED")
        && let Ok(seed) = seed.parse::<u64>()
    {
        let (cfg, runner, steps) = case(seed);
        let out = run(&cfg, &runner, &steps, |s| println!("{s}"));
        println!("{out:?}");
        return;
    }
    let mut parted = Vec::new();
    let (mut promoted, mut ahead, mut over, mut whole, mut shares, mut deep) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for seed in 0..CASES {
        let (cfg, runner, steps) = case(seed);
        if let Err((applied, why)) = run(&cfg, &runner, &steps, |_| {}) {
            parted.push((seed, cfg, runner, applied, why));
            continue;
        }
        let mut r = Reference::new(cfg.clone(), 0);
        for (step, now) in &steps {
            r.handle(step, *now);
        }
        promoted += u64::from(r.promoted > 0);
        ahead += u64::from(r.ahead > 0);
        over += u64::from(r.over > 0);
        whole += u64::from(steps.iter().any(|(s, _)| matches!(s, Step::Queued(l) if l.exclusive && l.request.cpu.is_zero())));
        shares += u64::from(steps.iter().any(|(s, _)| matches!(s, Step::Queued(l) if l.exclusive && !l.request.cpu.is_zero())));
        deep += u64::from(cfg.fair_by.len() > 1);
    }
    let total = parted.len();
    if let Some((seed, cfg, runner, applied, why)) = parted.into_iter().next() {
        let (fewer, why) = shrink(&cfg, &runner, applied, why);
        assert_eq!(total, 0, "{total} of {CASES} cases part; the first, cut down:\n{}", report(seed, &cfg, &runner, &fewer, &why));
    }
    println!("cases with promotions {promoted}, floors going ahead {ahead}, over a floor {over}, whole runs {whole}, shares {shares}, fair share deeper than repositories {deep}");
    assert!(promoted > 0 && ahead > 0 && over > 0 && whole > 0 && shares > 0 && deep > 0, "the cases don't use every rule: {promoted} {ahead} {over} {whole} {shares} {deep}");
}
