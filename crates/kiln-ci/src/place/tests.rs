//! The runner's admission (docs/reach.md §2, docs/needs.md §5, §6, §8 and E5), each criterion a
//! test, and the properties the person named: no grant past a runner's budget in any dimension
//! (G1), no run started twice, no run lost but by its owner's absence or epoch, what can start
//! offered in the input that let it (nobody polling), and every waiting asker told each change.

use std::collections::{BTreeMap, BTreeSet};

use super::ask::{Ask, Peer, RunUse, Sample};
use super::link::Rtt;
use super::msg::{Dim, Place, Reason, ToOwner};
use super::runner::{Admission, Init, Records, Said, Settings, Start};
use super::{Mutant, planted};
use crate::facts::RunnerFacts;
use crate::sched::needs;
use crate::sched::state::{Cpus, Fence, Resources};
use crate::sched::units::Ms;

const GIB: u64 = 1 << 30;
const MIN: i64 = 60_000;
const O1: Peer<'static> = Peer { name: "o1", epoch: 1 };
const O2: Peer<'static> = Peer { name: "o2", epoch: 1 };
const O3: Peer<'static> = Peer { name: "o3", epoch: 1 };

fn res(cpus: u64, mem_gib: u64) -> Resources {
    Resources { cpu: Cpus::whole(cpus), mem: mem_gib * GIB, ..Resources::default() }
}

fn linux() -> RunnerFacts {
    RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }
}

/// A machine of `cpus` and `mem_gib`, `busy` CPUs and `used_gib` in use by others, 500 GiB of
/// disk with 100 used.
fn machine(cpus: u64, mem_gib: u64, busy: u64, used_gib: u64) -> Sample {
    Sample { size: Resources { disk: 500 * GIB, ..res(cpus, mem_gib) }, used: Resources { disk: 100 * GIB, ..res(busy, used_gib) }, ..Sample::default() }
}

fn settings(backfill: bool) -> Settings {
    Settings { budget: None, overcommit_cpu: 1000, backfill, weights: BTreeMap::new() }
}

fn runner(sample: Sample, backfill: bool) -> Admission {
    Admission::new(Init { me: "linux-1".into(), epoch: 1, facts: linux(), labels: vec!["linux".into()], sample, records: Records::new(), settings: settings(backfill) }, 0)
}

fn ask(run: &str, spelling: &str) -> Result<Ask, String> {
    Ok(Ask { run: run.into(), needs: needs::parse(spelling)?, repo: "acme/app".into(), actor: "agent:1".into(), ..Ask::default() })
}

/// The offer to `owner` for `run` among what was said, if one was.
fn offer(said: &[Said], owner: &str, run: &str) -> Option<Fence> {
    said.iter().find_map(|(o, m)| match m {
        ToOwner::Offer { run: r, fence, .. } if o == owner && r == run => Some(*fence),
        _ => None,
    })
}

fn waiting(said: &[Said], run: &str) -> Option<(Reason, Place)> {
    said.iter().rev().find_map(|(_, m)| match m {
        ToOwner::Waiting { run: r, reason, place, .. } if r == run => Some((reason.clone(), *place)),
        _ => None,
    })
}

fn probe(a: &mut Admission, from: Peer<'_>, ask: Ask, predicted: Option<Ms>, now: i64) -> Vec<Said> {
    let mut out = Vec::new();
    a.probe(from, ask, predicted, now, &mut out);
    out
}

/// Takes and starts an offered run, as its owner would.
fn run_it(a: &mut Admission, from: Peer<'_>, run: &str, fence: Fence, now: i64) -> Result<Vec<Said>, String> {
    let mut out = Vec::new();
    a.take(from, run, fence, now, &mut out);
    if !out.iter().any(|(_, m)| matches!(m, ToOwner::Held { .. })) {
        return Err(format!("{run}: not held: {out:?}"));
    }
    match a.start(from, run, fence, now, &mut out) {
        Some(Start::New) => Ok(out),
        other => Err(format!("{run}: {other:?}: {out:?}")),
    }
}

fn ended(a: &mut Admission, from: Peer<'_>, run: &str, now: i64) -> Vec<Said> {
    let mut out = Vec::new();
    a.ended(from, run, true, now, &mut out);
    out
}

/// A probe that fits the runner's measured room less what it promised is offered at once; one
/// that doesn't waits with exactly one reason and a place; one it could never run is answered
/// never, saying why. The runner: 32 CPUs, 64 GiB, others using 20 CPUs and 50 GiB.
#[test]
fn e5_a_probe_is_offered_waits_or_is_never_by_the_runner_s_own_room() -> Result<(), String> {
    let mut a = runner(machine(32, 64, 20, 50), false);
    let said = probe(&mut a, O1, ask("r1", "cpus=8,mem=8g")?, None, 0);
    let f1 = offer(&said, "o1", "r1").ok_or(format!("r1 not offered: {said:?}"))?;
    assert!(said.iter().any(|(_, m)| matches!(m, ToOwner::Offer { got, free, .. } if *got == res(8, 8) && free.cpu == Cpus::whole(12))), "{said:?}");
    // 4 CPUs and 6 GiB left once r1's are held: 16 CPUs wait, short of CPUs, first in line.
    let said = probe(&mut a, O1, ask("r2", "cpus=16")?, None, 1);
    let (reason, place) = waiting(&said, "r2").ok_or(format!("r2 not waiting: {said:?}"))?;
    assert_eq!(place, Place::of(1));
    match reason {
        Reason::Room { short } => assert_eq!(short.iter().map(|s| (s.dim, s.free, s.ask)).collect::<Vec<_>>(), vec![(Dim::Cpu, 4000, 16_000)]),
        other => return Err(format!("r2: {other:?}")),
    }
    let said = probe(&mut a, O1, ask("r3", "mem=128g")?, None, 2);
    assert!(matches!(said.first(), Some((_, ToOwner::Never { why, .. })) if why == "64 GiB of memory, under 128 GiB"), "{said:?}");
    let said = probe(&mut a, O1, ask("r4", "macos")?, None, 2);
    assert!(matches!(said.first(), Some((_, ToOwner::Never { why, .. })) if why == "not os == macos (os = linux)"), "{said:?}");
    // A probe again is answered as it stands: r1's offer again, r2's wait.
    assert_eq!(offer(&probe(&mut a, O1, ask("r1", "cpus=8,mem=8g")?, None, 3), "o1", "r1"), Some(f1));
    assert!(waiting(&probe(&mut a, O1, ask("r2", "cpus=16")?, None, 3), "r2").is_some());
    a.check()
}

/// A declined or withdrawn offer frees its room in the same input, and the next in line is
/// offered; a later claim's fence is greater.
#[test]
fn e5_a_declined_or_withdrawn_offer_frees_its_room_for_the_next() -> Result<(), String> {
    let mut a = runner(machine(8, 32, 0, 0), true);
    let fa = offer(&probe(&mut a, O1, ask("a", "cpus=8")?, None, 0), "o1", "a").ok_or("a")?;
    assert!(offer(&probe(&mut a, O2, ask("b", "cpus=8")?, None, 1), "o2", "b").is_none());
    assert!(offer(&probe(&mut a, O3, ask("c", "cpus=8")?, None, 2), "o3", "c").is_none());
    let mut said = Vec::new();
    a.decline(O1, "a", fa, 3, &mut said);
    let fb = offer(&said, "o2", "b").ok_or(format!("b: {said:?}"))?;
    assert!(fb > fa, "a later claim's fence is greater");
    let mut said = Vec::new();
    a.withdraw(O2, "b", 4, &mut said);
    assert!(offer(&said, "o3", "c").is_some(), "{said:?}");
    let mut said = Vec::new();
    a.decline(O3, "c", fa, 5, &mut said);
    assert!(said.is_empty(), "a decline naming another claim changes nothing");
    a.check()
}

/// An offer is held for its owner's link's retransmission timeout (RFC 6298: 1 s before any
/// round trip is measured, then SRTT plus four RTTVAR), then lapses, its owner told, and its run
/// waits again at its place; a take or a start under the old claim is refused.
#[test]
fn e5_an_offer_not_taken_in_its_link_s_timeout_lapses() -> Result<(), String> {
    let mut a = runner(machine(8, 32, 0, 0), true);
    let f1 = offer(&probe(&mut a, O1, ask("a", "cpus=4")?, None, 0), "o1", "a").ok_or("a")?;
    assert_eq!(a.timer(), Some(Rtt::INITIAL as i64), "the timeout of a link not measured yet");
    let mut said = Vec::new();
    a.tick(999, &mut said);
    assert!(said.is_empty());
    a.tick(1000, &mut said);
    assert!(said.iter().any(|(_, m)| matches!(m, ToOwner::Lapsed { fence, .. } if *fence == f1)), "{said:?}");
    let f2 = offer(&said, "o1", "a").ok_or(format!("offered again: {said:?}"))?;
    assert!(f2 > f1);
    let mut said = Vec::new();
    a.take(O1, "a", f1, 1001, &mut said);
    assert!(matches!(said.first(), Some((_, ToOwner::Lapsed { .. }))), "{said:?}");
    assert_eq!(a.start(O1, "a", f1, 1001, &mut Vec::new()), None);
    run_it(&mut a, O1, "a", f2, 1002)?;
    // Measured, the link's timeout is its own: 100 ms round trips, 50 ms apart, held 300 ms.
    a.link("o1", Rtt::first(100, 1));
    let said = probe(&mut a, O1, ask("b", "cpus=4")?, None, 2000);
    assert!(offer(&said, "o1", "b").is_some());
    assert_eq!(a.timer(), Some(2300));
    a.check()
}

/// A run started here is never started again: a start said again starts nothing and isn't a
/// lapse; a probe of it after it ended is told how it ended, until its owner says it heard; a
/// restarted runner's record says the same; and a restarted owner's run that reuses its id is
/// another run.
#[test]
fn e5_a_run_started_here_is_never_started_again_and_a_reused_id_is_another_run() -> Result<(), String> {
    let mut a = runner(machine(8, 32, 0, 0), true);
    let f = offer(&probe(&mut a, O1, ask("a", "cpus=4")?, None, 0), "o1", "a").ok_or("a")?;
    run_it(&mut a, O1, "a", f, 1)?;
    let mut said = Vec::new();
    assert_eq!(a.start(O1, "a", f, 2, &mut said), Some(Start::Already));
    assert!(matches!(said.first(), Some((_, ToOwner::Ran { .. }))), "{said:?}");
    let said = ended(&mut a, O1, "a", 10);
    assert!(said.iter().any(|(_, m)| matches!(m, ToOwner::Ended { ok: true, .. })), "{said:?}");
    assert!(matches!(probe(&mut a, O1, ask("a", "cpus=4")?, None, 11).first(), Some((_, ToOwner::Ended { .. }))));
    // Started again (a new process, its epoch one past), with the record of what it started.
    let init = Init { me: "linux-1".into(), epoch: 2, facts: linux(), labels: vec![], sample: machine(8, 32, 0, 0), records: a.records().clone(), settings: settings(true) };
    let mut b = Admission::new(init, 20);
    assert!(matches!(probe(&mut b, O1, ask("a", "cpus=4")?, None, 21).first(), Some((_, ToOwner::Ended { .. }))));
    let g = offer(&probe(&mut b, O1, ask("z", "cpus=4")?, None, 22), "o1", "z").ok_or("z")?;
    assert!(g > f, "claims after a restart are fenced in its later epoch");
    // The owner started again too: its run `a` is another run, and the old record goes.
    let o1_again = Peer { name: "o1", epoch: 2 };
    let said = probe(&mut b, o1_again, ask("a", "cpus=4")?, None, 23);
    assert!(offer(&said, "o1", "a").is_some(), "{said:?}");
    assert!(b.records().keys().all(|(_, e, _)| *e == 2), "{:?}", b.records());
    b.check()
}

/// The record goes when its owner says it heard how the run ended, and not before it ended.
#[test]
fn e5_the_record_of_a_run_goes_on_its_owner_s_word() -> Result<(), String> {
    let mut a = runner(machine(8, 32, 0, 0), true);
    let f = offer(&probe(&mut a, O1, ask("a", "cpus=4")?, None, 0), "o1", "a").ok_or("a")?;
    run_it(&mut a, O1, "a", f, 1)?;
    a.ack(O1, "a");
    assert_eq!(a.records().len(), 1, "a run still running keeps its record");
    let _ = ended(&mut a, O1, "a", 5);
    a.ack(O1, "a");
    assert!(a.records().is_empty());
    a.check()
}

/// Taken and started, a run's room is held until it ends; its end frees it for the next in line.
/// A sample that shows more in use shrinks the room, and one that shows less offers what now fits.
#[test]
fn e5_the_room_follows_the_runs_and_the_samples() -> Result<(), String> {
    let mut a = runner(machine(16, 64, 0, 0), true);
    let f = offer(&probe(&mut a, O1, ask("a", "cpus=12,mem=32g")?, None, 0), "o1", "a").ok_or("a")?;
    run_it(&mut a, O1, "a", f, 1)?;
    assert!(offer(&probe(&mut a, O2, ask("b", "cpus=8")?, None, 2), "o2", "b").is_none(), "4 CPUs left");
    // a uses 2 of its 12 CPUs and 4 of its 32 GiB, other work 10 CPUs: the room is 16 - (12 - 2),
    // 6 CPUs, all held by a.
    let mut m = machine(16, 64, 12, 20);
    m.runs.push(RunUse { owner: "o1".into(), epoch: 1, run: "a".into(), used: res(2, 4) });
    let mut said = Vec::new();
    a.measured(m, 5_000, &mut said);
    assert!(offer(&said, "o2", "b").is_none());
    let ([room_cpu, ..], [held_cpu, ..]) = a.room();
    assert_eq!((room_cpu, held_cpu), (6_000, 12_000));
    // a ends: what it held comes back, but the other work's 10 CPUs leave 6, under b's 8.
    let said = ended(&mut a, O1, "a", 6_000);
    assert!(offer(&said, "o2", "b").is_none(), "{said:?}");
    let mut said = Vec::new();
    a.measured(machine(16, 64, 4, 8), 10_000, &mut said);
    assert!(offer(&said, "o2", "b").is_some(), "{said:?}");
    a.check()
}

/// An owner whose link is down is offered nothing: its probes leave the line, an untaken offer
/// lapses at once and its room goes to the next; a claim it took waits for it, however long, and
/// ends only when it's gone.
#[test]
fn e5_an_owner_away_keeps_its_taken_claim_until_it_is_gone() -> Result<(), String> {
    let mut a = runner(machine(8, 32, 0, 0), true);
    let fa = offer(&probe(&mut a, O1, ask("a", "cpus=4")?, None, 0), "o1", "a").ok_or("a")?;
    let fb = offer(&probe(&mut a, O1, ask("b", "cpus=4")?, None, 0), "o1", "b").ok_or("b")?;
    assert!(offer(&probe(&mut a, O2, ask("c", "cpus=4")?, None, 1), "o2", "c").is_none());
    let mut said = Vec::new();
    a.take(O1, "a", fa, 2, &mut said);
    let mut said = Vec::new();
    a.away("o1", 3, &mut said);
    assert!(offer(&said, "o2", "c").is_some(), "{said:?}");
    assert!(said.iter().all(|(o, _)| o != "o1"), "nothing is said to an owner away");
    assert_eq!(a.start(O1, "b", fb, 4, &mut Vec::new()), None);
    // An hour away, its taken claim still holds; back, it starts under it.
    let mut said = Vec::new();
    a.tick(60 * MIN, &mut said);
    a.back(O1, 60 * MIN, &mut said);
    assert!(said.iter().any(|(o, m)| o == "o1" && matches!(m, ToOwner::Held { fence, .. } if *fence == fa)), "{said:?}");
    a.away("o1", 61 * MIN, &mut Vec::new());
    // Gone (its lease ended): its claim goes.
    a.forget("o1", 62 * MIN, &mut Vec::new());
    assert_eq!(a.start(O1, "a", fa, 62 * MIN, &mut Vec::new()), None);
    a.check()
}

/// A run that takes the runner whole waits while CI legs run there, saying so; once none runs,
/// it's offered.
#[test]
fn e5_a_run_taking_the_runner_whole_waits_for_its_ci_legs() -> Result<(), String> {
    let mut m = machine(8, 32, 0, 0);
    m.ci_jobs = 2;
    let mut a = runner(m, true);
    let said = probe(&mut a, O1, ask("x", "exclusive")?, None, 0);
    assert_eq!(waiting(&said, "x").map(|w| w.0), Some(Reason::Whole { jobs: 2 }));
    let mut said = Vec::new();
    a.measured(machine(8, 32, 0, 0), 5_000, &mut said);
    assert!(offer(&said, "o1", "x").is_some(), "{said:?}");
    a.check()
}

/// The estimate, from the sums ahead in line and the claims here: the soonest shares each
/// dimension's room out as a fluid; the latest has the claims run to their timeouts.
#[test]
fn e5_a_waiting_probe_s_start_is_estimated_from_what_holds_and_waits() -> Result<(), String> {
    let mut a = runner(machine(8, 32, 0, 0), false);
    let mut first = ask("a", "cpus=6")?;
    first.timeout = Some(30 * 60_000);
    let ten = u64::try_from(10 * MIN).unwrap_or(0);
    let f = offer(&probe(&mut a, O1, first, Some(ten), 0), "o1", "a").ok_or("a")?;
    run_it(&mut a, O1, "a", f, 0)?;
    let said = probe(&mut a, O2, ask("b", "cpus=4")?, None, MIN);
    let start = said.iter().find_map(|(_, m)| if let ToOwner::Waiting { start, .. } = m { Some(*start) } else { None }).ok_or("b waits")?;
    // a has 9 minutes of 6 CPUs left over 8 CPUs: 6.75 minutes; and it ends by its timeout.
    assert_eq!((start.soonest, start.latest), (Some(MIN + 405_000), Some(30 * MIN)));
    a.check()
}

/// W05-AC17 for commands: a 12-CPU run among a stream of 1-CPU runs on one 16-CPU runner starts
/// by its room's bound with backfill declared; without it, 1-CPU runs take every CPU that frees
/// and it doesn't start while they keep coming. Returns when it started, if it did.
fn big_among_small(backfill: bool, spelling: &str) -> Result<(Option<i64>, Option<i64>), String> {
    let mut a = runner(machine(16, 64, 0, 0), backfill);
    let ten = Some(u64::try_from(10 * MIN).unwrap_or(0));
    let mut t = 0i64;
    let mut running: Vec<(String, i64)> = Vec::new();
    let small = Peer { name: "small", epoch: 1 };
    let big = Peer { name: "big", epoch: 1 };
    for i in 0..16 {
        let run = format!("s{i}");
        let f = offer(&probe(&mut a, small, ask(&run, "cpus=1")?, ten, t), "small", &run).ok_or(format!("{run} not offered"))?;
        run_it(&mut a, small, &run, f, t)?;
        running.push((run, t + 10 * MIN));
        t += 1000;
    }
    let said = probe(&mut a, big, ask("big", spelling)?, ten, t);
    let bound = match waiting(&said, "big").map(|w| w.0) {
        Some(Reason::Kept { at }) => at,
        _ => None,
    };
    let mut started = None;
    let mut n = 16;
    while t < 40 * MIN && started.is_none() {
        t += 1000;
        let run = format!("s{n}");
        n += 1;
        let said = probe(&mut a, small, ask(&run, "cpus=1")?, ten, t);
        if let Some(f) = offer(&said, "small", &run) {
            run_it(&mut a, small, &run, f, t)?;
            running.push((run, t + 10 * MIN));
        }
        let (done, still): (Vec<_>, Vec<_>) = running.into_iter().partition(|(_, end)| *end <= t);
        running = still;
        for (r, _) in done {
            for (owner, m) in ended(&mut a, small, &r, t) {
                if let ToOwner::Offer { run, fence, .. } = m {
                    let from = if owner == "big" { big } else { small };
                    if run == "big" {
                        started = Some(t);
                    }
                    run_it(&mut a, from, &run, fence, t)?;
                    running.push((run, t + 10 * MIN));
                }
            }
        }
        a.check()?;
    }
    Ok((started, bound))
}

#[test]
fn w05_ac17_e5_a_big_run_among_small_ones_starts_by_its_bound_with_backfill() -> Result<(), String> {
    let (started, bound) = big_among_small(true, "cpus=12")?;
    match (started, bound) {
        // The room's time, plus how far the runs it waited for ran past their predicted ends
        // (none here): by its bound.
        (Some(at), Some(b)) => assert!(at <= b, "started at {at}, its bound {b}"),
        other => return Err(format!("with backfill: {other:?}")),
    }
    let (started, _) = big_among_small(false, "cpus=12")?;
    assert_eq!(started, None, "without backfill, small runs overtake it while they come");
    Ok(())
}

/// Defect g: a run that takes the runner whole gets room kept as any big run does, so a stream of
/// small runs doesn't starve it; with that planted away, it starves.
#[test]
fn e5_a_run_taking_the_runner_whole_starts_by_its_bound_and_starving_it_is_caught() -> Result<(), String> {
    let (started, bound) = big_among_small(true, "exclusive")?;
    match (started, bound) {
        (Some(at), Some(b)) => assert!(at <= b, "started at {at}, its bound {b}"),
        other => return Err(format!("the whole runner: {other:?}")),
    }
    let (started, _) = planted(Some(Mutant::ExclusiveStarves), || big_among_small(true, "exclusive"))?;
    assert_eq!(started, None, "ExclusiveStarves wasn't caught");
    Ok(())
}

mod props {
    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Clone)]
    enum Op {
        Probe { owner: u8, run: u8, cpus: u8, mem_gib: u8, disk: bool, whole: bool, predicted: Option<u8> },
        Take { owner: u8, run: u8 },
        Decline { owner: u8, run: u8 },
        Withdraw { owner: u8, run: u8 },
        Start { owner: u8, run: u8 },
        End { owner: u8, run: u8 },
        Sample { busy: u8, used_gib: u8, ci: u8 },
        Away { owner: u8 },
        Back { owner: u8 },
        Forget { owner: u8 },
        Restart { owner: u8 },
        Drain { on: bool },
        Tick { ms: u16 },
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            4 => (0..3u8, 0..8u8, 0..9u8, 0..20u8, any::<bool>(), proptest::bool::weighted(0.1), proptest::option::of(1..30u8)).prop_map(|(owner, run, cpus, mem_gib, disk, whole, predicted)| Op::Probe { owner, run, cpus, mem_gib, disk, whole, predicted }),
            2 => (0..3u8, 0..8u8).prop_map(|(owner, run)| Op::Take { owner, run }),
            1 => (0..3u8, 0..8u8).prop_map(|(owner, run)| Op::Decline { owner, run }),
            1 => (0..3u8, 0..8u8).prop_map(|(owner, run)| Op::Withdraw { owner, run }),
            2 => (0..3u8, 0..8u8).prop_map(|(owner, run)| Op::Start { owner, run }),
            2 => (0..3u8, 0..8u8).prop_map(|(owner, run)| Op::End { owner, run }),
            1 => (0..17u8, 0..33u8, 0..2u8).prop_map(|(busy, used_gib, ci)| Op::Sample { busy, used_gib, ci }),
            1 => (0..3u8).prop_map(|owner| Op::Away { owner }),
            1 => (0..3u8).prop_map(|owner| Op::Back { owner }),
            1 => (0..3u8).prop_map(|owner| Op::Forget { owner }),
            1 => (0..3u8).prop_map(|owner| Op::Restart { owner }),
            1 => any::<bool>().prop_map(|on| Op::Drain { on }),
            1 => (0..3_000u16).prop_map(|ms| Op::Tick { ms }),
        ]
    }

    /// A runner driven by `ops` as owners would drive it, every property checked after each.
    struct World {
        a: Admission,
        now: i64,
        epochs: [u64; 3],
        away: [bool; 3],
        /// The live claim of each (owner, epoch, run) as offered, and what was started.
        fences: BTreeMap<(u8, u64, String), Fence>,
        started: BTreeMap<(u8, u64, String), u32>,
        running: BTreeSet<(u8, u64, String)>,
        /// What each owner was told last of each waiting run's place and reason.
        told: BTreeMap<(String, String), (Place, (u8, Option<i64>))>,
    }

    fn name(o: u8) -> String {
        format!("o{o}")
    }

    impl World {
        fn new() -> World {
            let a = Admission::new(Init { me: "r".into(), epoch: 1, facts: linux(), labels: vec![], sample: machine(16, 32, 0, 0), records: Records::new(), settings: settings(true) }, 0);
            World { a, now: 0, epochs: [1; 3], away: [false; 3], fences: BTreeMap::new(), started: BTreeMap::new(), running: BTreeSet::new(), told: BTreeMap::new() }
        }

        fn epoch(&self, o: u8) -> u64 {
            self.epochs.get(usize::from(o)).copied().unwrap_or(1)
        }

        fn heard(&mut self, said: Vec<Said>) -> Result<(), TestCaseError> {
            for (o, m) in said {
                let Some(i) = o.strip_prefix('o').and_then(|x| x.parse::<u8>().ok()) else { continue };
                prop_assert!(!self.away.get(usize::from(i)).copied().unwrap_or(false), "told {o}, away: {m:?}");
                let e = self.epoch(i);
                match m {
                    ToOwner::Offer { run, fence, .. } => {
                        self.told.remove(&(o.clone(), run.clone()));
                        self.fences.insert((i, e, run), fence);
                    }
                    ToOwner::Waiting { run, reason, place, .. } => {
                        self.told.insert((o.clone(), run), (place, reason.kind()));
                    }
                    other => {
                        self.told.remove(&(o.clone(), other.run().to_owned()));
                    }
                }
            }
            Ok(())
        }

        fn step(&mut self, op: Op) -> Result<(), TestCaseError> {
            // An owner whose link is down says nothing to the runner.
            let from = match op {
                Op::Probe { owner, .. } | Op::Take { owner, .. } | Op::Decline { owner, .. } | Op::Withdraw { owner, .. } | Op::Start { owner, .. } => Some(owner),
                _ => None,
            };
            if from.is_some_and(|o| self.away.get(usize::from(o)).copied().unwrap_or(false)) {
                return Ok(());
            }
            self.now += 1;
            let now = self.now;
            let mut out = Vec::new();
            match op {
                Op::Probe { owner, run, cpus, mem_gib, disk, whole, predicted } => {
                    let mut needs = crate::sched::needs::Needs { request: Resources { cpu: Cpus::whole(u64::from(cpus)), mem: u64::from(mem_gib) * GIB, disk: if disk { 50 * GIB } else { 0 }, gpu: 0 }, ..Default::default() };
                    needs.exclusive = whole;
                    let ask = Ask { run: format!("r{run}"), needs, repo: format!("repo{owner}"), actor: name(owner), ..Ask::default() };
                    let n = name(owner);
                    self.a.probe(Peer { name: &n, epoch: self.epoch(owner) }, ask, predicted.map(|p| u64::from(p) * 1000), now, &mut out);
                }
                Op::Take { owner, run } | Op::Decline { owner, run } | Op::Start { owner, run } => {
                    let key = (owner, self.epoch(owner), format!("r{run}"));
                    let n = name(owner);
                    let from = Peer { name: &n, epoch: key.1 };
                    if let Some(f) = self.fences.get(&key).copied() {
                        match op {
                            Op::Take { .. } => self.a.take(from, &key.2, f, now, &mut out),
                            Op::Decline { .. } => self.a.decline(from, &key.2, f, now, &mut out),
                            _ => {
                                if self.a.start(from, &key.2, f, now, &mut out) == Some(Start::New) {
                                    let count = self.started.entry(key.clone()).or_default();
                                    *count += 1;
                                    prop_assert!(*count <= 1, "{key:?} started twice");
                                    self.running.insert(key.clone());
                                    self.a.spawned(from, &key.2, &mut out);
                                }
                            }
                        }
                    }
                }
                Op::Withdraw { owner, run } => {
                    let n = name(owner);
                    self.a.withdraw(Peer { name: &n, epoch: self.epoch(owner) }, &format!("r{run}"), now, &mut out);
                }
                Op::End { owner, run } => {
                    let key = (owner, self.epoch(owner), format!("r{run}"));
                    if self.running.remove(&key) {
                        let n = name(owner);
                        self.a.ended(Peer { name: &n, epoch: key.1 }, &key.2, true, now, &mut out);
                    }
                }
                Op::Sample { busy, used_gib, ci } => {
                    let mut s = machine(16, 32, u64::from(busy), u64::from(used_gib));
                    s.ci_jobs = u32::from(ci);
                    self.a.measured(s, now, &mut out);
                }
                Op::Away { owner } => {
                    if let Some(a) = self.away.get_mut(usize::from(owner)) {
                        *a = true;
                    }
                    self.told.retain(|(o, _), _| *o != name(owner));
                    self.a.away(&name(owner), now, &mut out);
                }
                Op::Back { owner } => {
                    if let Some(a) = self.away.get_mut(usize::from(owner)) {
                        *a = false;
                    }
                    let n = name(owner);
                    self.a.back(Peer { name: &n, epoch: self.epoch(owner) }, now, &mut out);
                }
                Op::Forget { owner } => self.a.forget(&name(owner), now, &mut out),
                Op::Restart { owner } => {
                    // A new process of the owner, its epoch one past: it speaks first by coming back.
                    if let Some(e) = self.epochs.get_mut(usize::from(owner)) {
                        *e += 1;
                    }
                    if let Some(a) = self.away.get_mut(usize::from(owner)) {
                        *a = false;
                    }
                    self.told.retain(|(o, _), _| *o != name(owner));
                    let n = name(owner);
                    self.a.back(Peer { name: &n, epoch: self.epoch(owner) }, now, &mut out);
                }
                Op::Drain { on } => self.a.drain(on, now, &mut out),
                Op::Tick { ms } => {
                    self.now += i64::from(ms);
                    self.a.tick(self.now, &mut out);
                }
            }
            self.heard(out)?;
            // G1, its books, and work-conserving: what can start was offered, nobody polling.
            if let Err(why) = self.a.check() {
                return Err(TestCaseError::fail(why));
            }
            // No run lost: what started runs until it's said to have ended.
            let views = self.a.probes();
            for (o, e, run) in &self.running {
                let n = name(*o);
                prop_assert!(views.iter().any(|v| v.owner == n && v.epoch == *e && v.run == *run && v.state == "running"), "{n}.{e} {run} started and isn't running");
            }
            // Every waiting asker told each change of its place and reason.
            for v in views.iter().filter(|v| v.state == "waiting") {
                let i = v.owner.strip_prefix('o').and_then(|x| x.parse::<usize>().ok()).unwrap_or(0);
                if self.away.get(i).copied().unwrap_or(false) || self.epochs.get(i).copied() != Some(v.epoch) {
                    continue;
                }
                let truth = self.a.truth(Peer { name: &v.owner, epoch: v.epoch }, &v.run);
                let told = self.told.get(&(v.owner.clone(), v.run.clone())).copied();
                prop_assert_eq!(told, truth, "{}'s {} told {:?}, stands at {:?}", v.owner, v.run, told, truth);
            }
            Ok(())
        }
    }

    fn drive(ops: Vec<Op>) -> Result<(), TestCaseError> {
        let mut w = World::new();
        for op in ops {
            w.step(op)?;
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(3_000))]

        /// G1, no run started twice, no run lost, what frees offered with nobody polling, and
        /// every waiting asker told each change, over random probes, owners' words (some away,
        /// gone or restarted), samples, drains, ends and time.
        #[test]
        fn e5_g1_once_never_lost_live_and_told(ops in proptest::collection::vec(op(), 1..80)) {
            drive(ops)?;
        }
    }

    /// Defect f: a runner that tells a waiting run's place once, and never again, is caught by the
    /// property that every waiting asker hears each change.
    #[test]
    fn e5_a_runner_telling_a_place_once_is_caught() {
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig { cases: 3_000, failure_persistence: None, ..ProptestConfig::default() });
        let found = planted(Some(Mutant::QuietPlace), || runner.run(&proptest::collection::vec(op(), 1..80), drive));
        assert!(found.is_err(), "QuietPlace wasn't caught");
    }
}
