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
