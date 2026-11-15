//! The capacity view under the nemesis (joint plan step 13, `docs/design/capacity-view.md`; AC-T7.4.1).
//! A bounded set of seeds here; ROOT runs the sweeps (`run` over more seeds and runners).

use super::*;

fn total(all: Report, r: Report) -> Report {
    Report {
        steps: all.steps + r.steps,
        hex_started: all.hex_started + r.hex_started,
        hex_refused: all.hex_refused + r.hex_refused,
        runs_granted: all.runs_granted + r.runs_granted,
        runs_refused: all.runs_refused + r.runs_refused,
        runs_unanswered: all.runs_unanswered + r.runs_unanswered,
        runs_let_go: all.runs_let_go + r.runs_let_go,
        link_drops: all.link_drops + r.link_drops,
        keeper_crashes: all.keeper_crashes + r.keeper_crashes,
        crashes_at_write: all.crashes_at_write + r.crashes_at_write,
        runner_crashes: all.runner_crashes + r.runner_crashes,
        released_at_sync: all.released_at_sync + r.released_at_sync,
        adopted_at_sync: all.adopted_at_sync + r.adopted_at_sync,
        forgotten: all.forgotten + r.forgotten,
        wrongly_forgotten: all.wrongly_forgotten + r.wrongly_forgotten,
        samples: all.samples + r.samples,
        moved_in_window: all.moved_in_window + r.moved_in_window,
    }
}

/// The runner counts the suite runs (ROOT, 2026-11-14): one, as the recorded fleet configures each machine's keeper
/// (one kiln runner a machine); and two, the fewest a retirement needs, since it takes a live runner and never the last
/// ([`World::retire`]): a structural edge, no number picked.
const RECORDED: usize = 1;
const RETIRING: usize = 2;

/// Whether the runners' runs outlive their processes: kiln states it for neither, so each is its own case (ROOT,
/// 2026-11-14), never a coin.
const SURVIVE: [bool; 2] = [true, false];

/// The bounded suite's case: `runners` runners in the time where the recorded demand fills the room (ROOT's (ii)).
fn case(runners: usize, survive: bool) -> Case {
    Case {
        runners,
        survive,
        time: Time::Filled,
    }
}

/// The bounded suite's seeds and steps, pinned from ROOT's (6) sweep (`the_record_sweep`, filled time, 256 seeds at
/// 2,000 to 64,000 steps doubling; adacachy2 at main 7c0ed02e9, 2026-11-14, ~/.cache/hex-lead/units/sweep-filled.log):
/// - STEPS: the smallest budget at which every planted mistake is caught on some seed in some survive case: 2,000
///   already (each mistake's caught count there: 3 to 256 of 256), the unplanted cases failing on none at any budget.
/// - SEEDS: at that budget, the largest over the mistakes of each one's first catching seed plus one, its smaller
///   value of its two survive cases: Hex ignoring kiln's reservations (`LocalIgnoresForeign`) is first caught at seed
///   75 (survive=false; seed 125 with survive=true), every other mistake by seed 2, so 76.
/// - SEEDS_RETIRING: the two-runner case plants no mistake to be caught, so it runs as deep as the suite does, SEEDS.
const SEEDS: u64 = 76;
const SEEDS_RETIRING: u64 = SEEDS;
const STEPS: u64 = 2_000;

fn seeds(runners: usize, seeds: u64, steps: u64, time: Time) -> Report {
    SURVIVE.iter().fold(Report::default(), |all, survive| {
        (0..seeds).fold(all, |all, seed| {
            let case = Case {
                runners,
                survive: *survive,
                time,
            };
            let r = run(seed, case, steps, None)
                .unwrap_or_else(|v| panic!("{case:?}, seed {seed}: {v:?}"));
            total(all, r)
        })
    })
}

/// AC-T7.4.1, joint plan step 13: do run one machine, Hex's own boxes beside its one kiln runner on its keeper, as
/// recorded, its runs outliving its process in one case and ending with it in the other, under the nemesis at the
/// records' rates in the time where their mean demand fills the room (the keeper crashing between messages and at its
/// writes, before and after they land; the runner crashing; links dropped with what was on them, partitioned and
/// healed; a wait run out; runs' use moving at every step, a sample's window among them); every charge a recorded one,
/// kiln's legs' and Hex's oci boxes'. Expect the work running never past the room, every running run in the keeper's
/// books and the keeper deciding in no more room than its host measured at every step, and once settled the keeper's
/// books, the runner's and what it runs the same; and (non-vacuity) each fault struck, both sides refused for room,
/// syncs released what ended, a sync adopted work the person had released while its runner was away (CV6), and samples
/// saw use move in their windows.
#[test]
fn no_oversubscription_and_the_books_agree_across_crash_seeds() {
    let all = seeds(RECORDED, SEEDS, STEPS, Time::Filled);
    eprintln!("{RECORDED} runner, {SEEDS} seeds of {STEPS} steps each way: {all:?}");
    assert!(
        all.hex_started > 0
            && all.hex_refused > 0
            && all.runs_granted > 0
            && all.runs_refused > 0
            && all.runs_unanswered > 0
            && all.runs_let_go > 0
            && all.link_drops > 0
            && all.keeper_crashes > 0
            && all.crashes_at_write > 0
            && all.runner_crashes > 0
            && all.released_at_sync > 0
            && all.wrongly_forgotten > 0
            && all.adopted_at_sync > 0
            && all.samples > 0
            && all.moved_in_window > 0,
        "{all:?}"
    );
}

/// AC-T7.4.1, joint plan step 13 (CV6): do the same with two runners, the fewest a retirement needs ([`RETIRING`]), each
/// way; expect the same, and a runner retired and its reservations released by the person.
#[test]
fn a_runner_retired_beside_another_keeps_the_same() {
    let all = seeds(RETIRING, SEEDS_RETIRING, STEPS, Time::Filled);
    assert!(
        all.runs_granted > 0 && all.keeper_crashes > 0 && all.link_drops > 0 && all.forgotten > 0,
        "{RETIRING}: {all:?}"
    );
}

/// Runs the bounded seeds each way with `mutant` planted, its runner's runs outliving it and not: the first violation.
fn caught(mutant: Mutant) -> Option<Violation> {
    let found = SURVIVE.iter().find_map(|survive| {
        (0..SEEDS).find_map(|seed| {
            run(seed, case(RECORDED, *survive), STEPS, Some(mutant))
                .err()
                .map(|v| (seed, *survive, v))
        })
    });
    eprintln!("{mutant:?}: {found:?}");
    found.map(|(_, _, v)| v)
}

/// Non-vacuity, CV5: do plant a keeper's shell that releases a runner's reservations when its link
/// drops; expect some seed to run a run the keeper's books don't hold.
#[test]
fn releasing_on_silence_is_caught() {
    let found = caught(Mutant::ReleaseOnSilence);
    assert!(
        matches!(
            found,
            Some(Violation::Unbooked { .. } | Violation::Oversubscribed { .. })
        ),
        "{found:?}"
    );
}

/// Non-vacuity, CV5 (a holder's first obligation): do plant a runner that starts a run when it asks;
/// expect some seed to run an unbooked run or past the room.
#[test]
fn starting_before_the_grant_is_caught() {
    let found = caught(Mutant::StartBeforeGrant);
    assert!(
        matches!(
            found,
            Some(Violation::Unbooked { .. } | Violation::Oversubscribed { .. })
        ),
        "{found:?}"
    );
}

/// Non-vacuity, CV5 (a holder's third obligation): do plant a runner whose next process lists none of
/// the runs that outlived it; expect some seed to run a run the keeper released.
#[test]
fn forgetting_survivors_is_caught() {
    let found = caught(Mutant::ForgetSurvivors);
    assert!(
        matches!(
            found,
            Some(Violation::Unbooked { .. } | Violation::Oversubscribed { .. })
        ),
        "{found:?}"
    );
}

/// Non-vacuity, CV2: do plant a keeper that answers before its write lands, its crash losing the write;
/// expect some seed to run a run the restarted keeper doesn't hold.
#[test]
fn answering_before_the_write_is_caught() {
    let found = caught(Mutant::AnswerBeforeWrite);
    assert!(found.is_some(), "no seed caught it");
}

/// Non-vacuity, CV3: do plant Hex admitting against the room less its own boxes alone; expect some seed
/// past the room.
#[test]
fn hex_ignoring_the_holders_is_caught() {
    let found = caught(Mutant::LocalIgnoresForeign);
    assert!(
        matches!(found, Some(Violation::Oversubscribed { .. })),
        "{found:?}"
    );
}

/// Non-vacuity, CV5: do plant a keeper's shell that drops from a sync what the keeper doesn't hold, which
/// its sync would adopt after the person's wrong release; expect some seed to run a run the keeper's books
/// lack once its runner has synced, or that runner told the counts disagree.
#[test]
fn dropping_what_the_keeper_lacks_at_a_sync_is_caught() {
    let found = caught(Mutant::DropUnknownAtSync);
    assert!(
        matches!(
            found,
            Some(Violation::Unbooked { .. } | Violation::SyncDisagreed { .. })
        ),
        "{found:?}"
    );
}

/// Non-vacuity, CV12 (withdrawn): do plant a host that credits the room each joined runner's work, the smaller of two
/// reads on either side of its read of free memory, up to its reservations (the withdrawn rule, for any credit); expect
/// some seed whose keeper decides in more room than its host measured.
#[test]
fn crediting_any_use_is_caught() {
    let found = caught(Mutant::Credit);
    assert!(
        matches!(
            found,
            Some(Violation::Overcredited { .. } | Violation::Oversubscribed { .. })
        ),
        "{found:?}"
    );
}

/// Delivers everything said on `world`'s links, and the hints owed, until nothing more is said: how many deliveries.
fn drain(world: &mut World) -> usize {
    let mut delivered = 0;
    loop {
        let pending = |w: &World| {
            w.runners
                .iter()
                .any(|r| !r.link.to_keeper.is_empty() || !r.link.to_holder.is_empty())
        };
        if !pending(world) {
            world.hints().unwrap();
            if !pending(world) {
                return delivered;
            }
        }
        world.deliver_any().unwrap();
        delivered += 1;
    }
}

/// A machine of `runners` runners, each synced and every link quiet.
fn quiet(runners: usize) -> World {
    let mut world = World::new(0, case(runners, true), 0, None).unwrap();
    world.redial();
    drain(&mut world);
    assert!(
        world
            .runners
            .iter()
            .all(|r| r.holder.as_ref().unwrap().is_up())
    );
    world
}

/// AC-T7.4.1, joint plan step 13 (CLAUDE.md §2: a loop's bound derived, never picked): do have one runner, synced, ask
/// for one run more than its reservation bound and let go of each before its answer (CV5), so two words each wait on
/// its link to the keeper, more than twice the bound, then settle; expect it to settle with the books agreeing:
/// settling is bounded by the protocol's own count of what its links carry, not by a multiple of a runner's
/// reservations.
#[test]
fn a_backlog_on_a_link_settles_within_the_protocols_own_bound() {
    let mut world = quiet(RECORDED);
    let asks = world.budget.reservations + 1;
    for _ in 0..asks {
        world.ask(Some(0)).unwrap();
        world.let_go(Some(0)).unwrap();
    }
    assert_eq!(world.runners[0].link.to_keeper.len(), 2 * asks);
    let settled = world.settle();
    assert!(settled.is_ok(), "{settled:?}");
}

/// AC-T7.4.1, joint plan step 13 (ROOT, 2026-11-14: delivery the least rate that drains the links, from what each kind
/// says): each kind's deliveries are the protocol's own. Do, on a quiet machine of the recorded runner and of
/// retirement's two, all synced, make one event of each kind that says something, an outage's cause followed by its
/// end, then deliver everything said until the machine is quiet again; expect the deliveries counted the ones
/// [`deliveries`] gives the kind, and none of an outage's end's own.
#[test]
fn each_kinds_deliveries_are_the_protocols_own() {
    for runners in [RECORDED, RETIRING] {
        for kind in [
            Kind::Ask,
            Kind::EndRun,
            Kind::HexAdmit,
            Kind::HexEnd,
            Kind::Drop,
            Kind::Partition,
            Kind::CrashRunner,
        ] {
            let mut world = quiet(runners);
            match kind {
                Kind::EndRun => {
                    world.ask(Some(0)).unwrap();
                    drain(&mut world);
                    assert_eq!(world.runners[0].running.len(), 1);
                }
                Kind::HexEnd => {
                    world.hex_admit().unwrap();
                    world.command().unwrap();
                    world.command().unwrap();
                    drain(&mut world);
                    assert_eq!(world.boxes.len(), 1);
                }
                _ => {}
            }
            match kind {
                Kind::Ask => world.ask(Some(0)).unwrap(),
                Kind::EndRun => {
                    let id = *world.runners[0].running.keys().next().unwrap();
                    world.end_run(0, id);
                }
                Kind::HexAdmit => {
                    world.hex_admit().unwrap();
                    world.command().unwrap();
                    world.command().unwrap();
                }
                Kind::HexEnd => {
                    let key = *world.boxes.keys().next().unwrap();
                    world.hex_end(key).unwrap();
                }
                Kind::Drop => world.drop_link(Some(0)).unwrap(),
                Kind::Partition => {
                    world.drop_link(Some(0)).unwrap();
                    world.runners[0].link.partitioned = true;
                    world.redial();
                    assert_eq!(drain(&mut world), 0);
                    world.runners[0].link.partitioned = false;
                }
                _ => {
                    world.crash_runner(Some(0)).unwrap();
                    world.redial();
                    assert_eq!(drain(&mut world), 0);
                    world.restart_runner(Some(0)).unwrap();
                }
            }
            world.redial();
            let delivered = drain(&mut world);
            assert_eq!(
                delivered as f64,
                deliveries(kind, runners),
                "{kind:?} with {runners} runners"
            );
        }
        for end in [Kind::Heal, Kind::RestartRunner, Kind::RestartKeeper] {
            assert_eq!(deliveries(end, runners), 0.0, "{end:?}");
        }
    }
}

/// AC-T7.4.1, joint plan step 13 (CV7; ROOT, 2026-11-14 (f): what the code does on its own trigger, the nemesis does
/// there): the keeper's hints go out as its lane says them, after each message a link said and after each command of
/// its host. Do, on a quiet machine of two runners, deliver one runner's reserve to the keeper, then have Hex's node
/// admit a box; expect the other runner's hint on its link at once after the first, and one on each runner's link at
/// once after the second, no look at the keeper's hints between.
#[test]
fn the_keepers_hints_go_out_after_each_message_and_each_command() {
    let mut world = quiet(RETIRING);
    world.ask(Some(0)).unwrap();
    world.deliver_to_keeper(0).unwrap();
    let hint = |r: &Runner| {
        r.link
            .to_holder
            .iter()
            .any(|m| matches!(m, ToHolder::Hint { .. }))
    };
    assert!(hint(&world.runners[1]) && !hint(&world.runners[0]));
    drain(&mut world);
    world.hex_admit().unwrap();
    world.command().unwrap();
    world.command().unwrap();
    assert_eq!(world.boxes.len(), 1);
    assert!(world.runners.iter().all(hint));
}

/// AC-T7.4.1, joint plan step 13 (ROOT, 2026-11-14 (ii), (i)): a run's and a box's end comes at one over its recorded
/// length, stretched in the bounded suite's time by [`Rates::fill`] and not in the recorded time. Do make a machine in
/// each; expect a length's hazard in each to be that.
#[test]
fn an_end_comes_at_one_over_its_length_stretched() {
    let fill = rates().unwrap().fill;
    for (time, stretch) in [(Time::Filled, fill), (Time::Recorded, 1.0)] {
        let world = World::new(
            0,
            Case {
                runners: RECORDED,
                survive: true,
                time,
            },
            0,
            None,
        )
        .unwrap();
        for length in [1e-3, 1.0, 762.6] {
            assert_eq!(world.hazard(length).unwrap(), 1.0 / (stretch * length));
        }
    }
}

/// T-T7.4.1's determinism: one seed, one run.
#[test]
fn a_seed_names_one_run() {
    for survive in SURVIVE {
        assert_eq!(
            run(7, case(RECORDED, survive), STEPS, None),
            run(7, case(RECORDED, survive), STEPS, None)
        );
    }
}

/// A sweep's size from the environment, for the lead's record runs: `name`'s count, else the bounded suite's.
fn sized(name: &str, bounded: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(bounded)
}

/// AC-T7.4.1, joint plan step 13 (ROOT's (6) and (i) of 2026-11-14): the record sweep, the lead's, in release: for each
/// step budget of `HEX_CAPACITY_STEPS` (a comma-separated list; the bounded suite's alone unless set),
/// `HEX_CAPACITY_SEEDS` seeds (the bounded suite's unless set) each way ([`SURVIVE`]) with the recorded runner and with
/// retirement's two, then each planted mistake over the same seeds with the recorded runner, each way, in
/// `HEX_CAPACITY_TIME`'s time (`filled`, the bounded suite's, unless `recorded`: every rate and length as recorded).
/// Expect every seed to hold; each mistake's line says how many seeds caught it, the first, and the step it was caught
/// at.
#[test]
#[ignore = "the record sweep, sized by HEX_CAPACITY_SEEDS, HEX_CAPACITY_STEPS and HEX_CAPACITY_TIME, in release (the lead's)"]
fn the_record_sweep() {
    let n = sized("HEX_CAPACITY_SEEDS", SEEDS);
    let budgets: Vec<u64> = std::env::var("HEX_CAPACITY_STEPS")
        .map(|v| v.split(',').map(|s| s.trim().parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![STEPS]);
    let time = match std::env::var("HEX_CAPACITY_TIME").as_deref() {
        Ok("recorded") => Time::Recorded,
        Ok("filled") | Err(_) => Time::Filled,
        Ok(other) => panic!("HEX_CAPACITY_TIME is filled or recorded, not {other}"),
    };
    eprintln!("SWEEP rates={:?}", rates().unwrap());
    let mut held = true;
    for steps in budgets {
        for runners in [RECORDED, RETIRING] {
            for survive in SURVIVE {
                let case = Case {
                    runners,
                    survive,
                    time,
                };
                let mut all = Report::default();
                let mut failed = Vec::new();
                for seed in 0..n {
                    match run(seed, case, steps, None) {
                        Ok(r) => all = total(all, r),
                        Err(v) => failed.push((seed, v)),
                    }
                }
                held &= failed.is_empty();
                eprintln!(
                    "SWEEP time={time:?} steps={steps} runners={runners} survive={survive} seeds={n} failed={} first={:?} {all:?}",
                    failed.len(),
                    failed.first()
                );
            }
        }
        for mutant in [
            Mutant::ReleaseOnSilence,
            Mutant::StartBeforeGrant,
            Mutant::ForgetSurvivors,
            Mutant::AnswerBeforeWrite,
            Mutant::LocalIgnoresForeign,
            Mutant::DropUnknownAtSync,
            Mutant::Credit,
        ] {
            for survive in SURVIVE {
                let case = Case {
                    runners: RECORDED,
                    survive,
                    time,
                };
                let caught: Vec<(u64, Option<u64>)> = (0..n)
                    .filter_map(|seed| {
                        run(seed, case, steps, Some(mutant))
                            .err()
                            .map(|v| (seed, step_of(&v)))
                    })
                    .collect();
                eprintln!(
                    "SWEEP time={time:?} steps={steps} mutant={mutant:?} survive={survive} seeds={n} caught={} first_seed={:?} first_step={:?}",
                    caught.len(),
                    caught.first().map(|(seed, _)| seed),
                    caught.first().and_then(|(_, step)| *step)
                );
            }
        }
    }
    assert!(held, "a seed failed unplanted: see the SWEEP lines");
}

/// The step a violation was found at, where it names one: settling's findings come once every step has run.
fn step_of(v: &Violation) -> Option<u64> {
    match v {
        Violation::Oversubscribed { step, .. }
        | Violation::Unreported { step, .. }
        | Violation::Unbooked { step, .. }
        | Violation::SyncDisagreed { step, .. }
        | Violation::Overcredited { step, .. } => Some(*step),
        Violation::Disagree { .. } | Violation::Stopped(_) | Violation::Unsettled => None,
    }
}

/// AC-T7.4.1, joint plan step 13 (H10; R03: a machine's room recorded, never picked): the machine under the nemesis is the
/// recorded fleet's (`bench/sched/huawei-r5-day30.fleet.trace`, from kiln's runner facts of 2026-11-05) smallest by memory
/// of the hosts where Hex's own boxes and kiln's runs share one room, those that run Hex's `oci` class: where the two
/// contend for the least room. Do read the fleet's node rows; expect the room to be that host's CPUs, memory and GPUs.
#[test]
fn the_machine_is_the_recorded_fleets_smallest_shared_host() {
    let fleet = crate::replay::parse(
        include_str!("../../../../bench/sched/huawei-r5-day30.fleet.trace"),
        &crate::replay::Limits::default(),
    )
    .unwrap();
    let shared = fleet
        .nodes
        .iter()
        .filter(|n| n.classes.has(crate::model::Class::Oci))
        .map(|n| n.capacity)
        .min_by_key(|c| (c.mem_bytes, c.cpu_milli))
        .unwrap();
    assert_eq!(
        (ROOM.cpu_milli, ROOM.mem_bytes, ROOM.gpus),
        (shared.cpu_milli, shared.mem_bytes, u64::from(shared.gpus))
    );
}

/// AC-T7.4.1, CV8 (R03: a bound derived, never picked): the keeper holds a holder's name to a bound, which on this machine
/// is the longest name its runners say in their hellos. Do make a world of the most runners the suite runs
/// ([`RETIRING`]); expect its keeper's name bound to be its longest runner's name.
#[test]
fn the_keepers_name_bound_is_its_runners_longest_name() {
    let world = World::new(0, case(RETIRING, true), 0, None).unwrap();
    let longest = world.runners.iter().map(|r| r.name.len()).max().unwrap();
    assert_eq!(world.budget.name_bytes, longest);
}

/// AC-T7.4.1, joint plan step 13, CV8 (ROOT, 2026-11-14: the protocol's own bound wins): a runner's reservation bound is
/// as many runs of the smallest recorded charge as the room holds in its fewest dimension, but never more than one sync
/// carries. Do read the room and the smallest recorded charge; expect the bound the smaller of the two, here the sync's.
#[test]
fn a_runners_reservation_bound_is_the_protocols_own() {
    let smallest = kiln_legs()
        .iter()
        .map(|leg| leg.need)
        .reduce(Amounts::min)
        .unwrap();
    let fits = (ROOM.cpu_milli / smallest.cpu_milli).min(ROOM.mem_bytes / smallest.mem_bytes);
    let most = hex_core::capacity::most_held().unwrap();
    assert_eq!(
        reservations().unwrap(),
        usize::try_from(fits).unwrap().min(most)
    );
    assert_eq!(
        reservations().unwrap(),
        most,
        "the room holds fewer: {fits}"
    );
}

/// kiln's export of its CI, which the kiln trace was made from (`bench/sched/kiln_workload.md`).
fn export() -> serde_json::Value {
    serde_json::from_reader(flate2::read::GzDecoder::new(
        std::fs::File::open(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../bench/sched/kiln-ci-20261022-20261105.export.json.gz"
        ))
        .unwrap(),
    ))
    .unwrap()
}

/// The export's jobs that ran: a start and an end, the trace's legs.
fn ran(export: &serde_json::Value) -> Vec<&serde_json::Value> {
    export["runs"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|run| run["jobs"].as_array().unwrap())
        .filter(|job| job["started_at"].is_u64() && job["finished_at"].is_u64())
        .collect()
}

/// AC-T7.4.1, joint plan step 13 (ROOT, 2026-11-14: a leg below kiln's resolution is that unit, never 0, never dropped):
/// a recorded leg's need is measured to a resolution, a milli-core (the trace's unit) and a 4 KiB page, of which every
/// peak RSS kiln measured is a whole number. Do read kiln's export and every leg of the trace made from it; expect each
/// of the export's 3,750 measured peak RSSs whole pages, the trace's legs below the resolution counted, and every charge
/// the runners draw at least one unit in each dimension, one for each leg. (A leg kiln didn't measure takes its job's
/// median, between two measured ones for an even count: `kiln_workload.md`.) A charge of nothing is the keeper's own
/// edge case, refused Why::Empty (hex-api's capacity tests), which no recorded charge asks.
#[test]
fn a_recorded_legs_need_is_whole_units_of_its_resolution() {
    let export = export();
    let measured: Vec<u64> = ran(&export)
        .iter()
        .filter_map(|job| job["peak_rss"].as_u64())
        .collect();
    assert_eq!(measured.len(), 3_750);
    assert!(measured.iter().all(|m| m % RESOLUTION.mem_bytes == 0));
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../bench/sched/kiln-ci-20261022-20261105.trace"
    ))
    .unwrap();
    let legs: Vec<Amounts> = text.lines().filter_map(leg).map(|l| l.need).collect();
    assert_eq!(legs.len(), 4_655);
    let below = |f: fn(&Amounts) -> u64| legs.iter().filter(|l| f(l) == 0).count();
    assert_eq!((below(|l| l.cpu_milli), below(|l| l.mem_bytes)), (631, 58));
    assert_eq!(kiln_legs().len(), legs.len());
    assert!(kiln_legs().iter().all(|l| RESOLUTION.fits(l.need)));
}

/// AC-T7.4.1, joint plan step 13 (ROOT, 2026-11-14): Hex's own boxes are the recorded fleet's `oci` legs. Do read them;
/// expect each drawn as recorded, 0.3 to 3.4 cores and 128 MiB to 4 GiB.
#[test]
fn hex_boxes_are_the_recorded_fleets_oci_legs() {
    let boxes = oci_legs();
    assert_eq!(boxes.len(), 8_432);
    let low = boxes.iter().map(|l| l.need).reduce(Amounts::min).unwrap();
    let high = boxes.iter().map(|l| l.need).reduce(Amounts::max).unwrap();
    assert_eq!((low.cpu_milli, low.mem_bytes), (300, 128 << 20));
    assert_eq!((high.cpu_milli, high.mem_bytes), (3_400, 4 << 30));
}

/// AC-T7.4.1, joint plan step 13 (ROOT's ruling 7, 2026-11-14): the capacity view's tests on real machines ask the
/// recorded legs one after another. Do read them as those tests do; expect each list in the order its trace has it, every
/// leg of it once, kiln's first leg (recorded below a milli-core) drawn as one unit of its resolution.
#[test]
fn the_recorded_legs_are_asked_in_recorded_order() {
    let kiln: Vec<Amounts> = kiln_needs().collect();
    assert_eq!(kiln.len(), 4_655);
    let first = |cpu_milli, mem_bytes| Amounts {
        cpu_milli,
        mem_bytes,
        ..Amounts::ZERO
    };
    assert_eq!(
        kiln[..3],
        [
            first(1, 2_609_152),
            first(5_422, 35_218_735_104),
            first(371, 13_536_509_952)
        ]
    );
    let oci: Vec<Amounts> = oci_needs().collect();
    assert_eq!(oci.len(), 8_432);
    assert_eq!(oci[..2], [first(600, 512 << 20), first(300, 128 << 20)]);
}

/// AC-T7.4.1, joint plan step 13 (ROOT, 2026-11-14: workload rates from the traces): kiln's runs arrive at the machine as
/// they did at fedora, whose room it is. Do read kiln's export; expect fedora's legs, under the name it ran by until
/// 2026-10-26 and the one it ran by from 2026-10-27 (one machine renamed: the first name's last leg ends before the
/// second's first starts, and the second's facts are the room's), to be [`FEDORA_LEGS`], over the export's window of
/// [`KILN_WINDOW`] seconds.
#[test]
fn fedora_ran_696_of_the_exports_legs() {
    let export = export();
    let window = export["window"].as_array().unwrap();
    assert_eq!(
        (window[1].as_u64().unwrap() - window[0].as_u64().unwrap()) as f64,
        KILN_WINDOW
    );
    let jobs = ran(&export);
    let on = |name: &str| -> Vec<(u64, u64)> {
        jobs.iter()
            .filter(|job| job["agent"].as_str() == Some(name))
            .map(|job| {
                (
                    job["started_at"].as_u64().unwrap(),
                    job["finished_at"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    let (before, after) = (on("fedora.attlocal.net"), on("kiln-a094a12d.attlocal.net"));
    let last = before.iter().map(|(_, end)| *end).max().unwrap();
    let first = after.iter().map(|(start, _)| *start).min().unwrap();
    assert!(last < first, "{last} {first}");
    let facts = export["runners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "kiln-a094a12d.attlocal.net")
        .unwrap();
    assert_eq!(
        (
            facts["threads"].as_u64().unwrap() * 1_000,
            facts["mem_total"].as_u64().unwrap()
        ),
        (ROOM.cpu_milli, ROOM.mem_bytes)
    );
    assert_eq!((before.len(), after.len()), (355, 341));
    assert_eq!(
        before.len() + after.len(),
        usize::try_from(FEDORA_LEGS).unwrap()
    );
}

/// AC-T7.4.1, joint plan step 13 (ROOT, 2026-11-14 (a), (f), 5): every rate is a record's. Do read them; expect kiln's
/// runs to arrive at fedora's [`FEDORA_LEGS`] over its export's window; Hex's boxes at the fleet day's 8,432 `oci` legs
/// over its 15 `oci` hosts and the day; and links to drop, and outages to end, at kiln's link records of
/// 2026-11-14T04:36Z (`runners`: topo.drops, topo.up_ms, topo.path.outage_ms): adas-mac-studio.local 178 drops, 190,790,534
/// ms up, 114,855 ms of outage; adas-macbook-pro-2.local 78, 135,604,022, 75,192; adas-mac-studio-3.local 67, 190,345,527,
/// 113,336; adas-mac-studio.local-2 46, 229,313,558, 41,087; adacachy2 8, 133,839,471, 1,078,559: 377 drops over 879,893,112
/// ms up (1.54 an hour) and 1,423,029 ms of outage (3.77 s each).
#[test]
fn the_rates_are_the_records() {
    let r = rates().unwrap();
    assert_eq!(r.ask, 696.0 / 1_209_600.0);
    assert_eq!(r.admit, 8_432.0 / (15.0 * 86_400.0));
    assert_eq!(r.drop, 377.0 / 879_893.112);
    assert_eq!(r.heal, 377.0 / 1_423.029);
    let hosts = crate::replay::parse(FLEET, &crate::replay::Limits::default())
        .unwrap()
        .nodes
        .iter()
        .filter(|n| n.classes.has(crate::model::Class::Oci))
        .count();
    assert_eq!(hosts, 15);
}

/// AC-T7.4.1, joint plan step 13 (ROOT, 2026-11-14 (ii)): the bounded suite's time is the one at which the recorded mean
/// demand fills the room, with no free constant: each run's and box's length stretched by [`Rates::fill`], one over the
/// larger of CPU's and memory's ρ = (λ_kiln × E[length × need]_kiln + λ_hex × E[length × need]_hex) ÷ room, the mean held
/// by Little's law. Do compute ρ from both traces; expect the stretched ρ 1 in memory, the dimension that fills first,
/// less than 1 in CPU, and the stretch as the records give it.
#[test]
fn the_bounded_time_is_where_the_recorded_demand_fills_the_room() {
    let r = rates().unwrap();
    let rho = |of: fn(&Amounts) -> u64, room: u64| {
        let mean = |legs: &[Leg]| {
            legs.iter()
                .map(|l| l.runs_for * of(&l.need) as f64)
                .sum::<f64>()
                / legs.len() as f64
        };
        (r.ask * mean(kiln_legs()) + r.admit * mean(oci_legs())) / room as f64
    };
    let (cpu, mem) = (
        rho(|a| a.cpu_milli, ROOM.cpu_milli),
        rho(|a| a.mem_bytes, ROOM.mem_bytes),
    );
    assert!((mem * r.fill - 1.0).abs() < 1e-12, "{mem} {}", r.fill);
    assert!(cpu * r.fill < 1.0, "{cpu} {}", r.fill);
    assert_eq!(format!("{:.2}", r.fill), "15.17");
}
