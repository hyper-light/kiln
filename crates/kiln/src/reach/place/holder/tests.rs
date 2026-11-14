//! Placement holding its runs' room through its machine's keeper (`super`; docs/lb.md "A
//! machine's keeper"; Hex's capacity-view.md, the holder's items 1 to 6), against Hex's own keeper
//! behind the Capacity seam's simulator (`kiln_seams::capacity::SimCapacity`): each holder state as
//! placement stands on it; every start path (a CI leg, an owner's claim with its spec, a kernel
//! start with its spec, an agent session, this node's own run) starting only once synced and
//! granted, and giving its room back as it ends; a reserve refused, and one with no answer given up
//! and given back; a start that asks nothing starting at once; and a runner started again listing
//! its survivors.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use futures::FutureExt as _;
use hex_api::capacity::{self as hex, Budget, Host as _, ReservationId, ToKeeper};
use kiln_ci::facts::RunnerFacts;
use kiln_ci::place::ask::{Ask, Sample};
use kiln_ci::place::holder::BoxName;
use kiln_ci::place::keeper::{Keeper, NotGranted, Unsynced, WaitEnded};
use kiln_ci::place::msg::{FastHandle, ToOwner, ToRunner};
use kiln_ci::place::runner::Settings;
use kiln_ci::sched::needs::Needs;
use kiln_ci::sched::state::{Cpus, Fence, Resources};
use kiln_seams::capacity::{Capacity as _, Link, SimCapacity};

use super::super::{CI, Claim, Envelope, Out, Placed, Placer, Spec, load};

const GIB: u64 = 1 << 30;
/// This runner, as its holder's certificate names it.
const ME: &str = "runner-1";

fn fail(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// The machine: 8 CPUs, 32 GiB and 500 GiB of disk, no GPUs.
fn room() -> hex::Amounts {
    hex::Amounts { cpu_milli: 8_000, mem_bytes: 32 * GIB, disk_bytes: 500 * GIB, gpus: 0 }
}

/// Its keeper, Hex's own over records in memory, serving two holders as many reservations each as
/// one sync lists.
fn machine() -> anyhow::Result<SimCapacity> {
    let most = hex_core::capacity::most_held().map_err(fail)?;
    SimCapacity::new(Budget { holders: 2, reservations: most, name_bytes: 255 }, room()).map_err(fail)
}

/// This runner's placement, in `epoch`, measured as the machine.
fn runner(epoch: u64) -> Placer {
    let sample = Sample { size: Resources { cpu: Cpus::whole(8), mem: 32 * GIB, disk: 500 * GIB, gpu: 0 }, ..Sample::default() };
    Placer::new(ME, epoch, RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..RunnerFacts::default() }, Vec::new(), sample, Settings::default(), 0)
}

fn ask(run: &str, cpus: u64, gib: u64, key: Option<&str>) -> Ask {
    let needs = Needs { request: Resources { cpu: Cpus::whole(cpus), mem: gib * GIB, disk: 0, gpu: 0 }, ..Needs::default() };
    Ask { run: run.into(), needs, repo: "acme/app".into(), actor: "agent:1".into(), key: key.map(str::to_owned), ..Ask::default() }
}

/// Carries `p`'s words to its keeper on `link`, and the keeper's answers and hints back at `now`,
/// until neither side has more: placement's other outs, in order.
async fn talk(p: &mut Placer, link: &dyn Link, outs: Vec<Out>, now: i64) -> anyhow::Result<Vec<Out>> {
    let mut seen = Vec::new();
    let mut queue: VecDeque<Out> = outs.into();
    loop {
        while let Some(out) = queue.pop_front() {
            match out {
                Out::KeeperSay { link: n, word } if n == p.keeper_link() => link.say(word).await.map_err(fail)?,
                Out::KeeperSay { .. } => {}
                other => seen.push(other),
            }
        }
        match link.hear().now_or_never() {
            Some(word) => queue.extend(p.keeper_heard(word.map_err(fail)?, now)),
            None => return Ok(seen),
        }
    }
}

/// The keeper's words among `outs` said on `link`, nothing heard.
async fn say(link: &dyn Link, outs: Vec<Out>) -> anyhow::Result<()> {
    for out in outs {
        if let Out::KeeperSay { word, .. } = out {
            link.say(word).await.map_err(fail)?;
        }
    }
    Ok(())
}

/// A link to its keeper opened at `now`, and every word carried until synced.
async fn join(p: &mut Placer, cap: &SimCapacity, now: i64) -> anyhow::Result<Box<dyn Link>> {
    let link = cap.link().await.map_err(fail)?;
    let outs = p.keeper_linked(now);
    let _ = talk(p, &*link, outs, now).await?;
    Ok(link)
}

/// The keeper's books of this runner.
fn books(cap: &SimCapacity) -> anyhow::Result<BTreeMap<ReservationId, hex::Amounts>> {
    Ok(cap.host(|v| v.held_by(ME)).map_err(fail)?.map_err(fail)?.into_iter().map(|h| (h.id, h.amount)).collect())
}

fn synced(p: &Placer) -> bool {
    matches!(p.admission().keeper(), Keeper::Synced(_))
}

fn running(p: &Placer, owner: &str, run: &str) -> bool {
    p.admission().probes().iter().any(|v| v.owner == owner && v.run == run && v.state == "running")
}

fn link_is(p: &Placer, state: &str) -> bool {
    p.view().get("keeper").and_then(|k| k.get("link")).is_some_and(|l| l == state)
}

/// Owner "asker"'s word, of its epoch 1.
fn word(msg: ToRunner, spec: Option<u64>) -> anyhow::Result<String> {
    Ok(serde_json::to_string(&Envelope { epoch: 1, spec, ballot: None, members: Vec::new(), msg })?)
}

/// What the runner said to owner "asker" in `outs`.
fn to_asker(outs: &[Out]) -> Vec<ToOwner> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Say { peer, json } if peer == "asker" => serde_json::from_str::<Envelope<ToOwner>>(json).ok().map(|e| e.msg),
            _ => None,
        })
        .collect()
}

/// Owner "asker"'s run `a` probed here and its offer taken: its claim.
fn claimed(p: &mut Placer, a: &Ask, now: i64) -> anyhow::Result<Fence> {
    let outs = p.told("asker", &word(ToRunner::Probe { ask: Arc::new(a.clone()), elsewhere: 0 }, None)?, now);
    let fence = to_asker(&outs).into_iter().find_map(|m| match m {
        ToOwner::Offer { run, fence, .. } if run == a.run => Some(fence),
        _ => None,
    });
    let fence = fence.ok_or_else(|| anyhow::anyhow!("{} offered: {outs:?}", a.run))?;
    let _ = p.told("asker", &word(ToRunner::Take { run: a.run.clone(), fence }, None)?, now);
    Ok(fence)
}

fn reserves(outs: &[Out]) -> usize {
    outs.iter().filter(|o| matches!(o, Out::KeeperSay { word: ToKeeper::Reserve { .. }, .. })).count()
}

fn releases(outs: &[Out]) -> usize {
    outs.iter().filter(|o| matches!(o, Out::KeeperSay { word: ToKeeper::Release { .. }, .. })).count()
}

/// The holder's states as placement stands on them (Hex's capacity-view.md, "The protocol"; CV11):
/// with no keeper named it admits on its own room; named, it stands joining, its link down, and
/// nothing starts (a leg is refused, saying why, nothing reserved). Linked, its hello is said on its
/// first link (joining); welcomed, its sync said (syncing), still taking nothing new; synced, it
/// admits on the keeper's view (up). Its link ending takes it back down, joining again, until a new
/// link's sync is answered; the keeper starting again resets that link, and the next syncs again.
#[test]
fn holder_states_stand_through_placement_and_nothing_starts_until_synced() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let cap = machine()?;
        let mut p = runner(1);
        anyhow::ensure!(p.admission().keeper() == Keeper::None && p.view().get("keeper").is_some_and(serde_json::Value::is_null), "no keeper named: its own room, as ever");
        let _ = p.keeper_start(0);
        anyhow::ensure!(p.admission().keeper() == Keeper::Unsynced(Unsynced::Joining) && link_is(&p, "down"), "{:?}", p.view());
        let (verdict, outs) = p.leg(ask("leg-0", 1, 1, None), 1);
        anyhow::ensure!(verdict == Some(Err(Unsynced::Joining.why().to_owned())) && outs.is_empty(), "nothing reserved or started unsynced: {verdict:?} {outs:?}");
        let link = cap.link().await.map_err(fail)?;
        let outs = p.keeper_linked(2);
        anyhow::ensure!(link_is(&p, "joining") && p.keeper_link() == 1, "{:?}", p.view());
        let [Out::KeeperSay { link: 1, word: hello @ ToKeeper::Hello { .. } }] = outs.as_slice() else { anyhow::bail!("its hello on its first link: {outs:?}") };
        link.say(hello.clone()).await.map_err(fail)?;
        let welcome = link.hear().await.map_err(fail)?;
        let outs = p.keeper_heard(welcome, 3);
        anyhow::ensure!(link_is(&p, "syncing") && p.admission().keeper() == Keeper::Unsynced(Unsynced::Joining), "welcomed, not synced: {:?}", p.view());
        let [Out::KeeperSay { word: sync @ ToKeeper::Sync { .. }, .. }] = outs.as_slice() else { anyhow::bail!("its sync: {outs:?}") };
        link.say(sync.clone()).await.map_err(fail)?;
        let answer = link.hear().await.map_err(fail)?;
        let _ = p.keeper_heard(answer, 4);
        anyhow::ensure!(link_is(&p, "up") && synced(&p), "{:?}", p.view());
        drop(link);
        let _ = p.keeper_ended(WaitEnded::Died, 5);
        anyhow::ensure!(link_is(&p, "down") && p.admission().keeper() == Keeper::Unsynced(Unsynced::Joining), "its link ended: joining again");
        let (verdict, _) = p.leg(ask("leg-1", 1, 1, None), 5);
        anyhow::ensure!(verdict == Some(Err(Unsynced::Joining.why().to_owned())), "{verdict:?}");
        let link = join(&mut p, &cap, 6).await?;
        anyhow::ensure!(synced(&p) && p.keeper_link() == 2, "synced again on its second link");
        cap.restart().map_err(fail)?;
        anyhow::ensure!(link.hear().await.is_err(), "the keeper started again: its link ended");
        let _ = p.keeper_ended(WaitEnded::Reset, 7);
        anyhow::ensure!(p.admission().keeper() == Keeper::Unsynced(Unsynced::Joining));
        drop(link);
        let _link = join(&mut p, &cap, 8).await?;
        anyhow::ensure!(synced(&p) && p.keeper_link() == 3, "{:?}", p.view());
        Ok(())
    })
}

/// The holder's item 1 on every start path: each reserves its room through the keeper first and
/// starts only on the grant, what waits on it waiting with it. A CI leg is answered on its grant; an
/// owner's claim and the kernel's start hold their specs (`Spec::Wait`) until then; an agent
/// session and this node's own run are answered then too. The keeper's books hold each run's room,
/// each run's box is its reservation's, and each gives its room back as it ends.
#[test]
fn holder_every_start_path_starts_only_on_its_reserves_grant() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let cap = machine()?;
        let mut p = runner(1);
        let _ = p.keeper_start(0);
        let link = join(&mut p, &cap, 1).await?;
        anyhow::ensure!(synced(&p), "{:?}", p.admission().keeper());

        // A CI leg.
        let (verdict, outs) = p.leg(ask("leg-1", 1, 1, None), 10);
        anyhow::ensure!(verdict.is_none() && reserves(&outs) == 1 && !running(&p, CI, "leg-1"), "nothing starts before its grant: {verdict:?} {outs:?}");
        let seen = talk(&mut p, &*link, outs, 11).await?;
        anyhow::ensure!(seen.contains(&Out::Leg { run: "leg-1".into(), verdict: Ok(()) }) && running(&p, CI, "leg-1"), "admitted on its grant: {seen:?}");

        // An owner's claim, its spec held for its verdict.
        let fence = claimed(&mut p, &ask("r-1", 2, 4, None), 20)?;
        let outs = p.told("asker", &word(ToRunner::Start { run: "r-1".into(), fence, elsewhere: 0 }, Some(1))?, 21);
        anyhow::ensure!(reserves(&outs) == 1 && p.spec("asker", 1) == Spec::Wait && !running(&p, "asker", "r-1"), "its spec waits for its grant: {outs:?}");
        let seen = talk(&mut p, &*link, outs, 22).await?;
        anyhow::ensure!(seen.contains(&Out::SpecReady { peer: "asker".into(), id: 1 }), "{seen:?}");
        anyhow::ensure!(p.spec("asker", 1) == Spec::Start { run: "r-1".into() } && running(&p, "asker", "r-1"), "started on its grant");

        // The kernel's start, its spec held the same way.
        let at = p.admission().fast_word().epoch;
        let fast = ToRunner::Fast { ask: Arc::new(ask("k-1", 1, 2, None)), kernel: FastHandle { slot: 1, generation: 1 }, epoch: at, elsewhere: 0 };
        let outs = p.told("asker", &word(fast, Some(2))?, 30);
        anyhow::ensure!(reserves(&outs) == 1 && p.spec("asker", 2) == Spec::Wait && !running(&p, "asker", "k-1"), "{outs:?}");
        let seen = talk(&mut p, &*link, outs, 31).await?;
        anyhow::ensure!(seen.contains(&Out::SpecReady { peer: "asker".into(), id: 2 }) && p.spec("asker", 2) == Spec::Start { run: "k-1".into() } && running(&p, "asker", "k-1"), "{seen:?}");

        // An agent session its owner placed here.
        let fence = claimed(&mut p, &ask("s-1", 1, 2, Some("session:claude")), 40)?;
        let claim = Claim { owner: "asker".into(), epoch: 1, run: "s-1".into(), fence };
        let (ok, outs) = p.session_start(&claim, None, 41);
        anyhow::ensure!(ok.is_none() && reserves(&outs) == 1 && !running(&p, "asker", "s-1"), "{ok:?} {outs:?}");
        let seen = talk(&mut p, &*link, outs, 42).await?;
        anyhow::ensure!(seen.contains(&Out::Session { claim: claim.clone(), ok: true }) && running(&p, "asker", "s-1"), "{seen:?}");

        // This node's own run, placed here (its owner told to go at once, or once its offers are in).
        let _ = p.directory(&[], 50);
        let mut outs = p.ask(ask("l-1", 1, 1, None), 51);
        let go = |outs: &[Out]| {
            outs.iter().find_map(|o| match o {
                Out::Local { run, placed: Placed::Go { runner, fence } } if run == "l-1" && runner == ME => Some(*fence),
                _ => None,
            })
        };
        if go(&outs).is_none() {
            outs.extend(p.tick(5_000));
        }
        let fence = go(&outs).ok_or_else(|| anyhow::anyhow!("placed here: {outs:?}"))?;
        let (ok, outs) = p.starting("l-1", ME, fence, 5_001);
        anyhow::ensure!(ok.is_none() && reserves(&outs) == 1 && !running(&p, ME, "l-1"), "{ok:?} {outs:?}");
        let seen = talk(&mut p, &*link, outs, 5_002).await?;
        anyhow::ensure!(seen.contains(&Out::Started { run: "l-1".into(), ok: true }) && running(&p, ME, "l-1"), "{seen:?}");

        // The keeper's books hold each one's room, each run's box its reservation's.
        let held = books(&cap)?;
        let cpus: u64 = held.values().map(|a| a.cpu_milli).sum();
        anyhow::ensure!(held.len() == 5 && cpus == 6_000, "{held:?}");
        for (owner, run) in [(CI, "leg-1"), ("asker", "r-1"), ("asker", "k-1"), ("asker", "s-1"), (ME, "l-1")] {
            anyhow::ensure!(matches!(p.box_of(owner, 1, run), Some(BoxName::Reserved(id)) if held.contains_key(&id)), "{run}: {:?}", p.box_of(owner, 1, run));
        }

        // Each gives its room back as it ends.
        let mut outs = p.leg_ended("leg-1", true, 6_000);
        outs.extend(p.ended("asker", 1, true, 6_001));
        outs.extend(p.ended("asker", 2, true, 6_002));
        outs.extend(p.session_ended(&claim, 6_003));
        outs.extend(p.local_ended("l-1", true, 6_004));
        anyhow::ensure!(releases(&outs) == 5, "{outs:?}");
        let _ = talk(&mut p, &*link, outs, 6_005).await?;
        anyhow::ensure!(books(&cap)?.is_empty(), "{:?}", books(&cap)?);
        Ok(())
    })
}

/// The holder's items 4 and 5: Hex's boxes take the machine while the runner's view is stale, so
/// each reserve is refused for room, the refusal's view said first: a leg is refused saying why,
/// an owner's claim lapses (its spec refused, its run waiting here again on the room there is), and
/// the kernel's start is declined for room. Nothing starts and the keeper holds nothing of it.
#[test]
fn holder_a_reserve_refused_starts_nothing_and_says_why() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let cap = machine()?;
        let mut p = runner(1);
        let _ = p.keeper_start(0);
        let link = join(&mut p, &cap, 1).await?;
        let fence = claimed(&mut p, &ask("r-1", 2, 4, None), 2)?;
        let at = p.admission().fast_word().epoch;
        cap.host(|v| v.set_local(room())).map_err(fail)?.map_err(fail)?;
        let (verdict, outs) = p.leg(ask("leg-1", 1, 1, None), 3);
        anyhow::ensure!(verdict.is_none(), "its view still has room: reserved");
        let mut outs = outs;
        outs.extend(p.told("asker", &word(ToRunner::Start { run: "r-1".into(), fence, elsewhere: 0 }, Some(1))?, 3));
        let fast = ToRunner::Fast { ask: Arc::new(ask("k-1", 1, 1, None)), kernel: FastHandle { slot: 1, generation: 1 }, epoch: at, elsewhere: 0 };
        outs.extend(p.told("asker", &word(fast, Some(2))?, 3));
        let seen = talk(&mut p, &*link, outs, 4).await?;
        anyhow::ensure!(seen.contains(&Out::Leg { run: "leg-1".into(), verdict: Err(NotGranted::Room.why().to_owned()) }), "{seen:?}");
        anyhow::ensure!(matches!(p.spec("asker", 1), Spec::Refuse(why) if why.contains(NotGranted::Room.why())), "its spec refused");
        anyhow::ensure!(matches!(p.spec("asker", 2), Spec::Refuse(_)), "the kernel's start not started");
        let words = to_asker(&seen);
        anyhow::ensure!(words.iter().any(|m| matches!(m, ToOwner::Lapsed { run, .. } if run == "r-1")), "the claim lapsed, its owner told: {words:?}");
        anyhow::ensure!(words.iter().any(|m| matches!(m, ToOwner::FastDeclined { run, .. } if run == "k-1")), "the kernel's start declined: {words:?}");
        anyhow::ensure!(!running(&p, CI, "leg-1") && !running(&p, "asker", "r-1") && !running(&p, "asker", "k-1") && books(&cap)?.is_empty());
        Ok(())
    })
}

/// Hex's holder wait contract under placement (CV6, the holder's item 4; decision 9; no constants:
/// Shards' R03): its hello answered in 10 ms and its sync in 20, the slowest answer is 20 ms; a
/// leg's reserve said at 100 is due at 121, placement's timer then. At 121 the keeper, its link
/// alive, has said nothing for longer than the slowest answer it has given: the wait has run out,
/// told once, and judges nothing. The leg isn't refused and nothing is given back; the runner
/// stays synced and takes a new leg; no timer is set for that wait again. The grant that comes late
/// starts the leg. Its link's end is what gives a reserve up (`keeper_ended`).
#[test]
fn holder_a_reserve_past_the_slowest_answer_runs_out_and_keeps_waiting() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let cap = machine()?;
        let mut p = runner(1);
        let _ = p.keeper_start(0);
        let link = cap.link().await.map_err(fail)?;
        let hello = p.keeper_linked(0);
        anyhow::ensure!(p.keeper_due().is_none(), "before its first answer no time bounds a wait");
        say(&*link, hello).await?;
        let sync = p.keeper_heard(link.hear().await.map_err(fail)?, 10);
        say(&*link, sync).await?;
        let none = p.keeper_heard(link.hear().await.map_err(fail)?, 30);
        anyhow::ensure!(none.is_empty() && synced(&p), "{none:?}");
        let (verdict, outs) = p.leg(ask("leg-1", 2, 4, None), 100);
        anyhow::ensure!(verdict.is_none() && reserves(&outs) == 1, "{outs:?}");
        anyhow::ensure!(p.keeper_due() == Some(121) && p.timer().is_some_and(|t| t <= 121), "due once it waited longer than the slowest answer: {:?}", p.keeper_due());
        let early = p.tick(120);
        anyhow::ensure!(early.is_empty(), "still waiting at its due: {early:?}");
        let ran_out = p.tick(121);
        anyhow::ensure!(ran_out.is_empty(), "a run-out refuses no leg and gives nothing back: {ran_out:?}");
        anyhow::ensure!(synced(&p) && p.keeper_due().is_none(), "still synced, and told once: {:?}", p.keeper_due());
        anyhow::ensure!(p.tick(500).is_empty(), "nothing again later");
        let (verdict, more) = p.leg(ask("leg-2", 1, 1, None), 122);
        anyhow::ensure!(verdict.is_none() && reserves(&more) == 1, "a new leg's reserve said: {verdict:?} {more:?}");
        let seen = talk(&mut p, &*link, outs.into_iter().chain(more).collect(), 200).await?;
        anyhow::ensure!(seen.contains(&Out::Leg { run: "leg-1".into(), verdict: Ok(()) }) && running(&p, CI, "leg-1"), "the late grant starts it: {seen:?}");
        anyhow::ensure!(seen.contains(&Out::Leg { run: "leg-2".into(), verdict: Ok(()) }) && running(&p, CI, "leg-2"), "{seen:?}");
        anyhow::ensure!(releases(&seen) == 0 && books(&cap)?.len() == 2, "both held: {:?}", books(&cap)?);
        Ok(())
    })
}

/// A start that asks nothing reserves nothing (Hex's holder and keeper take no empty amount) and
/// starts at once, its box numbered in this process from 0 (agreed with Hex's lead): its cold start.
/// Its end gives nothing back.
#[test]
fn holder_a_start_asking_nothing_starts_at_once_unreserved() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let cap = machine()?;
        let mut p = runner(4);
        let _ = p.keeper_start(0);
        let _link = join(&mut p, &cap, 1).await?;
        for (n, run) in ["z-0", "z-1"].into_iter().enumerate() {
            let (verdict, outs) = p.leg(ask(run, 0, 0, None), 2);
            anyhow::ensure!(verdict == Some(Ok(())) && reserves(&outs) == 0 && running(&p, CI, run), "{verdict:?} {outs:?}");
            let n = u64::try_from(n)?;
            anyhow::ensure!(p.box_of(CI, 4, run) == Some(BoxName::Unreserved { epoch: 4, n }), "{:?}", p.box_of(CI, 4, run));
            anyhow::ensure!(p.box_of(CI, 4, run).map(|b| b.to_string()) == Some(format!("kiln-z4-{n}")));
        }
        let outs = p.leg_ended("z-0", true, 3);
        anyhow::ensure!(releases(&outs) == 0 && books(&cap)?.is_empty(), "{outs:?}");
        Ok(())
    })
}

/// The holder's item 3: a session's reservation is kept with its record before it starts. The
/// runner's process ends while the session goes on (its harness continues it), the keeper holding
/// its room (CV5); the next process, in a later epoch, takes the session up from its records and
/// lists its reservation, with the session's room, in its sync, which the keeper answers agreeing.
/// The session's end gives it back.
#[test]
fn holder_a_runner_started_again_lists_its_survivors_and_gives_each_back_as_it_ends() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let dir = tempfile::tempdir()?;
        let records = crate::records::open(&dir.path().join("place.db"))?;
        let cap = machine()?;
        let mut p = runner(1);
        p.keep_in(Arc::clone(&records));
        let _ = p.restore(load(&*records).await?, 0);
        let _ = p.keeper_start(0);
        let link = join(&mut p, &cap, 1).await?;
        let session = ask("s-1", 2, 4, Some("session:claude"));
        let fence = claimed(&mut p, &session, 10)?;
        let claim = Claim { owner: "asker".into(), epoch: 1, run: "s-1".into(), fence };
        let (_, outs) = p.session_start(&claim, None, 11);
        let seen = talk(&mut p, &*link, outs, 12).await?;
        anyhow::ensure!(seen.contains(&Out::Session { claim: claim.clone(), ok: true }), "{seen:?}");
        for tx in p.take_unkept() {
            records.commit(tx).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        let held = books(&cap)?;
        let &[(id, amount)] = held.iter().map(|(id, amount)| (*id, *amount)).collect::<Vec<_>>().as_slice() else { anyhow::bail!("one reservation: {held:?}") };
        let kept = load(&*records).await?;
        anyhow::ensure!(kept.ran.iter().any(|((o, e, r), row)| o == "asker" && *e == 1 && r == "s-1" && row.record.reservation == Some((id.epoch, id.seq))), "kept with its record: {kept:?}");
        // Its process ends; what it held stays held (CV5).
        drop(link);
        drop(p);
        let mut p = runner(2);
        p.keep_in(Arc::clone(&records));
        let _ = p.restore(load(&*records).await?, 20);
        let _ = p.keeper_start(20);
        anyhow::ensure!(p.box_of("asker", 1, "s-1") == Some(BoxName::Reserved(id)), "its survivor's box");
        let link = cap.link().await.map_err(fail)?;
        let hello = p.keeper_linked(21);
        let _ = talk(&mut p, &*link, hello, 21).await?;
        anyhow::ensure!(synced(&p), "{:?}", p.view());
        let view = match p.admission().keeper() {
            Keeper::Synced(view) => view,
            other => anyhow::bail!("{other:?}"),
        };
        anyhow::ensure!(view.held == amount.to_array() && view.own == [0; 4], "the survivor is held, for no start of this process's: {view:?}");
        anyhow::ensure!(books(&cap)? == held, "its sync listed it, the keeper agreeing: {:?}", books(&cap)?);
        let outs = p.session_ended(&claim, 30);
        anyhow::ensure!(releases(&outs) == 1, "{outs:?}");
        let _ = talk(&mut p, &*link, outs, 31).await?;
        anyhow::ensure!(books(&cap)?.is_empty(), "given back as it ended");
        Ok(())
    })
}

/// What each `HEX_CV_HELD=` line among `lines` says, in order.
fn held_lines(lines: &[String]) -> anyhow::Result<Vec<Vec<hex::Held>>> {
    lines.iter().filter_map(|l| l.split("HEX_CV_HELD=").nth(1)).map(|h| Ok(serde_json::from_slice(&::hex::decode(h.trim())?)?)).collect()
}

/// HEX-CV-2K's reads of the runner's log (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 13): a
/// line per sync answered, with its `agree`; per link lost, with the reserves it gave up unanswered;
/// per join again; and once nothing it said waits on the keeper, `HEX_CV_HELD=` and the hex of the
/// JSON of what it holds (Hex's `Holder::held`), logged again only when that changed, the last one
/// the keeper's books.
#[test]
fn holder_logs_each_sync_each_link_lost_each_rejoin_and_what_it_holds_once_settled() -> anyhow::Result<()> {
    crate::rt::block_on(async {
        let cap = machine()?;
        let mut p = runner(1);
        let _ = p.keeper_start(0);
        let link = join(&mut p, &cap, 1).await?;
        let (_, outs) = p.leg(ask("leg-1", 1, 1, None), 10);
        let _ = talk(&mut p, &*link, outs, 11).await?;
        let granted = books(&cap)?;
        anyhow::ensure!(granted.len() == 1, "{granted:?}");
        let outs = p.leg_ended("leg-1", true, 20);
        let _ = talk(&mut p, &*link, outs, 21).await?;
        // A reserve said and its answer never heard: the link goes, and it is given up.
        let (_, outs) = p.leg(ask("leg-2", 1, 1, None), 30);
        say(&*link, outs).await?;
        drop(link);
        let _ = p.keeper_ended(WaitEnded::Died, 31);
        let _link = join(&mut p, &cap, 40).await?;
        anyhow::ensure!(synced(&p), "synced again: {:?} {:?}", p.view().get("keeper"), books(&cap)?);
        let lines = p.keeper.as_ref().map(|h| h.logged.clone()).unwrap_or_default();
        let count = |what: &str| lines.iter().filter(|l| l.contains(what)).count();
        anyhow::ensure!(count("capacity keeper synced") == 2 && count("agree=true") == 2, "a line per sync, each agreeing: {lines:#?}");
        anyhow::ensure!(count("capacity keeper link lost") == 1 && lines.iter().any(|l| l.contains("capacity keeper link lost") && l.contains("unanswered=1")), "{lines:#?}");
        anyhow::ensure!(count("capacity keeper joined") == 1 && count("capacity keeper rejoined") == 1, "{lines:#?}");
        let held = held_lines(&lines)?;
        let leg_1: Vec<hex::Held> = granted.iter().map(|(id, amount)| hex::Held { id: *id, amount: *amount }).collect();
        anyhow::ensure!(held == vec![Vec::new(), leg_1, Vec::new()], "once settled, each change: {held:?} {lines:#?}");
        anyhow::ensure!(books(&cap)?.is_empty(), "the last one the keeper's books");
        Ok(())
    })
}
