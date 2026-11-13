//! The runner's ledger beside its machine's capacity keeper (docs/lb.md "A machine's keeper"; Hex's
//! docs/design/capacity-view.md CV3, CV7, CV10, CV11; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d,
//! step 13), each part a test: none known, it admits on its own room; unsynced, nothing new, saying
//! why, its kernel cordoned and what runs going on; synced, only within the room the keeper's view
//! leaves it, its CPUs never overcommitted past that room, no GPUs, nothing new while the view is
//! over, and what fits offered in the input that let it.

use super::ask::{Ask, Peer, Sample};
use super::keeper::{Keeper, Unsynced, View};
use super::msg::{Reason, ToOwner};
use super::runner::{Admission, Init, Quote, Records, Said, Settings, Start};
use crate::facts::RunnerFacts;
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
/// else holds `others` and this runner's holder `own` (CPUs, GiB).
fn synced(cpus: u64, mem_gib: u64, (others_cpus, others_gib): (u64, u64), (own_cpus, own_gib): (u64, u64)) -> Keeper {
    let held = [(others_cpus + own_cpus) * 1000, (others_gib + own_gib) * GIB, 0, 0];
    Keeper::Synced(View { room: [cpus * 1000, mem_gib * GIB, 500 * GIB, 0], held, own: [own_cpus * 1000, own_gib * GIB, 0, 0] })
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
/// frees nothing for the run waiting; a newer process of the runner taking its place with the keeper
/// tells the waiting run the new why.
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
    assert!(matches!(waiting(&news, "r2"), Some(Reason::Unsynced { why }) if why == Unsynced::Superseded.why()), "{news:?}");
    a.check()
}

/// The fold itself: what others hold is everything held less this runner's own, and the room left
/// for it is the room less that, none in a dimension that is over, and no GPUs.
#[test]
fn cv3_the_room_left_a_holder_is_the_room_less_what_others_hold() {
    let view = View { room: [8_000, 16, 100, 2], held: [9_000, 10, 50, 1], own: [2_000, 4, 0, 0] };
    assert_eq!(view.foreign(), [7_000, 6, 50, 1]);
    assert_eq!(view.room_for_holder(), [1_000, 10, 50, 0]);
    let over = View { room: [4_000, 16, 100, 0], held: [6_000, 0, 0, 0], own: [0; 4] };
    assert_eq!(over.room_for_holder(), [0, 16, 100, 0], "over in CPUs: none of them left");
    let stale = View { room: [4_000, 16, 100, 0], held: [1_000, 0, 0, 0], own: [2_000, 0, 0, 0] };
    assert_eq!(stale.foreign(), [0; 4], "its own past what is held says none of others'");
    assert_eq!(Keeper::Synced(view).room(), Some([1_000, 10, 50, 0]));
    assert_eq!((Keeper::None.room(), Keeper::Unsynced(Unsynced::NotServed).room()), (None, None));
    assert!(Keeper::Unsynced(Unsynced::Malformed).known() && !Keeper::None.known());
}
