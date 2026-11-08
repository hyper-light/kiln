//! The kernel's properties, one test each, and each test with the planted bug it catches
//! (docs/lb.md "Correctness"). The model ([`super::model`]) checks them all at once under random
//! interleavings; these pin each one down on a case small enough to read.

use super::model::Rng;
use super::*;

const C8: Amounts = [8 * CPU, 32 << 30, 400 << 30, 8];

/// One owner and `n` runners of `budget`, every one a member of one signature and watched.
fn fleet(n: u32, budget: Amounts) -> (Owner, Vec<Runner>, SigId) {
    let mut o = Owner::new(OwnerId(0));
    let mut out = Out::default();
    let mut rs = Vec::new();
    for i in 0..n {
        let mut r = Runner::new(RunnerId(i), budget, 1);
        r.watch(OwnerId(0));
        o.join(RunnerId(i), budget, r.word(), &mut out);
        rs.push(r);
    }
    let s = o.sig();
    for i in 0..n {
        o.add(s, RunnerId(i), &mut out);
    }
    (o, rs, s)
}

/// Delivers everything in flight between one owner and its runners, in order, until nothing is.
fn settle(o: &mut Owner, rs: &mut [Runner], out: &mut Out) {
    let mut rout = RunnerOut::new();
    loop {
        let mut moved = false;
        for (to, m) in std::mem::take(&mut out.to) {
            moved = true;
            if let Some(r) = rs.get_mut(to.0 as usize)
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

/// Asks once and carries the start through to its runner and back: where it started.
fn ask_start(o: &mut Owner, rs: &mut [Runner], s: SigId, need: Amounts) -> Option<(RunnerId, Handle, Handle)> {
    let mut out = Out::default();
    let a = o.ask(s, need, &mut out).ok()?;
    let (to, m) = a.send?;
    let reply = rs.get_mut(to.0 as usize)?.hear(m, &mut RunnerOut::new())?;
    o.hear(reply, &mut out);
    match reply {
        ToOwner::Started { at, .. } => Some((to, a.run, at)),
        _ => None,
    }
}

fn end(o: &mut Owner, rs: &mut [Runner], r: RunnerId, at: Handle) {
    let mut out = Out::default();
    if let Some((_, m)) = rs.get_mut(r.0 as usize).and_then(|x| x.end(at, &mut RunnerOut::new())) {
        o.hear(m, &mut out);
    }
}

/// Runs `f` with each mutant planted and says whether `f` failed under it (caught).
fn caught(m: Mutant, f: impl Fn() -> Result<(), String>) -> Result<(), String> {
    f()?;
    match with_mutant(Some(m), &f) {
        Err(_) => Ok(()),
        Ok(()) => Err(format!("{m:?} was not caught")),
    }
}

/// W05-R23 (least loaded first, W05-R04 in O(1)): the pick is the member with the most CPU free,
/// whatever order they joined in.
fn least_loaded() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(3, C8);
    // Load runner 0 with 5 CPUs and runner 2 with 2: runner 1 is the least loaded, then 2, then 0.
    for (r, cpus) in [(0u32, 5u64), (2, 2)] {
        for _ in 0..cpus {
            let mut out = Out::default();
            let Some(rr) = rs.get_mut(r as usize) else { return Err("no runner".into()) };
            let mut s0 = rr.start(Start { owner: OwnerId(0), run: Handle::NONE, epoch: 1, need: [CPU, 0, 0, 0] });
            if let ToOwner::Started { word, .. } = &mut s0 {
                o.hear(ToOwner::Hint { runner: RunnerId(r), word: *word }, &mut out);
            }
        }
    }
    let picks: Vec<u32> = (0..3).filter_map(|_| ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).map(|p| p.0.0)).collect();
    // 1 has 8 free, 2 has 6, 0 has 3: 1 (to 7), 1 (to 6, even with 2), then one of the two with
    // 6, never 0.
    if !matches!(picks.as_slice(), [1, 1, 1 | 2]) {
        return Err(format!("picks {picks:?}"));
    }
    Ok(())
}

#[test]
fn w05_r23_the_pick_is_the_least_loaded_member() -> Result<(), String> {
    caught(Mutant::MostLoaded, least_loaded)
}

/// Equals take turns: members with the same room are picked first in, first out.
fn turns() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(3, C8);
    let mut seen = Vec::new();
    for _ in 0..6 {
        let Some((r, _, at)) = ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]) else { return Err("no start".into()) };
        seen.push(r.0);
        // It ends at once: every member is back to the same room.
        end(&mut o, &mut rs, r, at);
    }
    if seen != vec![0, 1, 2, 0, 1, 2] {
        return Err(format!("turns {seen:?}"));
    }
    Ok(())
}

#[test]
fn w05_r04_equals_take_turns() -> Result<(), String> {
    caught(Mutant::NoTurn, turns)
}

/// W05-R03: no runner is ever admitted past its budget, in any dimension: memory short here.
fn within_budget() -> Result<(), String> {
    let mut r = Runner::new(RunnerId(0), [8 * CPU, 8, 0, 0], 1);
    let a = r.start(Start { owner: OwnerId(0), run: Handle::NONE, epoch: 1, need: [CPU, 6, 0, 0] });
    let b = r.start(Start { owner: OwnerId(0), run: Handle::NONE, epoch: 1, need: [CPU, 6, 0, 0] });
    if !matches!(a, ToOwner::Started { .. }) || !matches!(b, ToOwner::Declined { why: Why::Room, .. }) {
        return Err(format!("{a:?} {b:?}"));
    }
    let taken: u64 = r.admitted().iter().map(|a| a.3[1]).sum();
    if taken > 8 {
        return Err(format!("{taken} bytes of memory taken of 8"));
    }
    Ok(())
}

#[test]
fn w05_r03_no_runner_takes_past_its_budget() -> Result<(), String> {
    caught(Mutant::AdmitPastRoom, within_budget)
}

/// W05-R03's backfilling (EASY's guarantee to the first in line): a run that waits for the whole
/// runner keeps its room; smaller starts pass it only in room it can't use, and it starts as soon
/// as what ran there ends.
fn keeps_room() -> Result<(), String> {
    let mut r = Runner::new(RunnerId(0), [8 * CPU, 0, 0, 0], 1);
    let mut out = RunnerOut::new();
    let small = |n: u32| Start { owner: OwnerId(0), run: Handle { slot: n, generation: 1 }, epoch: 1, need: [CPU, 0, 0, 0] };
    let ToOwner::Started { at: a, .. } = r.start(small(1)) else { return Err("first start".into()) };
    let big = Start { owner: OwnerId(1), run: Handle { slot: 9, generation: 1 }, epoch: 1, need: [8 * CPU, 0, 0, 0] };
    if !matches!(r.wait(big), ToOwner::Queued { .. }) {
        return Err("the big run should queue".into());
    }
    // Seven CPUs are free, but every one of them is the big run's: no small start passes.
    if let ToOwner::Started { .. } = r.start(small(2)) {
        return Err("a small run took room the first in line needs".into());
    }
    r.end(a, &mut out);
    if !out.iter().any(|(o, m)| *o == OwnerId(1) && matches!(m, ToOwner::Ready { .. })) {
        return Err(format!("the big run wasn't held when the runner emptied: {out:?}"));
    }
    Ok(())
}

#[test]
fn w05_r03_the_first_in_line_keeps_its_room() -> Result<(), String> {
    caught(Mutant::NoKeep, keeps_room)
}

/// The owner deducts what it sends at once: many starts in flight before any answer, and none is
/// declined for room (with one owner its view is never more than the runner's room). One runner
/// has 8 CPUs free and three have 2 (another owner holds the rest): without the deduction every
/// start would go to the one with 8.
fn no_herding() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(4, C8);
    let mut out = Out::default();
    for r in 1..4u32 {
        let Some(rr) = rs.get_mut(r as usize) else { return Err("no runner".into()) };
        let ToOwner::Started { word, .. } = rr.start(Start { owner: OwnerId(1), run: Handle::NONE, epoch: 1, need: [6 * CPU, 0, 0, 0] }) else { return Err("no start".into()) };
        o.hear(ToOwner::Hint { runner: RunnerId(r), word }, &mut out);
    }
    let mut sends = Vec::new();
    for _ in 0..14 {
        let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
        sends.extend(a.send);
    }
    for (to, m) in sends {
        let reply = rs.get_mut(to.0 as usize).and_then(|r| r.hear(m, &mut RunnerOut::new()));
        if let Some(ToOwner::Declined { why, .. }) = reply {
            return Err(format!("declined for {why:?} with 14 starts in flight to 14 CPUs"));
        }
    }
    Ok(())
}

#[test]
fn starts_in_flight_never_herd() -> Result<(), String> {
    caught(Mutant::NoDeduct, no_herding)
}

/// The fence: a start sent under the epoch before the runner started again is declined, and the
/// owner, hearing the new epoch, takes what ran there for lost.
fn fenced() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, C8);
    let Some((_, first, _)) = ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]) else { return Err("no start".into()) };
    let mut out = Out::default();
    let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
    let Some((_, m)) = a.send else { return Err("no send".into()) };
    let Some(r) = rs.get_mut(0) else { return Err("no runner".into()) };
    r.restart();
    let reply = r.hear(m, &mut RunnerOut::new()).ok_or("no reply")?;
    if !matches!(reply, ToOwner::Declined { why: Why::Epoch, .. }) {
        return Err(format!("a start from before the restart: {reply:?}"));
    }
    o.hear(reply, &mut out);
    if !out.told.contains(&Told::Lost { run: first }) {
        return Err(format!("the run from before wasn't lost: {:?}", out.told));
    }
    Ok(())
}

#[test]
fn an_old_epochs_start_is_declined() -> Result<(), String> {
    caught(Mutant::NoFence, fenced)
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

/// A run no member has room for waits at two runners; the first to hold its room starts it and
/// the other is withdrawn: it starts once.
fn waits_once() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(3, [2 * CPU, 0, 0, 0]);
    let mut started = Vec::new();
    for _ in 0..6 {
        started.push(ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("fill")?);
    }
    let mut out = Out::default();
    let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
    if a.send.is_some() || out.to.len() != owner::PLACES {
        return Err(format!("it should wait at {} runners: {out:?}", owner::PLACES));
    }
    // Both places queue it; then a run ends at each at once, before the owner hears either.
    let mut rout = RunnerOut::new();
    let waits = std::mem::take(&mut out.to);
    let mut replies = Vec::new();
    for (to, m) in &waits {
        replies.extend(rs.get_mut(to.0 as usize).and_then(|r| r.hear(*m, &mut rout)));
    }
    for (to, _) in &waits {
        let Some(&(_, _, at)) = started.iter().find(|x| x.0 == *to) else { return Err("no run there".into()) };
        if let Some(m) = rs.get_mut(to.0 as usize).and_then(|r| r.end(at, &mut rout)) {
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
    let holding: usize = rs.iter().map(|r| r.admitted().iter().filter(|x| x.1 == a.run).count()).sum();
    if holding != 1 {
        return Err(format!("the waiting run runs on {holding} runners"));
    }
    if rs.iter().any(|r| !r.waiting().is_empty()) {
        return Err("a place is still held".into());
    }
    Ok(())
}

#[test]
fn a_run_that_waits_starts_once() -> Result<(), String> {
    caught(Mutant::GoTwice, waits_once)
}

/// A start declined for room (another owner took it) is placed again where there is room, and
/// its asker hears it started: no ask is lost.
fn declined_placed_again() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(2, [CPU, 0, 0, 0]);
    let mut out = Out::default();
    let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
    let Some((to, m)) = a.send else { return Err("no send".into()) };
    // Another owner fills that runner first.
    let Some(r) = rs.get_mut(to.0 as usize) else { return Err("no runner".into()) };
    let _ = r.start(Start { owner: OwnerId(1), run: Handle::NONE, epoch: 1, need: [CPU, 0, 0, 0] });
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

/// `ask_many` makes the same choices as asking one at a time, in the same order.
#[test]
fn ask_many_chooses_as_one_at_a_time() -> Result<(), String> {
    for (n, load, k) in [(5u32, 7u32, 3usize), (7, 19, 11), (3, 0, 30), (16, 40, 64), (9, 3, 2)] {
        let (mut o, mut rs, s) = fleet(n, C8);
        let mut rng = Rng(u64::from(n) * 31 + u64::from(load));
        let mut live = Vec::new();
        for _ in 0..load {
            live.extend(ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]));
        }
        // Some end, so the buckets differ.
        for _ in 0..load / 2 {
            if live.is_empty() {
                break;
            }
            let i = rng.below(live.len() as u64) as usize;
            let (r, _, at) = live.swap_remove(i);
            end(&mut o, &mut rs, r, at);
        }
        for need in [[CPU, 0, 0, 0], [2 * CPU, 0, 0, 0]] {
            let mut one = o.clone();
            let mut many = o.clone();
            let mut want = Vec::new();
            for _ in 0..k {
                if let Ok(a) = one.ask(s, need, &mut Out::default()) {
                    want.push(a.send.map(|(r, _)| r));
                }
            }
            let (mut starts, mut waits) = (Vec::new(), Vec::new());
            many.ask_many(s, need, k, &mut starts, &mut waits, &mut Out::default()).map_err(|e| format!("{e:?}"))?;
            let got: Vec<Option<RunnerId>> = starts.iter().map(|(r, _)| Some(*r)).chain(waits.iter().map(|_| None)).collect();
            if got != want {
                return Err(format!("n {n} load {load} k {k} need {need:?}: one at a time {want:?}, at once {got:?}"));
            }
            if one.order(s) != many.order(s) {
                return Err(format!("n {n} load {load} k {k}: buckets differ after: {:?} vs {:?}", one.order(s), many.order(s)));
            }
            for r in 0..n {
                if one.view(RunnerId(r)) != many.view(RunnerId(r)) {
                    return Err(format!("view of {r} differs"));
                }
            }
        }
    }
    Ok(())
}

/// The index against a plain model: a set of members per bucket and a cursor per bucket. Random
/// pushes, unlinks, moves and picks; its top, below, next in turn and each bucket's members in
/// turn order (id order from the cursor, wrapping) agree.
#[test]
fn the_index_keeps_buckets_and_turns() -> Result<(), String> {
    let mut rng = Rng(7);
    for round in 0..40u64 {
        let buckets = [3u32, 64, 65, 200, BUCKETS].get((round % 5) as usize).copied().unwrap_or(3);
        let n = 1 + rng.below(if round % 2 == 0 { 90 } else { 9000 }) as u32;
        let start = rng.next() as u32;
        let mut ix = Index::new(start);
        ix.grow(buckets, n);
        let first = ((u64::from(start) * u64::from(n.div_ceil(64) * 64)) >> 32) as u32;
        let mut at: Vec<Option<u32>> = vec![None; n as usize];
        let mut sets: Vec<std::collections::BTreeSet<u32>> = vec![std::collections::BTreeSet::new(); buckets as usize];
        let mut cursor: Vec<u32> = vec![first; buckets as usize];
        // In turn: the members at or after the cursor, then those before it.
        let turn = |set: &std::collections::BTreeSet<u32>, c: u32| -> Vec<u32> { set.range(c..).chain(set.range(..c)).copied().collect() };
        for _ in 0..400 {
            let r = rng.below(u64::from(n)) as u32;
            let b = rng.below(u64::from(buckets)) as u32;
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
                    // A pick from the top bucket: the next in turn, and the cursor past it.
                    let Some(top) = ix.top() else { return Err("no top with members".into()) };
                    let set = sets.get(top as usize).ok_or("b")?;
                    let want = turn(set, *cursor.get(top as usize).ok_or("c")?).first().copied().ok_or("empty top")?;
                    let got = ix.next_in(top);
                    if got != want {
                        return Err(format!("round {round}: next in turn {got}, want {want}"));
                    }
                    ix.took(top, got);
                    *cursor.get_mut(top as usize).ok_or("c")? = got + 1;
                }
                Some(from) => {
                    ix.relink(r, from, b);
                    sets.get_mut(from as usize).ok_or("b")?.remove(&r);
                    sets.get_mut(b as usize).ok_or("b")?.insert(r);
                    *at.get_mut(r as usize).ok_or("r")? = Some(b);
                }
            }
            let top = sets.iter().rposition(|l| !l.is_empty()).map(|t| t as u32);
            if ix.top() != top {
                return Err(format!("round {round}: top {:?}, want {top:?}", ix.top()));
            }
            let probe = rng.below(u64::from(buckets)) as u32;
            let below = sets.iter().take(probe as usize).rposition(|l| !l.is_empty()).map(|t| t as u32);
            if ix.below(probe) != below {
                return Err(format!("round {round}: below {probe} {:?}, want {below:?}", ix.below(probe)));
            }
            for (b, set) in sets.iter().enumerate() {
                let want = turn(set, cursor.get(b).copied().unwrap_or(0) % (n.div_ceil(64) * 64));
                let got: Vec<u32> = ix.bucket(b as u32).collect();
                if got != want {
                    return Err(format!("round {round}: bucket {b} {got:?}, want {want:?}"));
                }
            }
        }
    }
    Ok(())
}

/// A need of memory the head lacks walks on to a member that has it, and counts the walk.
#[test]
fn a_head_short_of_memory_is_walked_past() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(3, [8 * CPU, 8, 0, 0]);
    // Runner 0 keeps its CPUs but loses its memory to another owner.
    let Some(r0) = rs.get_mut(0) else { return Err("no runner".into()) };
    let ToOwner::Started { word, .. } = r0.start(Start { owner: OwnerId(1), run: Handle::NONE, epoch: 1, need: [0, 8, 0, 0] }) else { return Err("no start".into()) };
    o.hear(ToOwner::Hint { runner: RunnerId(0), word }, &mut Out::default());
    let got = ask_start(&mut o, &mut rs, s, [CPU, 4, 0, 0]).ok_or("no start")?;
    if got.0 == RunnerId(0) || o.counts.walked == 0 {
        return Err(format!("{got:?}, walked {}", o.counts.walked));
    }
    Ok(())
}

/// `peek` names the runner the next ask would go to, and decides nothing: the ask after goes there.
#[test]
fn peek_is_the_pick_without_the_decision() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(4, C8);
    for _ in 0..5 {
        let before = o.order(s);
        let want = o.peek(s, &[CPU, 0, 0, 0]);
        if o.order(s) != before {
            return Err("peek moved a member".into());
        }
        let got = ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).map(|x| x.0);
        if got != want {
            return Err(format!("peek said {want:?}, the ask went to {got:?}"));
        }
    }
    if o.peek(s, &[9 * CPU, 0, 0, 0]).is_some() {
        return Err("peek found room for more than any budget".into());
    }
    Ok(())
}

/// `ahead` counts the queued runs before one in a runner's line.
#[test]
fn ahead_is_a_queued_runs_place() -> Result<(), String> {
    let mut r = Runner::new(RunnerId(0), [2 * CPU, 0, 0, 0], 1);
    let big = |n: u32| Start { owner: OwnerId(0), run: Handle { slot: n, generation: 1 }, epoch: 1, need: [2 * CPU, 0, 0, 0] };
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

/// lb/wire's ask 1: a waiting run its asker gives up on is forgotten, its places withdrawn, and
/// nothing starts when room comes.
#[test]
fn a_cancelled_waiting_run_is_forgotten() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(2, [CPU, 0, 0, 0]);
    let mut live = Vec::new();
    for _ in 0..2 {
        live.push(ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("fill")?);
    }
    let mut out = Out::default();
    let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
    if a.send.is_some() {
        return Err("it should wait".into());
    }
    settle(&mut o, &mut rs, &mut out);
    if !o.cancel(a.run, &mut out) {
        return Err("cancelling a waiting run should forget it".into());
    }
    settle(&mut o, &mut rs, &mut out);
    for (r, _, at) in live {
        end(&mut o, &mut rs, r, at);
    }
    settle(&mut o, &mut rs, &mut out);
    if o.state(a.run).is_some() || rs.iter().any(|r| !r.waiting().is_empty() || !r.admitted().is_empty()) || !out.told.is_empty() {
        return Err(format!("after cancel: {:?}, lines {:?}, told {:?}", o.state(a.run), rs.iter().map(|r| r.waiting()).collect::<Vec<_>>(), out.told));
    }
    Ok(())
}

/// A start in flight when its asker gives up: told if it starts (the shell ends it), forgotten,
/// not placed again, if it's declined.
#[test]
fn a_cancelled_start_is_never_placed_again() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(2, [CPU, 0, 0, 0]);
    let mut out = Out::default();
    let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
    let Some((to, m)) = a.send else { return Err("no send".into()) };
    if o.cancel(a.run, &mut out) {
        return Err("a start in flight isn't forgotten at once".into());
    }
    // Another owner takes that runner's room first: the start is declined, and not placed again.
    let r = rs.get_mut(to.0 as usize).ok_or("runner")?;
    let _ = r.start(Start { owner: OwnerId(1), run: Handle::NONE, epoch: 1, need: [CPU, 0, 0, 0] });
    let reply = r.hear(m, &mut RunnerOut::new()).ok_or("no reply")?;
    o.hear(reply, &mut out);
    settle(&mut o, &mut rs, &mut out);
    let other = 1 - to.0 as usize;
    if o.state(a.run).is_some() || rs.get(other).is_some_and(|r| !r.admitted().is_empty()) {
        return Err(format!("a cancelled run was placed again: {:?}", o.state(a.run)));
    }
    // One that starts is told, so its shell can end it there.
    let b = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
    let Some((to, m)) = b.send else { return Err("no send".into()) };
    o.cancel(b.run, &mut out);
    let reply = rs.get_mut(to.0 as usize).ok_or("runner")?.hear(m, &mut RunnerOut::new()).ok_or("no reply")?;
    match o.hear(reply, &mut out) {
        Some(Told::Started { .. }) => Ok(()),
        other => Err(format!("a cancelled run that started: {other:?}")),
    }
}

/// lb/wire's ask 2: room in use outside the ledger is kept off the room for new runs, and a run
/// waiting for it is held once it's back.
#[test]
fn outside_room_is_kept_off_new_runs() -> Result<(), String> {
    let mut r = Runner::new(RunnerId(0), [8 * CPU, 0, 0, 0], 1);
    let mut out = RunnerOut::new();
    let st = |n: u32, cpus: u64| Start { owner: OwnerId(0), run: Handle { slot: n, generation: 1 }, epoch: 1, need: [cpus * CPU, 0, 0, 0] };
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
        return Err(format!("the waiting run wasn't held when the outside use left: {out:?}"));
    }
    Ok(())
}

/// lb/wire's ask 2: a budget change: what's free follows what's held, runs past a smaller
/// budget run on, nothing new starts until they fit; the owner's never follows the new budget.
#[test]
fn a_budget_change_follows_what_is_held() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, [8 * CPU, 0, 0, 0]);
    let mut out = RunnerOut::new();
    let first = ask_start(&mut o, &mut rs, s, [6 * CPU, 0, 0, 0]).ok_or("start")?;
    let r = rs.get_mut(0).ok_or("runner")?;
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
    o.set_budget(RunnerId(0), [12 * CPU, 0, 0, 0]);
    if o.ask(s, [10 * CPU, 0, 0, 0], &mut Out::default()).is_err() {
        return Err("10 of a budget of 12 could fit once 6 end".into());
    }
    let _ = first;
    Ok(())
}

/// A link that drops again before its runner's Synced arrives: the run said again in between
/// ended, its end lost with the second drop. The owner must still settle it when the link comes
/// back, not hold it as started for ever (the model's seed 28).
#[test]
fn a_link_that_drops_again_before_synced_loses_no_end() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, C8);
    let (r, run, at) = ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("no start")?;
    let (mut out, mut rout) = (Out::default(), RunnerOut::new());
    let runner = rs.get_mut(r.0 as usize).ok_or("runner")?;
    // The first drop. The link comes back; the owner hears the run said again, not yet Synced.
    o.link_down(r, &mut out);
    runner.link_down(OwnerId(0), &mut rout);
    runner.link_up(OwnerId(0), &mut rout);
    let (_, again) = *rout.first().ok_or("nothing said")?;
    o.hear(again, &mut out);
    rout.clear();
    // The run ends, and that word and the Synced go with a second drop.
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
        return Err(format!("after the second drop: {:?}, told {:?}", o.state(run), out.told));
    }
    Ok(())
}

/// The hand-off to the contended path: no member has room, so the ask is taken back and what it
/// would have sent is dropped unsent. Nothing of it is left at the owner or any runner, and the
/// room it would have waited for goes to the next ask.
#[test]
fn an_ask_taken_back_before_its_waits_are_sent_leaves_nothing() -> Result<(), String> {
    let (mut o, mut rs, s) = fleet(1, [CPU, 0, 0, 0]);
    let first = ask_start(&mut o, &mut rs, s, [CPU, 0, 0, 0]).ok_or("fill")?;
    let mut out = Out::default();
    let a = o.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
    if a.send.is_some() || out.to.is_empty() {
        return Err(format!("it should wait: {a:?} {out:?}"));
    }
    if !o.cancel(a.run, &mut out) {
        return Err("a waiting run is forgotten at once".into());
    }
    if out.to.iter().any(|(_, m)| matches!(m, ToRunner::Withdraw { .. })) {
        return Err(format!("nothing to withdraw from places never sent to: {out:?}"));
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

/// Several owners on one fleet: each round some runs end and every owner hears of it, then each
/// owner decides one run from that same view before hearing of the others' decisions, and all
/// the starts are delivered in a random order (a decline placed again goes round too). Starts
/// declined per decision after a fill that leaves `f` runs in flight, and how many runners the top
/// bucket held on average as the owners decided. `wide`: each owner spreads its picks over the
/// expected herd (the other owners' decisions in one word's delay, K - 1 here), as measured rates
/// tell it ([`Owner::set_spread`]).
fn herd(k: u32, n: u32, f: u32, rounds: u32, wide: bool) -> Result<(f64, f64, f64), String> {
    use std::collections::VecDeque;
    type Rs = Vec<Runner>;
    let budget = [8 * CPU, 0, 0, 0];
    let mut rng = Rng(u64::from(k) * 1009 + u64::from(f));
    let mut rs: Rs = (0..n).map(|i| Runner::new(RunnerId(i), budget, 1)).collect();
    let mut os: Vec<Owner> = Vec::new();
    let mut sigs: Vec<SigId> = Vec::new();
    for o in 0..k {
        let mut ow = Owner::new(OwnerId(o));
        let mut out = Out::default();
        for r in rs.iter_mut() {
            r.watch(OwnerId(o));
            ow.join(r.id(), budget, r.word(), &mut out);
        }
        let s = ow.sig();
        for i in 0..n {
            ow.add(s, RunnerId(i), &mut out);
        }
        if wide {
            // The other owners' decisions per second and a millisecond's delay: K - 1 in one delay.
            ow.set_spread(1000 * u64::from(k - 1), 1_000_000, u64::from(o + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        }
        os.push(ow);
        sigs.push(s);
    }
    // Delivers what's queued in a random order until nothing is, then every Hint owed.
    fn carry(os: &mut [Owner], rs: &mut [Runner], q: &mut VecDeque<(u32, RunnerId, ToRunner)>, rng: &mut Rng) -> Result<(), String> {
        loop {
            while !q.is_empty() {
                let i = rng.below(q.len() as u64) as usize;
                let (o, to, m) = q.remove(i).ok_or("queue")?;
                let mut rout = RunnerOut::new();
                let reply = rs.get_mut(to.0 as usize).ok_or("runner")?.hear(m, &mut rout);
                let mut said: Vec<(OwnerId, ToOwner)> = reply.into_iter().map(|m| (OwnerId(o), m)).collect();
                said.append(&mut rout);
                for (w, m) in said {
                    let mut out = Out::default();
                    os.get_mut(w.0 as usize).ok_or("owner")?.hear(m, &mut out);
                    q.extend(out.to.into_iter().map(|(r, m)| (w.0, r, m)));
                }
            }
            let mut rout = RunnerOut::new();
            for r in rs.iter_mut() {
                r.drain(&mut rout);
            }
            if rout.is_empty() {
                return Ok(());
            }
            for (w, m) in rout {
                let mut out = Out::default();
                os.get_mut(w.0 as usize).ok_or("owner")?.hear(m, &mut out);
                q.extend(out.to.into_iter().map(|(r, m)| (w.0, r, m)));
            }
        }
    }
    let mut q = VecDeque::new();
    // The fill: owners take turns, each ask carried through, so views stay exact.
    for i in 0..f {
        let o = i % k;
        let mut out = Out::default();
        let (ow, s) = (os.get_mut(o as usize).ok_or("owner")?, *sigs.get(o as usize).ok_or("sig")?);
        let a = ow.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
        q.extend(a.send.map(|(r, m)| (o, r, m)));
        q.extend(out.to.into_iter().map(|(r, m)| (o, r, m)));
        carry(&mut os, &mut rs, &mut q, &mut rng)?;
    }
    let before: u64 = os.iter().map(|o| o.counts.declined).sum();
    let mut top_members = 0u64;
    let mut cv_sum = 0f64;
    for _ in 0..rounds {
        // Some runs end, as many as will be asked for, and every owner hears.
        for _ in 0..k {
            let busy: Vec<usize> = rs.iter().enumerate().filter(|(_, r)| !r.admitted().is_empty()).map(|(i, _)| i).collect();
            let &r = busy.get(rng.below(busy.len() as u64) as usize).ok_or("no run to end")?;
            let runner = rs.get_mut(r).ok_or("runner")?;
            let runs = runner.admitted();
            let (_, _, at, _) = *runs.get(rng.below(runs.len() as u64) as usize).ok_or("run")?;
            let mut rout = RunnerOut::new();
            let ended = runner.end(at, &mut rout);
            for (w, m) in rout.into_iter().chain(ended) {
                let mut out = Out::default();
                os.get_mut(w.0 as usize).ok_or("owner")?.hear(m, &mut out);
                q.extend(out.to.into_iter().map(|(r, m)| (w.0, r, m)));
            }
        }
        carry(&mut os, &mut rs, &mut q, &mut rng)?;
        // How many runners have the most whole CPUs free, in the view every owner now shares.
        let views: Vec<u64> = (0..n).filter_map(|i| os.first()?.view(RunnerId(i)).map(|v| v[0] / CPU)).collect();
        let most = views.iter().copied().max().unwrap_or(0);
        top_members += views.iter().filter(|v| **v == most).count() as u64;
        // Every owner decides from that view, none hearing of the others'.
        for o in 0..k {
            let mut out = Out::default();
            let (ow, s) = (os.get_mut(o as usize).ok_or("owner")?, *sigs.get(o as usize).ok_or("sig")?);
            let a = ow.ask(s, [CPU, 0, 0, 0], &mut out).map_err(|e| format!("{e:?}"))?;
            q.extend(a.send.map(|(r, m)| (o, r, m)));
            q.extend(out.to.into_iter().map(|(r, m)| (o, r, m)));
        }
        carry(&mut os, &mut rs, &mut q, &mut rng)?;
        // The coefficient of variation of in-flight counts across the fleet, after the round.
        let counts: Vec<f64> = rs.iter().map(|r| r.admitted().len() as f64).collect();
        let mean = counts.iter().sum::<f64>() / f64::from(n);
        let var = counts.iter().map(|c| (c - mean) * (c - mean)).sum::<f64>() / f64::from(n);
        cv_sum += if mean > 0.0 { var.sqrt() / mean } else { 0.0 };
    }
    let declined: u64 = os.iter().map(|o| o.counts.declined).sum::<u64>() - before;
    Ok((declined as f64 / f64::from(k * rounds), top_members as f64 / f64::from(rounds), cv_sum / f64::from(rounds)))
}

/// Several owners: declines per decision with K owners, cursors spread over the fleet or all
/// started in one place; the same with picks widened over the expected herd; and what that costs
/// in balance, the CV of in-flight counts (`cargo test lb::tests::herding -- --nocapture` prints
/// the table; docs/lb.md "Several owners" has it). One owner never herds: it deducts what it
/// sends.
#[test]
fn herding_declines_with_and_without_the_spread() -> Result<(), String> {
    let (n, rounds) = (64u32, 400u32);
    println!("{:>4} {:>5} {:>10} {:>10} {:>10} {:>10} {:>10} {:>12}", "K", "rho", "unspread", "exact", "widened", "cv exact", "cv widened", "top members");
    for (rho_name, f) in [("0.50", n * 8 / 2), ("0.90", n * 8 * 9 / 10)] {
        for k in [1u32, 2, 4, 8, 16, 32] {
            let (unspread, _, _) = with_mutant(Some(Mutant::NoSpread), || herd(k, n, f, rounds, false))?;
            let (exact, top, cv_exact) = herd(k, n, f, rounds, false)?;
            let (widened, _, cv_wide) = herd(k, n, f, rounds, true)?;
            println!("{k:>4} {rho_name:>5} {unspread:>10.3} {exact:>10.3} {widened:>10.3} {cv_exact:>10.3} {cv_wide:>10.3} {top:>12.1}");
            if k == 1 && (exact != 0.0 || unspread != 0.0 || widened != 0.0) {
                return Err(format!("one owner was declined: {exact:.3} a decision, widened {widened:.3}"));
            }
            if k >= 8 && rho_name == "0.90" && widened > exact {
                return Err(format!("widening made K = {k} herd more: {widened:.3} against {exact:.3} declines a decision"));
            }
        }
    }
    Ok(())
}
