//! The integer auction in the place core (docs/lb.md "The auction" and "Adopted"; LB08, W05-R23,
//! W05-R26): asks that come in one step go where the auction puts them once it's declared, at
//! costs that read each runner's key multiplier as the kernel keeps it (its measured pace, said by
//! its own kernel's word), what it can't start now is probed as before, and where its costs can't
//! tell runs apart the kernel's own pick spreads them. Each rule with the mistake planted against
//! it caught.

use super::answer::Answer;
use super::msg::{Down, FastWord};
use super::owner::{Cmd, Owner};
use super::runner::Settings;
use super::weights_tests::{Lab, OWNER, ask, caught, machine};
use super::Mutant;

/// An owner of runners that take kernel starts, each (CPUs, speed, slowness), each joined by its
/// word; the auction declared or not.
fn fleet(runners: &[(u64, u32, u32)], auction: bool) -> Owner {
    let mut o = Owner::seeded(OWNER, 1, 7);
    o.configure(&Settings { auction, ..Settings::default() });
    let _ = o.machines(runners.iter().enumerate().map(|(i, (c, ..))| machine(&format!("r{i}"), *c, true)).collect());
    for (i, (cpus, speed, slowness)) in runners.iter().enumerate() {
        let rid = o.runner_id(&format!("r{i}"));
        let cpu = cpus * 1000;
        let w = FastWord { epoch: 1, room: [cpu, 0, 0, 0], whole: true, budget: [cpu, 64 << 30, 100 << 30, 0], speed: *speed, slowness: *slowness, measured: u32::from(*speed != 0 || *slowness != 0), ..FastWord::default() };
        o.fast_hint_c(rid, &w);
    }
    o
}

/// Each kernel start asked, in order: its run and runner, by the owner's `Go`.
fn went(o: &Owner, cmds: &[Cmd]) -> Vec<(String, String)> {
    cmds.iter().filter_map(|c| if let Cmd::Go { run, to, .. } = c { Some((run.to_string(), o.runner_name(*to).to_owned())) } else { None }).collect()
}

fn pairs(want: &[(&str, &str)]) -> Vec<(String, String)> {
    want.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
}

/// Off until declared: asks in one step are exactly those asks one at a time.
#[test]
fn lb08_the_auction_is_off_until_declared() -> Result<(), String> {
    let runners = [(2, 2048, 0), (2, 1024, 0), (4, 0, 0)];
    let asks = |tag: &str| -> Result<Vec<_>, String> { (0..6).map(|i| ask(&format!("{tag}{i}"), "cpus=1", Some(1000 * (1 + i % 3)))).collect() };
    let mut o1 = fleet(&runners, false);
    let mut one = Vec::new();
    for a in asks("a")? {
        o1.ask_c(a, 1, &mut one);
    }
    let mut o2 = fleet(&runners, false);
    let mut many = Vec::new();
    o2.ask_many_c(asks("a")?, 1, &mut many);
    if one != many {
        return Err(format!("one at a time: {one:?}; in one step: {many:?}"));
    }
    Ok(())
}

/// Asks of one kind that come together go where the auction puts them, at the least total expected
/// finish: of two runners with two slots each, one twice as fast, the two long runs take the fast
/// one (50 + 50 + 10 + 10 = 120 seconds, each within the 1/64 of its rate's class); one at a time,
/// the first two asked (a short and a long run) would (5 + 50 + 10 + 100 = 165).
fn joint() -> Result<(), String> {
    let mut o = fleet(&[(2, 2048, 0), (2, 1024, 0)], true);
    let asks = vec![ask("s1", "cpus=1", Some(10_000))?, ask("l1", "cpus=1", Some(100_000))?, ask("s2", "cpus=1", Some(10_000))?, ask("l2", "cpus=1", Some(100_000))?];
    let mut cmds = Vec::new();
    o.ask_many_c(asks, 1, &mut cmds);
    let got = went(&o, &cmds);
    let want = pairs(&[("s1", "r1"), ("l1", "r0"), ("s2", "r1"), ("l2", "r0")]);
    if got != want {
        return Err(format!("went to {got:?}, not {want:?}"));
    }
    Ok(())
}

#[test]
fn lb08_w05_r26_asks_in_one_step_go_where_the_auction_puts_them() -> Result<(), String> {
    caught(Mutant::AuctionOneByOne, joint)
}

/// A runner's cost reads its key multiplier, its speed over its slowness (docs/lb.md "Rank"): a
/// runner twice as fast but four times slowed by contention is half the speed of the other, and
/// the long run goes to the other.
fn reads_contention() -> Result<(), String> {
    let mut o = fleet(&[(1, 2048, 4096), (1, 1024, 0)], true);
    let asks = vec![ask("long", "cpus=1", Some(100_000))?, ask("short", "cpus=1", Some(10_000))?];
    let mut cmds = Vec::new();
    o.ask_many_c(asks, 1, &mut cmds);
    let got = went(&o, &cmds);
    let want = pairs(&[("long", "r1"), ("short", "r0")]);
    if got != want {
        return Err(format!("went to {got:?}, not {want:?}"));
    }
    Ok(())
}

#[test]
fn lb08_the_auctions_costs_read_slowness_under_contention() -> Result<(), String> {
    caught(Mutant::SpeedAloneCost, reads_contention)
}

/// A runner's measured pace reaches the auction through its own kernel's word (`Admission::paced`,
/// as the shell's sense paces it; the kernel folds it into the runner's multiplier): of a runner of
/// 2 CPUs measured twice as fast and one of 4 at the reference, the two long runs take the fast one
/// and the short ones the other. Never said, both are the reference, the costs can't tell the runs
/// apart, and the kernel's own pick puts the first long run on the one with the most room.
fn paced() -> Result<(), String> {
    let mut lab = Lab::new(&[(2, 2048, 0), (4, 1024, 0)], true, &Settings { auction: true, ..Settings::default() });
    lab.ask_many(vec![ask("s1", "cpus=1", Some(10_000))?, ask("l1", "cpus=1", Some(100_000))?, ask("s2", "cpus=1", Some(10_000))?, ask("l2", "cpus=1", Some(100_000))?], 1);
    let mut got = lab.went();
    got.sort();
    let want = pairs(&[("l1", "r0"), ("l2", "r0"), ("s1", "r1"), ("s2", "r1")]);
    if got != want {
        return Err(format!("went to {got:?}, not {want:?}"));
    }
    Ok(())
}

#[test]
fn lb08_w05_r26_a_runners_measured_pace_reaches_the_auction() -> Result<(), String> {
    caught(Mutant::UnpacedKernel, paced)
}

/// Where the auction's costs can't tell runs apart every assignment costs the same, and each run
/// goes as the kernel's own pick, one at a time, would put it: four runs on four idle runners of 8
/// CPUs, nothing measured (their times differ, the runners don't) go one each (W05-R23: the
/// auction's slots of one runner are all alike, and it would stack them on one); with no history
/// (the runners' speeds differ, their times don't) each goes to the fastest core free, the kernel's
/// narrow order.
fn ties_spread() -> Result<(), String> {
    for (runners, medians, places) in [(vec![(8, 0, 0); 4], [Some(1000), Some(2000), Some(3000), Some(4000)], 4), (vec![(8, 2048, 0), (8, 1024, 0), (8, 512, 0), (8, 256, 0)], [None; 4], 1)] {
        let asks = || medians.iter().enumerate().map(|(i, m)| ask(&format!("a{i}"), "cpus=1", *m)).collect::<Result<Vec<_>, _>>();
        let mut o = fleet(&runners, true);
        let mut cmds = Vec::new();
        o.ask_many_c(asks()?, 1, &mut cmds);
        let got = went(&o, &cmds);
        let distinct: std::collections::BTreeSet<&String> = got.iter().map(|(_, r)| r).collect();
        let mut one = fleet(&runners, false);
        let mut alone = Vec::new();
        for a in asks()? {
            one.ask_c(a, 1, &mut alone);
        }
        if got.len() != 4 || distinct.len() != places || got != went(&one, &alone) {
            return Err(format!("{runners:?}: {got:?}, one at a time {:?}", went(&one, &alone)));
        }
    }
    Ok(())
}

#[test]
fn lb08_w05_r23_where_the_auctions_costs_tie_the_kernels_pick_spreads_the_runs() -> Result<(), String> {
    caught(Mutant::AuctionTies, ties_spread)
}

/// What the auction can't start now is probed as before: of four asks on runners holding three
/// slots, the fourth in the step's order is probed, none lost.
fn rest_probed() -> Result<(), String> {
    let mut o = fleet(&[(2, 2048, 0), (1, 1024, 0)], true);
    let asks = vec![ask("a", "cpus=1", Some(100_000))?, ask("b", "cpus=1", Some(10_000))?, ask("c", "cpus=1", Some(10_000))?, ask("d", "cpus=1", Some(100_000))?];
    let mut cmds = Vec::new();
    o.ask_many_c(asks, 1, &mut cmds);
    let got = went(&o, &cmds);
    let probes = cmds.iter().filter(|c| matches!(c, Cmd::Tell { run, msg: Down::Probe { .. }, .. } if run.as_ref() == "d")).count();
    let started: Vec<&str> = got.iter().map(|(r, _)| r.as_str()).collect();
    if started != ["a", "b", "c"] || probes != 2 {
        return Err(format!("went {got:?}, {probes} probes of d: {cmds:?}"));
    }
    Ok(())
}

#[test]
fn lb08_w05_r16_what_the_auction_cant_start_now_is_probed() -> Result<(), String> {
    caught(Mutant::DropUnplaced, rest_probed)
}

/// A gang is answered never whichever way it's asked, as the place core has answered every gang
/// since it was built (docs/needs.md E5: commands can't be one of a gang yet; docs/lb.md "The
/// auction"): the auction's step is a batch, and it starts no part of a gang.
#[test]
fn lb08_a_gang_asked_in_one_step_is_answered_as_one_at_a_time() -> Result<(), String> {
    let mut o = fleet(&[(8, 0, 0), (8, 0, 0)], true);
    let mut cmds = Vec::new();
    o.ask_many_c(vec![ask("g0", "together:g/2,cpus=1", Some(1000))?, ask("g1", "together:g/2,cpus=1", Some(1000))?], 1, &mut cmds);
    let never = cmds.iter().filter(|c| matches!(c, Cmd::Answer { answer: Answer::Never { need, .. }, .. } if need.contains("together"))).count();
    if never != 2 || !went(&o, &cmds).is_empty() {
        return Err(format!("{cmds:?}"));
    }
    Ok(())
}

/// With the auction on or off, a step of asks past the fleet's room: each started once as the runs
/// before end, none lost, and with it on each it placed says so.
#[test]
fn lb08_with_the_auction_on_or_off_every_run_in_a_step_starts_once() -> Result<(), String> {
    for auction in [false, true] {
        let settings = Settings { auction, ..Settings::default() };
        let mut lab = Lab::new(&[(2, 2048, 0), (2, 1024, 0), (1, 0, 0)], true, &settings);
        let n = 9;
        let asks = (0..n).map(|i| ask(&format!("a{i}"), "cpus=1", Some(1000 * (1 + i as u64 % 3)))).collect::<Result<Vec<_>, _>>()?;
        lab.ask_many(asks, 1);
        let by_auction = lab.answers.iter().filter(|(_, a)| matches!(a, Answer::Started { why, .. } if why.contains("the auction"))).count();
        if (auction && by_auction != 5) || (!auction && by_auction != 0) {
            return Err(format!("auction {auction}: {by_auction} said the auction placed them: {:?}", lab.answers));
        }
        let mut ended = 0;
        while lab.end_one(2) {
            ended += 1;
            if ended > 4 * n {
                return Err(format!("auction {auction}: {ended} ends and runs still going"));
            }
        }
        let mut starts = lab.starts.clone();
        starts.sort_unstable();
        starts.dedup();
        if lab.starts.len() != n || starts.len() != n || ended != n {
            return Err(format!("auction {auction}: {} starts ({} distinct), {ended} ended, of {n}: {:?}", lab.starts.len(), starts.len(), lab.answers));
        }
    }
    Ok(())
}
