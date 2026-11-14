//! The runner's ledger beside its machine's capacity keeper (docs/lb.md "A machine's keeper"; Hex's
//! docs/design/capacity-view.md CV3, CV7, CV8, CV10, CV11, and the holder's items; kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 13), each part a test: none known, it admits on its own
//! room; unsynced, nothing new, saying why, its kernel cordoned and what runs going on; synced, only
//! within the room the keeper's view leaves it, its CPUs never overcommitted past that room, no
//! GPUs, no more claims than its holder may reserve, nothing new while the view is over, and what
//! fits offered in the input that let it; a reserve not granted, its claim lapsed and the run
//! waiting again, or a kernel start declined.

use super::ask::{Ask, Peer, Sample};
use super::keeper::{Keeper, NotGranted, Unsynced, View, WaitEnded, Waits};
use super::msg::{FastWhy, Reason, ToOwner};
use super::runner::{Admission, Init, Quote, Records, Said, Settings, Start};
use crate::facts::RunnerFacts;
use crate::lb;
use crate::sched::needs;
use crate::sched::state::{Cpus, Fence, PerMille, Resources};

const GIB: u64 = 1 << 30;
const O1: Peer<'static> = Peer { name: "o1", epoch: 1 };

fn res(cpus: u64, mem_gib: u64) -> Resources {
    Resources { cpu: Cpus::whole(cpus), mem: mem_gib * GIB, ..Resources::default() }
}

/// A runner on an idle Linux machine of `cpus` and `mem_gib` with 500 GiB of disk, its CPUs
/// overcommitted as the person's `overcommit` says.
fn runner(cpus: u64, mem_gib: u64, overcommit: PerMille) -> Admission {
    let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
    let sample = Sample { size: Resources { disk: 500 * GIB, ..res(cpus, mem_gib) }, ..Sample::default() };
    let settings = Settings { overcommit_cpu: overcommit, ..Settings::default() };
    Admission::new(Init { me: "linux-1".into(), epoch: 1, since: None, facts, labels: vec!["linux".into()], sample, records: Records::new(), settings }, 0)
}

/// The keeper's view of a machine of `cpus` and `mem_gib` with 500 GiB of disk, where everyone
/// else holds `others` and this runner's holder `own` (CPUs, GiB), its holder's count of claims
/// past any these tests reach.
fn synced(cpus: u64, mem_gib: u64, (others_cpus, others_gib): (u64, u64), (own_cpus, own_gib): (u64, u64)) -> Keeper {
    let held = [(others_cpus + own_cpus) * 1000, (others_gib + own_gib) * GIB, 0, 0];
    Keeper::Synced(View { room: [cpus * 1000, mem_gib * GIB, 500 * GIB, 0], held, own: [own_cpus * 1000, own_gib * GIB, 0, 0], most_claims: u64::MAX })
}

/// `keeper`, its holder's count of the claims it may hold live set to `most`.
fn most(keeper: Keeper, most: u64) -> Keeper {
    match keeper {
        Keeper::Synced(view) => Keeper::Synced(View { most_claims: most, ..view }),
        other => other,
    }
}

/// The CPUs and memory of the room it admits against now.
fn room(a: &Admission) -> (u64, u64) {
    let ([cpu, mem, ..], _) = a.room();
    (cpu, mem)
}

fn ask(run: &str, spelling: &str) -> Result<Ask, String> {
    Ok(Ask { run: run.into(), needs: needs::parse(spelling)?, repo: "acme/app".into(), actor: "agent:1".into(), ..Ask::default() })
}

fn probe(a: &mut Admission, run: &str, spelling: &str, now: i64) -> Result<Vec<Said>, String> {
    let mut out = Vec::new();
    a.probe(O1, ask(run, spelling)?, None, now, &mut out);
    Ok(out)
}

fn said(a: &mut Admission, keeper: Keeper, now: i64) -> Vec<Said> {
    let mut out = Vec::new();
    a.keeper_said(keeper, now, &mut out);
    out
}

fn offer(said: &[Said], run: &str) -> Option<Fence> {
    said.iter().find_map(|(_, m)| match m {
        ToOwner::Offer { run: r, fence, .. } if r == run => Some(*fence),
        _ => None,
    })
}

fn waiting(said: &[Said], run: &str) -> Option<Reason> {
    said.iter().rev().find_map(|(_, m)| match m {
        ToOwner::Waiting { run: r, reason, .. } if r == run => Some(reason.clone()),
        _ => None,
    })
}

fn never(said: &[Said], run: &str) -> Option<String> {
    said.iter().find_map(|(_, m)| match m {
        ToOwner::Never { run: r, why } if r == run => Some(why.clone()),
        _ => None,
    })
}

/// Takes and starts an offered run, as its owner would.
fn run_it(a: &mut Admission, run: &str, fence: Fence, now: i64) -> Result<(), String> {
    let mut out = Vec::new();
    a.take(O1, run, fence, now, &mut out);
    match a.start(O1, run, fence, now, &mut out) {
        Some(Start::New) => Ok(()),
        other => Err(format!("{run}: {other:?}: {out:?}")),
    }
}

fn running(a: &Admission, run: &str) -> bool {
    a.probes().iter().any(|p| p.run == run && p.state == "running")
}

/// CV10, kiln's opt-in rule: a runner knows no keeper until its shell names one, and admits on its
/// own room. Synced with a keeper whose view leaves it 2 CPUs, a run of 6 waits there for room; the
/// keeper taken out of its configuration, its own room is what it admits against again, and the run
/// is offered in that input.
#[test]
fn cv10_with_no_keeper_known_it_admits_on_its_own_room() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    assert_eq!(a.keeper(), Keeper::None, "off until its shell names a keeper");
    let _ = said(&mut a, synced(8, 16, (6, 0), (0, 0)), 0);
    let w = probe(&mut a, "r1", "cpus=6", 1)?;
    assert!(matches!(waiting(&w, "r1"), Some(Reason::Room { .. })), "6 CPUs past the 2 the keeper leaves it: {w:?}");
    let back = said(&mut a, Keeper::None, 2);
    assert!(offer(&back, "r1").is_some(), "its own 8 CPUs again: {back:?}");
    a.check()
}

/// CV11: a runner that knows its machine's keeper and isn't synced with it offers nothing new: a
/// probe waits there saying why, its room is hinted as none, its kernel's word says no room (so
/// owners' kernels pick elsewhere), its quotes wait and it says it isn't taking; the same word again
/// changes nothing. Once the keeper's sync is answered, what fits is offered in that same input,
/// nobody polling.
#[test]
fn cv11_unsynced_it_offers_nothing_and_says_why_until_synced() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, Keeper::Unsynced(Unsynced::Joining), 0);
    let w = probe(&mut a, "r1", "cpus=2", 1)?;
    assert!(offer(&w, "r1").is_none(), "{w:?}");
    assert!(matches!(waiting(&w, "r1"), Some(Reason::Unsynced { why }) if why == Unsynced::Joining.why()), "{w:?}");
    assert_eq!(a.hint(), Resources::default(), "its room hinted as none");
    assert_eq!(a.fast_word().room, [0; 4], "its kernel cordoned: its word says no room");
    assert!(matches!(a.quote(&ask("q", "cpus=1")?, 1), Quote::Waits { reason: Reason::Unsynced { .. }, .. }));
    assert!(matches!(a.taking(), Err(Reason::Unsynced { .. })));
    assert!(said(&mut a, Keeper::Unsynced(Unsynced::Joining), 1).is_empty(), "the same word again is no news");
    a.check()?;
    let up = said(&mut a, synced(8, 16, (0, 0), (0, 0)), 2);
    assert!(offer(&up, "r1").is_some(), "offered once synced: {up:?}");
    assert_eq!(a.taking(), Ok(()));
    a.check()
}

/// CV3: synced, it admits only within the room the keeper's view leaves it, the machine's room less
/// what everyone else holds: of 16 CPUs and 64 GiB, with Hex's own boxes holding 10 CPUs and 40 GiB,
/// 6 CPUs and 24 GiB. A run of 4 CPUs and 16 GiB is offered; once the keeper books it as this
/// runner's own it stays out of what others hold, and a run of 4 more waits for room though the
/// machine's own 16 CPUs are idle.
#[test]
fn cv3_synced_it_admits_only_within_the_room_the_keepers_view_leaves_it() -> Result<(), String> {
    let mut a = runner(16, 64, 1000);
    let _ = said(&mut a, synced(16, 64, (10, 40), (0, 0)), 0);
    assert_eq!(room(&a), (6_000, 24 * GIB), "its room: the keeper's room less what Hex holds");
    let w = probe(&mut a, "r1", "cpus=4,mem=16g", 1)?;
    let f = offer(&w, "r1").ok_or(format!("{w:?}"))?;
    run_it(&mut a, "r1", f, 1)?;
    let _ = said(&mut a, synced(16, 64, (10, 40), (4, 16)), 2);
    assert_eq!(room(&a), (6_000, 24 * GIB), "its own reservation counted once, as its claim");
    let w = probe(&mut a, "r2", "cpus=4", 3)?;
    assert!(matches!(waiting(&w, "r2"), Some(Reason::Room { .. })), "2 CPUs left of the 6: {w:?}");
    a.check()
}

/// CV3: the keeper grants no reserve past its room, so the person's CPU overcommit stops at the room
/// the keeper's view leaves it: on 8 CPUs overcommitted twice, 12 fit with no keeper known, and
/// through a keeper 8 do and 12 wait for room.
#[test]
fn cv3_its_cpus_are_never_overcommitted_past_the_keepers_room() -> Result<(), String> {
    let mut a = runner(8, 16, 2000);
    assert_eq!(a.quote(&ask("q", "cpus=12")?, 0), Quote::Now, "overcommitted twice, 12 of 8 fit");
    let _ = said(&mut a, synced(8, 16, (0, 0), (0, 0)), 1);
    assert!(matches!(a.quote(&ask("q", "cpus=12")?, 1), Quote::Waits { reason: Reason::Room { .. }, .. }), "past the keeper's room");
    assert_eq!(a.quote(&ask("q", "cpus=8")?, 1), Quote::Now);
    a.check()
}

/// CV3, CV5: Hex admits 4 CPUs of its own while a run here holds 6 of 8, so the room the keeper's
/// view leaves this runner, 4, is under what its claims hold: the view is over, nothing new is
/// admitted, and the run goes on, its claim within the runner's own budget (G1). As it ends, the run
/// waiting is offered in that input: its release is said on the one link before any later reserve, so
/// by that reserve the keeper holds the 4 CPUs it leaves. Once its holder gives the reservation back,
/// the keeper's books of it count as another's until its word drops it, and nothing more is offered.
#[test]
fn cv3_over_its_room_it_admits_nothing_new_and_what_runs_goes_on() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, synced(8, 16, (0, 0), (0, 0)), 0);
    let w = probe(&mut a, "r1", "cpus=6", 1)?;
    let f = offer(&w, "r1").ok_or(format!("{w:?}"))?;
    run_it(&mut a, "r1", f, 1)?;
    let _ = said(&mut a, synced(8, 16, (0, 0), (6, 0)), 2);
    let _ = said(&mut a, synced(8, 16, (4, 0), (6, 0)), 3);
    let w = probe(&mut a, "r2", "cpus=1", 4)?;
    assert!(matches!(waiting(&w, "r2"), Some(Reason::Room { .. })), "nothing new while over: {w:?}");
    assert!(running(&a, "r1"), "r1 runs on");
    a.check()?;
    let mut out = Vec::new();
    a.ended(O1, "r1", true, 5, &mut out);
    assert!(offer(&out, "r2").is_some(), "the keeper's 4 CPUs once r1's release is in: {out:?}");
    let w = probe(&mut a, "r3", "cpus=1", 6)?;
    assert!(offer(&w, "r3").is_some(), "{w:?}");
    let _ = said(&mut a, synced(8, 16, (4 + 6, 0), (0, 0)), 7);
    let w = probe(&mut a, "r4", "cpus=1", 8)?;
    assert!(matches!(waiting(&w, "r4"), Some(Reason::Room { .. })), "r1's release not yet done: {w:?}");
    a.check()
}

/// CV3: a keeper grants no holder GPUs by count (a GPU is held by its identity, which a count can't
/// say), and a run starts here only on its reserve's grant: where a keeper is known, synced or not,
/// a run asking GPUs never runs here, and says why.
#[test]
fn cv3_a_run_asking_gpus_never_runs_where_a_keeper_is_known() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, Keeper::Unsynced(Unsynced::Joining), 0);
    let w = probe(&mut a, "g1", "cpus=1,gpus=1", 1)?;
    assert!(never(&w, "g1").is_some_and(|why| why.contains("capacity keeper")), "{w:?}");
    let _ = said(&mut a, synced(8, 16, (0, 0), (0, 0)), 2);
    assert!(matches!(a.quote(&ask("q", "gpus=1")?, 2), Quote::Never { why } if why.contains("capacity keeper")));
    a.check()
}

/// CV11: a link to the keeper that drops while a run goes on leaves the run running, held in the
/// keeper's books: the runner cordons its kernel and offers nothing new, so a run that ends there
/// frees nothing for the run waiting. A newer process of the runner taking its place with the keeper
/// changes why it takes nothing new, which it says from then; to the run already told it waits
/// unsynced that is no news, as a strain's why moving isn't.
#[test]
fn cv11_unsynced_it_cordons_its_kernel_and_what_runs_goes_on() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, synced(8, 16, (0, 0), (0, 0)), 0);
    let w = probe(&mut a, "r1", "cpus=2", 1)?;
    let f = offer(&w, "r1").ok_or(format!("{w:?}"))?;
    run_it(&mut a, "r1", f, 1)?;
    let _ = said(&mut a, synced(8, 16, (0, 0), (2, 0)), 2);
    let [cpu, ..] = a.fast_word().room;
    assert_eq!(cpu, 6_000, "synced, nothing waiting: its kernel says its room");
    let _ = said(&mut a, Keeper::Unsynced(Unsynced::Joining), 3);
    assert_eq!(a.fast_word().room, [0; 4], "its kernel cordoned");
    assert!(running(&a, "r1"), "r1 runs on");
    let w = probe(&mut a, "r2", "cpus=1", 4)?;
    assert!(matches!(waiting(&w, "r2"), Some(Reason::Unsynced { .. })), "{w:?}");
    let mut out = Vec::new();
    a.ended(O1, "r1", true, 5, &mut out);
    assert!(offer(&out, "r2").is_none(), "nothing new while unsynced: {out:?}");
    let news = said(&mut a, Keeper::Unsynced(Unsynced::Superseded), 6);
    assert!(news.is_empty(), "{news:?}");
    assert!(matches!(a.taking(), Err(Reason::Unsynced { why }) if why == Unsynced::Superseded.why()));
    a.check()
}

/// The holder's items 1 and 6 (Hex's capacity-view.md): a run starts here only on its reserve's
/// grant, and no reserve is said while the keeper isn't synced, so a start under a claim taken while
/// synced lapses once it isn't: nothing starts and nothing is recorded, the run waits here again
/// saying why, and its owner may place it elsewhere. Synced again, the run is offered again.
#[test]
fn cv11_a_start_while_unsynced_lapses_and_starts_nothing() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, synced(8, 16, (0, 0), (0, 0)), 0);
    let w = probe(&mut a, "r1", "cpus=2", 1)?;
    let f = offer(&w, "r1").ok_or(format!("{w:?}"))?;
    let mut out = Vec::new();
    a.take(O1, "r1", f, 2, &mut out);
    let _ = said(&mut a, Keeper::Unsynced(Unsynced::Joining), 3);
    let mut out = Vec::new();
    assert_eq!(a.start(O1, "r1", f, 4, &mut out), None, "{out:?}");
    assert!(out.iter().any(|(_, m)| matches!(m, ToOwner::Lapsed { run, fence, why } if run == "r1" && *fence == f && why.contains("capacity keeper"))), "{out:?}");
    assert!(!running(&a, "r1") && a.records().is_empty(), "nothing started, nothing recorded");
    assert!(matches!(waiting(&out, "r1"), Some(Reason::Unsynced { .. })), "it waits here again, saying why: {out:?}");
    a.check()?;
    let back = said(&mut a, synced(8, 16, (0, 0), (0, 0)), 5);
    assert!(offer(&back, "r1").is_some(), "offered again once synced: {back:?}");
    a.check()
}

/// The fold itself: what others hold is everything held less this runner's own, and the room left
/// for it is the room less that, none in a dimension that is over, and no GPUs.
#[test]
fn cv3_the_room_left_a_holder_is_the_room_less_what_others_hold() {
    let view = View { room: [8_000, 16, 100, 2], held: [9_000, 10, 50, 1], own: [2_000, 4, 0, 0], ..View::default() };
    assert_eq!(view.foreign(), [7_000, 6, 50, 1]);
    assert_eq!(view.room_for_holder(), [1_000, 10, 50, 0]);
    let over = View { room: [4_000, 16, 100, 0], held: [6_000, 0, 0, 0], ..View::default() };
    assert_eq!(over.room_for_holder(), [0, 16, 100, 0], "over in CPUs: none of them left");
    let stale = View { room: [4_000, 16, 100, 0], held: [1_000, 0, 0, 0], own: [2_000, 0, 0, 0], ..View::default() };
    assert_eq!(stale.foreign(), [0; 4], "its own past what is held says none of others'");
    assert_eq!(Keeper::Synced(view).room(), Some([1_000, 10, 50, 0]));
    assert_eq!((Keeper::None.room(), Keeper::Unsynced(Unsynced::NotServed).room()), (None, None));
    assert!(Keeper::Unsynced(Unsynced::Malformed).known() && !Keeper::None.known());
}

/// CV8: a holder holds no more reservations than one sync lists (Hex's `most_held`), so the ledger
/// holds no more claims asking something than its shell says its holder may reserve: with two, r1
/// and r2 start, its hint, its kernel's word and its quotes then say no room, and r3 waits for room.
/// r1's release, said and not yet answered, still holds its reservation, so as r1 ends nothing more
/// is offered; once the keeper's answer frees it, r3 is offered in that input.
#[test]
fn cv8_it_holds_no_more_claims_than_its_holder_may_reserve() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, most(synced(8, 16, (0, 0), (0, 0)), 2), 0);
    for (run, now) in [("r1", 1), ("r2", 2)] {
        let w = probe(&mut a, run, "cpus=1", now)?;
        let f = offer(&w, run).ok_or(format!("{w:?}"))?;
        run_it(&mut a, run, f, now)?;
    }
    let _ = said(&mut a, most(synced(8, 16, (0, 0), (2, 0)), 2), 3);
    assert_eq!(a.hint(), Resources::default(), "its room hinted as none");
    assert_eq!(a.fast_word().room, [0; 4], "its kernel's word says no room");
    assert!(matches!(a.quote(&ask("q", "cpus=1")?, 3), Quote::Waits { reason: Reason::Room { .. }, .. }));
    let w = probe(&mut a, "r3", "cpus=1", 3)?;
    assert!(offer(&w, "r3").is_none(), "a third claim past the two its holder may reserve: {w:?}");
    assert!(matches!(waiting(&w, "r3"), Some(Reason::Room { .. })), "{w:?}");
    a.check()?;
    // r1 ends, its release said: until the keeper answers, its reservation is another's in the view
    // and one fewer is left to this runner's claims.
    let _ = said(&mut a, most(synced(8, 16, (1, 0), (1, 0)), 1), 4);
    let mut out = Vec::new();
    a.ended(O1, "r1", true, 4, &mut out);
    assert!(offer(&out, "r3").is_none(), "r1's release not yet answered: {out:?}");
    a.check()?;
    let freed = said(&mut a, most(synced(8, 16, (0, 0), (1, 0)), 2), 5);
    assert!(offer(&freed, "r3").is_some(), "offered as the keeper's answer frees r1's reservation: {freed:?}");
    a.check()
}

/// The holder's items 4 and 5: its shell reserves a claim's room before the ledger starts it, the
/// claim's own amount ([`Admission::live_claim`]). Refused for room as Hex's boxes took the machine
/// meanwhile (the refusal's view said first), the claim lapses: its owner is told why, nothing is
/// started or recorded, and the run waits here on the room there is, offered again under a new claim
/// once that room is back. A refusal for a claim no longer live, or for a run started, changes
/// nothing.
#[test]
fn holder_a_reserve_not_granted_lapses_its_claim_and_the_run_waits() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, synced(8, 16, (0, 0), (0, 0)), 0);
    let w = probe(&mut a, "r1", "cpus=4,mem=4g", 1)?;
    let f = offer(&w, "r1").ok_or(format!("{w:?}"))?;
    assert_eq!(a.live_claim(O1, "r1", f, 1), Some([4_000, 4 * GIB, 0, 0]), "what its reserve asks");
    let mut out = Vec::new();
    a.take(O1, "r1", f, 2, &mut out);
    let _ = said(&mut a, synced(8, 16, (6, 0), (0, 0)), 3);
    let mut out = Vec::new();
    a.not_granted(O1, "r1", f, NotGranted::Room, 3, &mut out);
    assert!(out.iter().any(|(_, m)| matches!(m, ToOwner::Lapsed { run, fence, why } if run == "r1" && *fence == f && why == NotGranted::Room.why())), "{out:?}");
    assert!(matches!(waiting(&out, "r1"), Some(Reason::Room { .. })), "it waits on the room there is: {out:?}");
    assert!(!running(&a, "r1") && a.records().is_empty() && a.live_claim(O1, "r1", f, 3).is_none(), "nothing started, nothing recorded");
    a.check()?;
    let back = said(&mut a, synced(8, 16, (0, 0), (0, 0)), 4);
    let g = offer(&back, "r1").ok_or(format!("offered again once its room is back: {back:?}"))?;
    assert_ne!(g, f, "under a new claim");
    let mut out = Vec::new();
    a.not_granted(O1, "r1", f, NotGranted::Unanswered(WaitEnded::Died), 5, &mut out);
    assert_eq!(a.live_claim(O1, "r1", g, 5), Some([4_000, 4 * GIB, 0, 0]), "a refusal of the lapsed claim leaves the new one: {out:?}");
    run_it(&mut a, "r1", g, 6)?;
    let mut out = Vec::new();
    a.not_granted(O1, "r1", g, NotGranted::Unanswered(WaitEnded::Reset), 7, &mut out);
    assert!(running(&a, "r1") && !a.records().is_empty(), "a run started stays started: {out:?}");
    a.check()
}

/// The holder's items 4 and 5 for a kernel start: its shell reserves before the ledger starts it,
/// and refused, the run's owner is told it was declined for room, in this runner's word with the
/// refusal's view in it (Hex's boxes hold 6 of the 8 CPUs, so 2 are left), so its kernel picks again
/// on the room there is. Nothing here holds the run.
#[test]
fn holder_a_kernel_start_not_granted_is_declined_for_room() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, synced(8, 16, (6, 0), (0, 0)), 0);
    let mut out = Vec::new();
    a.fast_not_granted(O1, "k1", lb::Handle::default(), 1, &mut out);
    let room = out.iter().find_map(|(owner, m)| match m {
        ToOwner::FastDeclined { run, why: FastWhy::Room, word, .. } if owner == "o1" && run == "k1" => Some(word.room),
        _ => None,
    });
    let [cpu, ..] = room.ok_or(format!("declined for room: {out:?}"))?;
    assert_eq!(cpu, 2_000, "its word says the room the keeper leaves it");
    assert!(a.probes().is_empty() && a.room().1 == [0; 4] && a.records().is_empty(), "nothing here holds it");
    a.check()
}

/// A run that asks nothing reserves nothing, as Hex's holder and keeper take no empty amount
/// (agreed with Hex's lead, 2026-11-13): its claim holds nothing, which is its shell's word to
/// start it without a reservation, and it isn't counted against what its holder may reserve, so it
/// starts while those are spent, beside a run that waits for one, and its end frees none. What it
/// uses is in the keeper's measured room (CV12). That is its cold start: once kiln has learned what
/// it asks, it reserves as any run does.
#[test]
fn holder_a_run_asking_nothing_starts_without_a_reservation() -> Result<(), String> {
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, most(synced(8, 16, (0, 0), (0, 0)), 1), 0);
    let w = probe(&mut a, "r1", "cpus=1", 1)?;
    let f = offer(&w, "r1").ok_or(format!("{w:?}"))?;
    run_it(&mut a, "r1", f, 1)?;
    let w = probe(&mut a, "r2", "cpus=1", 2)?;
    assert!(offer(&w, "r2").is_none(), "its holder's one reservation is r1's: {w:?}");
    assert_eq!(a.quote(&ask("q", "")?, 2), Quote::Now, "a run asking nothing fits beside them");
    let w = probe(&mut a, "z1", "", 3)?;
    let z = offer(&w, "z1").ok_or(format!("offered though the reservations are spent: {w:?}"))?;
    assert_eq!(a.live_claim(O1, "z1", z, 3), Some([0; 4]), "nothing to reserve: started without a reservation");
    run_it(&mut a, "z1", z, 3)?;
    assert!(running(&a, "z1") && !running(&a, "r2"), "z1 runs, and r2 still waits for a reservation");
    a.check()?;
    let mut out = Vec::new();
    a.ended(O1, "z1", true, 4, &mut out);
    assert!(offer(&out, "r2").is_none(), "z1 held no reservation, so its end frees none: {out:?}");
    let mut out = Vec::new();
    a.ended(O1, "r1", true, 5, &mut out);
    assert!(offer(&out, "r2").is_some(), "r1's end does: {out:?}");
    a.check()
}

/// The holder's waits (no constants: Shards' R03, as the user restated it on 2026-11-13): nothing
/// seeded, nothing multiplied. Before the keeper's first answer a word waits on liveness alone,
/// however long; the first answer is the bound, and a word's wait runs out only once it has waited
/// longer than the slowest answer, timed from the answer before it where it waited behind one, so
/// an answer as slow as the slowest is no run-out. A run-out says how long the word has waited. A
/// late answer comes to its own word and makes the slowest what it took. A link that dies ends
/// every word on it, the slowest kept for the next link's. A hint, something from the keeper that
/// answers no word, starts the oldest word's window again and times nothing (Hex's wait contract,
/// decision 9: run out only after a whole window with nothing from the keeper). A runner told its
/// keeper unsynced takes nothing new, saying so.
#[test]
fn holder_waits_follow_the_keepers_liveness_and_progress_alone() -> Result<(), String> {
    let mut w = Waits::default();
    w.said(0);
    assert_eq!((w.due(), w.ran_out(i64::MAX)), (None, None), "a cold start waits on liveness alone");
    assert_eq!(w.answered(40), Some(40));
    assert_eq!(w.slowest(), Some(40));
    w.said(100);
    w.said(110);
    assert_eq!(w.due(), Some(141), "the oldest word, past the slowest answer");
    assert_eq!((w.ran_out(140), w.ran_out(141)), (None, Some(41)), "run out once past the slowest answer, waited since its word");
    assert_eq!(w.answered(140), Some(40), "as slow as the slowest: no run-out");
    assert_eq!(w.due(), Some(181), "the next word waited behind it, so it's timed from that answer");
    assert_eq!(w.ran_out(181), Some(41), "nothing answered for longer than the slowest answer, waited since the answer before it");
    assert_eq!(w.answered(300), Some(160), "a late answer comes to its own word");
    assert_eq!((w.slowest(), w.due(), w.ran_out(1_000)), (Some(160), None, None), "the slowest is what it took");
    w.said(400);
    assert_eq!(w.ended(), 1, "the link died: its word ends");
    assert_eq!((w.waiting(), w.slowest()), (0, Some(160)), "the slowest stays the keeper's");
    assert_eq!(w.answered(500), None, "an answer no word waited for");
    w.said(600);
    assert_eq!(w.due(), Some(761), "the next link's words wait on the slowest answer seen");
    // Hex's wait contract (decision 9): given up only after a whole window with nothing from the
    // keeper. A hint answers nothing and shows it going on: the window runs from it.
    w.heard(700);
    assert_eq!(w.due(), Some(861), "a hint, something from the keeper, starts the window again");
    assert_eq!((w.ran_out(860), w.ran_out(861)), (None, Some(261)), "a whole window with nothing after it, waited since its word");
    assert_eq!(w.answered(900), Some(300), "the answer is timed from its word, not from the hint");
    assert_eq!(w.slowest(), Some(300));
    let mut a = runner(8, 16, 1000);
    let _ = said(&mut a, Keeper::Unsynced(Unsynced::Joining), 0);
    assert_eq!(a.taking(), Err(Reason::Unsynced { why: Unsynced::Joining.why().to_owned() }));
    a.check()
}
