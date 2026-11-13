//! The kernel's properties, one test each, and each test with the planted bug it catches (kiln
//! `crates/kiln-ci/src/lb/tests.rs` at f20858c9c, adapted to boxes: the pick packs, and a node's
//! agent that starts again keeps the boxes that outlived it). The model ([`super::model`]) checks
//! them all at once under random interleavings; these pin each one down on a case small enough to
//! read.

use super::model::Rng;
use super::*;

const C8: Amounts = [8 * CPU, 32 << 30, 400 << 30, 8];

fn n(r: RunnerId) -> usize {
    usize::try_from(r.0).unwrap()
}

/// One owner and `n` nodes of `budget`, every one a member of one signature and watched.
fn fleet(n: u32, budget: Amounts) -> (Owner, Vec<Runner>, SigId) {
    let mut o = Owner::new(OwnerId(0));
    let mut out = Out::default();
    let mut rs = Vec::new();
    for i in 0..n {
        let mut r = Runner::new(RunnerId(i), budget, 1);
        r.watch(OwnerId(0));
        o.join(RunnerId(i), budget, r.word(), &mut out).unwrap();
        rs.push(r);
    }
    let s = o.sig().unwrap();
    for i in 0..n {
        o.add(s, RunnerId(i), &mut out).unwrap();
    }
    (o, rs, s)
}

/// Delivers everything in flight between one owner and its nodes, in order, until nothing is.
fn settle(o: &mut Owner, rs: &mut [Runner], out: &mut Out) {
    let mut rout = RunnerOut::new();
    loop {
        let mut moved = false;
        for (to, m) in std::mem::take(&mut out.to) {
            moved = true;
            if let Some(r) = rs.get_mut(n(to))
                && let Some(reply) = r.hear(m, &mut rout)
            {
                o.hear(reply, out);
            }
        }
        for (_, m) in std::mem::take(&mut rout) {
            moved = true;
            o.hear(m, out);
        }
        for r in rs.iter_mut() {
            r.drain(&mut rout);
        }
        if !moved && rout.is_empty() {
            return;
        }
    }
}

/// Asks once and carries the start through to its node and back: where it started.
fn ask_start(
    o: &mut Owner,
    rs: &mut [Runner],
    s: SigId,
    need: Amounts,
) -> Option<(RunnerId, Handle, Handle)> {
    let mut out = Out::default();
    let a = o.ask(s, need, &mut out).ok()?;
    let (to, m) = a.send?;
    let reply = rs.get_mut(n(to))?.hear(m, &mut RunnerOut::new())?;
    o.hear(reply, &mut out);
    match reply {
        ToOwner::Started { at, .. } => Some((to, a.run, at)),
        _ => None,
    }
}

/// Deletes a box on its node, and its owner hears it.
fn end(o: &mut Owner, rs: &mut [Runner], r: RunnerId, at: Handle) {
    let mut out = Out::default();
    if let Some((_, m)) = rs
        .get_mut(n(r))
        .and_then(|x| x.end(at, &mut RunnerOut::new()))
    {
        o.hear(m, &mut out);
    }
}

/// Runs `f`, then with `m` planted, and says whether `f` failed under it (caught).
fn caught(m: Mutant, f: impl Fn() -> Result<(), String>) -> Result<(), String> {
    f()?;
    match with_mutant(Some(m), &f) {
        Err(_) => Ok(()),
        Ok(()) => Err(format!("{m:?} was not caught")),
    }
}

/// Hex's pick packs: the member with the least room that holds the need, whatever order they
/// joined in. (Kiln's test held W05-R23's least loaded: the most room.)
fn packs() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(3, C8);
    // Load node 0 with 5 CPUs and node 2 with 2: node 0 has 3 free, node 2 has 6, node 1 has 8.
    for (r, cpus) in [(0u32, 5u64), (2, 2)] {
        for _ in 0..cpus {
            let mut out = Out::default();
            let Some(rr) = rs.get_mut(r as usize) else {
                return Err("no node".into());
            };
            let s0 = rr.start(Start {
                owner: OwnerId(1),
                run: Handle::NONE,
                epoch: 1,
                need: [CPU, 0, 0, 0],
            });
            if let ToOwner::Started { word, .. } = s0 {
                o.hear(
                    ToOwner::Hint {
                        runner: RunnerId(r),
                        word,
                    },
                    &mut out,
                );
            }
        }
    }
    let picks: Vec<u32> = (0..4)
        .filter_map(|_| ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).map(|p| p.0.0))
        .collect();
    // 0 has 3 free: 0, 0, 0 (to 0 free), then 2, which has 6, the least that holds one; never 1.
    if picks != [0, 0, 0, 2] {
        return Err(format!("picks {picks:?}"));
    }
    Ok(())
}

#[test]
fn the_pick_packs_a_box_onto_the_least_room_that_holds_it() -> Result<(), String> {
    caught(Mutant::MostFree, packs)
}

/// Equals take turns: members with the same room are picked first in, first out.
fn turns() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(3, C8);
    let mut seen = Vec::new();
    for _ in 0..6 {
        let Some((r, _, at)) = ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]) else {
            return Err("no start".into());
        };
        seen.push(r.0);
        // It is deleted at once: every member is back to the same room.
        end(&mut o, &mut rs, r, at);
    }
    if seen != vec![0, 1, 2, 0, 1, 2] {
        return Err(format!("turns {seen:?}"));
    }
    Ok(())
}

#[test]
fn equals_take_turns() -> Result<(), String> {
    caught(Mutant::NoTurn, turns)
}

/// No node is ever admitted past its budget, in any dimension: memory short here.
fn within_budget() -> Result<(), String> {
    let mut r = Runner::new(RunnerId(0), [8 * CPU, 8, 0, 0], 1);
    let a = r.start(Start {
        owner: OwnerId(0),
        run: Handle::NONE,
        epoch: 1,
        need: [CPU, 6, 0, 0],
    });
    let b = r.start(Start {
        owner: OwnerId(0),
        run: Handle::NONE,
        epoch: 1,
        need: [CPU, 6, 0, 0],
    });
    if !matches!(a, ToOwner::Started { .. })
        || !matches!(b, ToOwner::Declined { why: Why::Room, .. })
    {
        return Err(format!("{a:?} {b:?}"));
    }
    let taken: u64 = r.admitted().iter().map(|a| a.3[1]).sum();
    if taken > 8 {
        return Err(format!("{taken} bytes of memory taken of 8"));
    }
    Ok(())
}

#[test]
fn no_node_takes_past_its_budget() -> Result<(), String> {
    caught(Mutant::AdmitPastRoom, within_budget)
}

/// Backfilling (EASY's guarantee to the first in line): a box that waits for the whole node keeps
/// its room; smaller starts pass it only in room it can't use, and it is held as soon as what ran
/// there is deleted.
fn keeps_room() -> Result<(), String> {
    let mut r = Runner::new(RunnerId(0), [8 * CPU, 0, 0, 0], 1);
    let mut out = RunnerOut::new();
    let small = |n: u32| Start {
        owner: OwnerId(0),
        run: Handle {
            slot: n,
            generation: 1,
        },
        epoch: 1,
        need: [CPU, 0, 0, 0],
    };
    let ToOwner::Started { at: a, .. } = r.start(small(1)) else {
        return Err("first start".into());
    };
    let big = Start {
        owner: OwnerId(1),
        run: Handle {
            slot: 9,
            generation: 1,
        },
        epoch: 1,
        need: [8 * CPU, 0, 0, 0],
    };
    if !matches!(r.wait(big), ToOwner::Queued { .. }) {
        return Err("the big box should queue".into());
    }
    // Seven CPUs are free, but every one of them is the big box's: no small start passes.
    if let ToOwner::Started { .. } = r.start(small(2)) {
        return Err("a small box took room the first in line needs".into());
    }
    r.end(a, &mut out);
    if !out
        .iter()
        .any(|(o, m)| *o == OwnerId(1) && matches!(m, ToOwner::Ready { .. }))
    {
        return Err(format!(
            "the big box wasn't held when the node emptied: {out:?}"
        ));
    }
    Ok(())
}

#[test]
fn the_first_in_line_keeps_its_room() -> Result<(), String> {
    caught(Mutant::NoKeep, keeps_room)
}

/// The owner deducts what it sends at once: many starts in flight before any answer, and none is
/// declined for room. One node has 8 CPUs free and three have 2 (another owner holds the rest):
/// without the deduction every start would go to one node.
fn no_herding() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(4, C8);
    let mut out = Out::default();
    for r in 1..4u32 {
        let Some(rr) = rs.get_mut(r as usize) else {
            return Err("no node".into());
        };
        let ToOwner::Started { word, .. } = rr.start(Start {
            owner: OwnerId(1),
            run: Handle::NONE,
            epoch: 1,
            need: [6 * CPU, 0, 0, 0],
        }) else {
            return Err("no start".into());
        };
        o.hear(
            ToOwner::Hint {
                runner: RunnerId(r),
                word,
            },
            &mut out,
        );
    }
    let mut sends = Vec::new();
    for _ in 0..14 {
        let a = o
            .ask(s, [CPU, 0, 0, 0], &mut out)
            .map_err(|e| format!("{e:?}"))?;
        sends.extend(a.send);
    }
    for (to, m) in sends {
        let reply = rs
            .get_mut(n(to))
            .and_then(|r| r.hear(m, &mut RunnerOut::new()));
        if let Some(ToOwner::Declined { why, .. }) = reply {
            return Err(format!(
                "declined for {why:?} with 14 starts in flight to 14 CPUs"
            ));
        }
    }
    Ok(())
}

#[test]
fn starts_in_flight_never_herd() -> Result<(), String> {
    caught(Mutant::NoDeduct, no_herding)
}

/// The kept boxes of a node whose agent started again: its links drop, it recovers what outlived
/// it, and the links come back.
fn recover_with_links(o: &mut Owner, r: &mut Runner, out: &mut Out) -> RunnerOut {
    let mut rout = RunnerOut::new();
    o.link_down(r.id(), out);
    r.link_down(OwnerId(0), &mut rout);
    let kept: Vec<Kept> = r
        .admitted()
        .iter()
        .map(|&(owner, run, _, need)| Kept { owner, run, need })
        .collect();
    r.recover(&kept);
    rout
}

/// The fence: a start sent under the epoch before the node's agent started again is declined.
fn fenced() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, C8);
    ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("no start")?;
    let mut out = Out::default();
    let a = o
        .ask(s, [CPU, 0, 0, 0], &mut out)
        .map_err(|e| format!("{e:?}"))?;
    let Some((_, m)) = a.send else {
        return Err("no send".into());
    };
    let r = rs.get_mut(0).ok_or("no node")?;
    recover_with_links(&mut o, r, &mut out);
    let reply = r.hear(m, &mut RunnerOut::new()).ok_or("no reply")?;
    if !matches!(
        reply,
        ToOwner::Declined {
            why: Why::Epoch,
            ..
        }
    ) {
        return Err(format!(
            "a start from before the agent started again: {reply:?}"
        ));
    }
    Ok(())
}

#[test]
fn an_old_epochs_start_is_declined() -> Result<(), String> {
    caught(Mutant::NoFence, fenced)
}

/// Hex's adaptation: a box outlives its node's agent. Once the agent recovers and its link is
/// back, the box is said again and its owner keeps it started; its room is still taken there.
/// (Kiln's runner started again empty, and its owner took the job for lost.)
fn outlives_the_agent() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, [2 * CPU, 0, 0, 0]);
    let (_, first, _) = ask_start(&mut o, &mut rs, s, [2 * CPU, 0, 0, 0]).ok_or("no start")?;
    let mut out = Out::default();
    let r = rs.get_mut(0).ok_or("no node")?;
    let mut rout = recover_with_links(&mut o, r, &mut out);
    r.link_up(OwnerId(0), &mut rout);
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    if !matches!(o.state(first), Some(State::Started { .. }))
        || out
            .told
            .iter()
            .any(|t| matches!(t, Told::Lost { .. } | Told::Ended { .. }))
    {
        return Err(format!(
            "after the agent recovered: {:?}, told {:?}",
            o.state(first),
            out.told
        ));
    }
    if r.word().cpu != 0 || r.admitted().len() != 1 {
        return Err(format!("the kept box's room isn't held: {:?}", r.word()));
    }
    Ok(())
}

#[test]
fn a_box_outlives_its_nodes_agent() -> Result<(), String> {
    caught(Mutant::RecoverForgets, outlives_the_agent)
}

/// Generations: a handle kept past its value finds nothing, even once its slot holds another.
fn generations() -> Result<(), String> {
    let mut s: Slab<u32> = Slab::new();
    let a = s.insert(1).ok_or("full")?;
    s.remove(a).ok_or("gone")?;
    let b = s.insert(2).ok_or("full")?;
    if a.slot != b.slot {
        return Err("the slot wasn't reused".into());
    }
    if s.get(a).is_some() || s.remove(a).is_some() {
        return Err("a stale handle found the newer value".into());
    }
    if s.get(b) != Some(&2) {
        return Err("the newer handle lost its value".into());
    }
    Ok(())
}

#[test]
fn an_old_generations_word_is_ignored() -> Result<(), String> {
    caught(Mutant::NoGeneration, generations)
}

/// A box no member has room for waits at two nodes; the first to hold its room starts it and the
/// other is withdrawn: it starts once.
fn waits_once() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(3, [2 * CPU, 0, 0, 0]);
    let mut started = Vec::new();
    for _ in 0..6 {
        started.push(ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("fill")?);
    }
    let mut out = Out::default();
    let a = o
        .ask(s, [CPU, 0, 0, 0], &mut out)
        .map_err(|e| format!("{e:?}"))?;
    if a.send.is_some() || out.to.len() != PLACES {
        return Err(format!("it should wait at {PLACES} nodes: {out:?}"));
    }
    // Both places queue it; then a box is deleted at each at once, before the owner hears either.
    let mut rout = RunnerOut::new();
    let waits = std::mem::take(&mut out.to);
    let mut replies = Vec::new();
    for (to, m) in &waits {
        replies.extend(rs.get_mut(n(*to)).and_then(|r| r.hear(*m, &mut rout)));
    }
    for (to, _) in &waits {
        let Some(&(_, _, at)) = started.iter().find(|x| x.0 == *to) else {
            return Err("no box there".into());
        };
        if let Some(m) = rs.get_mut(n(*to)).and_then(|r| r.end(at, &mut rout)) {
            rout.push(m);
        }
    }
    for m in replies {
        o.hear(m, &mut out);
    }
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    settle(&mut o, &mut rs, &mut out);
    let holding: usize = rs
        .iter()
        .map(|r| r.admitted().iter().filter(|x| x.1 == a.run).count())
        .sum();
    if holding != 1 {
        return Err(format!("the waiting box runs on {holding} nodes"));
    }
    if rs.iter().any(|r| !r.waiting().is_empty()) {
        return Err("a place is still held".into());
    }
    Ok(())
}

#[test]
fn a_box_that_waits_starts_once() -> Result<(), String> {
    caught(Mutant::GoTwice, waits_once)
}

/// A start declined for room (another owner took it) is placed again where there is room, and its
/// asker hears it started: no ask is lost.
fn declined_placed_again() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(2, [CPU, 0, 0, 0]);
    let mut out = Out::default();
    let a = o
        .ask(s, [CPU, 0, 0, 0], &mut out)
        .map_err(|e| format!("{e:?}"))?;
    let Some((to, m)) = a.send else {
        return Err("no send".into());
    };
    // Another owner fills that node first.
    let Some(r) = rs.get_mut(n(to)) else {
        return Err("no node".into());
    };
    let _ = r.start(Start {
        owner: OwnerId(1),
        run: Handle::NONE,
        epoch: 1,
        need: [CPU, 0, 0, 0],
    });
    let reply = r.hear(m, &mut RunnerOut::new()).ok_or("no reply")?;
    o.hear(reply, &mut out);
    settle(&mut o, &mut rs, &mut out);
    match o.state(a.run) {
        Some(State::Started { runner, .. }) if runner != to => Ok(()),
        other => Err(format!("after a decline: {other:?}")),
    }
}

#[test]
fn a_declined_start_is_placed_again() -> Result<(), String> {
    caught(Mutant::DropDeclined, declined_placed_again)
}

/// Never: more than any member's whole budget is refused at once.
#[test]
fn more_than_any_budget_is_never() -> Result<(), String> {
    let (mut o, _rs, s) = fleet(2, C8);
    match o.ask(s, [9 * CPU, 0, 0, 0], &mut Out::default()) {
        Err(Refused::Never) => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

/// The index against a plain model: a set of members per bucket and a cursor per bucket. Random
/// pushes, unlinks, moves and picks; its top, below, (Hex's) lowest at or above and next above,
/// next in turn and each bucket's members in turn order agree.
#[test]
fn the_index_keeps_buckets_and_turns() -> Result<(), String> {
    let mut rng = Rng(7);
    for round in 0..40u64 {
        let buckets = [3u32, 64, 65, 200, BUCKETS]
            .get(usize::try_from(round % 5).unwrap())
            .copied()
            .unwrap_or(3);
        let n = 1 + u32::try_from(rng.below(if round % 2 == 0 { 90 } else { 9000 })).unwrap();
        let start = u32::try_from(rng.next() >> 32).unwrap();
        let mut ix = Index::new(start);
        if !ix.grow(buckets, n) {
            return Err("grow".into());
        }
        let first =
            u32::try_from((u64::from(start) * u64::from(n.div_ceil(64) * 64)) >> 32).unwrap();
        let mut at: Vec<Option<u32>> = vec![None; n as usize];
        let mut sets: Vec<std::collections::BTreeSet<u32>> =
            vec![std::collections::BTreeSet::new(); buckets as usize];
        let mut cursor: Vec<u32> = vec![first; buckets as usize];
        // In turn: the members at or after the cursor, then those before it.
        let turn = |set: &std::collections::BTreeSet<u32>, c: u32| -> Vec<u32> {
            set.range(c..).chain(set.range(..c)).copied().collect()
        };
        for _ in 0..400 {
            let r = u32::try_from(rng.below(u64::from(n))).unwrap();
            let b = u32::try_from(rng.below(u64::from(buckets))).unwrap();
            match at.get(r as usize).copied().flatten() {
                None => {
                    ix.push(r, b);
                    sets.get_mut(b as usize).ok_or("b")?.insert(r);
                    *at.get_mut(r as usize).ok_or("r")? = Some(b);
                }
                Some(from) if rng.below(4) == 0 => {
                    ix.unlink(r, from);
                    sets.get_mut(from as usize).ok_or("b")?.remove(&r);
                    *at.get_mut(r as usize).ok_or("r")? = None;
                }
                Some(_) if rng.below(3) == 0 => {
                    // A pick from the lowest bucket: the next in turn, and the cursor past it.
                    let Some(low) = ix.at_or_above(0) else {
                        return Err("no bucket with members".into());
                    };
                    let set = sets.get(low as usize).ok_or("b")?;
                    let want = turn(set, *cursor.get(low as usize).ok_or("c")?)
                        .first()
                        .copied()
                        .ok_or("empty bucket")?;
                    let got = ix.next_in(low);
                    if got != want {
                        return Err(format!("round {round}: next in turn {got}, want {want}"));
                    }
                    ix.took(low, got);
                    *cursor.get_mut(low as usize).ok_or("c")? = got + 1;
                }
                Some(from) => {
                    ix.relink(r, from, b);
                    sets.get_mut(from as usize).ok_or("b")?.remove(&r);
                    sets.get_mut(b as usize).ok_or("b")?.insert(r);
                    *at.get_mut(r as usize).ok_or("r")? = Some(b);
                }
            }
            let top = sets
                .iter()
                .rposition(|l| !l.is_empty())
                .map(|t| u32::try_from(t).unwrap());
            if ix.top() != top {
                return Err(format!("round {round}: top {:?}, want {top:?}", ix.top()));
            }
            let probe = u32::try_from(rng.below(u64::from(buckets))).unwrap();
            let below = sets
                .iter()
                .take(probe as usize)
                .rposition(|l| !l.is_empty())
                .map(|t| u32::try_from(t).unwrap());
            if ix.below(probe) != below {
                return Err(format!(
                    "round {round}: below {probe} {:?}, want {below:?}",
                    ix.below(probe)
                ));
            }
            let at_or_above = sets
                .iter()
                .enumerate()
                .skip(probe as usize)
                .find(|(_, l)| !l.is_empty())
                .map(|(t, _)| u32::try_from(t).unwrap());
            if ix.at_or_above(probe) != at_or_above {
                return Err(format!(
                    "round {round}: at or above {probe} {:?}, want {at_or_above:?}",
                    ix.at_or_above(probe)
                ));
            }
            let above = sets
                .iter()
                .enumerate()
                .skip(probe as usize + 1)
                .find(|(_, l)| !l.is_empty())
                .map(|(t, _)| u32::try_from(t).unwrap());
            if ix.above(probe) != above {
                return Err(format!(
                    "round {round}: above {probe} {:?}, want {above:?}",
                    ix.above(probe)
                ));
            }
            for (b, set) in sets.iter().enumerate() {
                let want = turn(
                    set,
                    cursor.get(b).copied().unwrap_or(0) % (n.div_ceil(64) * 64),
                );
                let got: Vec<u32> = ix.bucket(u32::try_from(b).unwrap()).collect();
                if got != want {
                    return Err(format!("round {round}: bucket {b} {got:?}, want {want:?}"));
                }
            }
        }
    }
    Ok(())
}

/// A need of memory the first member lacks walks on to a member that has it, and counts the walk.
#[test]
fn a_member_short_of_memory_is_walked_past() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(3, [8 * CPU, 8, 0, 0]);
    // Node 0 keeps its CPUs but loses its memory to another owner.
    let Some(r0) = rs.get_mut(0) else {
        return Err("no node".into());
    };
    let ToOwner::Started { word, .. } = r0.start(Start {
        owner: OwnerId(1),
        run: Handle::NONE,
        epoch: 1,
        need: [0, 8, 0, 0],
    }) else {
        return Err("no start".into());
    };
    o.hear(
        ToOwner::Hint {
            runner: RunnerId(0),
            word,
        },
        &mut Out::default(),
    );
    let got = ask_start(&mut o, &mut rs, s, [CPU, 4, 0, 0]).ok_or("no start")?;
    if got.0 == RunnerId(0) || o.counts.walked == 0 {
        return Err(format!("{got:?}, walked {}", o.counts.walked));
    }
    Ok(())
}

/// `ahead` counts the queued boxes before one in a node's line.
#[test]
fn ahead_is_a_queued_boxs_place() -> Result<(), String> {
    let mut r = Runner::new(RunnerId(0), [2 * CPU, 0, 0, 0], 1);
    let big = |n: u32| Start {
        owner: OwnerId(0),
        run: Handle {
            slot: n,
            generation: 1,
        },
        epoch: 1,
        need: [2 * CPU, 0, 0, 0],
    };
    let _ = r.start(big(0));
    let mut ats = Vec::new();
    for n in 1..4 {
        match r.wait(big(n)) {
            ToOwner::Queued { at, .. } => ats.push(at),
            other => return Err(format!("{other:?}")),
        }
    }
    let places: Vec<Option<u32>> = ats.iter().map(|at| r.ahead(*at)).collect();
    if places != vec![Some(0), Some(1), Some(2)] {
        return Err(format!("{places:?}"));
    }
    Ok(())
}

/// A waiting box its asker deletes is forgotten, its places withdrawn, and nothing starts when
/// room comes.
#[test]
fn a_deleted_waiting_box_is_forgotten() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(2, [CPU, 0, 0, 0]);
    let mut live = Vec::new();
    for _ in 0..2 {
        live.push(ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("fill")?);
    }
    let mut out = Out::default();
    let a = o
        .ask(s, [CPU, 0, 0, 0], &mut out)
        .map_err(|e| format!("{e:?}"))?;
    if a.send.is_some() {
        return Err("it should wait".into());
    }
    settle(&mut o, &mut rs, &mut out);
    if !o.cancel(a.run, &mut out) {
        return Err("deleting a waiting box should forget it".into());
    }
    settle(&mut o, &mut rs, &mut out);
    for (r, _, at) in live {
        end(&mut o, &mut rs, r, at);
    }
    settle(&mut o, &mut rs, &mut out);
    if o.state(a.run).is_some()
        || rs
            .iter()
            .any(|r| !r.waiting().is_empty() || !r.admitted().is_empty())
        || !out.told.is_empty()
    {
        return Err(format!(
            "after delete: {:?}, told {:?}",
            o.state(a.run),
            out.told
        ));
    }
    Ok(())
}

/// A start in flight when its asker deletes it: told if it starts (the shell deletes it there),
/// forgotten, not placed again, if it's declined.
#[test]
fn a_deleted_start_is_never_placed_again() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(2, [CPU, 0, 0, 0]);
    let mut out = Out::default();
    let a = o
        .ask(s, [CPU, 0, 0, 0], &mut out)
        .map_err(|e| format!("{e:?}"))?;
    let Some((to, m)) = a.send else {
        return Err("no send".into());
    };
    if o.cancel(a.run, &mut out) {
        return Err("a start in flight isn't forgotten at once".into());
    }
    // Another owner takes that node's room first: the start is declined, and not placed again.
    let r = rs.get_mut(n(to)).ok_or("node")?;
    let _ = r.start(Start {
        owner: OwnerId(1),
        run: Handle::NONE,
        epoch: 1,
        need: [CPU, 0, 0, 0],
    });
    let reply = r.hear(m, &mut RunnerOut::new()).ok_or("no reply")?;
    o.hear(reply, &mut out);
    settle(&mut o, &mut rs, &mut out);
    let other = 1 - n(to);
    if o.state(a.run).is_some() || rs.get(other).is_some_and(|r| !r.admitted().is_empty()) {
        return Err(format!(
            "a deleted box was placed again: {:?}",
            o.state(a.run)
        ));
    }
    // One that starts is told, so its shell can delete it there.
    let b = o
        .ask(s, [CPU, 0, 0, 0], &mut out)
        .map_err(|e| format!("{e:?}"))?;
    let Some((to, m)) = b.send else {
        return Err("no send".into());
    };
    o.cancel(b.run, &mut out);
    let reply = rs
        .get_mut(n(to))
        .ok_or("node")?
        .hear(m, &mut RunnerOut::new())
        .ok_or("no reply")?;
    match o.hear(reply, &mut out) {
        Some(Told::Started { .. }) => Ok(()),
        other => Err(format!("a deleted box that started: {other:?}")),
    }
}

/// Room in use outside the ledger (the host's own, what runs beside Hex) is kept off the room for
/// new boxes, and a box waiting for it is held once it's back.
#[test]
fn outside_room_is_kept_off_new_boxes() -> Result<(), String> {
    let mut r = Runner::new(RunnerId(0), [8 * CPU, 0, 0, 0], 1);
    let mut out = RunnerOut::new();
    let st = |n: u32, cpus: u64| Start {
        owner: OwnerId(0),
        run: Handle {
            slot: n,
            generation: 1,
        },
        epoch: 1,
        need: [cpus * CPU, 0, 0, 0],
    };
    r.outside([6 * CPU, 0, 0, 0], &mut out);
    if r.word().cpu != 2 * CPU {
        return Err(format!("room with 6 CPUs outside: {:?}", r.word()));
    }
    if !matches!(r.start(st(1, 3)), ToOwner::Declined { why: Why::Room, .. }) {
        return Err("3 CPUs fit beside 6 outside".into());
    }
    if !matches!(r.wait(st(2, 3)), ToOwner::Queued { .. }) {
        return Err("the wait should queue".into());
    }
    r.outside([0; 4], &mut out);
    if !out.iter().any(|(_, m)| matches!(m, ToOwner::Ready { .. })) {
        return Err(format!(
            "the waiting box wasn't held when the outside use left: {out:?}"
        ));
    }
    Ok(())
}

/// A budget change: what's free follows what's held, boxes past a smaller budget run on, nothing
/// new starts until they fit; the owner's never follows the new budget.
#[test]
fn a_budget_change_follows_what_is_held() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, [8 * CPU, 0, 0, 0]);
    let mut out = RunnerOut::new();
    ask_start(&mut o, &mut rs, s, [6 * CPU, 0, 0, 0]).ok_or("start")?;
    let r = rs.get_mut(0).ok_or("node")?;
    r.set_budget([4 * CPU, 0, 0, 0], &mut out);
    if r.word().cpu != 0 || r.admitted().len() != 1 {
        return Err(format!("6 held of 4: {:?}", r.word()));
    }
    r.set_budget([12 * CPU, 0, 0, 0], &mut out);
    if r.word().cpu != 6 * CPU {
        return Err(format!("6 held of 12: {:?}", r.word()));
    }
    if o.ask(s, [10 * CPU, 0, 0, 0], &mut Out::default()) != Err(Refused::Never) {
        return Err("the owner didn't know of the new budget yet: 10 is never".into());
    }
    o.set_budget(RunnerId(0), [12 * CPU, 0, 0, 0])
        .map_err(|e| format!("{e:?}"))?;
    if o.ask(s, [10 * CPU, 0, 0, 0], &mut Out::default()).is_err() {
        return Err("10 of a budget of 12 could fit once 6 are deleted".into());
    }
    Ok(())
}

/// A link that drops again before its node's Synced arrives: the box said again in between was
/// deleted, its end lost with the second drop. The owner must still settle it when the link comes
/// back, not hold it as started for ever (kiln's model's seed 28).
#[test]
fn a_link_that_drops_again_before_synced_loses_no_end() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, C8);
    let (r, run, at) = ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("no start")?;
    let (mut out, mut rout) = (Out::default(), RunnerOut::new());
    let runner = rs.get_mut(n(r)).ok_or("node")?;
    // The first drop. The link comes back; the owner hears the box said again, not yet Synced.
    o.link_down(r, &mut out);
    runner.link_down(OwnerId(0), &mut rout);
    runner.link_up(OwnerId(0), &mut rout);
    let (_, again) = *rout.first().ok_or("nothing said")?;
    o.hear(again, &mut out);
    rout.clear();
    // The box is deleted, and that word and the Synced go with a second drop.
    let _ = runner.end(at, &mut rout);
    o.link_down(r, &mut out);
    runner.link_down(OwnerId(0), &mut rout);
    rout.clear();
    // The link is back for good: the count says one ended.
    runner.link_up(OwnerId(0), &mut rout);
    for (_, m) in std::mem::take(&mut rout) {
        o.hear(m, &mut out);
    }
    if o.state(run).is_some() || !out.told.contains(&Told::Ended { run }) {
        return Err(format!(
            "after the second drop: {:?}, told {:?}",
            o.state(run),
            out.told
        ));
    }
    Ok(())
}

/// No member has room, so the ask is deleted before what it would have sent goes: nothing of it
/// is left at the owner or any node, and the room it would have waited for goes to the next ask.
#[test]
fn an_ask_deleted_before_its_waits_are_sent_leaves_nothing() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, [CPU, 0, 0, 0]);
    let first = ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("fill")?;
    let mut out = Out::default();
    let a = o
        .ask(s, [CPU, 0, 0, 0], &mut out)
        .map_err(|e| format!("{e:?}"))?;
    if a.send.is_some() || out.to.is_empty() {
        return Err(format!("it should wait: {a:?} {out:?}"));
    }
    if !o.cancel(a.run, &mut out) {
        return Err("a waiting box is forgotten at once".into());
    }
    if out
        .to
        .iter()
        .any(|(_, m)| matches!(m, ToRunner::Withdraw { .. }))
    {
        return Err(format!(
            "nothing to withdraw from places never sent to: {out:?}"
        ));
    }
    out.clear();
    if o.state(a.run).is_some() || rs.iter().any(|r| !r.waiting().is_empty()) {
        return Err(format!("left behind: {:?}", o.state(a.run)));
    }
    end(&mut o, &mut rs, first.0, first.2);
    if ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).is_none() {
        return Err("the room didn't come back".into());
    }
    Ok(())
}
