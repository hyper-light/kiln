//! The owner's tickets and answers (docs/reach.md §2, docs/needs.md §9 and §13), each criterion a
//! test, the defects review found among them.

use std::collections::BTreeMap;

use super::answer::{Answer, At, Count};
use super::ask::Ask;
use super::link::Offset;
use super::msg::{Dim, Estimate, Place, Reason, Short, ToOwner, ToRunner};
use super::owner::{Act, Link, Machine, Owner, Size};
use super::{Mutant, planted};
use crate::facts::RunnerFacts;
use crate::sched::needs;
use crate::sched::state::{Cpus, Fence, Resources};

const GIB: u64 = 1 << 30;

fn res(cpus: u64, mem_gib: u64) -> Resources {
    Resources { cpu: Cpus::whole(cpus), mem: mem_gib * GIB, ..Resources::default() }
}

fn machine(name: &str, os: &str, arch: &str, cpus: u64, mem_gib: u64) -> Machine {
    let mut facts = RunnerFacts { os: os.into(), arch: arch.into(), ..RunnerFacts::default() };
    facts.cpu.threads = u32::try_from(cpus).unwrap_or(0);
    facts.mem.total = mem_gib * GIB;
    Machine { name: name.into(), facts, labels: vec![os.into()], size: Size::known(&res(cpus, mem_gib)), epoch: 1, connected: true, places: true, idle: None }
}

fn fleet() -> Vec<Machine> {
    vec![machine("lin-a", "linux", "x86_64", 32, 64), machine("lin-b", "linux", "x86_64", 16, 48), machine("mac", "macos", "aarch64", 18, 64), Machine { connected: false, ..machine("lin-c", "linux", "x86_64", 32, 64) }]
}

fn ask(run: &str, spelling: &str) -> Result<Ask, String> {
    Ok(Ask { run: run.into(), needs: needs::parse(spelling)?, repo: "acme/app".into(), actor: "agent:1".into(), ..Ask::default() })
}

fn owner() -> Owner {
    let mut o = Owner::new("me", 1);
    let _ = o.machines(fleet());
    o
}

/// The runners each act told something, and what.
fn told(acts: &[Act]) -> Vec<(String, &'static str)> {
    acts.iter()
        .filter_map(|a| match a {
            Act::Tell { runner, msg } => Some((
                runner.clone(),
                match msg {
                    ToRunner::Probe { .. } => "probe",
                    ToRunner::Withdraw { .. } => "withdraw",
                    ToRunner::Take { .. } => "take",
                    ToRunner::Decline { .. } => "decline",
                    ToRunner::Start { .. } => "start",
                    ToRunner::Ack { .. } => "ack",
                },
            )),
            _ => None,
        })
        .collect()
}

fn answer(acts: &[Act]) -> Option<&Answer> {
    acts.iter().rev().find_map(|a| if let Act::Answer { answer, .. } = a { Some(answer) } else { None })
}

fn waiting(run: &str, reason: Reason, place: u32, start: Estimate, free: Resources) -> ToOwner {
    ToOwner::Waiting { run: run.into(), reason, place: Place::of(place), start, predicted: None, free }
}

fn soon(at: i64) -> Estimate {
    Estimate { soonest: Some(at), latest: None }
}

fn short(dim: Dim, free: u64, ask: u64) -> Reason {
    Reason::Room { short: vec![Short { dim, free, ask }] }
}

fn offer(run: &str, fence: Fence) -> ToOwner {
    ToOwner::Offer { run: run.into(), fence, got: res(4, 0), free: res(8, 8), predicted: None, disk: Default::default() }
}

/// Never names the need no runner meets and the runner nearest to it; a Linux run is probed at the
/// Linux runners alone; one whose link is down isn't probed, and counts as link down.
#[test]
fn e5_never_names_the_need_and_a_linux_run_probes_only_linux_runners() -> Result<(), String> {
    let mut o = owner();
    let acts = o.ask(ask("r1", "linux/arm64")?, 0);
    match answer(&acts) {
        Some(Answer::Never { need, nearest }) => {
            assert_eq!(need, "meets all of os == linux, arch == aarch64");
            assert_eq!(nearest.as_deref(), Some("lin-a, linux/x86_64, 32 CPUs, 64 GiB of memory"));
        }
        other => return Err(format!("{other:?}")),
    }
    let acts = o.ask(ask("r2", "mem=512g")?, 0);
    assert!(matches!(answer(&acts), Some(Answer::Never { need, .. }) if need == "has room for 512 GiB of memory"), "{acts:?}");
    let acts = o.ask(ask("r3", "linux/amd64,cpus=8")?, 0);
    assert_eq!(told(&acts), vec![("lin-a".to_owned(), "probe"), ("lin-b".to_owned(), "probe")], "the Mac and the runner whose link is down aren't probed");
    Ok(())
}

/// Queued says each runner's reason counted (summing to the runners known), its best place, the
/// expected start and the nearest ask that would start now, once every runner probed has answered.
#[test]
fn e5_queued_counts_reasons_across_runners_with_place_start_and_nearest() -> Result<(), String> {
    let mut o = owner();
    let _ = o.ask(ask("r1", "linux/amd64,cpus=12,mem=40g")?, 0);
    let acts = o.heard("lin-a", waiting("r1", short(Dim::Cpu, 6_000, 12_000), 2, soon(180_000), res(6, 50)), 1);
    assert!(answer(&acts).is_none(), "lin-b hasn't answered yet");
    let acts = o.heard("lin-b", waiting("r1", short(Dim::Mem, 20 * GIB, 40 * GIB), 1, soon(540_000), res(16, 20)), 2);
    let Some(Answer::Queued { of, reasons, place, start, nearest }) = answer(&acts) else { return Err(format!("{acts:?}")) };
    assert_eq!(*of, 4);
    assert_eq!(reasons.iter().map(|c| c.runners).sum::<u32>(), 4, "the counts sum to the runners known");
    for (reason, n) in [("too few idle CPUs", 1), ("too little free memory", 1), ("not linux", 1), ("link down", 1)] {
        assert!(reasons.contains(&Count { reason: reason.into(), runners: n }), "{reason}: {reasons:?}");
    }
    assert_eq!(place.as_ref(), Some(&At { lo: 1, hi: 1, runner: "lin-b".into() }));
    assert_eq!(start.soonest, Some(180_000));
    assert_eq!(nearest.as_ref().map(|n| (n.ask, n.runner.as_str())), Some((res(6, 40), "lin-a")));
    let words = answer(&acts).map(|a| a.words(0)).unwrap_or_default();
    assert!(words.starts_with("queued: 0 of 4 can start it now (") && words.contains("1st at lin-b") && words.contains("cpus=6,mem=40g would start now on lin-a"), "{words}");
    Ok(())
}

/// Defects b and f: the first offer wins and the rest are withdrawn; held, the asker is told it's
/// starting, not started; started only on the runner's word; each change of a waiting run's
/// answer is news, and a word that changes nothing isn't.
#[test]
fn e5_starting_then_started_on_the_runner_s_word_and_news_on_change() -> Result<(), String> {
    let mut o = owner();
    let _ = o.ask(ask("r0", "linux/amd64,cpus=4")?, 0);
    let _ = o.heard("lin-a", waiting("r0", short(Dim::Cpu, 0, 4000), 3, soon(10), res(0, 8)), 1);
    let acts = o.heard("lin-b", waiting("r0", short(Dim::Cpu, 0, 4000), 5, soon(10), res(0, 8)), 1);
    assert!(matches!(answer(&acts), Some(Answer::Queued { .. })), "{acts:?}");
    let acts = o.heard("lin-a", waiting("r0", Reason::Line, 2, soon(10), res(0, 8)), 2);
    assert!(matches!(answer(&acts), Some(Answer::Queued { place: Some(At { lo: 2, .. }), .. })), "a better place is news: {acts:?}");
    assert!(answer(&o.heard("lin-a", waiting("r0", Reason::Line, 2, soon(10), res(0, 8)), 3)).is_none(), "nothing changed: no news");
    let (fa, fb) = (Fence { epoch: 1, n: 1 }, Fence { epoch: 7, n: 3 });
    let acts = o.heard("lin-a", offer("r0", fa), 4);
    assert_eq!(told(&acts), vec![("lin-a".to_owned(), "take"), ("lin-b".to_owned(), "withdraw"), ("lin-c".to_owned(), "withdraw")]);
    assert_eq!(told(&o.heard("lin-b", offer("r0", fb), 4)), vec![("lin-b".to_owned(), "decline")]);
    assert!(o.heard("lin-a", offer("r0", fa), 4).is_empty(), "doubled");
    let acts = o.heard("lin-a", ToOwner::Held { run: "r0".into(), fence: fa }, 5);
    assert!(matches!(answer(&acts), Some(Answer::Starting { runner, .. }) if runner == "lin-a"), "{acts:?}");
    assert!(acts.iter().any(|a| matches!(a, Act::Go { runner, fence, .. } if runner == "lin-a" && *fence == fa)));
    assert!(!o.starting("r0", "lin-b", fb), "not the runner holding it");
    assert!(o.starting("r0", "lin-a", fa));
    assert!(!o.starting("r0", "lin-a", fa), "a start goes once");
    let acts = o.heard("lin-a", ToOwner::Started { run: "r0".into(), fence: fa }, 6);
    assert!(matches!(answer(&acts), Some(Answer::Started { runner, .. }) if runner == "lin-a"), "{acts:?}");
    assert_eq!(o.stage("r0"), Some(("running", Some("lin-a"))));
    let acts = o.heard("lin-a", ToOwner::Ended { run: "r0".into(), fence: fa, ok: true }, 7);
    assert!(matches!(answer(&acts), Some(Answer::Ended { ok: true, .. })), "{acts:?}");
    assert_eq!(told(&acts), vec![("lin-a".to_owned(), "ack")]);
    // Planted: told started once held.
    let acts = planted(Some(Mutant::StartedAtHeld), || -> Result<Vec<Act>, String> {
        let mut o = owner();
        let _ = o.ask(ask("r1", "linux/amd64")?, 0);
        let _ = o.heard("lin-a", offer("r1", fa), 1);
        Ok(o.heard("lin-a", ToOwner::Held { run: "r1".into(), fence: fa }, 2))
    })?;
    assert!(matches!(answer(&acts), Some(Answer::Started { .. })), "StartedAtHeld planted, as the model catches it");
    Ok(())
}

/// Defects a and b: a ticket goes back to waiting only on its runner's word that it didn't start
/// it, the asker told where it stands instead of starting; a runner whose link is down keeps what
/// it holds; only its absence, or its new epoch, ends a claim there.
#[test]
fn e5_a_disconnect_waits_and_only_absence_or_a_new_epoch_ends_a_claim() -> Result<(), String> {
    let mut o = owner();
    let f = Fence { epoch: 1, n: 1 };
    let _ = o.ask(ask("r1", "linux/amd64,cpus=4")?, 0);
    let _ = o.heard("lin-a", offer("r1", f), 1);
    let _ = o.heard("lin-a", ToOwner::Held { run: "r1".into(), fence: f }, 2);
    assert!(o.starting("r1", "lin-a", f));
    let acts = o.heard("lin-a", ToOwner::Lapsed { run: "r1".into(), fence: f, why: "its claim lapsed".into() }, 3);
    assert_eq!(told(&acts), vec![("lin-a".to_owned(), "probe"), ("lin-b".to_owned(), "probe")]);
    assert!(matches!(answer(&acts), Some(Answer::Queued { .. })), "the asker, told it was starting, is told it waits: {acts:?}");
    // Started on lin-b, whose link then goes down: it waits for lin-b, in the directory or out of it.
    let h = Fence { epoch: 9, n: 1 };
    let _ = o.heard("lin-b", offer("r1", h), 6);
    let _ = o.heard("lin-b", ToOwner::Held { run: "r1".into(), fence: h }, 7);
    assert!(o.starting("r1", "lin-b", h));
    assert!(o.away("lin-b").is_empty());
    let down: Vec<Machine> = fleet().into_iter().map(|m| if m.name == "lin-b" { Machine { connected: false, ..m } } else { m }).collect();
    let acts = o.machines(down.clone());
    assert!(answer(&acts).is_none(), "{acts:?}");
    assert_eq!(o.stage("r1"), Some(("starting", Some("lin-b"))), "a runner whose link is down is still there");
    // Back: its start is said again, for its record to answer.
    let acts = o.machines(fleet());
    assert!(acts.iter().any(|a| matches!(a, Act::Restart { runner, fence, .. } if runner == "lin-b" && *fence == h)), "{acts:?}");
    // Gone from the directory: lost with it.
    let gone: Vec<Machine> = fleet().into_iter().filter(|m| m.name != "lin-b").collect();
    let acts = o.machines(gone);
    assert!(matches!(answer(&acts), Some(Answer::Lost { runner, .. }) if runner == "lin-b"), "{acts:?}");
    assert!(told(&acts).is_empty(), "no probe anywhere: it isn't started again");
    // Running on lin-a when lin-a starts again under a new epoch: lost.
    let mut o = owner();
    let _ = o.ask(ask("r2", "linux/amd64")?, 0);
    let _ = o.heard("lin-a", offer("r2", f), 1);
    let _ = o.heard("lin-a", ToOwner::Held { run: "r2".into(), fence: f }, 2);
    let _ = o.starting("r2", "lin-a", f);
    let _ = o.heard("lin-a", ToOwner::Started { run: "r2".into(), fence: f }, 3);
    let again: Vec<Machine> = fleet().into_iter().map(|m| if m.name == "lin-a" { Machine { epoch: 2, ..m } } else { m }).collect();
    let acts = o.machines(again);
    assert!(matches!(answer(&acts), Some(Answer::Lost { runner, .. }) if runner == "lin-a"), "{acts:?}");
    // Planted: a disconnect taken for gone loses what runs there.
    let lost = planted(Some(Mutant::LostOnDisconnect), || -> Result<bool, String> {
        let mut o = owner();
        let _ = o.ask(ask("r3", "linux/amd64")?, 0);
        let _ = o.heard("lin-b", offer("r3", h), 1);
        let _ = o.heard("lin-b", ToOwner::Held { run: "r3".into(), fence: h }, 2);
        let _ = o.starting("r3", "lin-b", h);
        Ok(matches!(answer(&o.machines(down)), Some(Answer::Lost { .. })))
    })?;
    assert!(lost, "LostOnDisconnect planted, as the model catches it");
    Ok(())
}

/// Defect c: a size not measured is unknown, never too small: the run is probed there, and the
/// runner answers for itself.
#[test]
fn e5_a_size_not_measured_is_unknown_never_too_small() -> Result<(), String> {
    let unmeasured = Machine { size: Size::default(), ..machine("fresh", "linux", "x86_64", 0, 0) };
    let mut o = Owner::new("me", 1);
    let _ = o.machines(vec![unmeasured.clone()]);
    let acts = o.ask(ask("r1", "cpus=64,mem=256g,disk=1024g")?, 0);
    assert_eq!(told(&acts), vec![("fresh".to_owned(), "probe")], "{acts:?}");
    let never = planted(Some(Mutant::NeverOnUnknown), || -> Result<bool, String> {
        let mut o = Owner::new("me", 1);
        let _ = o.machines(vec![unmeasured]);
        Ok(matches!(answer(&o.ask(ask("r1", "cpus=64")?, 0)), Some(Answer::Never { .. })))
    })?;
    assert!(never, "NeverOnUnknown planted, as the model catches it");
    Ok(())
}

/// Defect h: during a rolling upgrade, a run no runner that places could start now goes to an
/// older runner idle enough for it, by the shell's own pick; refused there, it waits at the runners
/// that place. Only older runners suited: it goes to them at once.
#[test]
fn e5_an_idle_older_runner_takes_what_the_placing_ones_cant_start() -> Result<(), String> {
    let older = Machine { places: false, idle: Some(res(16, 64)), ..machine("old", "linux", "x86_64", 16, 64) };
    let fleet = vec![machine("new", "linux", "x86_64", 32, 64), older.clone()];
    let first_answer = |o: &mut Owner| -> Result<Vec<Act>, String> {
        let _ = o.machines(fleet.clone());
        let _ = o.ask(ask("r1", "linux,cpus=4")?, 0);
        Ok(o.heard("new", waiting("r1", short(Dim::Cpu, 0, 4000), 1, soon(60_000), res(0, 8)), 1))
    };
    let mut o = Owner::new("me", 1);
    let acts = first_answer(&mut o)?;
    assert!(acts.iter().any(|a| matches!(a, Act::Fallback { runners, .. } if runners == &["old".to_owned()])), "{acts:?}");
    assert_eq!(o.stage("r1"), Some(("fallback", None)));
    // An offer meanwhile is left to lapse back into its runner's line.
    assert!(o.heard("new", offer("r1", Fence { epoch: 1, n: 1 }), 2).is_empty());
    let _ = o.fallback_refused("r1");
    assert_eq!(o.stage("r1"), Some(("waiting", None)));
    // Only older runners suited: handed to them at once.
    let mut o = Owner::new("me", 1);
    let _ = o.machines(vec![older]);
    assert!(matches!(o.ask(ask("r2", "linux")?, 0).last(), Some(Act::Fallback { .. })));
    // Planted: older runners never used while one places.
    let acts = planted(Some(Mutant::SkipOlder), || first_answer(&mut Owner::new("me", 1)))?;
    assert!(!acts.iter().any(|a| matches!(a, Act::Fallback { .. })), "SkipOlder wasn't caught");
    Ok(())
}

/// Defect j: a runner's times are shown in this node's clock, corrected by the link's measured
/// offset (RFC 5905); shown as they are, they'd be hours off.
#[test]
fn e5_estimates_are_corrected_to_the_asker_s_clock() -> Result<(), String> {
    let hours = 5 * 3_600_000;
    let check = |o: &mut Owner| -> Result<Option<i64>, String> {
        let _ = o.machines(vec![machine("lin-a", "linux", "x86_64", 8, 8)]);
        o.link("lin-a", Link { rtt: None, offset: Offset { theta: hours, delta: 2 } });
        let _ = o.ask(ask("r1", "cpus=4")?, 1_000);
        let acts = o.heard("lin-a", waiting("r1", Reason::Line, 2, soon(hours + 61_000), res(0, 8)), 1_001);
        Ok(match answer(&acts) {
            Some(Answer::Queued { start, .. }) => start.soonest,
            _ => None,
        })
    };
    assert_eq!(check(&mut Owner::new("me", 1))?, Some(61_000));
    assert_eq!(planted(Some(Mutant::SkewedClock), || check(&mut Owner::new("me", 1)))?, Some(hours + 61_000), "SkewedClock planted, as the model catches it");
    Ok(())
}

/// Task 4: an offer is declined, the run kept in that runner's line, when another runner is sure
/// to finish it sooner: its latest start plus its predicted run there before this one's predicted
/// end. With either prediction unknown, the first offer is taken.
#[test]
fn e5_an_offer_gives_way_to_a_runner_sure_to_finish_sooner() -> Result<(), String> {
    let mut o = owner();
    let _ = o.ask(ask("r1", "linux/amd64")?, 0);
    let fast = ToOwner::Waiting { run: "r1".into(), reason: Reason::Line, place: Place::of(1), start: Estimate { soonest: Some(30_000), latest: Some(60_000) }, predicted: Some(600_000), free: res(0, 0) };
    let _ = o.heard("lin-b", fast, 1);
    let slow = ToOwner::Offer { run: "r1".into(), fence: Fence { epoch: 1, n: 1 }, got: res(0, 0), free: res(8, 8), predicted: Some(3_600_000), disk: Default::default() };
    let acts = o.heard("lin-a", slow, 2);
    assert_eq!(told(&acts), vec![("lin-a".to_owned(), "decline")], "{acts:?}");
    assert_eq!(o.stage("r1"), Some(("waiting", None)));
    // lin-b's news (it slipped): lin-a is probed again, with no timer.
    let slipped = ToOwner::Waiting { run: "r1".into(), reason: Reason::Line, place: Place::of(4), start: Estimate { soonest: Some(30_000), latest: Some(9_000_000) }, predicted: Some(600_000), free: res(0, 0) };
    assert_eq!(told(&o.heard("lin-b", slipped, 3)), vec![("lin-a".to_owned(), "probe")]);
    let unknown = ToOwner::Offer { run: "r1".into(), fence: Fence { epoch: 1, n: 2 }, got: res(0, 0), free: res(8, 8), predicted: None, disk: Default::default() };
    assert!(told(&o.heard("lin-a", unknown, 4)).contains(&("lin-a".to_owned(), "take")));
    Ok(())
}

/// A cancelled ticket withdraws its probes and declines a claim it took; an offer for a ticket gone
/// is declined.
#[test]
fn e5_a_cancelled_ticket_lets_go_of_what_it_holds() -> Result<(), String> {
    let mut o = owner();
    let _ = o.ask(ask("r1", "linux/amd64")?, 0);
    assert_eq!(told(&o.cancel("r1")), vec![("lin-a".to_owned(), "withdraw"), ("lin-b".to_owned(), "withdraw"), ("lin-c".to_owned(), "withdraw")]);
    let f = Fence { epoch: 1, n: 1 };
    assert_eq!(told(&o.heard("lin-a", offer("r1", f), 1)), vec![("lin-a".to_owned(), "decline")]);
    let _ = o.ask(ask("r2", "linux/amd64")?, 2);
    let _ = o.heard("lin-a", offer("r2", f), 3);
    assert_eq!(told(&o.cancel("r2")), vec![("lin-a".to_owned(), "decline")]);
    Ok(())
}

mod props {
    use proptest::prelude::*;

    use super::*;

    /// What a runner may say, as the property drives it: an offer under a fence of its own, a
    /// hold of the last offer the owner took from it, a lapse of a claim it never started under,
    /// its process started, a place and reason in line, its link down or back, or its loss;
    /// delivered in any order, doubled and late.
    #[derive(Debug, Clone)]
    enum Ev {
        Offer { runner: u8, n: u8 },
        Held { runner: u8 },
        Lapse { runner: u8 },
        Start,
        Started { runner: u8 },
        Waiting { runner: u8, place: u8, line: bool },
        Away { runner: u8 },
        Back { runner: u8 },
        Lost { runner: u8 },
    }

    fn ev() -> impl Strategy<Value = Ev> {
        prop_oneof![
            (0..2u8, 0..4u8).prop_map(|(runner, n)| Ev::Offer { runner, n }),
            (0..2u8).prop_map(|runner| Ev::Held { runner }),
            (0..2u8).prop_map(|runner| Ev::Lapse { runner }),
            Just(Ev::Start),
            (0..2u8).prop_map(|runner| Ev::Started { runner }),
            (0..2u8, 1..20u8, any::<bool>()).prop_map(|(runner, place, line)| Ev::Waiting { runner, place, line }),
            (0..2u8).prop_map(|runner| Ev::Away { runner }),
            (0..2u8).prop_map(|runner| Ev::Back { runner }),
            (0..2u8).prop_map(|runner| Ev::Lost { runner }),
        ]
    }

    fn drive(evs: Vec<Ev>) -> Result<(), TestCaseError> {
        let names = ["lin-a", "lin-b"];
        let name = |r: u8| names.get(usize::from(r)).copied().unwrap_or("lin-a");
        let mut o = Owner::new("me", 1);
        let mut machines = vec![machine("lin-a", "linux", "x86_64", 32, 64), machine("lin-b", "linux", "x86_64", 16, 48)];
        let _ = o.machines(machines.clone());
        let _ = o.ask(ask("r1", "linux/amd64").map_err(TestCaseError::fail)?, 0);
        let mut started: Vec<(String, Fence)> = Vec::new();
        let mut lost: Vec<String> = Vec::new();
        let mut now = 0;
        for e in evs {
            now += 1;
            let fence = |r: u8, n: u8| Fence { epoch: u64::from(r) + 1, n: u64::from(n) };
            let acts = match e {
                Ev::Offer { runner, n } => o.heard(name(runner), ToOwner::Offer { run: "r1".into(), fence: fence(runner, n), got: res(1, 0), free: res(8, 8), predicted: None, disk: Default::default() }, now),
                Ev::Held { runner } => (0..4u8).flat_map(|n| o.heard(name(runner), ToOwner::Held { run: "r1".into(), fence: fence(runner, n) }, now)).collect(),
                Ev::Lapse { runner } => {
                    let mut acts = Vec::new();
                    for n in 0..4u8 {
                        // A runner never says a claim it started under lapsed.
                        if !started.contains(&(name(runner).to_owned(), fence(runner, n))) {
                            acts.extend(o.heard(name(runner), ToOwner::Lapsed { run: "r1".into(), fence: fence(runner, n), why: String::new() }, now));
                        }
                    }
                    acts
                }
                Ev::Start => {
                    for (r, idx) in [("lin-a", 0u8), ("lin-b", 1)] {
                        for n in 0..4u8 {
                            if o.starting("r1", r, fence(idx, n)) {
                                started.push((r.to_owned(), fence(idx, n)));
                            }
                        }
                    }
                    Vec::new()
                }
                Ev::Started { runner } => {
                    let mine: Vec<Fence> = started.iter().filter(|(r, _)| r == name(runner)).map(|(_, f)| *f).collect();
                    mine.into_iter().flat_map(|f| o.heard(name(runner), ToOwner::Started { run: "r1".into(), fence: f }, now)).collect()
                }
                Ev::Waiting { runner, place, line } => {
                    let reason = if line { Reason::Line } else { short(Dim::Cpu, 0, 1000) };
                    o.heard(name(runner), waiting("r1", reason, u32::from(place), soon(now + 60_000), res(0, 8)), now)
                }
                Ev::Away { runner } => o.away(name(runner)),
                Ev::Back { runner } => o.back(name(runner)),
                Ev::Lost { runner } => {
                    lost.push(name(runner).to_owned());
                    machines.retain(|m| m.name != name(runner));
                    o.machines(machines.clone())
                }
            };
            // A run's start goes to one runner, once.
            let runners: std::collections::BTreeSet<&String> = started.iter().map(|(r, _)| r).collect();
            prop_assert!(runners.len() <= 1, "started on {runners:?}");
            prop_assert!(started.len() <= 1, "started twice: {started:?}");
            // Started is told only for a start that went, on that runner's word.
            if let Some(Answer::Started { runner, .. }) = answer(&acts) {
                prop_assert!(started.iter().any(|(r, _)| r == runner), "told started on {runner}, where no start went");
            }
            // Lost only with a runner gone.
            if let Some(Answer::Lost { runner, .. }) = answer(&acts) {
                prop_assert!(lost.contains(runner), "lost on {runner}, still there");
            }
            // Every waiting asker hears each change of its answer.
            if let Some(now_answer) = o.answer_now("r1") {
                prop_assert_eq!(o.told("r1"), Some(&now_answer));
            }
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(5_000))]

        /// A run's start goes to one runner, once; it's told started only on that runner's word,
        /// lost only with its runner gone, and each change of its answer while it waits, over
        /// random interleavings of offers (together, doubled, late), holds, lapses, places, links
        /// down and back, and runners lost.
        #[test]
        fn e5_one_start_started_on_the_word_lost_only_when_gone_and_told(evs in proptest::collection::vec(ev(), 1..80)) {
            drive(evs)?;
        }
    }

    /// Defect f at the owner: one that tells the first answer and no news after is caught.
    #[test]
    fn e5_an_owner_telling_no_news_is_caught() {
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig { cases: 5_000, failure_persistence: None, ..ProptestConfig::default() });
        let found = planted(Some(Mutant::QuietAnswer), || runner.run(&proptest::collection::vec(ev(), 1..80), drive));
        assert!(found.is_err(), "QuietAnswer wasn't caught");
    }

    /// The fleet the property uses is the one the unit tests use, but for the runners away.
    #[test]
    fn e5_the_property_s_fleet_places() {
        let o = owner();
        assert!(o.runs().is_empty());
        let m: BTreeMap<String, bool> = fleet().into_iter().map(|m| (m.name, m.connected)).collect();
        assert_eq!(m.get("lin-c"), Some(&false));
    }
}
