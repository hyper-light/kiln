//! The kernel's properties, one test each, and each test with the planted bug it catches
//! (docs/lb.md "Correctness"). The model ([`super::model`]) checks them all at once under random
//! interleavings; these pin each one down on a case small enough to read.

use super::model::Rng;
use super::*;

const C8: Amounts = [8 * CPU, 32 << 30, 400 << 30, 8];

/// One owner and `n` runners of `budget`, every one a member of one signature and watched.
pub(super) fn fleet(n: u32, budget: Amounts) -> (Owner, Vec<Runner>, SigId) {
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
pub(super) fn settle(o: &mut Owner, rs: &mut [Runner], out: &mut Out) {
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
pub(super) fn ask_start(o: &mut Owner, rs: &mut [Runner], s: SigId, need: Amounts) -> Option<(RunnerId, Handle, Handle)> {
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

fn end_run(o: &mut Owner, rs: &mut [Runner], r: RunnerId, at: Handle) {
    end(o, rs, r, at);
}

fn end(o: &mut Owner, rs: &mut [Runner], r: RunnerId, at: Handle) {
    let mut out = Out::default();
    if let Some((_, m)) = rs.get_mut(r.0 as usize).and_then(|x| x.end(at, &mut RunnerOut::new())) {
        o.hear(m, &mut out);
    }
}

/// Runs `f` with each mutant planted and says whether `f` failed under it (caught).
pub(super) fn caught(m: Mutant, f: impl Fn() -> Result<(), String>) -> Result<(), String> {
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

/// One kind of work, and another: what the shell numbers (a job, a tree).
const KIND: KindId = KindId(0);
const OTHER: KindId = KindId(1);

/// Asks once for a run of `kind` and carries the start through to its runner and back.
fn ask_start_kind(o: &mut Owner, rs: &mut [Runner], s: SigId, kind: KindId, need: Amounts) -> Option<(RunnerId, Handle, Handle)> {
    let mut out = Out::default();
    let a = o.ask_kind(s, kind, need, &mut out).ok()?;
    let (to, m) = a.send?;
    let reply = rs.get_mut(to.0 as usize)?.hear(m, &mut RunnerOut::new())?;
    o.hear(reply, &mut out);
    match reply {
        ToOwner::Started { at, .. } => Some((to, a.run, at)),
        _ => None,
    }
}

/// A kind's work goes back to where it last went well. Of three runners with the same room: a
/// kind's next run goes back to the runner its shell said it went well on, on to the next in turn
/// when that runner let it down, and never, while another is as free, to the one that last did.
/// Another kind's work, and work of no kind, don't take that place: each goes to its own, or in
/// turn. On the general entries and on the fast ones.
fn good_places() -> Result<(), String> {
    // How each run of the kind ends: Some(true) well, Some(false) let down, None nothing said.
    let ends = [Some(true), None, Some(true), Some(false), Some(true), Some(true), Some(false), Some(false), Some(true), None];
    let want = vec![0u32, 0, 0, 0, 1, 1, 1, 2, 0, 0];
    for fast in [false, true] {
        let (mut o, mut rs, s) = if fast {
            let (o, rs, s, _) = rig(3, 0).map_err(str::to_owned)?;
            (o, rs, s)
        } else {
            fleet(3, C8)
        };
        let run = |o: &mut Owner, rs: &mut [Runner], kind: KindId, end: Option<bool>| -> Result<u32, String> {
            let r = if fast {
                let held = fast_start_kind(o, rs, s, kind, CPU as u32)?.ok_or("no pick")?;
                fast_end(o, rs, held)?;
                held.0
            } else {
                let (r, _, at) = ask_start_kind(o, rs, s, kind, [CPU, 0, 0, 0]).ok_or("no start")?;
                end_run(o, rs, r, at);
                r.0
            };
            match end {
                Some(true) => o.won(RunnerId(r), kind),
                Some(false) => o.failed(RunnerId(r), kind),
                None => {}
            }
            Ok(r)
        };
        let mut seen = Vec::new();
        for end in ends {
            seen.push(run(&mut o, &mut rs, KIND, end)?);
        }
        if seen != want {
            return Err(format!("fast {fast}: the kind's runs went to {seen:?}; going back to where it went well takes {want:?}"));
        }
        // The kind's good place is runner 0. Another kind takes the next in turn and makes that
        // its own; work of no kind takes turns; the first kind still goes back to 0.
        let mut seen = Vec::new();
        for kind in [OTHER, KIND, KindId::NONE, OTHER, KIND, KindId::NONE, KindId::NONE, OTHER, KIND] {
            seen.push(run(&mut o, &mut rs, kind, Some(true))?);
        }
        if seen != [1, 0, 2, 1, 0, 0, 1, 1, 0] {
            return Err(format!("fast {fast}: three kinds' runs went to {seen:?}: each of two kinds to its own place, 1 and 0, and work of no kind in turn"));
        }
    }
    Ok(())
}

#[test]
fn w05_r04_a_kinds_work_goes_back_to_where_it_went_well() -> Result<(), String> {
    caught(Mutant::NoStay, good_places)?;
    caught(Mutant::NoTurn, good_places)
}

/// One run of one kind at a time on an idle fleet of `n` runners (every one as free as the others
/// at each pick), `runs` of them. Runner 0 lets down every run. Of the others' runs, one in `1000
/// / lose` (drawn) ends in a way its shell can't tell from a node's fault and says so
/// ([`Owner::failed`]); the rest it says went well. With `says` false the shell says nothing of
/// any run, so equals only take turns. The share of the runs that went to runner 0.
fn failing_share(n: u32, lose: u64, runs: u32, says: bool, fast: bool) -> Result<f64, String> {
    let mut rng = Rng(u64::from(n) * 1000 + lose);
    let mut to_failing = 0u32;
    let (mut o, mut rs, s) = if fast {
        let (o, rs, s, _) = rig(n, 0).map_err(str::to_owned)?;
        (o, rs, s)
    } else {
        fleet(n, C8)
    };
    for _ in 0..runs {
        let r = if fast {
            let held = fast_start_kind(&mut o, &mut rs, s, KIND, CPU as u32)?.ok_or("no pick")?;
            fast_end(&mut o, &mut rs, held)?;
            held.0
        } else {
            let (r, _, at) = ask_start_kind(&mut o, &mut rs, s, KIND, [CPU, 0, 0, 0]).ok_or("no start")?;
            end_run(&mut o, &mut rs, r, at);
            r.0
        };
        let well = r != 0 && rng.below(1000) >= lose;
        if r == 0 {
            to_failing += 1;
        }
        if says {
            if well {
                o.won(RunnerId(r), KIND);
            } else {
                o.failed(RunnerId(r), KIND);
            }
        }
    }
    Ok(f64::from(to_failing) / f64::from(runs))
}

/// A runner that lets a kind's work down doesn't keep it. One that went well and then goes bad
/// loses the kind at its first let-down and isn't given it again while another is as free. One
/// that never went well gets the kind's work only while the kind is looking for a place: when the
/// shell can tell a node's fault from the work's own (no healthy runner is ever said to have let
/// the work down), once; when it can't, and says of a share q of the others' runs that they were
/// let down, one run a round of the others' good runs: q / (q + n - 1) of the runs (each of the n -
/// 1 others keeps the kind 1 / q runs, the mean of a geometric count; for two runners Zelen's q_B
/// / (q_A + q_B) with q_A = 1, JASA 64(325), 1969). Taking turns gives it 1 / n.
fn a_bad_place_is_left() -> Result<(), String> {
    for fast in [false, true] {
        // It went well once, then lets every run down: two runs, the second its last.
        let (mut o, mut rs, s) = if fast {
            let (o, rs, s, _) = rig(4, 0).map_err(str::to_owned)?;
            (o, rs, s)
        } else {
            fleet(4, C8)
        };
        let mut seen = Vec::new();
        for i in 0..40u32 {
            let r = if fast {
                let held = fast_start_kind(&mut o, &mut rs, s, KIND, CPU as u32)?.ok_or("no pick")?;
                fast_end(&mut o, &mut rs, held)?;
                held.0
            } else {
                let (r, _, at) = ask_start_kind(&mut o, &mut rs, s, KIND, [CPU, 0, 0, 0]).ok_or("no start")?;
                end_run(&mut o, &mut rs, r, at);
                r.0
            };
            seen.push(r);
            if r == 0 && i > 0 {
                o.failed(RunnerId(r), KIND);
            } else {
                o.won(RunnerId(r), KIND);
            }
        }
        if seen.iter().filter(|r| **r == 0).count() != 2 || seen.get(..2) != Some(&[0, 0]) {
            return Err(format!("fast {fast}: a good place that went bad was given the kind's runs {seen:?}: its first two, and no more"));
        }
        for (n, lose) in [(2u32, 200u64), (3, 100), (3, 300), (10, 100), (64, 100)] {
            let q = lose as f64 / 1000.0;
            let expected = q / (q + f64::from(n - 1));
            let runs = 200_000;
            let share = failing_share(n, lose, runs, true, fast)?;
            let turns = failing_share(n, lose, runs, false, fast)?;
            let turn = 1.0 / f64::from(n);
            // Its turn, exactly, when nothing is said: the runs divide evenly.
            if (turns - turn).abs() > 1.0 / f64::from(runs) {
                return Err(format!("{n} runners (fast {fast}): taking turns gave the failing runner {turns}, not its turn {turn}"));
            }
            if (share / expected - 1.0).abs() > 0.05 || share >= turn {
                return Err(format!("{n} runners, {q} of the others' runs said let down (fast {fast}): the failing runner got {share} of the runs; one a round is {expected}, its turn {turn}"));
            }
        }
        // The shell can tell: no healthy runner is said to have let the work down. One run.
        let share = failing_share(10, 0, 10_000, true, fast)?;
        if share > 1.0 / 10_000.0 {
            return Err(format!("the others never letting the work down (fast {fast}), the failing runner got {share} of 10,000 runs: more than one"));
        }
    }
    Ok(())
}

#[test]
fn w05_r04_a_runner_that_lets_a_kind_down_doesnt_keep_it() -> Result<(), String> {
    caught(Mutant::StaysOnALoser, a_bad_place_is_left)
}

/// W05-R04, W05-R23: on measured runners a kind goes back to its good place only while that
/// runner's cores are as fast as the fastest with one free. Runners 0 and 1 have cores twice the
/// reference's and runner 2 the reference's: the kind's narrow runs, let down on 0, go on to 1,
/// stay there while they go well, and go back to 0 once 1 measures slower (heat, a power limit:
/// its word says a lower speed), though the run there went well: the rank decides first.
#[test]
fn w05_r04_a_good_place_holds_only_among_cores_as_fast() -> Result<(), String> {
    let budget = [8 * CPU, 0, 0, 0];
    let mut o = Owner::new(OwnerId(0));
    let mut out = Out::default();
    let s = o.sig();
    let mut rs = Vec::new();
    for (i, speed) in [(0u32, 2 * SPEED_ONE), (1, 2 * SPEED_ONE), (2, SPEED_ONE)] {
        let mut r = Runner::new(RunnerId(i), budget, 1);
        r.watch(OwnerId(0));
        r.measure(speed, SPEED_ONE);
        o.join(RunnerId(i), budget, r.word(), &mut out);
        o.add(s, RunnerId(i), &mut out);
        rs.push(r);
    }
    let mut seen = Vec::new();
    // How each run ends: let down, well, well, well as its runner slows; the fifth is only asked.
    for (well, slows) in [(false, false), (true, false), (true, false), (true, true), (true, false)] {
        let Some((r, _, at)) = ask_start_kind(&mut o, &mut rs, s, KIND, [CPU, 0, 0, 0]) else { return Err("no start".into()) };
        seen.push(r.0);
        if slows {
            rs.get_mut(r.0 as usize).ok_or("runner")?.measure(3 * SPEED_ONE / 2, SPEED_ONE);
        }
        end_run(&mut o, &mut rs, r, at);
        if well {
            o.won(r, KIND);
        } else {
            o.failed(r, KIND);
        }
    }
    if seen != [0, 1, 1, 1, 0] {
        return Err(format!("the kind's narrow runs went to {seen:?}: 0, then 1 while they go well there, then 0 again once 1 slows"));
    }
    Ok(())
}

/// `inflight` runs of one kind on `n` idle runners of eight cores, then `steps` times one ends (on
/// a runner drawn by how full it is, so twin fleets end alike) and another is asked, each run said
/// to have gone well or nothing said (`says`). A digest of how many runners held each number of
/// runs after each step, and how many runners were given a run after the first `inflight`.
fn churn(n: u32, inflight: u32, steps: u32, says: bool) -> Result<(u64, u32), String> {
    let (mut o, mut rs, s, _) = rig(n, 0).map_err(str::to_owned)?;
    let mut held: Vec<Vec<(u32, Handle, Handle)>> = vec![Vec::new(); n as usize];
    for _ in 0..inflight {
        let x = fast_start_kind(&mut o, &mut rs, s, KIND, CPU as u32)?.ok_or("fill: no pick")?;
        held.get_mut(x.0 as usize).ok_or("runner")?.push(x);
    }
    let mut rng = Rng(u64::from(n) << 32 | u64::from(inflight));
    let (mut digest, mut given) = (0xcbf2_9ce4_8422_2325u64, vec![false; n as usize]);
    for _ in 0..steps {
        // The k-th fullest of the runners that hold a run: the same fullness in a twin fleet,
        // whichever runner that is there.
        let mut busy: Vec<(usize, u32)> = held.iter().zip(0u32..).filter(|(h, _)| !h.is_empty()).map(|(h, r)| (h.len(), r)).collect();
        busy.sort_unstable_by_key(|x| (std::cmp::Reverse(x.0), x.1));
        let &(_, r) = busy.get(rng.below(busy.len() as u64) as usize).ok_or("no run in flight")?;
        let x = held.get_mut(r as usize).and_then(Vec::pop).ok_or("run")?;
        fast_end(&mut o, &mut rs, x)?;
        if says {
            o.won(RunnerId(r), KIND);
        }
        let x = fast_start_kind(&mut o, &mut rs, s, KIND, CPU as u32)?.ok_or("no pick")?;
        held.get_mut(x.0 as usize).ok_or("runner")?.push(x);
        if let Some(g) = given.get_mut(x.0 as usize) {
            *g = true;
        }
        let mut full = [0u32; 9];
        for h in &held {
            if let Some(c) = full.get_mut(h.len()) {
                *c += 1;
            }
        }
        for c in full {
            digest = (digest ^ u64::from(c)).wrapping_mul(0x0100_0000_01b3);
        }
    }
    Ok((digest, given.iter().filter(|g| **g).count() as u32))
}

/// W05-R04, W05-R23: a good place moves no load. With every run said to have gone well and with
/// nothing said, the fleet is as full step by step (as many runners hold each number of runs):
/// the pick goes to one of the most free either way, and only which of equals differs. What
/// differs is where: with fewer runs in flight than runners, work that goes back to where it went
/// well stays on as many runners as there are runs, where taking turns takes it round every
/// runner. `cargo test -p kiln-ci lb::tests::w05_r04_a_good_place_moves_no_load -- --nocapture`
/// prints the table in docs/lb.md.
#[test]
fn w05_r04_a_good_place_moves_no_load() -> Result<(), String> {
    let n = 64u32;
    println!("runs in flight | runners given runs, taking turns | going back to where it went well");
    for inflight in [1u32, 4, 16, 32, 64, 128, 256, 460] {
        let steps = 20 * n;
        let (turns, round) = churn(n, inflight, steps, false)?;
        let (stays, kept) = churn(n, inflight, steps, true)?;
        println!("{inflight:>14} | {round:>32} | {kept:>32}");
        if turns != stays {
            return Err(format!("{inflight} runs in flight on {n} runners: the fleet isn't as full step by step with good places as without"));
        }
        if round != n {
            return Err(format!("{inflight} runs in flight: taking turns gave runs to {round} of {n} runners in {steps} steps"));
        }
        if inflight <= n && kept != inflight {
            return Err(format!("{inflight} runs in flight: good places kept the work on {kept} runners, not {inflight}"));
        }
    }
    Ok(())
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
fn rig(n: u32, each: u32) -> Result<(Owner, Vec<Runner>, SigId, Vec<(u32, Handle, Handle)>), &'static str> {
    let budget = [8 * CPU, 0, 0, 0];
    let mut rs: Vec<Runner> = (0..n).map(|i| Runner::new(RunnerId(i), budget, 1)).collect();
    let mut o = Owner::new(OwnerId(0));
    let mut out = Out::default();
    for r in rs.iter_mut() {
        r.watch(OwnerId(0));
        o.join(r.id(), budget, r.word(), &mut out);
    }
    let s = o.sig();
    for i in 0..n {
        o.add(s, RunnerId(i), &mut out);
    }
    let mut held = Vec::new();
    for _ in 0..n * each {
        let (r, run, epoch) = o.ask_fast(s, 1000).ok_or("fill: no pick")?;
        let (at, b) = rs.get_mut(r as usize).ok_or("runner")?.start_fast(OwnerId(0), run, epoch, 1000).ok_or("fill: no start")?;
        if !o.started_fast(r, run, at, b) {
            return Err("fill: the word didn't take the fast path");
        }
        held.push((r, at, run));
    }
    Ok((o, rs, s, held))
}

/// A batch of ends put off and a batch of picks go to the same buckets as the same ends and picks
/// one at a time; only which of equals in a bucket gets a run may differ.
#[test]
fn w05_r26_batched_rises_choose_as_one_at_a_time_up_to_equals() -> Result<(), &'static str> {
    let need = [CPU, 0, 0, 0];
    let (mut one, mut rs1, s1, held1) = rig(6, 4)?;
    let (mut many, mut rs2, s2, held2) = rig(6, 4)?;
    // Ends on runners 0, 2, 2 and 5: a runner can be hit twice in a batch.
    let which = |h: &[(u32, Handle, Handle)]| -> Vec<(u32, Handle, Handle)> {
        let of = |r: u32, nth: usize| h.iter().filter(|x| x.0 == r).nth(nth).copied();
        [(0, 0), (2, 0), (2, 1), (5, 0)].iter().filter_map(|&(r, n)| of(r, n)).collect()
    };
    let (ends1, ends2) = (which(&held1), which(&held2));
    if ends1.len() != 4 {
        return Err("the fill didn't spread as expected");
    }
    for (r, at, run) in ends1 {
        let (_, b) = rs1.get_mut(r as usize).ok_or("runner")?.end_fast(at).ok_or("end")?;
        if !one.ended_fast(r, run, b) {
            return Err("end not taken");
        }
    }
    for (r, at, run) in ends2 {
        let (_, b) = rs2.get_mut(r as usize).ok_or("runner")?.end_fast(at).ok_or("end")?;
        if !many.ended_batch(r, run, b) {
            return Err("batched end not taken");
        }
    }
    // Four picks one at a time; four in a batch.
    for _ in 0..4 {
        let (r, run, epoch) = one.ask_fast(s1, 1000).ok_or("one: no pick")?;
        let (at, b) = rs1.get_mut(r as usize).ok_or("runner")?.start_fast(OwnerId(0), run, epoch, 1000).ok_or("one: no start")?;
        if !one.started_fast(r, run, at, b) {
            return Err("one: the word didn't take the fast path");
        }
    }
    let (mut starts, mut waits, mut out) = (Vec::new(), Vec::new(), Out::default());
    many.ask_many(s2, KindId::NONE, need, 4, &mut starts, &mut waits, &mut out).map_err(|_| "ask_many")?;
    if starts.len() != 4 || !waits.is_empty() {
        return Err("the batch didn't start four");
    }
    for (to, st) in starts {
        let (at, b) = rs2.get_mut(to.0 as usize).ok_or("runner")?.start_fast(OwnerId(0), st.run, st.epoch, 1000).ok_or("no start")?;
        if !many.started_fast(to.0, st.run, at, b) {
            return Err("the word didn't take the fast path");
        }
    }
    let sorted = |o: &Owner| -> Vec<u64> {
        let mut v: Vec<u64> = (0..6).filter_map(|r| o.view(RunnerId(r)).map(|a| a[0])).collect();
        v.sort_unstable();
        v
    };
    assert_eq!(sorted(&one), sorted(&many));
    assert_eq!(one.order(s1).len(), many.order(s2).len());
    Ok(())
}

/// W05-R04: of a batch's ends, only the runners on which the batch's kind of work just went well
/// are gone back to among their equals. Runners 3 and 1 did the kind well, 2 another kind, 4 let
/// it down and nothing is said of 0, all five as free as the rest of the fleet: the batch's runs
/// go to 3 and 1 (in the order their ends came), then on in the bucket's turn, where the other
/// three are with every other member.
#[test]
fn w05_r04_a_batch_goes_back_to_where_its_kind_went_well() -> Result<(), String> {
    let (mut owner, mut runners, sig, held) = rig(8, 1)?;
    for (r, said) in [(4u32, Some((false, KIND))), (3, Some((true, KIND))), (0, None), (1, Some((true, KIND))), (2, Some((true, OTHER)))] {
        let &(_, at, run) = held.iter().find(|x| x.0 == r).ok_or("held")?;
        let (_, word) = runners.get_mut(r as usize).ok_or("runner")?.end_fast(at).ok_or("end")?;
        if !owner.ended_batch(r, run, word) { return Err("batch end declined".into()); }
        match said {
            Some((true, kind)) => owner.won(RunnerId(r), kind),
            Some((false, kind)) => owner.failed(RunnerId(r), kind),
            None => {}
        }
    }
    let (mut starts, mut waits, mut out) = (Vec::new(), Vec::new(), Out::default());
    owner.ask_many(sig, KIND, [CPU, 0, 0, 0], 5, &mut starts, &mut waits, &mut out).map_err(|e| format!("batch: {e:?}"))?;
    let got: Vec<u32> = starts.iter().map(|x| x.0.0).collect();
    // The fill took each of the eight once, so the full bucket's turn is at runner 0 again.
    if got != [3, 1, 0, 2, 4] || !waits.is_empty() {
        return Err(format!("the batch's runs went to {got:?}: 3 and 1 first, where its kind went well, then 0, 2 and 4 in the bucket's turn"));
    }
    // Nothing said of any: the bucket's turn alone, as if each end had been made at once.
    let (mut owner, mut runners, sig, held) = rig(8, 1)?;
    for r in [4u32, 3, 0, 1, 2] {
        let &(_, at, run) = held.iter().find(|x| x.0 == r).ok_or("held")?;
        let (_, word) = runners.get_mut(r as usize).ok_or("runner")?.end_fast(at).ok_or("end")?;
        if !owner.ended_batch(r, run, word) { return Err("batch end declined".into()); }
    }
    let (mut starts, mut waits, mut out) = (Vec::new(), Vec::new(), Out::default());
    owner.ask_many(sig, KIND, [CPU, 0, 0, 0], 5, &mut starts, &mut waits, &mut out).map_err(|e| format!("batch: {e:?}"))?;
    let got: Vec<u32> = starts.iter().map(|x| x.0.0).collect();
    if got != [0, 1, 2, 3, 4] || !waits.is_empty() {
        return Err(format!("with nothing said of any run the batch's runs went to {got:?}, not on in the bucket's turn"));
    }
    Ok(())
}

/// W05-R26, W05-R04: sorting a batch's pending rises must retain arrival order within each
/// room class, including when a smaller batch leaves some rises for the normal index to apply.
#[test]
fn w05_r26_batch_overlay_preserves_equal_turns_and_remaining_rises() -> Result<(), String> {
    let (mut owner, mut runners, sig, held) = rig(64, 1)?;
    // Give half the runners one extra run, making two distinct room classes.
    for _ in 0..held.len() / 2 {
        ask_start(&mut owner, &mut runners, sig, [CPU, 0, 0, 0]).ok_or("extra fill")?;
    }
    // Odds descending, then evens ascending: arrival order differs from runner-id order and
    // mixes both room classes. Each runner appears once.
    let mut risen = Vec::new();
    for &(r, at, run) in held.iter().rev().step_by(2).chain(held.iter().step_by(2)) {
        let (_, word) = runners.get_mut(r as usize).ok_or("runner")?.end_fast(at).ok_or("end")?;
        if !owner.ended_batch(r, run, word) { return Err("batch end declined".into()); }
        // The kind's work went well on every one: its batch goes back to them (W05-R04).
        owner.won(RunnerId(r), KIND);
        risen.push((r, word.cpu));
    }
    let most = risen.iter().map(|x| x.1).max().ok_or("no rises")?;
    let expected: Vec<u32> = risen.iter().filter(|x| x.1 == most).map(|x| x.0).collect();
    let (mut starts, mut waits, mut out) = (Vec::new(), Vec::new(), Out::default());
    owner.ask_many(sig, KIND, [CPU, 0, 0, 0], expected.len(), &mut starts, &mut waits, &mut out).map_err(|e| format!("batch: {e:?}"))?;
    let got: Vec<u32> = starts.iter().map(|x| x.0.0).collect();
    if got != expected || !waits.is_empty() { return Err(format!("turns: {got:?}, expected {expected:?}")); }
    for (to, start) in starts {
        let (at, word) = runners.get_mut(to.0 as usize).ok_or("runner")?.start_fast(start.owner, start.run, start.epoch, CPU as u32).ok_or("start")?;
        if !owner.started_fast(to.0, start.run, at, word) { return Err("start word declined".into()); }
    }
    // The unconsumed lower-class rises were applied, so every runner's exact view still agrees
    // with its ledger after the pending buffer was compacted and reused.
    for runner in &runners {
        if owner.view(runner.id()).map(|v| v[0]) != Some(runner.word().cpu) {
            return Err(format!("view differs on {:?}", runner.id()));
        }
    }
    Ok(())
}

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
            many.ask_many(s, KindId::NONE, need, k, &mut starts, &mut waits, &mut Out::default()).map_err(|e| format!("{e:?}"))?;
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

/// W05-R23: GPU order can outweigh CPU order, but never bypass fit or the ledger.
#[test]
fn w05_r23_ranked_gpu_pick_deducts_once_and_refuses_nonfits() -> Result<(), String> {
    let (mut o, mut rs, sig) = fleet(3, C8);
    let mut out = Out::default();
    let ask = o.ask_ranked(sig, [CPU, 0, 0, 8], |r, _| Some(u64::from(r.0)), &mut out).map_err(|e| format!("{e:?}"))?;
    let (r, start) = ask.send.ok_or("first start")?;
    assert_eq!(r, RunnerId(2));
    assert_eq!(o.view(r).ok_or("first view")?[3], 0);
    let reply = rs.get_mut(r.0 as usize).ok_or("runner")?.hear(start, &mut RunnerOut::new()).ok_or("reply")?;
    o.hear(reply, &mut out);
    let second = o.ask_ranked(sig, [CPU, 0, 0, 8], |r, _| Some(u64::from(r.0)), &mut out).map_err(|e| format!("{e:?}"))?;
    assert_eq!(second.send.ok_or("second start")?.0, RunnerId(1));
    assert_eq!(o.view(RunnerId(2)).ok_or("second view")?[3], 0);
    Ok(())
}

/// W05-R23: equal GPU pressure must retain the existing narrow CPU finish ordering.
#[test]
fn w05_r23_gpu_ties_keep_measured_per_core_cpu_fit() -> Result<(), String> {
    let mut owner = Owner::new(OwnerId(0));
    let mut out = Out::default();
    let sig = owner.sig();
    for (id, cpu, speed) in [(0, 32 * CPU, SPEED_ONE), (1, 4 * CPU, 2 * SPEED_ONE)] {
        let budget = [cpu, 0, 0, 1];
        let mut runner = Runner::new(RunnerId(id), budget, 1);
        runner.measure(speed, SPEED_ONE);
        owner.join(RunnerId(id), budget, runner.word(), &mut out);
        owner.add(sig, RunnerId(id), &mut out);
    }
    let ask = owner.ask_ranked(sig, [CPU, 0, 0, 1], |_, _| Some(1), &mut out).map_err(|e| format!("{e:?}"))?;
    assert_eq!(ask.send.ok_or("a start")?.0, RunnerId(1), "one core finishes sooner on the faster core, not on the node with more idle cores");
    Ok(())
}

/// W05-R23: an old admitted handle cannot become visible while its reused owner slot
/// is starting, including a delayed old Started word before the actual new admission.
#[test]
fn w05_r23_reused_start_publishes_only_its_new_runner_handle() -> Result<(), String> {
    let (mut owner, mut runners, sig) = crate::lb::tests::fleet(1, [8 * CPU, 0, 0, 0]);
    let runner = runners.first_mut().ok_or("runner")?;
    let (r, old, epoch) = owner.ask_fast(sig, CPU as u32).ok_or("first ask")?;
    let (old_at, b) = runner.start_fast(owner.id(), old, epoch, CPU as u32).ok_or("first start")?;
    if !owner.started_fast(r, old, old_at, b) { return Err("first Started declined".into()); }
    let (ended, b) = runner.end_fast(old_at).ok_or("first end")?;
    if ended != old || !owner.ended_fast(r, old, b) { return Err("first Ended declined".into()); }
    let (r, new, epoch) = owner.ask_fast(sig, CPU as u32).ok_or("reused ask")?;
    if new.slot != old.slot || new.generation == old.generation { return Err("slot was not reused with a new generation".into()); }
    let starting = Some(State::Starting { runner: RunnerId(r) });
    if owner.state(new) != starting || owner.started_fast(r, old, old_at, b) || owner.state(new) != starting {
        return Err("old admission changed the reused start".into());
    }
    let (new_at, b) = runner.start_fast(owner.id(), new, epoch, CPU as u32).ok_or("new start")?;
    if new_at == old_at || !owner.started_fast(r, new, new_at, b) || owner.state(new) != Some(State::Started { runner: RunnerId(r), at: new_at }) {
        return Err("new admission did not replace the runner handle".into());
    }
    Ok(())
}

/// A run of `cpu` asked on the fast entry and started where it's sent: its runner, its handle
/// there and its own (`None`: the fast entry had no pick). A runner with more runs than its inline
/// records hold takes the start, and the owner its word, on the general entries.
fn fast_start(o: &mut Owner, rs: &mut [Runner], s: SigId, cpu: u32) -> Result<Option<(u32, Handle, Handle)>, String> {
    fast_start_kind(o, rs, s, KindId::NONE, cpu)
}

/// [`fast_start`] for a run of `kind`.
fn fast_start_kind(o: &mut Owner, rs: &mut [Runner], s: SigId, kind: KindId, cpu: u32) -> Result<Option<(u32, Handle, Handle)>, String> {
    let Some((r, run, epoch)) = o.ask_fast_of(s, kind, cpu) else { return Ok(None) };
    let runner = rs.get_mut(r as usize).ok_or("runner")?;
    if let Some((at, b)) = runner.start_fast(OwnerId(0), run, epoch, cpu) {
        if !o.started_fast(r, run, at, b) {
            return Err("a Started word left the fast path".into());
        }
        return Ok(Some((r, at, run)));
    }
    let reply = runner.start(Start { owner: OwnerId(0), run, epoch, need: [u64::from(cpu), 0, 0, 0] });
    let ToOwner::Started { at, .. } = reply else { return Err(format!("the runner declined a start its owner's view had room for: {reply:?}")) };
    o.hear(reply, &mut Out::default());
    Ok(Some((r, at, run)))
}

/// A run ended, on the fast entries where they take it and the general ones where they don't.
fn fast_end(o: &mut Owner, rs: &mut [Runner], (r, at, run): (u32, Handle, Handle)) -> Result<(), String> {
    let runner = rs.get_mut(r as usize).ok_or("runner")?;
    if let Some((_, b)) = runner.end_fast(at) {
        if !o.ended_fast(r, run, b) {
            return Err("an Ended word left the fast path".into());
        }
        return Ok(());
    }
    let (_, word) = runner.end(at, &mut RunnerOut::new()).ok_or("the runner knows no such run")?;
    o.hear(word, &mut Out::default());
    Ok(())
}

/// A run ended and its shell says with the end that its kind's work went well there: on the fast
/// entry that takes the two in one where it takes them, the general end and the word after it
/// where it doesn't.
fn fast_end_won(o: &mut Owner, rs: &mut [Runner], (r, at, run): (u32, Handle, Handle), kind: KindId) -> Result<(), String> {
    let runner = rs.get_mut(r as usize).ok_or("runner")?;
    if let Some((_, b)) = runner.end_fast(at) {
        if !o.ended_won(r, run, b, kind) {
            return Err("an Ended word said with its win left the fast path".into());
        }
        return Ok(());
    }
    let (_, word) = runner.end(at, &mut RunnerOut::new()).ok_or("the runner knows no such run")?;
    o.hear(word, &mut Out::default());
    o.won(RunnerId(r), kind);
    Ok(())
}

/// W05-R04, W05-R23, W05-R26: a pick whose freed runner only ties the most free (it swaps with
/// the next in turn, or goes back to that runner when it is the next or the good place of the kind
/// asked) chooses what it would with every move made as it comes, and leaves the same picks to
/// come and the same views. Twin owners over twin fleets, one each way, through the same ends,
/// the same words of their shell about them (went well, let down, nothing) and the same picks, of
/// three kinds and of none, of one and two cores and half a core, on fleets within one membership
/// word and across words.
#[test]
fn w05_r26_a_tied_pick_chooses_and_leaves_what_its_two_moves_do() -> Result<(), String> {
    use super::owner::{turns, with_two_moves};
    let (swaps0, backs0, stays0) = turns();
    let mut rng = Rng(11);
    for case in 0..64u32 {
        let n = [2u32, 3, 5, 63, 64, 65, 130, 257].get((case % 8) as usize).copied().unwrap_or(2);
        let each = (case / 8) % 4;
        let (mut a, mut ra, sa, mut ha) = rig(n, each).map_err(str::to_owned)?;
        let (mut b, mut rb, sb, mut hb) = rig(n, each).map_err(str::to_owned)?;
        // Each run's kind (the fill's have none), in step with `ha` and `hb`.
        let mut kinds = vec![KindId::NONE; ha.len()];
        for step in 0..500u32 {
            // Mostly an end and then a pick; now and then two ends, or a pick with none before it.
            let ends = match rng.below(8) {
                0 => 0,
                1 => 2,
                _ => 1,
            };
            for _ in 0..ends {
                if ha.is_empty() {
                    break;
                }
                let i = rng.below(ha.len() as u64) as usize;
                let (x, y, kind) = (ha.swap_remove(i), hb.swap_remove(i), kinds.swap_remove(i));
                if x != y {
                    return Err(format!("case {case} step {step}: the twins hold {x:?} and {y:?}"));
                }
                // Of half of the runs their shell says they went well (of half of those with the
                // end, in one), of a quarter that their runner let them down, of the rest nothing:
                // one twin keeps what that says of a runner whose move is put off with the move,
                // the other makes every move first.
                let said = rng.below(8);
                if said < 2 {
                    fast_end_won(&mut a, &mut ra, x, kind)?;
                } else {
                    fast_end(&mut a, &mut ra, x)?;
                }
                fast_end(&mut b, &mut rb, y)?;
                match said {
                    0 | 1 => with_two_moves(|| b.won(RunnerId(y.0), kind)),
                    2 | 3 => {
                        a.won(RunnerId(x.0), kind);
                        with_two_moves(|| b.won(RunnerId(y.0), kind));
                    }
                    4 | 5 => {
                        a.failed(RunnerId(x.0), kind);
                        with_two_moves(|| b.failed(RunnerId(y.0), kind));
                    }
                    _ => {}
                }
                if a.order(sa) != b.order(sb) {
                    return Err(format!("case {case} step {step} ({n} runners): after an end on {}, the picks to come differ:\n{:?}\n{:?}", x.0, a.order(sa), b.order(sb)));
                }
            }
            // One core mostly; two cores or half a core now and then, so a pick doesn't always take
            // what the last end gave back, and a view moves within its bucket.
            let cpu = match rng.below(6) {
                0 => 2 * CPU as u32,
                1 => CPU as u32 / 2,
                _ => CPU as u32,
            };
            // Of one of three kinds, or of none.
            let kind = match rng.below(4) {
                3 => KindId::NONE,
                k => KindId(k as u32),
            };
            let pa = fast_start_kind(&mut a, &mut ra, sa, kind, cpu)?;
            let pb = with_two_moves(|| fast_start_kind(&mut b, &mut rb, sb, kind, cpu))?;
            if pa != pb {
                return Err(format!("case {case} step {step} ({n} runners): a run of {kind:?} picked {pa:?}, with every move made first {pb:?}"));
            }
            kinds.extend(pa.map(|_| kind));
            ha.extend(pa);
            hb.extend(pb);
            if a.order(sa) != b.order(sb) {
                return Err(format!("case {case} step {step} ({n} runners): the picks to come differ:\n{:?}\n{:?}", a.order(sa), b.order(sb)));
            }
            if (0..n).any(|r| a.view(RunnerId(r)) != b.view(RunnerId(r))) {
                return Err(format!("case {case} step {step}: views differ"));
            }
            // Every runner is in the bucket its view says: down the order of picks to come, whole
            // free cores never rise (a move wrongly skipped leaves one out of place).
            let free: Vec<u64> = a.order(sa).iter().filter_map(|r| a.view(*r)).map(|v| v[0] / CPU).collect();
            if free.iter().zip(free.iter().skip(1)).any(|(x, y)| x < y) {
                return Err(format!("case {case} step {step} ({n} runners): a runner is out of its bucket: free cores in pick order {free:?}"));
            }
        }
        // Every run ended: each view is its runner's whole room again, in both.
        for (x, y) in ha.drain(..).zip(hb.drain(..)) {
            fast_end(&mut a, &mut ra, x)?;
            fast_end(&mut b, &mut rb, y)?;
        }
        a.settle();
        b.settle();
        if a.order(sa) != b.order(sb) || ra.iter().any(|r| a.view(r.id()).map(|v| v[0]) != Some(r.word().cpu)) {
            return Err(format!("case {case}: the twins part once every run has ended"));
        }
    }
    let (swaps, backs, stays) = turns();
    if swaps <= swaps0 || backs <= backs0 || stays <= stays0 {
        return Err(format!("the trace made {} swaps, {} picks back to the runner put off as the turn came to it and {} as its kind's good place: each is to be covered", swaps - swaps0, backs - backs0, stays - stays0));
    }
    Ok(())
}

/// W05-R04: on an idle fleet with one run in flight, each pick takes the next runner in turn,
/// and does it by swapping it with the runner the last run just left.
#[test]
fn w05_r04_an_idle_fleets_picks_take_turns_by_swapping() -> Result<(), String> {
    use super::owner::turns;
    for n in [2u32, 5, 64, 65, 200] {
        let (mut o, mut rs, s, _) = rig(n, 0).map_err(str::to_owned)?;
        let mut held = fast_start(&mut o, &mut rs, s, CPU as u32)?.ok_or("the first pick")?;
        let (swaps0, _, _) = turns();
        for step in 0..3 * n {
            fast_end(&mut o, &mut rs, held)?;
            let next = fast_start(&mut o, &mut rs, s, CPU as u32)?.ok_or("a pick")?;
            if next.0 != (held.0 + 1) % n {
                return Err(format!("{n} runners, step {step}: after runner {} the pick went to {}", held.0, next.0));
            }
            held = next;
        }
        if turns().0 - swaps0 != u64::from(3 * n) {
            return Err(format!("{n} runners: {} of {} picks swapped", turns().0 - swaps0, 3 * n));
        }
    }
    Ok(())
}
