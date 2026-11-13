//! kiln's runner as its machine's capacity holder (`place::holder`; docs/lb.md "A machine's
//! keeper"; Hex's docs/design/capacity-view.md, "What kiln's runner implements as a holder", CV5,
//! CV8, CV9, CV11; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 13), against Hex's own keeper
//! over records in memory (`hex_api::capacity::sim::SimView`): each holder state, a run admitted
//! only once synced and granted, a reserve with no answer given up and given back, a lost link and
//! the sync after it, each close as the contract has it, survivors and releases counted against
//! the claims it may hold, the keeper's refusals typed; and kiln-ci's simulator, runners holding
//! through one keeper under crashes on either side, dropped links, partitions and heals, with no
//! oversubscription on any seed and the two sides' books the same once healed.

use std::collections::{BTreeMap, VecDeque};

use hex_api::capacity::sim::SimView;
use hex_api::capacity::{self as hex, Budget, CapacityView, Held, Host, ReservationId, ToHolder, ToKeeper};

use super::ask::{Ask, Peer, Sample};
use super::holder::{BoxName, Event, Holding, Link, Numbering, Out, Paired};
use super::keeper::{Keeper, NotGranted, Unsynced, View, WaitEnded};
use super::msg::{Reason, ToOwner};
use super::runner::{Admission, Init, Records, Said, Settings, Start};
use crate::facts::RunnerFacts;
use crate::sched::needs;
use crate::sched::state::{Cpus, Fence, Resources};

const GIB: u64 = 1 << 30;
const O1: Peer<'static> = Peer { name: "o1", epoch: 1 };

fn debug(e: impl std::fmt::Debug) -> String {
    format!("{e:?}")
}

/// The most one sync lists: what a holder may hold, and what these keepers keep for one.
fn most() -> Result<usize, String> {
    hex_core::capacity::most_held().map_err(|e| e.to_string())
}

/// A machine of `cpus` and `gib` with 500 GiB of disk and no GPUs, its keeper serving `holders`,
/// each up to `reservations`.
fn machine(cpus: u64, gib: u64, holders: usize, reservations: usize) -> Result<SimView, String> {
    let budget = Budget { holders, reservations, name_bytes: hex_core::capacity::MESSAGE_BOUND };
    SimView::new(budget, hex::Amounts { cpu_milli: cpus * 1000, mem_bytes: gib * GIB, disk_bytes: 500 * GIB, gpus: 0 }).map_err(debug)
}

fn holding(name: &str, epoch: u64, survivors: &[Held]) -> Result<Holding, String> {
    Holding::start(name.to_owned(), epoch, survivors).map_err(debug)
}

/// `cpus` and `gib`, as the ledger counts them.
fn amounts(cpus: u64, gib: u64) -> [u64; 4] {
    [cpus * 1000, gib * GIB, 0, 0]
}

fn hex_amounts(cpus: u64, gib: u64) -> hex::Amounts {
    hex::Amounts::from_array(amounts(cpus, gib))
}

fn said(words: Vec<ToKeeper>) -> Out {
    Out { send: words, events: Vec::new() }
}

/// One link between a holding and its keeper that loses nothing: each word said is heard by the
/// keeper in order, its answer heard back, then the hints owed on that link; what the holding says
/// to those is carried too, until nothing is left. Every event, in order.
fn talk(keeper: &mut SimView, link: u64, h: &mut Holding, out: Out, now: i64) -> Result<Vec<Event>, String> {
    let mut events = out.events;
    let mut words: VecDeque<ToKeeper> = out.send.into();
    while let Some(word) = words.pop_front() {
        let mut heard = vec![keeper.hear(link, word).map_err(debug)?];
        heard.extend(keeper.hints().map_err(debug)?.into_iter().filter(|(to, _)| *to == link).map(|(_, hint)| hint));
        for msg in heard {
            let back = h.heard(msg, now);
            words.extend(back.send);
            events.extend(back.events);
        }
    }
    Ok(events)
}

/// Links `h` to its keeper on `link` at `now` and carries every word until synced: every event.
fn join(keeper: &mut SimView, link: u64, h: &mut Holding, now: i64) -> Result<Vec<Event>, String> {
    let out = h.linked(now);
    talk(keeper, link, h, out, now)
}

/// A reserve said: its reservation and its word.
fn reserve(h: &mut Holding, amounts: [u64; 4], now: i64) -> Result<(ReservationId, ToKeeper), String> {
    h.reserve(amounts, now).map_err(debug)?.ok_or_else(|| "it asks something: a reserve is said".to_owned())
}

fn view_of(h: &Holding) -> Result<View, String> {
    match h.keeper() {
        Keeper::Synced(view) => Ok(view),
        other => Err(format!("not synced: {other:?}")),
    }
}

fn granted(events: &[Event], id: ReservationId) -> bool {
    events.contains(&Event::Granted(id))
}

fn not_granted(events: &[Event], id: ReservationId) -> Option<NotGranted> {
    events.iter().find_map(|e| match e {
        Event::NotGranted(i, why) if *i == id => Some(*why),
        _ => None,
    })
}

fn books(keeper: &SimView, name: &str) -> Result<BTreeMap<ReservationId, hex::Amounts>, String> {
    Ok(keeper.held_by(name).map_err(debug)?.into_iter().map(|held| (held.id, held.amount)).collect())
}

/// The holder's states (Hex's capacity-view.md, "The protocol"): `Down` until its link opens,
/// `Joining` once its hello is said, `Syncing` once welcomed and its sync said, `Up` once its sync
/// is answered. Until then it stands `Joining` to its ledger and says no reserve (CV11); synced, it
/// stands on the keeper's view, the claims it may hold the most one sync lists (CV8). Its hello
/// names it and its epoch, and says no scope; its sync lists what it holds.
#[test]
fn holder_states_down_joining_syncing_up_and_no_reserve_before_up() -> Result<(), String> {
    let mut keeper = machine(8, 16, 1, most()?)?;
    let mut h = holding("runner-1", 3, &[])?;
    assert_eq!(h.link(), Link::Down);
    assert_eq!(h.keeper(), Keeper::Unsynced(Unsynced::Joining));
    assert_eq!(h.reserve(amounts(1, 1), 0), Err(NotGranted::Unsynced(Unsynced::Joining)), "no reserve before its sync is answered");
    let hello = h.linked(1);
    assert_eq!(hello.send, vec![ToKeeper::Hello { holder: "runner-1".into(), epoch: 3, scope: None }]);
    assert_eq!(hello.events, vec![Event::Keeper(Keeper::Unsynced(Unsynced::Joining))]);
    assert_eq!(h.link(), Link::Joining);
    let word = hello.send.into_iter().next().ok_or("its hello")?;
    let welcome = keeper.hear(1, word).map_err(debug)?;
    assert!(matches!(welcome, ToHolder::Welcome { .. }), "{welcome:?}");
    let sync = h.heard(welcome, 2);
    assert_eq!(h.link(), Link::Syncing);
    assert!(matches!(sync.send.as_slice(), [ToKeeper::Sync { held, .. }] if held.is_empty()), "{sync:?}");
    assert!(sync.events.is_empty(), "still joining, to its ledger: {sync:?}");
    assert_eq!(h.reserve(amounts(1, 1), 2), Err(NotGranted::Unsynced(Unsynced::Joining)));
    let word = sync.send.into_iter().next().ok_or("its sync")?;
    let synced = keeper.hear(1, word).map_err(debug)?;
    let up = h.heard(synced, 3);
    assert_eq!(h.link(), Link::Up);
    let view = view_of(&h)?;
    assert_eq!(up.events, vec![Event::Keeper(Keeper::Synced(view)), Event::Synced { agree: true, released: 0, adopted: 0, disagreed: 0 }]);
    assert_eq!((view.room, view.held, view.own), ([8000, 16 * GIB, 500 * GIB, 0], [0; 4], [0; 4]));
    assert_eq!(view.most_claims, u64::try_from(most()?).map_err(debug)?, "the most one sync lists");
    Ok(())
}

/// A runner on an idle Linux machine of `cpus` and `gib` with 500 GiB of disk, its CPUs not
/// overcommitted.
fn runner(cpus: u64, gib: u64) -> Admission {
    let facts = RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() };
    let sample = Sample { size: Resources { cpu: Cpus::whole(cpus), mem: gib * GIB, disk: 500 * GIB, ..Resources::default() }, ..Sample::default() };
    let settings = Settings { overcommit_cpu: 1000, ..Settings::default() };
    Admission::new(Init { me: "runner-1".into(), epoch: 1, since: None, facts, labels: vec!["linux".into()], sample, records: Records::new(), settings }, 0)
}

fn ask(run: &str, spelling: &str) -> Result<Ask, String> {
    Ok(Ask { run: run.into(), needs: needs::parse(spelling)?, repo: "acme/app".into(), actor: "agent:1".into(), ..Ask::default() })
}

fn offer(said: &[Said], run: &str) -> Option<Fence> {
    said.iter().rev().find_map(|(_, m)| match m {
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

fn running(a: &Admission, run: &str) -> bool {
    a.probes().iter().any(|p| p.run == run && p.state == "running")
}

/// Where the runner stands, as its ledger takes it from the holding's events, in order.
fn stands(a: &mut Admission, events: &[Event], now: i64, out: &mut Vec<Said>) {
    for event in events {
        if let Event::Keeper(keeper) = event {
            a.keeper_said(*keeper, now, out);
        }
    }
}

/// The holder's items 1 and 6 with the runner's ledger: not synced with its keeper, nothing is
/// offered; once its sync is answered, the room the keeper's view leaves it is offered in that
/// input. A taken claim's room is reserved (`Admission::live_claim`) and its run starts only on
/// the grant (`Admission::start`), the keeper's books holding it. Hex's boxes then take the rest of
/// the machine, which the runner hasn't heard: a claim offered on its stale view is refused for
/// room, its view said first, and lapses (`Admission::not_granted`): nothing starts, and its run
/// waits on the room there is.
#[test]
fn holder_a_run_starts_only_once_synced_and_granted() -> Result<(), String> {
    let mut keeper = machine(8, 16, 1, most()?)?;
    let mut h = holding("runner-1", 1, &[])?;
    let mut a = runner(8, 16);
    let mut out = Vec::new();
    a.keeper_said(h.keeper(), 0, &mut out);
    a.probe(O1, ask("r1", "cpus=2,mem=4g")?, None, 1, &mut out);
    assert!(offer(&out, "r1").is_none(), "nothing offered unsynced: {out:?}");
    let events = join(&mut keeper, 1, &mut h, 2)?;
    let mut out = Vec::new();
    stands(&mut a, &events, 2, &mut out);
    let fence = offer(&out, "r1").ok_or_else(|| format!("offered in the input that synced it: {out:?}"))?;
    a.take(O1, "r1", fence, 3, &mut out);
    let claim = a.live_claim(O1, "r1", fence, 3).ok_or("its claim is live")?;
    assert_eq!(claim, amounts(2, 4));
    let (id, word) = reserve(&mut h, claim, 3)?;
    assert!(!running(&a, "r1"), "nothing starts before its grant");
    let events = talk(&mut keeper, 1, &mut h, said(vec![word]), 4)?;
    assert!(granted(&events, id), "{events:?}");
    assert_eq!(a.start(O1, "r1", fence, 4, &mut out), Some(Start::New), "started on its grant");
    assert!(running(&a, "r1"));
    assert_eq!(books(&keeper, "runner-1")?, BTreeMap::from([(id, hex_amounts(2, 4))]), "the keeper's books hold it");
    keeper.set_local(hex_amounts(6, 12)).map_err(debug)?;
    let mut out = Vec::new();
    a.probe(O1, ask("r2", "cpus=2,mem=4g")?, None, 5, &mut out);
    let fence = offer(&out, "r2").ok_or_else(|| format!("offered on its stale view: {out:?}"))?;
    a.take(O1, "r2", fence, 5, &mut out);
    let (id2, word) = reserve(&mut h, a.live_claim(O1, "r2", fence, 5).ok_or("live")?, 5)?;
    let events = talk(&mut keeper, 1, &mut h, said(vec![word]), 6)?;
    assert_eq!(not_granted(&events, id2), Some(NotGranted::Room), "{events:?}");
    assert!(matches!(events.first(), Some(Event::Keeper(Keeper::Synced(view))) if view.held == amounts(8, 16)), "the refusal's view first: {events:?}");
    let mut out = Vec::new();
    stands(&mut a, &events, 6, &mut out);
    a.not_granted(O1, "r2", fence, NotGranted::Room, 6, &mut out);
    assert!(!running(&a, "r2") && a.live_claim(O1, "r2", fence, 6).is_none(), "nothing started, its claim lapsed: {out:?}");
    assert!(matches!(waiting(&out, "r2"), Some(Reason::Room { .. })), "waits on the room there is: {out:?}");
    assert_eq!(books(&keeper, "runner-1")?.len(), 1);
    a.check()
}

/// The holder's item 5 under its waits (no constants: Shards' R03): its hello answered in 10 ms
/// and its sync in 20, the slowest answer is 20 ms. Before then nothing is due (a cold start waits
/// on liveness alone). A reserve said at 100 is due at 121: at 120 it still waits; at 121 the
/// keeper, its link alive, has said nothing for longer than the slowest answer it has given,
/// so it is stalled: the reserve isn't granted and is given back at once (the keeper may have
/// granted it), and the runner stands `Stalled`, saying no reserve. The keeper's grant that comes
/// late starts nothing and is given back too; its answer ends the stall and makes the slowest what
/// it took; once both releases are answered the keeper holds nothing of the runner's.
#[test]
fn holder_a_reserve_with_no_answer_is_given_up_and_given_back() -> Result<(), String> {
    let mut keeper = machine(8, 16, 1, most()?)?;
    let mut h = holding("runner-1", 1, &[])?;
    let hello = h.linked(0);
    assert_eq!(h.due(), None, "before its first answer, no time bounds a wait");
    let mut words = hello.send.into_iter();
    let welcome = keeper.hear(1, words.next().ok_or("hello")?).map_err(debug)?;
    let sync = h.heard(welcome, 10);
    let mut words = sync.send.into_iter();
    let synced = keeper.hear(1, words.next().ok_or("sync")?).map_err(debug)?;
    let _ = h.heard(synced, 30);
    assert_eq!(h.due(), None, "nothing waits");
    let (id, word) = reserve(&mut h, amounts(2, 4), 100)?;
    assert_eq!(h.due(), Some(121), "said at 100, stalled once it waited longer than the slowest answer, 20 ms");
    let early = h.woke(120);
    assert_eq!(early, Out::default(), "still a reserve at its due");
    let stalled = h.woke(121);
    assert_eq!(stalled.events, vec![Event::Keeper(Keeper::Unsynced(Unsynced::Stalled)), Event::NotGranted(id, NotGranted::Unanswered(WaitEnded::Stalled))]);
    assert!(matches!(stalled.send.as_slice(), [ToKeeper::Release { id: r, .. }] if *r == id), "given back at once: {stalled:?}");
    assert_eq!(h.reserve(amounts(1, 1), 122), Err(NotGranted::Unsynced(Unsynced::Stalled)), "nothing new while stalled");
    assert_eq!(h.due(), None, "stalled: nothing more to wake for until an answer or the link's end");
    let grant = keeper.hear(1, word).map_err(debug)?;
    assert!(matches!(grant, ToHolder::Granted { .. }), "{grant:?}");
    let late = h.heard(grant, 200);
    assert!(!granted(&late.events, id), "nothing starts on a grant given up: {late:?}");
    assert!(matches!(late.send.as_slice(), [ToKeeper::Release { id: r, .. }] if *r == id), "the late grant given back: {late:?}");
    assert!(matches!(h.keeper(), Keeper::Synced(_)), "an answer ends the stall");
    for word in stalled.send.into_iter().chain(late.send) {
        let answer = keeper.hear(1, word).map_err(debug)?;
        let _ = h.heard(answer, 210);
    }
    assert!(books(&keeper, "runner-1")?.is_empty(), "both releases answered, the keeper holds nothing of its");
    let _ = reserve(&mut h, amounts(1, 1), 300)?;
    assert_eq!(h.due(), Some(401), "the slowest answer now the late grant's, 100 ms");
    Ok(())
}

/// Hex's holder wait contract (decision 9, as its lead restated it on 2026-11-13): a reserve is
/// given up only after a whole window of the slowest answer with nothing from the keeper. Its hello
/// answered in 10 ms and its sync in 20, a reserve said at 100 would be due at 121; Hex's boxes
/// moving the view at 115 have the keeper hint the runner, something from it that answers nothing,
/// so the reserve's window runs from 115: at 121 it is still waited for, at 136 a whole window has
/// passed with nothing, and it is given up and given back. The hint times no answer.
#[test]
fn holder_a_hint_from_the_keeper_starts_its_reserves_window_again() -> Result<(), String> {
    let mut keeper = machine(8, 16, 1, most()?)?;
    let mut h = holding("runner-1", 1, &[])?;
    let hello = h.linked(0);
    let welcome = keeper.hear(1, hello.send.into_iter().next().ok_or("hello")?).map_err(debug)?;
    let sync = h.heard(welcome, 10);
    let synced = keeper.hear(1, sync.send.into_iter().next().ok_or("sync")?).map_err(debug)?;
    let _ = h.heard(synced, 30);
    let (id, _) = reserve(&mut h, amounts(2, 4), 100)?;
    assert_eq!(h.due(), Some(121));
    keeper.set_local(hex_amounts(1, 1)).map_err(debug)?;
    let hints: Vec<ToHolder> = keeper.hints().map_err(debug)?.into_iter().filter(|(to, _)| *to == 1).map(|(_, hint)| hint).collect();
    let [hint @ ToHolder::Hint { .. }] = hints.as_slice() else { return Err(format!("a hint for the synced link: {hints:?}")) };
    let _ = h.heard(hint.clone(), 115);
    assert_eq!(h.due(), Some(136), "the window runs from the keeper's last word");
    assert_eq!(h.woke(121), Out::default(), "something came from the keeper within the window: still waited for");
    let stalled = h.woke(136);
    assert_eq!(stalled.events, vec![Event::Keeper(Keeper::Unsynced(Unsynced::Stalled)), Event::NotGranted(id, NotGranted::Unanswered(WaitEnded::Stalled))]);
    assert!(matches!(stalled.send.as_slice(), [ToKeeper::Release { id: r, .. }] if *r == id), "given back at once: {stalled:?}");
    Ok(())
}

/// CV5 on a lost link: a run granted keeps running and its reservation held; a reserve asked as the
/// link died isn't granted, its start doesn't happen, whether it never reached the keeper or was
/// granted and its answer lost with the link. The new link's sync lists what runs, which leaves the
/// lost grant out and so releases it; the counts agree, and the keeper's books are the runner's.
#[test]
fn holder_a_lost_link_gives_up_its_asks_and_the_next_sync_keeps_what_runs() -> Result<(), String> {
    let mut keeper = machine(8, 16, 1, most()?)?;
    let mut h = holding("runner-1", 1, &[])?;
    let _ = join(&mut keeper, 1, &mut h, 0)?;
    let (a_id, word) = reserve(&mut h, amounts(2, 4), 1)?;
    assert!(granted(&talk(&mut keeper, 1, &mut h, said(vec![word]), 2)?, a_id));
    let (b_id, b_word) = reserve(&mut h, amounts(1, 2), 3)?;
    let (c_id, _) = reserve(&mut h, amounts(1, 1), 3)?;
    let lost = keeper.hear(1, b_word).map_err(debug)?;
    assert!(matches!(lost, ToHolder::Granted { .. }), "{lost:?}");
    keeper.link_down(1).map_err(debug)?;
    let ended = h.ended(WaitEnded::Died);
    assert_eq!(
        ended.events,
        vec![
            Event::Keeper(Keeper::Unsynced(Unsynced::Joining)),
            Event::NotGranted(b_id, NotGranted::Unanswered(WaitEnded::Died)),
            Event::NotGranted(c_id, NotGranted::Unanswered(WaitEnded::Died)),
        ]
    );
    assert!(ended.send.is_empty(), "no link to say anything on");
    assert_eq!(h.link(), Link::Down);
    assert_eq!(h.held().map(|held| held.id).collect::<Vec<_>>(), vec![a_id], "what runs stays held");
    let events = join(&mut keeper, 2, &mut h, 4)?;
    assert!(events.contains(&Event::Synced { agree: true, released: 1, adopted: 0, disagreed: 0 }), "the lost grant left out, and released: {events:?}");
    assert_eq!(books(&keeper, "runner-1")?, BTreeMap::from([(a_id, hex_amounts(2, 4))]));
    Ok(())
}

/// The holder's item 5, each close as the contract has it. Its session gone (the keeper started
/// again: `Session`), its reserve isn't granted and it says hello again at once on the same link,
/// and syncs. A newer process of the runner there (`OlderEpoch`), it stands superseded for good and
/// never says hello again. A keeper that serves no more holders (`HoldersFull`), it stands not
/// served until a new link, and says hello again on one. A name the keeper can't take (`Invalid`),
/// it stands malformed: its own defect.
#[test]
fn holder_closes_as_the_contract_has_them() -> Result<(), String> {
    let mut keeper = machine(8, 16, 1, most()?)?;
    let mut old = holding("runner-1", 1, &[])?;
    let _ = join(&mut keeper, 1, &mut old, 0)?;
    keeper.restart().map_err(debug)?;
    let (id, word) = reserve(&mut old, amounts(1, 1), 1)?;
    let events = talk(&mut keeper, 1, &mut old, said(vec![word]), 2)?;
    assert_eq!(not_granted(&events, id), Some(NotGranted::Unanswered(WaitEnded::Reset)), "{events:?}");
    assert_eq!(old.link(), Link::Up, "hello again on the same link, and synced: {events:?}");
    let mut new = holding("runner-1", 2, &[])?;
    let _ = join(&mut keeper, 2, &mut new, 3)?;
    assert_eq!(new.link(), Link::Up);
    let (id, word) = reserve(&mut old, amounts(1, 1), 4)?;
    let events = talk(&mut keeper, 1, &mut old, said(vec![word]), 4)?;
    assert_eq!(not_granted(&events, id), Some(NotGranted::Unanswered(WaitEnded::Reset)), "its session superseded: {events:?}");
    assert_eq!(old.keeper(), Keeper::Unsynced(Unsynced::Superseded), "its hello again closed as older: {events:?}");
    assert_eq!(old.reserve(amounts(1, 1), 5), Err(NotGranted::Unsynced(Unsynced::Superseded)));
    assert!(old.linked(5).send.is_empty(), "never says hello again");
    let mut other = holding("runner-2", 1, &[])?;
    let events = join(&mut keeper, 3, &mut other, 6)?;
    assert_eq!(other.keeper(), Keeper::Unsynced(Unsynced::NotServed), "{events:?}");
    let again = other.linked(7);
    assert!(matches!(again.send.as_slice(), [ToKeeper::Hello { .. }]), "a new link says hello again: {again:?}");
    let mut bad = holding("runner 3", 1, &[])?;
    let events = join(&mut keeper, 4, &mut bad, 8)?;
    assert_eq!(bad.keeper(), Keeper::Unsynced(Unsynced::Malformed), "{events:?}");
    Ok(())
}

/// CV8 and the holder's item 3: a runner started again holds its survivors (its last process's
/// runs that outlived it, from its records) and lists them in its sync, the keeper adopting what it
/// lacks. Each counts against the claims it may hold and as everyone else's room, held for no run
/// its ledger holds, until its work ends and its release is answered; a start granted is its own.
/// A run whose work ends while its link is down is left out of the next sync, which releases it.
#[test]
fn holder_survivors_and_unanswered_releases_count_against_its_claims() -> Result<(), String> {
    let most = u64::try_from(most()?).map_err(debug)?;
    let mut keeper = machine(8, 16, 1, usize::try_from(most).map_err(debug)?)?;
    let survivor = Held { id: ReservationId { epoch: 1, seq: 0 }, amount: hex_amounts(2, 4) };
    let mut h = holding("runner-1", 2, &[survivor])?;
    let events = join(&mut keeper, 1, &mut h, 0)?;
    assert!(events.contains(&Event::Synced { agree: true, released: 0, adopted: 1, disagreed: 0 }), "the keeper adopts what it lacked: {events:?}");
    let view = view_of(&h)?;
    assert_eq!((view.held, view.own, view.most_claims), (amounts(2, 4), [0; 4], most - 1));
    assert_eq!(view.room_for_holder(), [6000, 12 * GIB, 500 * GIB, 0], "its room less the survivor's");
    let (id, word) = reserve(&mut h, amounts(1, 2), 1)?;
    assert!(granted(&talk(&mut keeper, 1, &mut h, said(vec![word]), 2)?, id));
    let view = view_of(&h)?;
    assert_eq!((view.own, view.most_claims), (amounts(1, 2), most - 1), "a start's grant is its own");
    let out = h.release(survivor.id, 3);
    assert!(matches!(out.send.as_slice(), [ToKeeper::Release { id: r, .. }] if *r == survivor.id), "{out:?}");
    assert_eq!(view_of(&h)?.most_claims, most - 1, "the keeper's until it answers");
    let _ = talk(&mut keeper, 1, &mut h, out, 4)?;
    let view = view_of(&h)?;
    assert_eq!((view.held, view.own, view.most_claims), (amounts(1, 2), amounts(1, 2), most), "answered");
    keeper.link_down(1).map_err(debug)?;
    let _ = h.ended(WaitEnded::Died);
    let out = h.release(id, 5);
    assert!(out.send.is_empty(), "no link: the next sync says it");
    let events = join(&mut keeper, 2, &mut h, 6)?;
    assert!(events.contains(&Event::Synced { agree: true, released: 1, adopted: 0, disagreed: 0 }), "{events:?}");
    assert!(books(&keeper, "runner-1")?.is_empty());
    assert_eq!(view_of(&h)?.most_claims, most);
    Ok(())
}

/// The keeper's refusals as the ledger takes them: one more than the keeper keeps for a holder is
/// `Full` (CV8), a GPU by count is `Devices` (CV3). A start that asks nothing says no reserve at
/// all and starts at once, its cold start; unsynced, not even that (CV11).
#[test]
fn holder_refusals_are_typed_and_a_run_asking_nothing_reserves_nothing() -> Result<(), String> {
    let mut keeper = machine(8, 16, 1, 1)?;
    let mut h = holding("runner-1", 1, &[])?;
    assert_eq!(h.reserve([0; 4], 0), Err(NotGranted::Unsynced(Unsynced::Joining)), "nothing new unsynced, asking nothing or not");
    let _ = join(&mut keeper, 1, &mut h, 0)?;
    assert_eq!(h.reserve([0; 4], 1), Ok(None), "nothing to reserve: it starts at once");
    let (first, word) = reserve(&mut h, amounts(1, 1), 1)?;
    assert!(granted(&talk(&mut keeper, 1, &mut h, said(vec![word]), 2)?, first));
    let (second, word) = reserve(&mut h, amounts(1, 1), 3)?;
    let events = talk(&mut keeper, 1, &mut h, said(vec![word]), 4)?;
    assert_eq!(not_granted(&events, second), Some(NotGranted::Full), "{events:?}");
    let out = h.release(first, 5);
    let _ = talk(&mut keeper, 1, &mut h, out, 5)?;
    let (gpu, word) = reserve(&mut h, [1000, GIB, 0, 1], 6)?;
    let events = talk(&mut keeper, 1, &mut h, said(vec![word]), 7)?;
    assert_eq!(not_granted(&events, gpu), Some(NotGranted::Devices), "{events:?}");
    assert!(books(&keeper, "runner-1")?.is_empty());
    Ok(())
}

/// Kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 17 (#73 part 7 slice 2): a holding numbered by its process's
/// shared numbering and a sandbox naming a box through that numbering never give two boxes one name; a numbering of
/// another epoch's isn't taken (it names another process's boxes).
#[test]
fn s17_p7_a_holding_and_its_processs_sandbox_share_one_numbering() -> Result<(), String> {
    let shared = Numbering::new(7);
    let mut h = holding("runner-1", 7, &[])?.numbered_by(shared.clone());
    let names = [h.unreserved(), shared.next(), h.unreserved(), shared.clone().next()];
    let want: Vec<BoxName> = (0..4).map(|n| BoxName::Unreserved { epoch: 7, n }).collect();
    assert_eq!(names.to_vec(), want);
    let mut other = holding("runner-1", 7, &[])?.numbered_by(Numbering::new(8));
    assert_eq!(other.unreserved(), BoxName::Unreserved { epoch: 7, n: 0 }, "another epoch's numbering taken");
    assert_eq!(Numbering::new(8).next(), BoxName::Unreserved { epoch: 8, n: 0 });
    Ok(())
}

/// The boxes kiln runs on its machine are named so its keeper pairs each with what kiln holds there
/// (agreed with Hex's lead): a reservation's box `kiln-r<epoch>-<seq>`, the box of a run that asks
/// nothing `kiln-z<epoch>-<n>`, numbered from 0 in its runner process, each number decimal. Each is
/// a name Hex's boxes take and pairs back with itself; an id that isn't one written so (a leading
/// zero, a sign, a part more or fewer, a number past a `u64`, another prefix) pairs with nothing.
#[test]
fn holder_boxes_are_named_by_what_they_hold_and_pair_byte_for_byte() -> Result<(), String> {
    let mut h = holding("runner-1", 7, &[])?;
    let reserved = BoxName::Reserved(ReservationId { epoch: 7, seq: 12 });
    assert_eq!(reserved.to_string(), "kiln-r7-12");
    assert_eq!(h.unreserved(), BoxName::Unreserved { epoch: 7, n: 0 });
    assert_eq!(h.unreserved().to_string(), "kiln-z7-1");
    let widest = BoxName::Reserved(ReservationId { epoch: u64::MAX, seq: u64::MAX });
    for name in [reserved, BoxName::Unreserved { epoch: 7, n: 1 }, widest] {
        assert_eq!(name.id().map_err(debug)?.as_str(), name.to_string(), "a name Hex's boxes take");
        assert_eq!(BoxName::pair(&name.to_string()), Paired::Kiln(name));
    }
    for id in ["kiln-r07-12", "kiln-r+7-12", "kiln-r7-012", "kiln-r7-12-1", "kiln-r7", "kiln-r7-", "kiln-r-12", "kiln-z7-01", "kiln-x7-12", "Kiln-r7-12", "kiln-r18446744073709551616-0", "box-1", ""] {
        assert_eq!(BoxName::pair(id), Paired::Unpaired, "{id:?}");
    }
    Ok(())
}

// ------------------------------------------------------------------------------- the simulator

/// SplitMix64 (Steele, Lea and Flood, OOPSLA 2014), as Hex's simulator draws: one seed names a run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// In `0..n`; 0 for none.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    fn one_in(&mut self, n: u64) -> bool {
        self.below(n) == 0
    }
}

/// A link: the keeper's number for it, and the words in flight each way, in order.
struct Wire {
    id: u64,
    up: VecDeque<ToKeeper>,
    down: VecDeque<ToHolder>,
}

/// A runner: its process (none while down), its durable records (its epoch, one more at every
/// start of its process, and the reservations whose work may run, each written before its run
/// starts and out before its release is said), its link, and the starts waiting for their grants.
struct Runner {
    name: String,
    holding: Option<Holding>,
    epoch: u64,
    record: BTreeMap<ReservationId, hex::Amounts>,
    link: Option<Wire>,
    asked: BTreeMap<ReservationId, hex::Amounts>,
}

/// What the nemesis did, over a seed set: so the test knows it tried each fault.
#[derive(Debug, Default)]
struct Tally {
    started: u64,
    unreserved: u64,
    stalled: u64,
    keeper_restarts: u64,
    crashes: u64,
    partitions: u64,
    sessions_lost: u64,
    released_by_sync: u64,
    refused_room: u64,
}

/// One machine of 16 CPUs and 64 GiB whose keeper is Hex's, Hex's own boxes admitted there
/// besides, and kiln's runners holding through it, under a nemesis (module docs).
struct Sim {
    keeper: SimView,
    room: hex::Amounts,
    runners: Vec<Runner>,
    /// The work that runs: each run by its runner and reservation, and what it holds.
    running: BTreeMap<(usize, ReservationId), hex::Amounts>,
    /// Links whose end the keeper hasn't heard yet (its transport may find one gone later than
    /// the runner's does).
    unheard: Vec<u64>,
    next_link: u64,
    now: i64,
    rng: Rng,
    tally: Tally,
}

impl Sim {
    fn new(seed: u64, runners: usize) -> Result<Self, String> {
        let budget = Budget { holders: runners, reservations: most()?, name_bytes: hex_core::capacity::MESSAGE_BOUND };
        let room = hex::Amounts { cpu_milli: 16_000, mem_bytes: 64 * GIB, disk_bytes: 500 * GIB, gpus: 0 };
        let keeper = SimView::new(budget, room).map_err(debug)?;
        let mut list = Vec::new();
        for n in 0..runners {
            let name = format!("runner-{n}");
            list.push(Runner { holding: Some(holding(&name, 1, &[])?), name, epoch: 1, record: BTreeMap::new(), link: None, asked: BTreeMap::new() });
        }
        Ok(Self { keeper, room, runners: list, running: BTreeMap::new(), unheard: Vec::new(), next_link: 0, now: 0, rng: Rng(seed), tally: Tally::default() })
    }

    fn step(&mut self) -> Result<(), String> {
        self.now += 1 + i64::try_from(self.rng.below(20)).map_err(debug)?;
        let n = u64::try_from(self.runners.len()).map_err(debug)?;
        let i = usize::try_from(self.rng.below(n)).map_err(debug)?;
        match self.rng.below(20) {
            0..=4 => self.ask(i),
            5..=6 => self.end_run(i),
            7..=9 => self.deliver_up(i).map(|_| ()),
            10..=12 => self.deliver_down(i).map(|_| ()),
            13 => self.wake(i),
            14..=15 => self.heal(i),
            16 => self.give_up(i),
            17 => self.local(),
            18 => match self.rng.below(4) {
                0 => self.partition(i),
                1 => self.session_lost(i),
                2 => self.hear_ends(),
                _ => self.crash(i),
            },
            _ => {
                if self.rng.one_in(4) {
                    self.keeper_restart()
                } else {
                    self.wake(i)
                }
            }
        }?;
        self.check()
    }

    /// What a holding's input came to: its words on its link, and its events acted on as its
    /// shell would: a run started on its grant alone, its record written first.
    fn apply(&mut self, i: usize, out: Out) -> Result<(), String> {
        let r = self.runners.get_mut(i).ok_or("no such runner")?;
        match r.link.as_mut() {
            Some(wire) => wire.up.extend(out.send),
            None if out.send.is_empty() => {}
            None => return Err(format!("{}: said {:?} with no link", r.name, out.send)),
        }
        for event in out.events {
            match event {
                Event::Granted(id) => {
                    let amount = r.asked.remove(&id).ok_or_else(|| format!("{}: a grant for no start waiting: {id:?}", r.name))?;
                    // The keeper writes a reservation before it grants it (CV2): its books hold it.
                    let books = self.keeper.held_by(&r.name).map_err(debug)?;
                    if !books.contains(&Held { id, amount }) {
                        return Err(format!("{}: granted {id:?}, which the keeper's books don't hold: {books:?}", r.name));
                    }
                    r.record.insert(id, amount);
                    self.running.insert((i, id), amount);
                    self.tally.started += 1;
                }
                Event::NotGranted(id, why) => {
                    r.asked.remove(&id);
                    match why {
                        NotGranted::Unanswered(WaitEnded::Stalled) => self.tally.stalled += 1,
                        NotGranted::Room => self.tally.refused_room += 1,
                        _ => {}
                    }
                }
                Event::Synced { agree, released, adopted, disagreed } => {
                    if !agree {
                        return Err(format!("{}: its sync's counts disagree (released {released}, adopted {adopted}, disagreed {disagreed})", r.name));
                    }
                    self.tally.released_by_sync += released;
                }
                Event::Keeper(_) => {}
            }
        }
        Ok(())
    }

    /// The hints the keeper owes, each on its link, where a runner still holds that link.
    fn hints(&mut self) -> Result<(), String> {
        for (link, hint) in self.keeper.hints().map_err(debug)? {
            if let Some(wire) = self.runners.iter_mut().filter_map(|r| r.link.as_mut()).find(|w| w.id == link) {
                wire.down.push_back(hint);
            }
        }
        Ok(())
    }

    /// A run asks to start: a share of the machine, or one in eight nothing.
    fn ask(&mut self, i: usize) -> Result<(), String> {
        let now = self.now;
        let amount = if self.rng.one_in(8) {
            hex::Amounts::ZERO
        } else {
            hex::Amounts { cpu_milli: 250 * (1 + self.rng.below(16)), mem_bytes: GIB * (1 + self.rng.below(8)), disk_bytes: 0, gpus: 0 }
        };
        let Some(r) = self.runners.get_mut(i) else { return Ok(()) };
        let Some(h) = r.holding.as_mut() else { return Ok(()) };
        match h.reserve(amount.to_array(), now) {
            Ok(None) => self.tally.unreserved += 1,
            Ok(Some((id, word))) => {
                let wire = r.link.as_mut().ok_or_else(|| format!("{}: a reserve said with no link", r.name))?;
                wire.up.push_back(word);
                r.asked.insert(id, amount);
            }
            Err(_) => {}
        }
        Ok(())
    }

    /// One of its runs ends: out of its records first, then its release said. Its process down,
    /// its records stay as they were, and its next process finds the work gone.
    fn end_run(&mut self, i: usize) -> Result<(), String> {
        let now = self.now;
        let runs: Vec<ReservationId> = self.running.keys().filter(|(at, _)| *at == i).map(|(_, id)| *id).collect();
        let pick = usize::try_from(self.rng.below(u64::try_from(runs.len()).map_err(debug)?)).map_err(debug)?;
        let Some(&id) = runs.get(pick) else { return Ok(()) };
        self.running.remove(&(i, id));
        let Some(r) = self.runners.get_mut(i) else { return Ok(()) };
        let Some(h) = r.holding.as_mut() else { return Ok(()) };
        r.record.remove(&id);
        let out = h.release(id, now);
        self.apply(i, out)
    }

    /// A start waiting for its grant is given up (its claim lapsed meanwhile).
    fn give_up(&mut self, i: usize) -> Result<(), String> {
        let now = self.now;
        let Some(r) = self.runners.get_mut(i) else { return Ok(()) };
        let Some(&id) = r.asked.keys().next() else { return Ok(()) };
        let Some(h) = r.holding.as_mut() else { return Ok(()) };
        r.asked.remove(&id);
        let out = h.release(id, now);
        self.apply(i, out)
    }

    /// The keeper hears its oldest word in flight on runner `i`'s link and answers it there, then
    /// says the hints it owes.
    fn deliver_up(&mut self, i: usize) -> Result<bool, String> {
        let Some(wire) = self.runners.get_mut(i).and_then(|r| r.link.as_mut()) else { return Ok(false) };
        let Some(word) = wire.up.pop_front() else { return Ok(false) };
        let answer = self.keeper.hear(wire.id, word).map_err(debug)?;
        wire.down.push_back(answer);
        self.hints()?;
        Ok(true)
    }

    /// Runner `i` hears the oldest word in flight to it.
    fn deliver_down(&mut self, i: usize) -> Result<bool, String> {
        let now = self.now;
        let Some(r) = self.runners.get_mut(i) else { return Ok(false) };
        let (Some(h), Some(wire)) = (r.holding.as_mut(), r.link.as_mut()) else { return Ok(false) };
        let Some(msg) = wire.down.pop_front() else { return Ok(false) };
        let out = h.heard(msg, now);
        self.apply(i, out)?;
        Ok(true)
    }

    /// Its shell wakes it at its due.
    fn wake(&mut self, i: usize) -> Result<(), String> {
        let now = self.now;
        let Some(h) = self.runners.get_mut(i).and_then(|r| r.holding.as_mut()) else { return Ok(()) };
        if h.due().is_some_and(|due| due <= now) {
            let out = h.woke(now);
            return self.apply(i, out);
        }
        Ok(())
    }

    /// Runner `i`'s link is cut, with what was on it: its transport tells it at once, the keeper's
    /// at once or later.
    fn partition(&mut self, i: usize) -> Result<(), String> {
        let at_once = self.rng.one_in(2);
        let Some(r) = self.runners.get_mut(i) else { return Ok(()) };
        let Some(wire) = r.link.take() else { return Ok(()) };
        let out = r.holding.as_mut().map(|h| h.ended(WaitEnded::Died)).unwrap_or_default();
        if at_once {
            self.keeper.link_down(wire.id).map_err(debug)?;
        } else {
            self.unheard.push(wire.id);
        }
        self.tally.partitions += 1;
        self.apply(i, out)
    }

    /// The keeper drops runner `i`'s session behind a link the runner still holds: its next word
    /// there is closed for its session.
    fn session_lost(&mut self, i: usize) -> Result<(), String> {
        let Some(wire) = self.runners.get(i).and_then(|r| r.link.as_ref()) else { return Ok(()) };
        self.keeper.link_down(wire.id).map_err(debug)?;
        self.tally.sessions_lost += 1;
        Ok(())
    }

    /// The keeper hears every link end it hadn't.
    fn hear_ends(&mut self) -> Result<(), String> {
        for link in std::mem::take(&mut self.unheard) {
            self.keeper.link_down(link).map_err(debug)?;
        }
        Ok(())
    }

    /// Runner `i` heals: its process starts again where it was down (its epoch one more, written
    /// before it says anything, and its survivors the runs its records list whose work still runs),
    /// else a new link where it has none.
    fn heal(&mut self, i: usize) -> Result<(), String> {
        let now = self.now;
        let Some(r) = self.runners.get_mut(i) else { return Ok(()) };
        if r.holding.is_none() {
            r.epoch += 1;
            let running = &self.running;
            r.record.retain(|id, _| running.contains_key(&(i, *id)));
            let survivors: Vec<Held> = r.record.iter().map(|(id, amount)| Held { id: *id, amount: *amount }).collect();
            r.holding = Some(holding(&r.name, r.epoch, &survivors)?);
            return Ok(());
        }
        if r.link.is_some() {
            return Ok(());
        }
        self.next_link += 1;
        r.link = Some(Wire { id: self.next_link, up: VecDeque::new(), down: VecDeque::new() });
        let out = r.holding.as_mut().map(|h| h.linked(now)).unwrap_or_default();
        self.apply(i, out)
    }

    /// Runner `i`'s process crashes: what it asked never starts, its link goes (the keeper hears
    /// later), and its runs' work dies with it or outlives it.
    fn crash(&mut self, i: usize) -> Result<(), String> {
        let work_dies = self.rng.one_in(2);
        let Some(r) = self.runners.get_mut(i) else { return Ok(()) };
        if r.holding.take().is_none() {
            return Ok(());
        }
        r.asked.clear();
        if let Some(wire) = r.link.take() {
            self.unheard.push(wire.id);
        }
        if work_dies {
            self.running.retain(|(at, _), _| *at != i);
        }
        self.tally.crashes += 1;
        Ok(())
    }

    /// The keeper's process starts again from its records: every link goes, with what was on it.
    fn keeper_restart(&mut self) -> Result<(), String> {
        self.keeper.restart().map_err(debug)?;
        self.unheard.clear();
        self.tally.keeper_restarts += 1;
        for i in 0..self.runners.len() {
            let Some(r) = self.runners.get_mut(i) else { continue };
            if r.link.take().is_none() {
                continue;
            }
            let out = r.holding.as_mut().map(|h| h.ended(WaitEnded::Reset)).unwrap_or_default();
            self.apply(i, out)?;
        }
        Ok(())
    }

    /// Hex's own boxes now hold a share of what the holders leave them (Hex admits only what fits).
    fn local(&mut self) -> Result<(), String> {
        let free = self.keeper.room_for_local().map_err(debug)?;
        let local = hex::Amounts { cpu_milli: self.rng.below(free.cpu_milli + 1), mem_bytes: self.rng.below(free.mem_bytes + 1), disk_bytes: 0, gpus: 0 };
        self.keeper.set_local(local).map_err(debug)?;
        self.hints()
    }

    /// After every step: the work running and Hex's own boxes fit the machine (no
    /// oversubscription), every run's reservation is in the keeper's books (never released on
    /// silence: CV5), and the keeper holds nothing past its room.
    fn check(&self) -> Result<(), String> {
        let mut used = self.keeper.local();
        for amount in self.running.values() {
            used = used.checked_add(*amount).ok_or("the work running overflows a count")?;
        }
        if !used.fits(self.room) {
            return Err(format!("oversubscribed at {} ms: {used:?} runs on a room of {:?}", self.now, self.room));
        }
        for ((i, id), amount) in &self.running {
            let r = self.runners.get(*i).ok_or("no such runner")?;
            let books = self.keeper.held_by(&r.name).map_err(debug)?;
            if !books.contains(&Held { id: *id, amount: *amount }) {
                return Err(format!("{}: {id:?} runs, and the keeper's books don't hold it: {books:?}", r.name));
            }
        }
        let view = self.keeper.view().map_err(debug)?;
        if !view.held.fits(view.room) {
            return Err(format!("the keeper holds past its room: {view:?}"));
        }
        Ok(())
    }

    /// Every partition healed, every process started again, every word delivered and nothing lost:
    /// then each runner is synced, and the keeper's books of it, its own, its records and what it
    /// runs are the same (CV5: "the counts agree after a partition heals").
    fn settle(&mut self) -> Result<(), String> {
        self.hear_ends()?;
        for i in 0..self.runners.len() {
            self.heal(i)?;
            self.heal(i)?;
        }
        for _ in 0..100_000 {
            let mut moved = false;
            for i in 0..self.runners.len() {
                moved |= self.deliver_up(i)?;
                moved |= self.deliver_down(i)?;
            }
            if !moved {
                return self.agree();
            }
        }
        Err("it never settled".into())
    }

    fn agree(&self) -> Result<(), String> {
        let mut all = self.keeper.local();
        for (i, r) in self.runners.iter().enumerate() {
            let h = r.holding.as_ref().ok_or("down after settling")?;
            if h.link() != Link::Up {
                return Err(format!("{}: {:?} after settling, standing {:?}", r.name, h.link(), h.keeper()));
            }
            if !r.asked.is_empty() {
                return Err(format!("{}: starts still asked after settling: {:?}", r.name, r.asked));
            }
            let books = books(&self.keeper, &r.name)?;
            let own: BTreeMap<ReservationId, hex::Amounts> = h.held().map(|held| (held.id, held.amount)).collect();
            let runs: BTreeMap<ReservationId, hex::Amounts> = self.running.iter().filter(|((at, _), _)| *at == i).map(|((_, id), amount)| (*id, *amount)).collect();
            if books != own || own != runs || runs != r.record {
                return Err(format!("{}: the keeper's books {books:?}, its own {own:?}, its records {:?}, what runs {runs:?}", r.name, r.record));
            }
            for amount in books.values() {
                all = all.checked_add(*amount).ok_or("the books overflow a count")?;
            }
        }
        let view = self.keeper.view().map_err(debug)?;
        if view.held != all {
            return Err(format!("the keeper's view holds {:?}, its books and Hex's own boxes {all:?}", view.held));
        }
        Ok(())
    }
}

/// kiln-ci's simulator (Hex's capacity-view.md, "Tests": kiln's binding of the holder against
/// `capacity::sim::SimView`): one to three runners hold through one machine's keeper beside Hex's
/// own boxes, under a nemesis that restarts the keeper from its records, crashes runners (their
/// runs' work dying with them or outliving them), cuts links with what was on them (the keeper
/// hearing it at once or later), drops sessions behind live links, delays words until the keeper
/// stalls, and gives up starts. On every one of a fixed set of seeds, after every step, the work
/// running fits the machine and is in the keeper's books; healed and settled, every 150 steps and
/// at the end, each runner's books on both sides, its records and what runs are the same, and no
/// sync disagreed. The nemesis is held to having tried each fault over the set.
#[test]
fn holder_sim_no_oversubscription_and_the_books_agree_once_healed() -> Result<(), String> {
    let mut seen = Tally::default();
    for seed in 0..24u64 {
        let runners = 1 + usize::try_from(seed % 3).map_err(debug)?;
        let mut sim = Sim::new(seed, runners)?;
        for step in 0..600u32 {
            sim.step().map_err(|e| format!("seed {seed}, step {step}: {e}"))?;
            if step % 150 == 149 {
                sim.settle().map_err(|e| format!("seed {seed}, settling at step {step}: {e}"))?;
            }
        }
        sim.settle().map_err(|e| format!("seed {seed}, settling at the end: {e}"))?;
        let t = &sim.tally;
        seen.started += t.started;
        seen.unreserved += t.unreserved;
        seen.stalled += t.stalled;
        seen.keeper_restarts += t.keeper_restarts;
        seen.crashes += t.crashes;
        seen.partitions += t.partitions;
        seen.sessions_lost += t.sessions_lost;
        seen.released_by_sync += t.released_by_sync;
        seen.refused_room += t.refused_room;
    }
    let tried = [seen.started, seen.unreserved, seen.stalled, seen.keeper_restarts, seen.crashes, seen.partitions, seen.sessions_lost, seen.released_by_sync, seen.refused_room];
    assert!(tried.iter().all(|n| *n > 0), "the nemesis tried each fault: {seen:?}");
    Ok(())
}
