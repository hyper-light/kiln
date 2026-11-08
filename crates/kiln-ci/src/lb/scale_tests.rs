//! Room tokens and learned wait weights (docs/lb.md "Room tokens", "Learned wait weights"; LB08): each rule pinned down on a case small enough to read,
//! each with the mistake planted against it caught. The model (model.rs) checks tokens' safety
//! under random interleavings; comparisons at scale are not here.

use std::collections::VecDeque;

use super::model::Rng;
use super::*;

/// Owners and runners, every pair linked, messages carried in order.
struct Net {
    os: Vec<Owner>,
    rs: Vec<Runner>,
    sigs: Vec<SigId>,
    down: VecDeque<(u32, RunnerId, ToRunner)>,
    up: VecDeque<(u32, OwnerId, ToOwner)>,
    told: Vec<(u32, Told)>,
}

impl Net {
    /// `owners` owners and a runner of each budget, every runner a member of each owner's one
    /// signature; tokens on or off everywhere. Owner 0 is every runner's first watcher.
    fn new(owners: u32, budgets: &[Amounts], tokens: bool) -> Net {
        let mut rs: Vec<Runner> = budgets.iter().zip(0u32..).map(|(b, i)| Runner::new(RunnerId(i), *b, 1)).collect();
        for r in rs.iter_mut() {
            r.set_tokens(tokens);
        }
        let mut os = Vec::new();
        let mut sigs = Vec::new();
        for o in 0..owners {
            let mut ow = Owner::new(OwnerId(o));
            ow.set_tokens(tokens, u64::from(o) + 1);
            let mut out = Out::default();
            for r in rs.iter_mut() {
                r.watch(OwnerId(o));
                ow.join(r.id(), r.budget(), r.word_for(OwnerId(o)), &mut out);
            }
            let s = ow.sig();
            for r in rs.iter() {
                ow.add(s, r.id(), &mut out);
            }
            os.push(ow);
            sigs.push(s);
        }
        Net { os, rs, sigs, down: VecDeque::new(), up: VecDeque::new(), told: Vec::new() }
    }

    fn send(&mut self, o: u32, out: &mut Out) {
        for (r, m) in out.to.drain(..) {
            self.down.push_back((o, r, m));
        }
        for t in out.told.drain(..) {
            self.told.push((o, t));
        }
    }

    /// Asks owner `o` for `need`.
    fn ask(&mut self, o: u32, need: Amounts) -> Result<Handle, String> {
        let s = *self.sigs.get(o as usize).ok_or("sig")?;
        let mut out = Out::default();
        let a = self.os.get_mut(o as usize).ok_or("owner")?.ask(s, need, &mut out).map_err(|e| format!("{e:?}"))?;
        if let Some((r, m)) = a.send {
            self.down.push_back((o, r, m));
        }
        self.send(o, &mut out);
        Ok(a.run)
    }

    /// Carries everything in flight, then every Hint owed, until nothing moves.
    fn settle(&mut self) -> Result<(), String> {
        loop {
            while let Some((o, r, m)) = self.down.pop_front() {
                let mut rout = RunnerOut::new();
                let reply = self.rs.get_mut(r.0 as usize).ok_or("runner")?.hear(m, &mut rout);
                self.up.extend(reply.map(|m| (r.0, OwnerId(o), m)));
                self.up.extend(rout.into_iter().map(|(w, m)| (r.0, w, m)));
            }
            while let Some((_, w, m)) = self.up.pop_front() {
                let mut out = Out::default();
                let t = self.os.get_mut(w.0 as usize).ok_or("owner")?.hear(m, &mut out);
                self.told.extend(t.map(|t| (w.0, t)));
                self.send(w.0, &mut out);
            }
            let mut rout = RunnerOut::new();
            for r in self.rs.iter_mut() {
                r.drain(&mut rout);
            }
            let quiet = rout.is_empty() && self.down.is_empty();
            for (w, m) in rout {
                self.up.push_back((m.runner().0, w, m));
            }
            if quiet {
                return Ok(());
            }
        }
    }

    /// Ends a run of owner `o`'s at runner `r` (the first there that is its).
    fn end(&mut self, o: u32, r: u32) -> Result<(), String> {
        let runner = self.rs.get_mut(r as usize).ok_or("runner")?;
        let at = runner.admitted().iter().find(|a| a.0 == OwnerId(o)).map(|a| a.2).ok_or("no run of that owner there")?;
        let mut rout = RunnerOut::new();
        let ended = runner.end(at, &mut rout);
        self.up.extend(ended.map(|(w, m)| (r, w, m)));
        self.up.extend(rout.into_iter().map(|(w, m)| (r, w, m)));
        self.settle()
    }

    fn view(&self, o: u32, r: u32) -> u64 {
        self.os.get(o as usize).and_then(|ow| ow.view(RunnerId(r))).map_or(0, |v| v[0])
    }
}

/// Runs `f` and says whether it fails with mutant `m` planted (caught).
fn caught(m: Mutant, f: impl Fn() -> Result<(), String>) -> Result<(), String> {
    f()?;
    match with_mutant(Some(m), &f) {
        Err(_) => Ok(()),
        Ok(()) => Err(format!("{m:?} was not caught")),
    }
}

const ONE_CPU: Amounts = [CPU, 0, 0, 0];

/// With one owner a token is all of a runner's room: every word is the one hints would send.
#[test]
fn lb08_with_one_owner_a_token_is_the_whole_room() -> Result<(), String> {
    let budgets = [[4 * CPU, 8, 0, 0], [2 * CPU, 4, 0, 1], [8 * CPU, 16, 0, 0]];
    let mut a = Net::new(1, &budgets, true);
    let mut b = Net::new(1, &budgets, false);
    let mut rng = Rng(7);
    for step in 0..300 {
        if rng.below(3) == 0 {
            let r = rng.below(3) as u32;
            for n in [&mut a, &mut b] {
                if n.rs.get(r as usize).is_some_and(|x| !x.admitted().is_empty()) {
                    n.end(0, r)?;
                }
            }
        } else {
            let need = [CPU * (1 + rng.below(2)), rng.below(3), 0, 0];
            // Only what starts at once: a run that waits is placed by draws under tokens.
            let s = *a.sigs.first().ok_or("sig")?;
            if a.os.first().and_then(|o| o.peek(s, &need)).is_none() {
                continue;
            }
            a.ask(0, need)?;
            b.ask(0, need)?;
            a.settle()?;
            b.settle()?;
        }
        for r in 0..3u32 {
            let (x, y) = (a.rs.get(r as usize).ok_or("r")?, b.rs.get(r as usize).ok_or("r")?);
            if x.word_for(OwnerId(0)) != y.word() || a.os.first().and_then(|o| o.view(RunnerId(r))) != b.os.first().and_then(|o| o.view(RunnerId(r))) {
                return Err(format!("step {step}: runner {r} differs under tokens: {:?} against {:?}", x.word_for(OwnerId(0)), y.word()));
            }
        }
    }
    Ok(())
}

/// Room a run freed goes back to the owner whose run it was (its idle queue), not to the runner's
/// first watcher; the first watcher's view keeps the rest.
fn freed_goes_back() -> Result<(), String> {
    let mut n = Net::new(2, &[[4 * CPU, 0, 0, 0]], true);
    if n.view(0, 0) != 4 * CPU || n.view(1, 0) != 0 {
        return Err(format!("at the start the first watcher holds it all: {} and {}", n.view(0, 0), n.view(1, 0)));
    }
    // Owner 1 holds nothing: its run waits there, is held at once, and goes.
    n.ask(1, ONE_CPU)?;
    n.settle()?;
    if n.rs.first().map(|r| r.admitted().len()) != Some(1) || n.view(0, 0) != 3 * CPU {
        return Err(format!("owner 1's run should run, owner 0 seeing the 3 CPUs left: {}", n.view(0, 0)));
    }
    n.end(1, 0)?;
    if n.view(1, 0) != CPU || n.view(0, 0) != 3 * CPU {
        return Err(format!("the CPU owner 1's run freed is its token: owner 1 sees {}, owner 0 {}", n.view(1, 0), n.view(0, 0)));
    }
    Ok(())
}

#[test]
fn lb08_freed_room_goes_back_to_the_owner_whose_run_freed_it() -> Result<(), String> {
    caught(Mutant::FreedToFirst, freed_goes_back)
}

/// A token its owner can't use is given back and forwarded to the owner whose run last waited
/// there for room, once: then no word says another owner wants.
fn forwarded_once() -> Result<(), String> {
    let mut n = Net::new(3, &[[2 * CPU, 0, 0, 0]], true);
    // Owner 2 fills the runner; owner 1's run then waits there for room.
    n.ask(2, [2 * CPU, 0, 0, 0])?;
    n.settle()?;
    n.ask(1, ONE_CPU)?;
    n.settle()?;
    if n.rs.first().map(|r| r.waiting().len()) != Some(1) {
        return Err("owner 1's run should wait in the runner's line".into());
    }
    // Owner 2's run ends: the line takes a CPU for owner 1's run, the other is owner 2's token,
    // which it can't use: it goes to owner 1, the one that waited.
    n.end(2, 0)?;
    let r = n.rs.first().ok_or("runner")?;
    if r.admitted().len() != 1 || n.view(1, 0) != CPU || n.view(2, 0) != 0 {
        return Err(format!("owner 1's run runs and it holds the CPU left; owner 2 holds nothing: {} and {}", n.view(1, 0), n.view(2, 0)));
    }
    for o in [0u32, 2] {
        if r.word_for(OwnerId(o)).wants {
            return Err(format!("after the forward, owner {o} still hears that another wants: it could be forwarded again"));
        }
    }
    Ok(())
}

#[test]
fn lb08_a_token_its_owner_cant_use_is_forwarded_once() -> Result<(), String> {
    caught(Mutant::NeverWants, forwarded_once)?;
    caught(Mutant::ForwardAgain, forwarded_once)
}

/// Under tokens an owner's views are what it holds, so its waits are drawn from the signature's
/// members, not put on the same "nearest" two.
fn waits_spread() -> Result<(), String> {
    let budgets = vec![ONE_CPU; 16];
    let mut n = Net::new(2, &budgets, true);
    for _ in 0..16 {
        n.ask(0, ONE_CPU)?;
    }
    n.settle()?;
    for _ in 0..16 {
        n.ask(1, ONE_CPU)?;
    }
    let places: std::collections::BTreeSet<u32> = n.down.iter().filter(|(o, _, m)| *o == 1 && matches!(m, ToRunner::Wait(_))).map(|(_, r, _)| r.0).collect();
    if places.len() < 8 {
        return Err(format!("16 waits went to {} runners of 16: {places:?}", places.len()));
    }
    Ok(())
}

#[test]
fn lb08_waits_are_drawn_from_the_signature() -> Result<(), String> {
    caught(Mutant::NearestPlaces, waits_spread)
}

/// A token cut back when another owner's run takes the room under it is told to its owner.
#[test]
fn lb08_a_shrunk_token_is_told() -> Result<(), String> {
    caught(Mutant::NoHint, || {
        let mut n = Net::new(2, &[[2 * CPU, 0, 0, 0]], true);
        n.ask(1, ONE_CPU)?;
        n.settle()?;
        n.end(1, 0)?;
        // Owner 1 holds a CPU; owner 0 takes both.
        n.ask(0, [2 * CPU, 0, 0, 0])?;
        n.settle()?;
        if n.view(1, 0) != 0 {
            return Err(format!("owner 1's token was taken, but it still sees {}", n.view(1, 0)));
        }
        Ok(())
    })
}

/// An owner whose every runner is full, with learned weights: what its waits learn.
fn full(runners: u32, weights: &[(u32, u64)]) -> (Owner, SigId) {
    let mut o = Owner::new(OwnerId(0));
    o.set_weights(true, 11);
    let mut out = Out::default();
    let none = Word { epoch: 1, whole: false, cpu: 0, rest: [0; 3], speed: 0, slowness: 0, wants: false };
    for r in 0..runners {
        o.join(RunnerId(r), ONE_CPU, none, &mut out);
    }
    let s = o.sig();
    for r in 0..runners {
        o.add(s, RunnerId(r), &mut out);
    }
    for &(r, w) in weights {
        o.set_weight(RunnerId(r), w);
    }
    (o, s)
}

/// Algorithm 1 (Lattanzi et al.): a weight predicted 48 times too high halves while its runner's
/// fractional load passes 2T, and stops at the first halving within it: 48 → 3 against two of 1
/// (x = 3/5 of a wait's need, under 2/3).
fn learns() -> Result<(), String> {
    let (mut o, s) = full(3, &[(0, 48 * CPU)]);
    let mut out = Out::default();
    for _ in 0..40 {
        o.ask(s, ONE_CPU, &mut out).map_err(|e| format!("{e:?}"))?;
    }
    let w = o.weight(RunnerId(0));
    if w != 3 * CPU {
        return Err(format!("runner 0's weight is {w}, Algorithm 1 ends at {}", 3 * CPU));
    }
    Ok(())
}

#[test]
fn lb08_algorithm_1_halves_an_overpredicted_weight_until_within_2t() -> Result<(), String> {
    caught(Mutant::NoLearn, learns)
}

/// Places are drawn in proportion to the weights: of four runners weighted 2, 2, 2 and 3, the
/// heaviest is the first place a third of the time (a quarter if drawn uniformly).
fn proportional() -> Result<(), String> {
    let (mut o, s) = full(4, &[(0, 2 * CPU), (1, 2 * CPU), (2, 2 * CPU), (3, 3 * CPU)]);
    let mut first = 0u32;
    let draws = 4000u32;
    for _ in 0..draws {
        let mut out = Out::default();
        o.ask(s, ONE_CPU, &mut out).map_err(|e| format!("{e:?}"))?;
        if out.to.iter().find(|(_, m)| matches!(m, ToRunner::Wait(_))).is_some_and(|(r, _)| r.0 == 3) {
            first += 1;
        }
    }
    // A third of 4,000 is 1,333 with a standard deviation of 30; a quarter is 1,000.
    if !(1200..=1470).contains(&first) {
        return Err(format!("the runner weighted 3 of 9 was first {first} times of {draws}"));
    }
    Ok(())
}

#[test]
fn lb08_places_are_drawn_in_proportion_to_weights() -> Result<(), String> {
    caught(Mutant::Unweighted, proportional)
}

/// Predictions that aren't helping (one runner of 16 weighted 2^20 times the rest) put more than
/// 2⌈log2 m⌉T on it: the owner draws uniformly again and says which runner and why.
fn falls_back() -> Result<(), String> {
    let (mut o, s) = full(16, &[(0, CPU << 20)]);
    let mut out = Out::default();
    for _ in 0..10 {
        o.ask(s, ONE_CPU, &mut out).map_err(|e| format!("{e:?}"))?;
    }
    match o.fell_back() {
        Some(f) if f.runner == RunnerId(0) && f.load > f.bound => Ok(()),
        other => Err(format!("the owner kept predictions that weren't helping: {other:?}")),
    }
}

#[test]
fn lb08_predictions_that_dont_help_fall_back_and_say_why() -> Result<(), String> {
    caught(Mutant::NoFallback, falls_back)
}
