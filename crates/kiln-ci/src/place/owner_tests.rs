//! The owner's tickets and answers (docs/reach.md §2, docs/needs.md §9 and §13), each criterion a
//! test, the defects review found among them.

use std::collections::BTreeMap;

use super::answer::{Answer, At, Count};
use super::ask::Ask;
use super::link::{Offset, Rtt};
use super::msg::{Dim, Estimate, Place, Reason, Short, ToOwner, ToRunner};
use super::owner::{Act, Link, Machine, Owner, Size};
use super::{Mutant, planted};
use crate::facts::RunnerFacts;
use crate::sched::needs;
use crate::sched::state::{Class, Cpus, Fence, Resources};

const GIB: u64 = 1 << 30;

fn res(cpus: u64, mem_gib: u64) -> Resources {
    Resources { cpu: Cpus::whole(cpus), mem: mem_gib * GIB, ..Resources::default() }
}

fn machine(name: &str, os: &str, arch: &str, cpus: u64, mem_gib: u64) -> Machine {
    let mut facts = RunnerFacts { os: os.into(), arch: arch.into(), ..RunnerFacts::default() };
    facts.cpu.threads = u32::try_from(cpus).unwrap_or(0);
    facts.mem.total = mem_gib * GIB;
    Machine { name: name.into(), facts, labels: vec![os.into()], size: Size::known(&res(cpus, mem_gib)), epoch: 1, connected: true, places: true, syncs: false, idle: None, fast: false }
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
                    ToRunner::Fast { .. } => "fast",
                    ToRunner::Sync => "sync",
                    ToRunner::FastRelease { .. } => "fast_release",
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
    ToOwner::Waiting { run: run.into(), reason, place: Place::of(place), start, predicted: None, free, pace: Default::default() }
}

fn soon(at: i64) -> Estimate {
    Estimate { soonest: Some(at), latest: None }
}

fn short(dim: Dim, free: u64, ask: u64) -> Reason {
    Reason::Room { short: vec![Short { dim, free, ask }] }
}

fn offer(run: &str, fence: Fence) -> ToOwner {
    ToOwner::Offer { run: run.into(), fence, got: res(4, 0), free: res(8, 8), predicted: None, disk: Default::default(), pace: Default::default() }
}

/// An offer from `runner`, and its hold let pass with no other answer: taken (P6: an offer is held
/// for the others probed, the unmeasured link's initial timeout at most).
fn offered(o: &mut Owner, runner: &str, run: &str, fence: Fence, now: i64) -> Vec<Act> {
    let mut acts = o.heard(runner, offer(run, fence), now);
    acts.extend(o.tick(now + i64::try_from(Rtt::INITIAL).unwrap_or(0)));
    acts
}

/// AUDIT L1, L2: a probed runner whose machine is strained, or whose health bars the work, isn't
/// waited at as a queue is: the owner probes others drawn afresh, as past a never, so retries
/// don't land where the gate would yield or the work would fail for the machine's sake.
#[test]
fn l1_l2_a_strained_or_barred_runner_is_widened_past() -> Result<(), String> {
    let mut o = Owner::new("me", 1);
    let _ = o.machines((0..4).map(|i| machine(&format!("lin-{i}"), "linux", "x86_64", 8, 16)).collect());
    let probes = |acts: &[Act]| told(acts).into_iter().filter(|(_, w)| *w == "probe").map(|(r, _)| r).collect::<Vec<String>>();
    let first = probes(&o.ask(ask("r1", "linux/amd64,cpus=2")?, 0));
    let [a, b] = first.as_slice() else { return Err(format!("two drawn: {first:?}")) };
    let _ = o.heard(a, waiting("r1", Reason::Strained { why: "stalled".into() }, 1, Estimate::default(), res(0, 0)), 1);
    let more = probes(&o.heard(b, waiting("r1", Reason::Barred { cause: "disk".into(), why: "its work volume ran full".into() }, 1, Estimate::default(), res(8, 16)), 2));
    assert_eq!(more.len(), 2, "the other two probed: {more:?}");
    assert!(more.iter().all(|r| !first.contains(r)), "{first:?} then {more:?}");
    Ok(())
}

/// Hex's capacity-view.md CV11: probed runners that say they aren't synced with their machine's
/// capacity keeper admit nothing new there, so the owner places the run elsewhere: it probes the
/// other two drawn afresh, as past strained runners. Were such a runner waited at as one short of
/// room is, the owner would wait where it probed.
#[test]
fn cv11_a_runner_not_synced_with_its_keeper_is_widened_past() -> Result<(), String> {
    let mut o = Owner::new("me", 1);
    let _ = o.machines((0..4).map(|i| machine(&format!("lin-{i}"), "linux", "x86_64", 8, 16)).collect());
    let probes = |acts: &[Act]| told(acts).into_iter().filter(|(_, w)| *w == "probe").map(|(r, _)| r).collect::<Vec<String>>();
    let first = probes(&o.ask(ask("r1", "linux/amd64,cpus=2")?, 0));
    let [a, b] = first.as_slice() else { return Err(format!("two drawn: {first:?}")) };
    let unsynced = || waiting("r1", Reason::Unsynced { why: "joining".into() }, 1, Estimate::default(), res(8, 16));
    let _ = o.heard(a, unsynced(), 1);
    let more = probes(&o.heard(b, unsynced(), 2));
    assert_eq!(more.len(), 2, "the other two probed: {more:?}");
    assert!(more.iter().all(|r| !first.contains(r)), "{first:?} then {more:?}");
    Ok(())
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

/// W05-R26: an owner keeps each runner's pace as its latest word says it, for the kernel to rank
/// by; a runner that never said one, or a name it doesn't know, is unmeasured.
#[test]
fn w05_r26_the_owner_keeps_each_runners_latest_pace() -> Result<(), String> {
    let mut o = owner();
    let _ = o.ask(ask("r1", "linux/amd64,cpus=12,mem=40g")?, 0);
    let fast = crate::pace::Pace { speed: 1400, slowness: 1100, disk_bps: 2_000_000_000 };
    let mut w = waiting("r1", short(Dim::Cpu, 6_000, 12_000), 2, soon(180_000), res(6, 50));
    if let ToOwner::Waiting { pace, .. } = &mut w {
        *pace = fast;
    }
    let _ = o.heard("lin-a", w, 1);
    assert_eq!(o.pace("lin-a"), fast);
    assert!(o.pace("lin-b").is_unmeasured() && o.pace("nobody").is_unmeasured());
    // Its next word says it's unmeasured again (a restart): so it is.
    let _ = o.heard("lin-a", waiting("r1", short(Dim::Cpu, 6_000, 12_000), 2, soon(180_000), res(6, 50)), 2);
    assert!(o.pace("lin-a").is_unmeasured());
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
    // Every runner probed has answered: the offer is taken at once, the other probe withdrawn (the
    // runner whose link is down was never probed: P6 probes two drawn among the linked).
    let acts = o.heard("lin-a", offer("r0", fa), 4);
    assert_eq!(told(&acts), vec![("lin-a".to_owned(), "take"), ("lin-b".to_owned(), "withdraw")]);
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
        let _ = offered(&mut o, "lin-a", "r1", fa, 1);
        Ok(o.heard("lin-a", ToOwner::Held { run: "r1".into(), fence: fa }, 2000))
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
    let _ = offered(&mut o, "lin-a", "r1", f, 1);
    let _ = o.heard("lin-a", ToOwner::Held { run: "r1".into(), fence: f }, 2);
    assert!(o.starting("r1", "lin-a", f));
    let acts = o.heard("lin-a", ToOwner::Lapsed { run: "r1".into(), fence: f, why: "its claim lapsed".into() }, 3);
    assert_eq!(told(&acts), vec![("lin-a".to_owned(), "probe"), ("lin-b".to_owned(), "probe")]);
    assert!(matches!(answer(&acts), Some(Answer::Queued { .. })), "the asker, told it was starting, is told it waits: {acts:?}");
    // Started on lin-b, whose link then goes down: it waits for lin-b, in the directory or out of it.
    let h = Fence { epoch: 9, n: 1 };
    let _ = offered(&mut o, "lin-b", "r1", h, 6);
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
    let _ = offered(&mut o, "lin-a", "r2", f, 1);
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
        let _ = offered(&mut o, "lin-b", "r3", h, 1);
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
    let fast = ToOwner::Waiting { run: "r1".into(), reason: Reason::Line, place: Place::of(1), start: Estimate { soonest: Some(30_000), latest: Some(60_000) }, predicted: Some(600_000), free: res(0, 0), pace: Default::default() };
    let _ = o.heard("lin-b", fast, 1);
    let slow = ToOwner::Offer { run: "r1".into(), fence: Fence { epoch: 1, n: 1 }, got: res(0, 0), free: res(8, 8), predicted: Some(3_600_000), disk: Default::default(), pace: Default::default() };
    let acts = o.heard("lin-a", slow, 2);
    assert_eq!(told(&acts), vec![("lin-a".to_owned(), "decline")], "{acts:?}");
    assert_eq!(o.stage("r1"), Some(("waiting", None)));
    // lin-b's news (it slipped): lin-a is probed again, with no timer.
    let slipped = ToOwner::Waiting { run: "r1".into(), reason: Reason::Line, place: Place::of(4), start: Estimate { soonest: Some(30_000), latest: Some(9_000_000) }, predicted: Some(600_000), free: res(0, 0), pace: Default::default() };
    assert_eq!(told(&o.heard("lin-b", slipped, 3)), vec![("lin-a".to_owned(), "probe")]);
    let unknown = ToOwner::Offer { run: "r1".into(), fence: Fence { epoch: 1, n: 2 }, got: res(0, 0), free: res(8, 8), predicted: None, disk: Default::default(), pace: Default::default() };
    assert!(told(&o.heard("lin-a", unknown, 4)).contains(&("lin-a".to_owned(), "take")));
    Ok(())
}

/// A cancelled ticket withdraws its probes and declines a claim it took; an offer for a ticket gone
/// is declined.
#[test]
fn e5_a_cancelled_ticket_lets_go_of_what_it_holds() -> Result<(), String> {
    let mut o = owner();
    let _ = o.ask(ask("r1", "linux/amd64")?, 0);
    assert_eq!(told(&o.cancel("r1")), vec![("lin-a".to_owned(), "withdraw"), ("lin-b".to_owned(), "withdraw")]);
    let f = Fence { epoch: 1, n: 1 };
    assert_eq!(told(&o.heard("lin-a", offer("r1", f), 1)), vec![("lin-a".to_owned(), "decline")]);
    let _ = o.ask(ask("r2", "linux/amd64")?, 2);
    let _ = o.heard("lin-a", offer("r2", f), 3);
    assert_eq!(told(&o.cancel("r2")), vec![("lin-a".to_owned(), "decline"), ("lin-b".to_owned(), "withdraw")], "an offer held while choosing is declined");
    let _ = o.ask(ask("r3", "linux/amd64")?, 4);
    let _ = offered(&mut o, "lin-a", "r3", f, 5);
    assert_eq!(told(&o.cancel("r3")), vec![("lin-a".to_owned(), "decline")]);
    Ok(())
}

/// `n` Linux runners of 8 CPUs and 16 GiB, linked and placing.
fn linux_fleet(n: usize) -> Vec<Machine> {
    (0..n).map(|j| machine(&format!("r{j}"), "linux", "x86_64", 8, 16)).collect()
}

fn probes(acts: &[Act]) -> Vec<String> {
    let mut v: Vec<String> = told(acts).into_iter().filter(|(_, what)| *what == "probe").map(|(r, _)| r).collect();
    v.sort_unstable();
    v
}

fn probes_of(acts: &[Act], run: &str) -> Vec<String> {
    let mut v: Vec<String> = acts.iter().filter_map(|a| match a {
        Act::Tell { runner, msg: ToRunner::Probe { ask, .. } } if ask.run == run => Some(runner.clone()),
        _ => None,
    }).collect();
    v.sort_unstable();
    v
}

/// docs/needs.md §11, AUDIT P6: a run is probed at two runners drawn at random among those that
/// suit it (Mitzenmacher's two choices), each runner drawn as often as the others; it waits where
/// they said it waits, and is probed further only when each probed said never or went away.
#[test]
fn p6_a_run_probes_two_drawn_at_random_and_further_only_on_never_or_away() -> Result<(), String> {
    let mut o = Owner::seeded("me", 1, 42);
    let _ = o.machines(linux_fleet(10));
    let mut hits: BTreeMap<String, u32> = BTreeMap::new();
    for i in 0..5_000 {
        let run = format!("x{i}");
        let p = probes(&o.ask(ask(&run, "linux")?, 0));
        assert_eq!(p.len(), 2, "{p:?}");
        assert_ne!(p.first(), p.get(1));
        for r in p {
            *hits.entry(r).or_default() += 1;
        }
        let _ = o.cancel(&run);
    }
    // Each runner is drawn with chance 1/5 a run: 1,000 of 5,000 expected, sd 28.
    assert_eq!(hits.len(), 10);
    assert!(hits.values().all(|h| (880..=1_120).contains(h)), "{hits:?}");
    // Both say it waits: no more probes; the asker told it's queued.
    let p = probes(&o.ask(ask("w", "linux")?, 0));
    let mut acts = Vec::new();
    for r in &p {
        acts.extend(o.heard(r, waiting("w", Reason::Line, 2, soon(10), res(0, 8)), 1));
    }
    assert!(probes(&acts).is_empty(), "a run that waits stays queued there: {acts:?}");
    assert!(matches!(answer(&acts), Some(Answer::Queued { of: 10, .. })), "{acts:?}");
    // Both say never: two more are probed, none of the two.
    let p = probes(&o.ask(ask("n", "linux")?, 0));
    let mut acts = Vec::new();
    for r in &p {
        acts.extend(o.heard(r, ToOwner::Never { run: "n".into(), why: "no".into() }, 1));
    }
    let more = probes(&acts);
    assert_eq!(more.len(), 2, "{acts:?}");
    assert!(more.iter().all(|r| !p.contains(r)), "{p:?} then {more:?}");
    // One says never and the other's link goes down: two more.
    let p = probes(&o.ask(ask("a", "linux")?, 0));
    let (Some(first), Some(second)) = (p.first(), p.get(1)) else { return Err(format!("{p:?}")) };
    assert!(probes(&o.heard(first, ToOwner::Never { run: "a".into(), why: "no".into() }, 1)).is_empty(), "the other may still take it");
    let more = probes_of(&o.away(second), "a");
    assert_eq!(more.len(), 2, "{more:?}");
    assert!(more.iter().all(|r| !p.contains(r)));
    assert_eq!(o.probed("a").len(), 4);
    Ok(())
}

/// docs/needs.md §5 and §11: of the offers held, the emptiest by the engine's fullness is taken
/// and the other declined; between offers as full, `interactive` (with `land` and `critical`) takes
/// the most room, `batch` and `background` the least.
#[test]
fn p6_the_emptiest_offer_is_taken_and_ties_go_by_class() -> Result<(), String> {
    let two = |o: &mut Owner, run: &str, class: Class| -> Result<Vec<String>, String> {
        let _ = o.machines(linux_fleet(2));
        Ok(probes(&o.ask(Ask { class, ..ask(run, "linux")? }, 0)))
    };
    let offer_free = |run: &str, n: u64, free: Resources| ToOwner::Offer { run: run.into(), fence: Fence { epoch: 1, n }, got: res(1, 0), free, predicted: None, disk: Default::default(), pace: Default::default() };
    let mut o = Owner::new("me", 1);
    assert_eq!(two(&mut o, "r1", Class::Batch)?, vec!["r0".to_owned(), "r1".to_owned()]);
    // r0 offers first, three quarters full; r1 a quarter: r1's is taken, r0's declined.
    assert!(told(&o.heard("r0", offer_free("r1", 1, res(2, 16)), 1)).is_empty(), "held for the other");
    let acts = o.heard("r1", offer_free("r1", 2, res(6, 16)), 1);
    assert_eq!(told(&acts), vec![("r1".to_owned(), "take"), ("r0".to_owned(), "decline")]);
    // As full (half, by CPUs), one with all its memory free and one with half.
    for (class, want) in [(Class::Interactive, "r0"), (Class::Land, "r0"), (Class::Critical, "r0"), (Class::Batch, "r1"), (Class::Background, "r1")] {
        let mut o = Owner::new("me", 1);
        let _ = two(&mut o, "t", class)?;
        let _ = o.heard("r0", offer_free("t", 1, res(4, 16)), 1);
        let acts = o.heard("r1", offer_free("t", 2, res(4, 8)), 1);
        assert!(told(&acts).contains(&(want.to_owned(), "take")), "{class:?}: {acts:?}");
    }
    assert_eq!(super::owner::fullness(&Size::known(&res(8, 16)), &res(4, 8)), 500_000);
    assert_eq!(super::owner::fullness(&Size::default(), &res(4, 8)), u64::MAX, "nothing known ranks last");
    Ok(())
}

/// RFC 6298 and P6: an offer is held, while another runner probed hasn't answered, its link's
/// timeout less its round trip (so a take reaches the runner within the timeout it holds the offer
/// for): the owner's timer says when, and then the offer is taken.
#[test]
fn p6_an_offer_is_held_its_timeout_less_its_round_trip() -> Result<(), String> {
    let mut o = Owner::new("me", 1);
    let _ = o.machines(linux_fleet(2));
    let rtt = Rtt { srtt: 10, rttvar: 5, granularity: 1 };
    o.link("r0", Link { rtt: Some(rtt), offset: Offset::default() });
    let _ = o.ask(ask("r1", "linux")?, 0);
    assert!(o.heard("r0", offer("r1", Fence { epoch: 1, n: 1 }), 100).is_empty());
    assert_eq!(o.timer(), Some(120), "RTO 30 less SRTT 10");
    assert!(o.tick(119).is_empty());
    assert_eq!(told(&o.tick(120)), vec![("r0".to_owned(), "take"), ("r1".to_owned(), "withdraw")]);
    assert_eq!(o.stage("r1"), Some(("taking", Some("r0"))));
    Ok(())
}

/// docs/needs.md §11, P6 at high load: a run every probed runner holds full is probed further,
/// by data: at the runners whose latest published room fits it, and only those (d of them drawn
/// when more do); none does, and it waits where it is.
#[test]
fn p6_both_full_widens_only_to_runners_hinting_room() -> Result<(), String> {
    let full = |o: &mut Owner, run: &str| -> Result<(Vec<String>, Vec<Act>), String> {
        let p = probes(&o.ask(ask(run, "linux,cpus=4")?, 0));
        let mut acts = Vec::new();
        for r in &p {
            acts.extend(o.heard(r, waiting(run, short(Dim::Cpu, 0, 4000), 1, soon(10), res(0, 8)), 1));
        }
        Ok((p, acts))
    };
    // No hints: it waits at both, nothing more said to runners.
    let mut o = Owner::seeded("me", 1, 3);
    let _ = o.machines(linux_fleet(10));
    let (_, acts) = full(&mut o, "a")?;
    assert!(probes(&acts).is_empty(), "{acts:?}");
    assert!(matches!(answer(&acts), Some(Answer::Queued { .. })));
    // One runner hints room for 4 CPUs, one room for 2 only, one an older hint of room it has
    // since said is gone: only the first is probed.
    let mut o = Owner::seeded("me", 1, 3);
    let _ = o.machines(linux_fleet(10));
    let p = probes(&o.ask(ask("b", "linux,cpus=4")?, 0));
    let others: Vec<String> = (0..10).map(|j| format!("r{j}")).filter(|r| !p.contains(r)).collect();
    let (Some(roomy), Some(small), Some(stale)) = (others.first(), others.get(1), others.get(2)) else { return Err(format!("{others:?}")) };
    o.hint(roomy, res(6, 16), 1);
    o.hint(small, res(2, 16), 1);
    o.hint(stale, res(0, 16), 2);
    o.hint(stale, res(8, 16), 1);
    let mut acts = Vec::new();
    for r in &p {
        acts.extend(o.heard(r, waiting("b", short(Dim::Cpu, 0, 4000), 1, soon(10), res(0, 8)), 1));
    }
    assert_eq!(probes_of(&acts, "b"), vec![roomy.clone()], "{acts:?}");
    // It says it waits too: no runner left hinting room, it waits at the three.
    let acts = o.heard(roomy, waiting("b", short(Dim::Cpu, 0, 4000), 1, soon(10), res(0, 8)), 2);
    assert!(probes(&acts).is_empty(), "{acts:?}");
    // Many hint room: two drawn among them, none of the rest.
    let mut o = Owner::seeded("me", 1, 3);
    let _ = o.machines(linux_fleet(10));
    for j in 0..10 {
        o.hint(&format!("r{j}"), res(8, 16), 1);
    }
    let (p, acts) = full(&mut o, "c")?;
    let more = probes_of(&acts, "c");
    assert_eq!(more.len(), 2, "{acts:?}");
    assert!(more.iter().all(|r| !p.contains(r)));
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
        /// The time passes an offer's hold (P6).
        Tick,
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
            Just(Ev::Tick),
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
                Ev::Offer { runner, n } => o.heard(name(runner), ToOwner::Offer { run: "r1".into(), fence: fence(runner, n), got: res(1, 0), free: res(8, 8), predicted: None, disk: Default::default(), pace: Default::default() }, now),
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
                Ev::Tick => {
                    now += i64::try_from(Rtt::INITIAL).unwrap_or(0);
                    o.tick(now)
                }
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

/// W05-R13, docs/needs.md E5: a run waits for what's so, never for an answer that can't come. A
/// probe unanswered on a link that stays up (a runner that took this owner for away, a word lost
/// on the way: "haven't answered" all day) is said again each time its link's timeout runs out, the
/// timeout doubled each time (RFC 6298 §5.5) to at most a minute (§2.5); once the runner answers,
/// or the run is placed, it isn't said again.
#[test]
fn w05_r13_an_unanswered_probe_is_said_again_backing_off_until_answered() -> Result<(), String> {
    let mut o = owner();
    assert_eq!(probes(&o.ask(ask("r1", "linux/amd64,cpus=4")?, 0)), vec!["lin-a", "lin-b"]);
    let _ = o.heard("lin-a", waiting("r1", Reason::Line, 2, soon(10), res(0, 8)), 1);
    // lin-b never answers: said again at 1 s (an unmeasured link's timeout, §2.1), then 2, 4, ...
    let mut at = 0i64;
    for n in 0..8u32 {
        let due = at.saturating_add(i64::try_from(Rtt::backed_off(Rtt::INITIAL, n)).map_err(|e| e.to_string())?);
        assert!(probes(&o.tick(due - 1)).is_empty(), "not before its timeout ({n})");
        assert_eq!(probes(&o.tick(due)), vec!["lin-b"], "said again at its timeout, lin-a's answer kept ({n})");
        at = due;
    }
    assert_eq!(at, 1_000 + 2_000 + 4_000 + 8_000 + 16_000 + 32_000 + 60_000 + 60_000, "a minute at most");
    // Answered: not said again.
    let _ = o.heard("lin-b", waiting("r1", Reason::Line, 3, soon(10), res(0, 8)), at + 1);
    assert!(probes(&o.tick(at + 3_600_000)).is_empty(), "answered");
    // Placed while another probe is unanswered: that one isn't said again.
    let mut o = owner();
    let _ = o.ask(ask("r2", "linux/amd64")?, 0);
    let _ = offered(&mut o, "lin-a", "r2", Fence { epoch: 1, n: 1 }, 1);
    assert_eq!(o.stage("r2"), Some(("taking", Some("lin-a"))));
    assert!(probes(&o.tick(3_600_000)).is_empty(), "placed");
    Ok(())
}

/// W05-R03: a claim holds a runner's room for a run that starts there. A run the shell gives up
/// before its start is said (its tree couldn't be copied there, its link or custody wouldn't take
/// it, what asked for it went away) declines its claim, so its room is free there at once; held
/// for good instead, every run asking room there waits behind it. A cancel once its start went out
/// leaves the claim to its runner's word, as before. A go nothing took gives up a run told to go
/// once, never one whose start is said again.
#[test]
fn w05_r03_a_run_given_up_before_its_start_lets_its_claim_go() -> Result<(), String> {
    let f = Fence { epoch: 1, n: 1 };
    let declined = |acts: &[Act], to: &str| acts.iter().any(|a| matches!(a, Act::Tell { runner, msg: ToRunner::Decline { fence, .. } } if runner == to && *fence == f));
    let at = |o: &mut Owner, run: &str, start: bool| -> Result<(), String> {
        let _ = o.ask(ask(run, "linux/amd64,cpus=12,mem=40g")?, 0);
        let _ = offered(o, "lin-a", run, f, 1);
        let _ = o.heard("lin-a", ToOwner::Held { run: run.into(), fence: f }, 2);
        if start && !o.starting(run, "lin-a", f) {
            return Err(format!("{run} didn't start"));
        }
        Ok(())
    };
    // Given up as its tree was copied: declined where it's held, its ticket gone.
    let mut o = owner();
    at(&mut o, "r1", true)?;
    let acts = o.unsaid("r1");
    assert!(declined(&acts, "lin-a"), "{acts:?}");
    assert_eq!(o.stage("r1"), None);
    // Cancelled once its start went out: the runner's word ends it.
    at(&mut o, "r2", true)?;
    assert!(!declined(&o.cancel("r2"), "lin-a"));
    // A go nothing took, told once: given up.
    at(&mut o, "r3", false)?;
    let acts = o.undriven("r3").unwrap_or_default();
    assert!(declined(&acts, "lin-a"), "{acts:?}");
    assert_eq!(o.stage("r3"), None);
    // A start said again that nothing took: its start went out, left to its runner.
    at(&mut o, "r4", true)?;
    assert!(o.undriven("r4").is_none());
    assert_eq!(o.stage("r4"), Some(("starting", Some("lin-a"))));
    Ok(())
}

/// W05-R25: a runner started again ends its old process's claims, not those its new one made. A
/// probe said again can reach the new process before its new epoch reaches this owner, and what it
/// starts there under its new epoch runs on (kiln-sim place, seed 157).
#[test]
fn w05_r25_a_restarted_runner_s_new_claims_are_its_own() -> Result<(), String> {
    let mut o = owner();
    let _ = o.ask(ask("r1", "linux/amd64")?, 0);
    let f = Fence { epoch: 2, n: 1 };
    let _ = offered(&mut o, "lin-a", "r1", f, 1);
    let _ = o.heard("lin-a", ToOwner::Held { run: "r1".into(), fence: f }, 2);
    assert!(o.starting("r1", "lin-a", f));
    let _ = o.heard("lin-a", ToOwner::Started { run: "r1".into(), fence: f }, 3);
    let again: Vec<Machine> = fleet().into_iter().map(|m| if m.name == "lin-a" { Machine { epoch: 2, ..m } } else { m }).collect();
    let acts = o.machines(again);
    assert!(answer(&acts).is_none(), "{acts:?}");
    assert_eq!(o.stage("r1"), Some(("running", Some("lin-a"))));
    Ok(())
}
