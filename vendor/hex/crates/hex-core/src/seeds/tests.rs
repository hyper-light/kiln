//! The seeds' tests (decision 5, goal 5 plan step 3: HEX-SEEDS-1 to 6). Against an oracle under random starts, failed
//! starts, creates, claims, completions, seed deaths, seeds past the hang bound and the clock moving on, the pool never
//! holds more seeds than its bound, never hands a create a seed that is starting, busy or gone, answers every event as
//! the oracle does (its waits, refusals and stale reports among them) and counts what it counts, with every path taken
//! over the run (non-vacuity counters); and each case the plan names, every seed busy past the line's W* and a seed
//! that dies mid-create, is answered as the design says (decision 9, the person's ruling of 2026-11-13, for the first),
//! as are a seed past the hang bound, the order of the line, a failed start, a line none can come for and a late
//! report.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use super::*;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A pool's bounds and its waits' seeds, in milliseconds: `None` for a wait left measuring, following its seeds until
/// its first answer.
#[derive(Debug, Clone, Copy)]
struct Setup {
    limits: Limits,
    wait: Option<u64>,
    hang: Option<u64>,
}

impl Setup {
    /// The pool, each wait as it would be once it had seen an answer of the wait it is to start with (test-only
    /// [`Patience::seeded`]; a node's pool starts with none and follows its seeds, decision 9).
    fn pool(self) -> Seeds {
        let mut pool = Seeds::new(self.limits);
        calibrate(&mut pool.wait, &mut pool.hang, self);
        pool
    }
}

fn calibrate(wait: &mut Patience, hang: &mut Patience, setup: Setup) {
    let seed = |name, ms_: Option<u64>| match ms_ {
        Some(n) => Patience::seeded(name, ms(n)),
        None => Patience::measuring(name),
    };
    *wait = seed(WAIT, setup.wait);
    *hang = seed(HANG, setup.hang);
}

fn setup(seeds: usize, waiting: usize, wait: Option<u64>, hang: Option<u64>) -> Setup {
    Setup {
        limits: Limits {
            seeds: NonZeroUsize::new(seeds).unwrap(),
            waiting,
        },
        wait,
        hang,
    }
}

/// A pool whose waits start at `wait` and `hang` milliseconds.
fn seeds(seeds: usize, waiting: usize, wait: u64, hang: u64) -> Seeds {
    setup(seeds, waiting, Some(wait), Some(hang)).pool()
}

/// Starts a seed and reports it up at `now`, with no create waiting.
fn up(pool: &mut Seeds, now: Duration) -> SeedId {
    let seed = pool.start().unwrap();
    assert_eq!(pool.started(seed, now), Ok(None));
    seed
}

/// What a shell does to the pool. A report names its seed or ticket by `k`: most often one a shell would report on,
/// sometimes any it was ever given ([`pick`]).
#[derive(Debug, Clone, Copy)]
enum Event {
    /// The shell takes a start order.
    Start,
    /// A seed comes up.
    Started(usize),
    /// A start fails.
    StartFailed(usize),
    /// A create asks for a seed.
    Create,
    /// A waiting create claims.
    Claim(usize),
    /// A seed is done with its create.
    Done(usize),
    /// A seed dies.
    Died(usize),
    /// The shell's wait for a seed's answer passes the hang bound: it asks after the seeds past it, each followed by
    /// its life from then.
    PastHang,
    /// The clock moves on by this many milliseconds.
    Tick(u8),
}

/// The `k`th of `focus` three times in four, else the `k`th of `all`.
fn pick<T: Copy>(k: usize, focus: &[T], all: &[T]) -> Option<T> {
    if !k.is_multiple_of(4) && !focus.is_empty() {
        focus.get(k / 4 % focus.len()).copied()
    } else if all.is_empty() {
        None
    } else {
        all.get(k % all.len()).copied()
    }
}

/// What a slot's seed is doing, as the oracle keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Starting,
    Idle {
        served: bool,
    },
    Busy {
        since: Duration,
        until: Option<Duration>,
        unclaimed: Option<Ticket>,
    },
}

/// How a create in line waits, as the oracle keeps it: within the line's W* as it was when it came, until the time
/// held; or following its seeds by their life alone, past its W* or from its start where the line had none.
#[derive(Debug, Clone, Copy)]
enum Wait {
    Within(Duration),
    Following,
}

/// The pool as plainly as it can be said: each slot's seed and what it does, the line and the answers awaiting
/// their claims as lists in the order they came, whether it asks for starts, and what it has counted. It names
/// nothing itself: it takes the pool's seeds and tickets once the test has checked each is new, and the pool's choice
/// of slot or seed once the test has checked the design allows it.
struct Oracle {
    seeds: Vec<Option<(SeedId, Phase)>>,
    up_before: Vec<bool>,
    line: Vec<(Ticket, Duration, Wait)>,
    answered: Vec<(Ticket, Result<SeedId, Refused>)>,
    asking: bool,
    counts: Counts,
    handed: Vec<u64>,
    /// The line's wait and the hang bound, each the slowest answer seen (`crate::patience`, tested on its own): from
    /// each wait that ends in a seed and each create a seed is done with; no sample from a wait that ran out or a seed
    /// that died (Karn's point), after a run-out the next wait following its party until an answer comes.
    wait: Patience,
    hang: Patience,
}

impl Oracle {
    fn new(setup: Setup) -> Self {
        let limits = setup.limits;
        let slots = limits.seeds.get();
        Self {
            seeds: vec![None; slots],
            up_before: vec![false; slots],
            line: Vec::new(),
            answered: Vec::new(),
            asking: true,
            counts: Counts::default(),
            handed: vec![0; slots],
            wait: Patience::measuring(WAIT),
            hang: Patience::measuring(HANG),
        }
        .calibrated(setup)
    }

    fn calibrated(mut self, setup: Setup) -> Self {
        calibrate(&mut self.wait, &mut self.hang, setup);
        self
    }

    fn phase(&self, seed: SeedId) -> Option<Phase> {
        match self.seeds.get(seed.slot) {
            Some(Some((s, phase))) if *s == seed => Some(*phase),
            _ => None,
        }
    }

    fn set(&mut self, seed: SeedId, phase: Option<Phase>) {
        self.seeds[seed.slot] = phase.map(|p| (seed, p));
    }

    fn with(&self, phase: impl Fn(Phase) -> bool) -> Vec<SeedId> {
        self.seeds
            .iter()
            .flatten()
            .filter(|(_, p)| phase(*p))
            .map(|(s, _)| *s)
            .collect()
    }

    fn vacant(&self) -> Vec<usize> {
        (0..self.seeds.len())
            .filter(|&slot| self.seeds[slot].is_none())
            .collect()
    }

    fn wanted(&self) -> usize {
        if self.asking { self.vacant().len() } else { 0 }
    }

    fn hand(&mut self, seed: SeedId, now: Duration, unclaimed: Option<Ticket>) {
        let until = self.hang.deadline().map(|hang| now + hang);
        self.set(
            seed,
            Some(Phase::Busy {
                since: now,
                until,
                unclaimed,
            }),
        );
        self.counts.handed += 1;
        self.handed[seed.slot] += 1;
        let busy = self.with(|p| matches!(p, Phase::Busy { .. })).len() as u64;
        self.counts.most_busy = self.counts.most_busy.max(busy);
    }

    /// A seed up and free goes to the first in line, however long it has waited: past its W* and yet to claim, its
    /// run-out is told first; its wait is a sample. With none in line it idles.
    fn free(
        &mut self,
        seed: SeedId,
        now: Duration,
        served: bool,
        seen: &mut Paths,
    ) -> Option<Ticket> {
        if self.line.is_empty() {
            self.set(seed, Some(Phase::Idle { served }));
            return None;
        }
        let (ticket, since, wait) = self.line.remove(0);
        if let Wait::Within(until) = wait
            && until < now
        {
            self.wait.ran_out(until - since);
            seen.handed_past_bound += 1;
        }
        self.wait.answered(now - since);
        self.hand(seed, now, Some(ticket));
        self.answered.push((ticket, Ok(seed)));
        Some(ticket)
    }

    /// A create comes to the line at `now`: within the line's W*, or following its seeds from its start where the line
    /// has none; with when it claims again.
    fn wait_in_line(&self, now: Duration) -> (Wait, Option<Duration>) {
        match self.wait.deadline() {
            Some(wait) => (Wait::Within(now + wait), Some(now + wait)),
            None => (Wait::Following, None),
        }
    }

    /// The `i`th in line claims at `now`. Within its W* it waits on. Past it its run-out is told, once, and it follows
    /// its seeds by their life alone, waiting on for the shell's wake: it is refused in line only when none can come
    /// ([`Oracle::none_can_come`]). Following from its start, the same.
    fn claim_in_line(&mut self, i: usize, now: Duration, seen: &mut Paths) -> Claim {
        let (ticket, since, wait) = self.line[i];
        match wait {
            Wait::Within(until) if now < until => {
                seen.claim_waits += 1;
                Claim::Wait { until: Some(until) }
            }
            Wait::Within(until) => {
                self.wait.ran_out(until - since);
                seen.run_out_at_claim += 1;
                self.line[i] = (ticket, since, Wait::Following);
                Claim::Wait { until: None }
            }
            Wait::Following => {
                seen.follow_waits += 1;
                Claim::Wait { until: None }
            }
        }
    }

    /// With the starting stopped and no seed up or starting, none can come for the line: each create in it is refused,
    /// its wait the time it waited, no wait moved. Says whether any was.
    fn none_can_come(&mut self, now: Duration) -> bool {
        if self.asking || self.line.is_empty() || !self.seeds.iter().all(Option::is_none) {
            return false;
        }
        for (ticket, since, _) in std::mem::take(&mut self.line) {
            self.counts.refused_waited += 1;
            let wait = now - since;
            self.answered.push((ticket, Err(Refused::Waited { wait })));
        }
        true
    }

    /// A seed that left while handed to a create yet to claim it: that create's answer is `refused`.
    fn refuse(&mut self, unclaimed: Option<Ticket>, refused: Refused) -> Result<(), TestCaseError> {
        if let Some(ticket) = unclaimed {
            let Some(answer) = self.answered.iter_mut().find(|(t, _)| *t == ticket) else {
                return Err(TestCaseError::fail(format!(
                    "ticket {ticket} was handed a seed and has no answer"
                )));
            };
            answer.1 = Err(refused);
        }
        Ok(())
    }
}

/// How many times each path was taken over a run: one no case took is one the run proves nothing of.
#[derive(Debug, Clone, Copy, Default)]
struct Paths {
    handed_at_once: usize,
    waited: usize,
    handed_on_start: usize,
    handed_on_done: usize,
    line_full: usize,
    run_out_at_claim: usize,
    follow_waits: usize,
    handed_past_bound: usize,
    refused_on_failed_start: usize,
    refused_on_death: usize,
    claim_waits: usize,
    claim_seed: usize,
    claim_died: usize,
    died_serving: usize,
    died_after_serving: usize,
    died_unserved: usize,
    past_hang: usize,
    start_failed: usize,
    start_stopped: usize,
    asked_again: usize,
    restarted: usize,
    gone: usize,
    wrong_state: usize,
    no_ticket: usize,
    all_busy: usize,
}

/// HEX-SEEDS-2: a seed handed to a create is up and serves no other.
fn handing(
    up: &BTreeSet<SeedId>,
    serving: &mut BTreeSet<SeedId>,
    seed: SeedId,
) -> Result<(), TestCaseError> {
    prop_assert!(
        up.contains(&seed),
        "handed {} to a create, but it isn't up",
        seed
    );
    prop_assert!(
        serving.insert(seed),
        "handed {} to a create, but it serves another",
        seed
    );
    Ok(())
}

/// Plays `events` on a pool of `limits` and on the oracle, and checks the pool after each against the oracle and
/// against the shell's own record of what it was given.
fn play(setup: Setup, events: &[Event], paths: &Cell<Paths>) -> Result<(), TestCaseError> {
    let limits = setup.limits;
    let mut pool = setup.pool();
    let mut oracle = Oracle::new(setup);
    let mut seen = paths.get();
    let mut now = Duration::ZERO;
    let bound = limits.seeds.get();
    // The shell's record, kept apart from the oracle's slots: every seed and ticket it was given, the seeds starting
    // and up, and those serving a create.
    let mut issued: Vec<SeedId> = Vec::new();
    let mut tickets: Vec<Ticket> = Vec::new();
    let mut starting: BTreeSet<SeedId> = BTreeSet::new();
    let mut up: BTreeSet<SeedId> = BTreeSet::new();
    let mut serving: BTreeSet<SeedId> = BTreeSet::new();
    for &event in events {
        match event {
            Event::Start => {
                let got = pool.start();
                let vacant = oracle.vacant();
                if oracle.asking && !vacant.is_empty() {
                    let Some(seed) = got else {
                        return Err(TestCaseError::fail("no start given while one was wanted"));
                    };
                    prop_assert!(
                        vacant.contains(&seed.slot),
                        "started {} in a slot that isn't vacant",
                        seed
                    );
                    prop_assert!(
                        issued.iter().all(|s| s.n != seed.n),
                        "seed number {} given twice",
                        seed.n
                    );
                    oracle.set(seed, Some(Phase::Starting));
                    issued.push(seed);
                    starting.insert(seed);
                } else {
                    if !vacant.is_empty() {
                        seen.start_stopped += 1;
                    }
                    prop_assert_eq!(got, None);
                }
            }
            Event::Started(k) => {
                let Some(seed) = pick(k, &oracle.with(|p| p == Phase::Starting), &issued) else {
                    continue;
                };
                let got = pool.started(seed, now);
                match oracle.phase(seed) {
                    None => {
                        seen.gone += 1;
                        prop_assert_eq!(got, Err(Stale::Gone(seed)));
                    }
                    Some(Phase::Starting) => {
                        let restart = oracle.up_before[seed.slot];
                        oracle.up_before[seed.slot] = true;
                        oracle.counts.started += 1;
                        if restart {
                            oracle.counts.restarted += 1;
                            seen.restarted += 1;
                        }
                        let want = oracle.free(seed, now, false, &mut seen);
                        prop_assert_eq!(got, Ok(want));
                        prop_assert!(starting.remove(&seed));
                        up.insert(seed);
                        if want.is_some() {
                            seen.handed_on_start += 1;
                            handing(&up, &mut serving, seed)?;
                        }
                    }
                    Some(_) => {
                        seen.wrong_state += 1;
                        let expected = "starting";
                        prop_assert_eq!(got, Err(Stale::State { seed, expected }));
                    }
                }
            }
            Event::StartFailed(k) => {
                let Some(seed) = pick(k, &oracle.with(|p| p == Phase::Starting), &issued) else {
                    continue;
                };
                let got = pool.start_failed(seed, now);
                match oracle.phase(seed) {
                    None => {
                        seen.gone += 1;
                        prop_assert_eq!(got, Err(Stale::Gone(seed)));
                    }
                    Some(Phase::Starting) => {
                        oracle.set(seed, None);
                        oracle.counts.start_failed += 1;
                        oracle.asking = false;
                        seen.start_failed += 1;
                        // With no seed up or starting, none can come for the line: each create in it is refused.
                        let refuse = oracle.none_can_come(now);
                        if refuse {
                            seen.refused_on_failed_start += 1;
                        }
                        prop_assert_eq!(got, Ok(refuse));
                        prop_assert!(starting.remove(&seed));
                    }
                    Some(_) => {
                        seen.wrong_state += 1;
                        let expected = "starting";
                        prop_assert_eq!(got, Err(Stale::State { seed, expected }));
                    }
                }
            }
            Event::Create => {
                let got = pool.create(now);
                oracle.counts.asked += 1;
                if !oracle.asking {
                    seen.asked_again += 1;
                }
                oracle.asking = true;
                let idle = oracle.with(|p| matches!(p, Phase::Idle { .. }));
                if !idle.is_empty() {
                    let Ok(Admit::Seed(seed)) = got else {
                        return Err(TestCaseError::fail(format!(
                            "a create with {idle:?} idle was answered {got:?}"
                        )));
                    };
                    prop_assert!(
                        idle.contains(&seed),
                        "handed {} at once, but it isn't idle",
                        seed
                    );
                    oracle.hand(seed, now, None);
                    handing(&up, &mut serving, seed)?;
                    seen.handed_at_once += 1;
                } else if oracle.line.len() + oracle.answered.len() < limits.waiting {
                    let Ok(Admit::Wait { ticket, until }) = got else {
                        return Err(TestCaseError::fail(format!(
                            "a create with room in line was answered {got:?}"
                        )));
                    };
                    prop_assert!(
                        tickets.iter().all(|t| *t < ticket),
                        "ticket {} doesn't come after every earlier one",
                        ticket
                    );
                    let (wait, wakes) = oracle.wait_in_line(now);
                    prop_assert_eq!(until, wakes);
                    oracle.line.push((ticket, now, wait));
                    oracle.counts.waited += 1;
                    tickets.push(ticket);
                    seen.waited += 1;
                } else {
                    oracle.counts.refused_busy += 1;
                    seen.line_full += 1;
                    let waiting = limits.waiting;
                    prop_assert_eq!(got, Err(Refused::Busy { waiting }));
                }
            }
            Event::Claim(k) => {
                let held: Vec<Ticket> = oracle
                    .line
                    .iter()
                    .map(|(t, _, _)| *t)
                    .chain(oracle.answered.iter().map(|(t, _)| *t))
                    .collect();
                let Some(ticket) = pick(k, &held, &tickets) else {
                    continue;
                };
                let got = pool.claim(ticket, now);
                if let Some(i) = oracle.answered.iter().position(|(t, _)| *t == ticket) {
                    match oracle.answered.remove(i).1 {
                        Ok(seed) => {
                            let Some(Phase::Busy { since, until, .. }) = oracle.phase(seed) else {
                                return Err(TestCaseError::fail(format!(
                                    "ticket {ticket} was handed {seed}, which serves nothing"
                                )));
                            };
                            let unclaimed = None;
                            oracle.set(
                                seed,
                                Some(Phase::Busy {
                                    since,
                                    until,
                                    unclaimed,
                                }),
                            );
                            seen.claim_seed += 1;
                            prop_assert_eq!(got, Ok(Claim::Seed(seed)));
                        }
                        Err(refused) => {
                            match refused {
                                Refused::Died { .. } => seen.claim_died += 1,
                                Refused::Busy { .. } | Refused::Waited { .. } => {}
                            }
                            prop_assert_eq!(got, Ok(Claim::Refused(refused)));
                        }
                    }
                } else if let Some(i) = oracle.line.iter().position(|(t, _, _)| *t == ticket) {
                    let want = oracle.claim_in_line(i, now, &mut seen);
                    prop_assert_eq!(got, Ok(want));
                } else {
                    seen.no_ticket += 1;
                    prop_assert_eq!(got, Err(Stale::Ticket(ticket)));
                }
            }
            Event::Done(k) => {
                let holding = oracle.with(|p| {
                    matches!(
                        p,
                        Phase::Busy {
                            unclaimed: None,
                            ..
                        }
                    )
                });
                let Some(seed) = pick(k, &holding, &issued) else {
                    continue;
                };
                let got = pool.done(seed, now);
                match oracle.phase(seed) {
                    None => {
                        seen.gone += 1;
                        prop_assert_eq!(got, Err(Stale::Gone(seed)));
                    }
                    Some(Phase::Busy {
                        since,
                        unclaimed: None,
                        ..
                    }) => {
                        oracle.counts.done += 1;
                        oracle.hang.answered(now - since);
                        prop_assert!(serving.remove(&seed));
                        let want = oracle.free(seed, now, true, &mut seen);
                        prop_assert_eq!(got, Ok(want));
                        if want.is_some() {
                            seen.handed_on_done += 1;
                            handing(&up, &mut serving, seed)?;
                        }
                    }
                    Some(_) => {
                        seen.wrong_state += 1;
                        let expected = "serving a create that holds it";
                        prop_assert_eq!(got, Err(Stale::State { seed, expected }));
                    }
                }
            }
            Event::Died(k) => {
                let Some(seed) = pick(k, &oracle.with(|_| true), &issued) else {
                    continue;
                };
                let got = pool.died(seed, now);
                match oracle.phase(seed) {
                    None => {
                        seen.gone += 1;
                        prop_assert_eq!(got, Err(Stale::Gone(seed)));
                    }
                    Some(phase) => {
                        oracle.set(seed, None);
                        oracle.counts.died += 1;
                        let want = match phase {
                            Phase::Busy { unclaimed, .. } => {
                                let refused = Refused::Died { seed };
                                oracle.counts.refused_died += 1;
                                oracle.refuse(unclaimed, refused)?;
                                seen.died_serving += 1;
                                Some(refused)
                            }
                            Phase::Idle { served: true } => {
                                seen.died_after_serving += 1;
                                None
                            }
                            Phase::Starting | Phase::Idle { served: false } => {
                                oracle.asking = false;
                                seen.died_unserved += 1;
                                None
                            }
                        };
                        // With the starting stopped and no seed left, none can come for the line.
                        if oracle.none_can_come(now) {
                            seen.refused_on_death += 1;
                        }
                        prop_assert_eq!(got, Ok(want));
                        starting.remove(&seed);
                        up.remove(&seed);
                        serving.remove(&seed);
                    }
                }
            }
            Event::PastHang => {
                // Each seed serving its create past the bound has its run-out told, once, and is followed by its life
                // from then: never taken for hung (decision 9).
                let late = oracle
                    .with(|p| matches!(p, Phase::Busy { until: Some(until), .. } if until <= now));
                for &seed in &late {
                    let Some(Phase::Busy {
                        since,
                        until: Some(until),
                        unclaimed,
                    }) = oracle.phase(seed)
                    else {
                        return Err(TestCaseError::fail(format!(
                            "{seed} was past its bound serving nothing"
                        )));
                    };
                    oracle.hang.ran_out(until - since);
                    let until = None;
                    oracle.set(
                        seed,
                        Some(Phase::Busy {
                            since,
                            until,
                            unclaimed,
                        }),
                    );
                }
                seen.past_hang += late.len();
                prop_assert_eq!(pool.past_hang(now), late.len());
                prop_assert_eq!(pool.past_hang(now), 0, "a run-out told twice");
            }
            Event::Tick(n) => now += ms(u64::from(n)),
        }
        // HEX-SEEDS-6: the pool counts what the oracle counts, holds what it holds, and asks for what it asks for.
        let counts = pool.counts();
        prop_assert_eq!(counts, oracle.counts);
        prop_assert_eq!(pool.handed(), &oracle.handed[..]);
        prop_assert_eq!(
            pool.deadlines(),
            (oracle.wait.deadline(), oracle.hang.deadline())
        );
        prop_assert_eq!(pool.wanted(), oracle.wanted());
        prop_assert_eq!(pool.starting(), starting.len());
        prop_assert_eq!(pool.idle() + pool.busy(), up.len());
        prop_assert_eq!(pool.busy(), serving.len());
        prop_assert_eq!(
            pool.idle(),
            oracle.with(|p| matches!(p, Phase::Idle { .. })).len()
        );
        prop_assert_eq!(pool.waiting(), oracle.line.len());
        // HEX-SEEDS-1: never more seeds starting or up than the bound, and never two in one slot.
        let held: Vec<SeedId> = starting.iter().chain(&up).copied().collect();
        prop_assert!(
            held.len() <= bound,
            "{} seeds held past a bound of {}",
            held.len(),
            bound
        );
        let slots: BTreeSet<usize> = held.iter().map(|s| s.slot).collect();
        prop_assert_eq!(slots.len(), held.len(), "two seeds in one slot: {:?}", held);
        // HEX-SEEDS-3: no create waits while a seed idles.
        prop_assert!(
            pool.idle() == 0 || pool.waiting() == 0,
            "{} creates wait beside {} idle seeds",
            pool.waiting(),
            pool.idle()
        );
        // HEX-SEEDS-6: every create asked is handed, refused or waits; every create handed is done, refused or served.
        prop_assert_eq!(
            counts.asked,
            counts.handed + counts.refused_busy + counts.refused_waited + pool.waiting() as u64
        );
        prop_assert_eq!(
            counts.handed,
            counts.done + counts.refused_died + pool.busy() as u64
        );
        prop_assert_eq!(counts.handed, pool.handed().iter().sum::<u64>());
        prop_assert!(counts.restarted <= counts.died);
        prop_assert!(counts.started - counts.restarted <= bound as u64);
        prop_assert!(counts.most_busy <= bound as u64);
    }
    if pool.counts().most_busy == bound as u64 {
        seen.all_busy += 1;
    }
    paths.set(seen);
    Ok(())
}

/// Starts and creates often, claims and completions about as often, deaths, asks after seeds past the hang bound and
/// failed starts rarer, and the clock moving on in steps that cross the small bounds below within a few events.
fn event() -> impl Strategy<Value = Event> {
    prop_oneof![
        6 => Just(Event::Start),
        5 => any::<usize>().prop_map(Event::Started),
        1 => any::<usize>().prop_map(Event::StartFailed),
        8 => Just(Event::Create),
        6 => any::<usize>().prop_map(Event::Claim),
        7 => any::<usize>().prop_map(Event::Done),
        2 => any::<usize>().prop_map(Event::Died),
        2 => Just(Event::PastHang),
        4 => (0u8..=12).prop_map(Event::Tick),
    ]
}

/// Small pools, short lines and short waits, which histories fill, cross and drain often, calibrated and not; and
/// a node's first-start pool of one seed, measuring.
fn bounds() -> impl Strategy<Value = Setup> {
    prop_oneof![
        8 => (
            1usize..=4,
            0usize..=3,
            proptest::option::of(0u64..=20),
            proptest::option::of(0u64..=40),
        )
            .prop_map(|(seeds, waiting, wait, hang)| setup(seeds, waiting, wait, hang)),
        1 => (0usize..=256).prop_map(|waiting| setup(1, waiting, None, None)),
    ]
}

/// HEX-SEEDS-1 to 6, against the oracle: under random starts, failed starts, creates, claims, completions, deaths,
/// seeds past the hang bound and the clock moving on, every create goes to an idle seed that is up, or waits, or is
/// refused, as the
/// oracle says; no seed serves two creates; the bound is never passed; every report about a seed that left, out of
/// order or with a ticket no create holds is refused by type; the counts equal the oracle's; and no event panics.
/// Every path is taken over the run.
#[test]
fn against_an_oracle_no_create_meets_a_busy_or_gone_seed_the_bound_holds_and_the_counts_are_true() {
    let paths = Cell::new(Paths::default());
    let mut runner = TestRunner::new(Config {
        cases: 1024,
        source_file: Some(file!()),
        ..Config::default()
    });
    let histories = (bounds(), proptest::collection::vec(event(), 0..200));
    if let Err(e) = runner.run(&histories, |(setup, events)| play(setup, &events, &paths)) {
        panic!("{e}");
    }
    let p = paths.get();
    for (path, n) in [
        ("a create handed an idle seed at once", p.handed_at_once),
        ("a create that waits", p.waited),
        (
            "a seed handed to a waiting create as it comes up",
            p.handed_on_start,
        ),
        (
            "a seed handed to a waiting create as it is done",
            p.handed_on_done,
        ),
        ("a create refused with the line full", p.line_full),
        ("a claim past its W*, its run-out told", p.run_out_at_claim),
        ("a following claim told to wait on", p.follow_waits),
        (
            "a seed handed to a create past its W* before it claimed",
            p.handed_past_bound,
        ),
        (
            "a create refused in line as the last start failed",
            p.refused_on_failed_start,
        ),
        (
            "a create refused in line as the last seed died",
            p.refused_on_death,
        ),
        ("a claim told to wait on", p.claim_waits),
        ("a claim handed its seed", p.claim_seed),
        ("a claim meeting its seed's death", p.claim_died),
        ("a seed dying mid-create", p.died_serving),
        ("a seed dying idle after serving", p.died_after_serving),
        ("a seed dying before serving", p.died_unserved),
        ("a seed past the hang bound, its run-out told", p.past_hang),
        ("a failed start", p.start_failed),
        (
            "a start refused while the starting is stopped",
            p.start_stopped,
        ),
        ("a create that asks again", p.asked_again),
        ("a restart", p.restarted),
        ("a report about a seed that left", p.gone),
        ("a report out of order", p.wrong_state),
        ("a claim with a ticket no create holds", p.no_ticket),
        ("every seed serving at once", p.all_busy),
    ] {
        assert!(n > 0, "no history took {path}: {p:?}");
    }
}

/// HEX-SEEDS-4, the plan's second case: a seed that dies mid-create refuses that create by type, leaves its slot,
/// and is started there again, counted as a restart; the create's late report finds it gone, and the next create is
/// handed the new seed. A waiting create handed a seed that dies before it claims meets the refusal at its claim.
#[test]
fn a_seed_that_dies_mid_create_refuses_that_create_and_is_restarted_and_counted() {
    let mut pool = seeds(1, 4, 1_000, 1_000);
    let first = up(&mut pool, ms(0));
    assert_eq!(pool.create(ms(1)), Ok(Admit::Seed(first)));
    assert_eq!(
        pool.died(first, ms(1)),
        Ok(Some(Refused::Died { seed: first }))
    );
    assert_eq!(pool.done(first, ms(2)), Err(Stale::Gone(first)));
    assert_eq!(pool.died(first, ms(2)), Err(Stale::Gone(first)));
    assert_eq!(pool.wanted(), 1);
    let second = pool.start().unwrap();
    assert_eq!(second.slot(), first.slot());
    assert_ne!(second, first);
    assert_eq!(pool.started(second, ms(3)), Ok(None));
    assert_eq!(pool.create(ms(4)), Ok(Admit::Seed(second)));
    let c = pool.counts();
    assert_eq!(
        (c.died, c.refused_died, c.started, c.restarted),
        (1, 1, 2, 1)
    );

    let Ok(Admit::Wait { ticket, .. }) = pool.create(ms(5)) else {
        panic!("a create with its only seed busy didn't wait");
    };
    assert_eq!(pool.done(second, ms(6)), Ok(Some(ticket)));
    assert_eq!(
        pool.died(second, ms(6)),
        Ok(Some(Refused::Died { seed: second }))
    );
    assert_eq!(
        pool.claim(ticket, ms(7)),
        Ok(Claim::Refused(Refused::Died { seed: second }))
    );
    let c = pool.counts();
    assert_eq!((c.died, c.refused_died, c.done), (2, 2, 1));
}

/// HEX-SEEDS-3: seeds that free go to the waiting creates in the order they came, however long each has waited: one
/// past the line's W* when a seed frees, yet to claim, is handed it, its run-out told and its wait the sample the
/// line's W* rises to (decision 9); with no one waiting the seed idles, and the next create takes it at once.
#[test]
fn seeds_go_to_the_waiting_creates_in_the_order_they_came() {
    let mut pool = seeds(1, 8, 10, 1_000);
    let seed = up(&mut pool, ms(0));
    assert_eq!(pool.create(ms(0)), Ok(Admit::Seed(seed)));
    let mut wait = |at| match pool.create(ms(at)) {
        Ok(Admit::Wait { ticket, .. }) => ticket,
        other => panic!("a create with its only seed busy was answered {other:?}"),
    };
    let (a, b, c) = (wait(0), wait(5), wait(6));
    assert_eq!(pool.done(seed, ms(12)), Ok(Some(a)));
    assert_eq!(
        pool.deadlines().0,
        Some(ms(12)),
        "the line's W* rose to the wait past it"
    );
    assert_eq!(pool.claim(a, ms(12)), Ok(Claim::Seed(seed)));
    assert_eq!(pool.done(seed, ms(13)), Ok(Some(b)));
    assert_eq!(pool.claim(b, ms(13)), Ok(Claim::Seed(seed)));
    assert_eq!(pool.done(seed, ms(14)), Ok(Some(c)));
    assert_eq!(pool.claim(c, ms(14)), Ok(Claim::Seed(seed)));
    assert_eq!(pool.done(seed, ms(15)), Ok(None));
    assert_eq!((pool.idle(), pool.waiting()), (1, 0));
    assert_eq!(pool.create(ms(15)), Ok(Admit::Seed(seed)));
}

/// Decision 9 (original; the person's ruling of 2026-11-13: W* is a bound only while answers come within it): a create
/// handed no seed within the line's W* was refused at its claim, though the seeds were freeing for the creates ahead
/// of it. Do: one seed, the line's W* 10 ms, the seed busy from 0 ms; creates wait from 0 ms and 1 ms; claim the
/// second at its W*'s end, 11 ms; the seed done at 15 ms (to the first), the second claiming at 21 ms, and the seed
/// done again at 25 ms. Expect: at 11 ms the second waits on, its run-out told, for the shell's wake alone; so at
/// 21 ms; handed the seed at 25 ms, its 24 ms the line's W* then; none refused.
#[test]
fn original_a_create_past_the_lines_w_star_waits_on_while_seeds_free() {
    let mut pool = seeds(1, 4, 10, 1_000);
    let seed = up(&mut pool, ms(0));
    assert_eq!(pool.create(ms(0)), Ok(Admit::Seed(seed)));
    let Ok(Admit::Wait { ticket: first, .. }) = pool.create(ms(0)) else {
        panic!("a create with its only seed busy didn't wait");
    };
    let Ok(Admit::Wait {
        ticket: second,
        until,
    }) = pool.create(ms(1))
    else {
        panic!("a create with its only seed busy didn't wait");
    };
    assert_eq!(until, Some(ms(11)));
    assert_eq!(pool.claim(second, ms(11)), Ok(Claim::Wait { until: None }));
    assert_eq!(pool.done(seed, ms(15)), Ok(Some(first)));
    assert_eq!(pool.claim(first, ms(15)), Ok(Claim::Seed(seed)));
    assert_eq!(pool.claim(second, ms(21)), Ok(Claim::Wait { until: None }));
    assert_eq!(pool.done(seed, ms(25)), Ok(Some(second)));
    assert_eq!(pool.claim(second, ms(25)), Ok(Claim::Seed(seed)));
    assert_eq!(pool.deadlines().0, Some(ms(24)));
    assert_eq!(pool.counts().refused_waited, 0);
}

/// Decision 9 (original; the person's ruling of 2026-11-13: W* is a bound only while answers come within it): a seed
/// that had served its create for the hang bound, the slowest create answer the pool had seen, was taken for hung, to
/// be killed, and its create refused ("a create seed did not answer within 1.16752ms", oci_box). Do: one seed, the hang
/// bound 30 ms, serving a create from 0 ms, a second create waiting in line from 5 ms; ask after the seeds past the
/// bound at 29 ms, at 30 ms twice and at 1,000 s; the seed done at 1,000 s. Expect: none past it at 29 ms; at 30 ms the
/// seed's run-out told, once, and the seed followed by its life (no time it passes, the next seed's wait following
/// it), never taken for hung; done at 1,000 s, its answer the sample the hang bound rises to, and the seed handed to
/// the create in line; no seed died or was restarted.
#[test]
fn original_a_seed_past_the_hang_bound_is_followed_by_its_life_to_its_answer() {
    let mut pool = seeds(1, 4, 1_000, 30);
    let first = up(&mut pool, ms(0));
    assert_eq!(pool.create(ms(0)), Ok(Admit::Seed(first)));
    let Ok(Admit::Wait { ticket, .. }) = pool.create(ms(5)) else {
        panic!("a create with its only seed busy didn't wait");
    };
    assert_eq!(pool.past_hang(ms(29)), 0);
    assert_eq!(pool.until(first), Ok(Some(ms(30))));
    assert_eq!(pool.past_hang(ms(30)), 1, "the run-out told");
    assert_eq!(pool.past_hang(ms(30)), 0, "told once");
    assert_eq!(pool.until(first), Ok(None), "followed by its life");
    assert_eq!(pool.deadlines().1, None, "the next seed's wait follows it");
    assert_eq!(pool.past_hang(ms(1_000_000)), 0);
    assert_eq!(pool.done(first, ms(1_000_000)), Ok(Some(ticket)));
    assert_eq!(pool.claim(ticket, ms(1_000_000)), Ok(Claim::Seed(first)));
    assert_eq!(pool.deadlines().1, Some(ms(1_000_000)));
    let c = pool.counts();
    assert_eq!(
        (c.done, c.died, c.refused_died, c.started, c.restarted),
        (1, 0, 0, 1, 0)
    );
}

/// HEX-SEEDS-3, the plan's first case, under decision 9 (the person's ruling of 2026-11-13: W* is a bound only while
/// answers come within it; past it a create follows its party, the busy seeds, by their life alone, as at every local
/// site): with every seed busy, a create waits in line; past the line's W* its run-out is told and it waits on while
/// its seeds live, however long none frees, until a seed freed goes to it. With the line full, a create is refused by
/// type at once. Do: two seeds busy, the line's W* 10 ms; creates wait from 1 ms and 5 ms, and a third finds the line
/// full; claim the first at 10 ms, 11 ms, 22 ms and 1,000 s, no seed freed; a seed done at 1,000 s; the second claims
/// then. Expect: told to wait on to 11 ms; at 11 ms the run-out told and to wait on for the shell's wake alone; so at
/// 22 ms, a whole window past W* with none freed, and at 1,000 s; handed the seed done, its 999.999 s the line's W*;
/// the second, past its W* too, waiting on; none refused in line.
#[test]
fn a_create_past_the_lines_w_star_waits_on_while_its_seeds_live_however_long_none_frees() {
    let mut pool = seeds(2, 2, 10, 1_000);
    up(&mut pool, ms(0));
    up(&mut pool, ms(0));
    let Ok(Admit::Seed(first)) = pool.create(ms(0)) else {
        panic!("an idle seed wasn't handed out");
    };
    assert!(matches!(pool.create(ms(0)), Ok(Admit::Seed(_))));
    let Ok(Admit::Wait {
        ticket: late,
        until,
    }) = pool.create(ms(1))
    else {
        panic!("a create with every seed busy didn't wait");
    };
    assert_eq!(until, Some(ms(11)));
    let Ok(Admit::Wait { ticket: next, .. }) = pool.create(ms(5)) else {
        panic!("a create with every seed busy didn't wait");
    };
    assert_eq!(pool.create(ms(5)), Err(Refused::Busy { waiting: 2 }));
    assert_eq!(
        pool.claim(late, ms(10)),
        Ok(Claim::Wait {
            until: Some(ms(11))
        })
    );
    let following = Ok(Claim::Wait { until: None });
    assert_eq!(pool.claim(late, ms(11)), following);
    assert_eq!(
        pool.deadlines().0,
        None,
        "the run-out told: the line's next wait follows its seeds"
    );
    assert_eq!(
        pool.claim(late, ms(22)),
        following,
        "a whole window past W*, none freed"
    );
    assert_eq!(pool.claim(late, ms(1_000_000)), following);
    assert_eq!(pool.done(first, ms(1_000_000)), Ok(Some(late)));
    assert_eq!(pool.claim(late, ms(1_000_000)), Ok(Claim::Seed(first)));
    assert_eq!(pool.deadlines().0, Some(ms(999_999)));
    assert_eq!(pool.claim(next, ms(1_000_000)), following);
    let c = pool.counts();
    assert_eq!(
        (
            c.asked,
            c.handed,
            c.waited,
            c.refused_busy,
            c.refused_waited,
            c.done
        ),
        (5, 3, 2, 1, 0, 1)
    );
    assert_eq!(c.most_busy, 2);
}

/// Decision 9 (the person's ruling of 2026-11-13: a following wait is refused at its party's end): a create in line
/// whose seeds are all gone, none asked for, is refused at once, since no seed can come; following its seeds by their
/// life alone, it would otherwise wait for ever. Do: one slot, the line's W* 10 ms; a create waits in line from 0 ms
/// for the seed starting, which dies at 2 ms before it serves (so the starting stops until the next create). Expect:
/// the create refused at its claim at 2 ms, its wait the 2 ms it waited; the line empty; neither wait moved (no seed
/// was slow: none came).
#[test]
fn a_create_in_line_is_refused_at_once_when_its_last_seed_dies_and_none_can_come() {
    let mut pool = seeds(1, 4, 10, 1_000);
    let seed = pool.start().unwrap();
    let Ok(Admit::Wait { ticket, until }) = pool.create(ms(0)) else {
        panic!("a create with no seed up didn't wait");
    };
    assert_eq!(until, Some(ms(10)));
    assert_eq!(pool.died(seed, ms(2)), Ok(None));
    assert_eq!(
        pool.wanted(),
        0,
        "the starting stopped until the next create"
    );
    assert_eq!(
        pool.claim(ticket, ms(2)),
        Ok(Claim::Refused(Refused::Waited { wait: ms(2) }))
    );
    assert_eq!(pool.waiting(), 0);
    assert_eq!(pool.deadlines(), (Some(ms(10)), Some(ms(1_000))));
}

/// HEX-SEEDS-5: a failed start stops the starting until the next create, as does a seed that dies before it serves a
/// create; a seed that dies after serving one has its slot asked for at once.
#[test]
fn a_failed_start_or_a_seed_dead_before_serving_stops_the_starting_until_the_next_create() {
    let mut pool = seeds(2, 4, 1_000, 1_000);
    let failed = pool.start().unwrap();
    assert_eq!(pool.start_failed(failed, ms(0)), Ok(false));
    assert_eq!((pool.wanted(), pool.start()), (0, None));
    let Ok(Admit::Wait { ticket, .. }) = pool.create(ms(0)) else {
        panic!("a create with no seed up didn't wait");
    };
    assert_eq!(pool.wanted(), 2);
    let served = pool.start().unwrap();
    let unserved = pool.start().unwrap();
    assert_eq!(pool.started(served, ms(1)), Ok(Some(ticket)));
    assert_eq!(pool.claim(ticket, ms(1)), Ok(Claim::Seed(served)));
    assert_eq!(pool.done(served, ms(2)), Ok(None));
    assert_eq!(pool.died(served, ms(2)), Ok(None));
    assert_eq!(pool.wanted(), 1);
    assert_eq!(pool.died(unserved, ms(2)), Ok(None));
    assert_eq!((pool.wanted(), pool.start()), (0, None));
    assert_eq!(pool.started(unserved, ms(3)), Err(Stale::Gone(unserved)));
    assert!(matches!(pool.create(ms(4)), Ok(Admit::Wait { .. })));
    assert_eq!(pool.wanted(), 2);
    let c = pool.counts();
    assert_eq!(
        (c.start_failed, c.died, c.started, c.restarted),
        (1, 2, 1, 0)
    );
}

/// HEX-SEEDS-1: a new pool asks for every slot and no more: once each holds a seed, no start is given, and each seed
/// fills a slot of its own.
#[test]
fn a_pool_asks_for_its_slots_and_no_more() {
    let mut pool = seeds(3, 0, 0, 0);
    assert_eq!(pool.wanted(), 3);
    let seeds: Vec<SeedId> = (0..3).map(|_| pool.start().unwrap()).collect();
    assert_eq!((pool.wanted(), pool.start()), (0, None));
    let slots: BTreeSet<usize> = seeds.iter().map(|s| s.slot()).collect();
    assert_eq!(slots, BTreeSet::from([0, 1, 2]));
    assert_eq!(pool.starting(), 3);
}

/// HEX-SEEDS-4: a report about a seed that left its slot is refused by type and leaves the slot's next seed as it was;
/// a report out of order is refused by type, as is a claim with a ticket no create holds.
#[test]
fn a_late_or_out_of_order_report_is_refused_by_type() {
    let mut pool = seeds(1, 4, 1_000, 1_000);
    let old = up(&mut pool, ms(0));
    let serving = "serving a create that holds it";
    assert_eq!(
        pool.done(old, ms(0)),
        Err(Stale::State {
            seed: old,
            expected: serving
        })
    );
    let starting = Stale::State {
        seed: old,
        expected: "starting",
    };
    assert_eq!(pool.started(old, ms(0)), Err(starting));
    assert_eq!(pool.start_failed(old, ms(0)), Err(starting));
    assert_eq!(pool.create(ms(0)), Ok(Admit::Seed(old)));
    assert_eq!(pool.died(old, ms(0)), Ok(Some(Refused::Died { seed: old })));
    let new = pool.start().unwrap();
    for late in [
        pool.started(old, ms(1)).map(|_| ()),
        pool.start_failed(old, ms(1)).map(|_| ()),
        pool.done(old, ms(1)).map(|_| ()),
        pool.died(old, ms(1)).map(|_| ()),
    ] {
        assert_eq!(late, Err(Stale::Gone(old)));
    }
    assert_eq!(pool.starting(), 1);
    assert_eq!(pool.started(new, ms(2)), Ok(None));
    assert_eq!(pool.create(ms(2)), Ok(Admit::Seed(new)));
    assert_eq!(pool.claim(Ticket(7), ms(2)), Err(Stale::Ticket(Ticket(7))));
}

/// HEX-SEEDS-1 under Ada's rule of 2026-11-06 (AC-T7.1.3): `SEEDS`' 32 was the bench's width and fedora's cores; a
/// node's seeds now start at its parallelism and grow with the creates that find none free, up to the bound derived
/// from its memory. Do: a pool of 1 slot at first, at most 3, and four creates, each seed started as the pool asks.
/// Expect: the second and third creates each add a slot to start, so each is handed a seed of its own; the fourth,
/// past the bound, waits and adds none; the slots never pass the bound.
#[test]
fn a_growing_pool_adds_a_slot_for_each_create_that_finds_none_free_up_to_its_bound() {
    let mut pool = Seeds::growing(setup(3, 8, None, None).limits, 1);
    assert_eq!(pool.wanted(), 1);
    up(&mut pool, ms(0));
    assert!(matches!(pool.create(ms(1)), Ok(Admit::Seed(_))));
    for n in 2..=3 {
        let Ok(Admit::Wait { ticket, .. }) = pool.create(ms(n)) else {
            panic!("create {n} found a seed idle");
        };
        assert_eq!(pool.wanted(), 1, "create {n} added a slot to start");
        let seed = pool.start().unwrap();
        assert_eq!(pool.started(seed, ms(n)), Ok(Some(ticket)));
        assert!(matches!(pool.claim(ticket, ms(n)), Ok(Claim::Seed(s)) if s == seed));
    }
    assert!(matches!(pool.create(ms(4)), Ok(Admit::Wait { .. })));
    assert_eq!(pool.wanted(), 0, "past the bound no slot is added");
    assert_eq!(pool.handed().len(), 3);
    assert_eq!(
        pool.counts().waited,
        3,
        "each create that found none free is counted"
    );
}

/// Decision 9 (Ada's rules of 2026-11-06 and 2026-11-13; AC-T7.1.3): the line's wait and the hang bound are each the
/// slowest answer the pool has seen, with no margin, and a wait past its bound makes the next one follow its party. Do:
/// on a pool of one seed whose waits have seen 4 ms, serve a create for 3 ms while another waits 2 ms in line; let the
/// second's seed pass its bound; then let a create's wait in line pass its bound, the seed done at 13 ms. Expect: the
/// first waits within the 4 ms; the faster answers leaving both bounds at 4 ms; the seed past its bound at 7 ms and
/// not before, its run-out told and no sample, the seed followed by its life and the next seed's wait following (no
/// bound); the create told the run-out at 11 ms and to wait on, the line's next wait following too; at 13 ms the seed
/// done, its 10 ms the hang bound then, and handed to the create, its 6 ms the line's W* then.
#[test]
fn the_wait_and_the_hang_bound_are_the_slowest_answer_and_follow_past_a_run_out() {
    let mut pool = seeds(1, 4, 4, 4);
    let seed = up(&mut pool, ms(0));
    assert_eq!(pool.deadlines(), (Some(ms(4)), Some(ms(4))));
    assert_eq!(pool.create(ms(0)), Ok(Admit::Seed(seed)));
    assert_eq!(pool.until(seed), Ok(Some(ms(4))));
    let Ok(Admit::Wait { ticket, until }) = pool.create(ms(1)) else {
        panic!("a create with its only seed busy didn't wait");
    };
    assert_eq!(until, Some(ms(5)));
    assert_eq!(pool.done(seed, ms(3)), Ok(Some(ticket)));
    assert_eq!(pool.deadlines(), (Some(ms(4)), Some(ms(4))));
    assert_eq!(pool.claim(ticket, ms(3)), Ok(Claim::Seed(seed)));
    assert_eq!(pool.until(seed), Ok(Some(ms(7))));
    assert_eq!(pool.past_hang(ms(6)), 0);
    assert_eq!(pool.past_hang(ms(7)), 1);
    assert_eq!(pool.deadlines(), (Some(ms(4)), None));
    assert_eq!(pool.until(seed), Ok(None), "followed by its life");

    let Ok(Admit::Wait { ticket, until }) = pool.create(ms(7)) else {
        panic!("a create with its only seed busy didn't wait");
    };
    assert_eq!(until, Some(ms(11)));
    assert_eq!(pool.claim(ticket, ms(11)), Ok(Claim::Wait { until: None }));
    assert_eq!(pool.deadlines(), (None, None));
    assert_eq!(pool.done(seed, ms(13)), Ok(Some(ticket)));
    assert_eq!(pool.claim(ticket, ms(13)), Ok(Claim::Seed(seed)));
    assert_eq!(pool.deadlines(), (Some(ms(6)), Some(ms(10))));
}

/// A failed start with no seed up or starting refuses the line at once, since no seed can come for it. Do: on a new
/// pool of one slot, fail its start with a create waiting unarmed in line. Expect: the create refused, its wait the
/// time it waited; the line empty.
#[test]
fn a_failed_start_with_no_seed_left_refuses_the_line() {
    let mut pool = setup(1, 4, None, None).pool();
    let Ok(Admit::Wait {
        ticket,
        until: None,
    }) = pool.create(ms(0))
    else {
        panic!("a create with no seed up didn't wait unarmed");
    };
    let failed = pool.start().unwrap();
    assert_eq!(pool.start_failed(failed, ms(3)), Ok(true));
    assert_eq!(
        pool.claim(ticket, ms(3)),
        Ok(Claim::Refused(Refused::Waited { wait: ms(3) }))
    );
    assert_eq!(pool.waiting(), 0);
}

/// AC-T7.4.1 (Karn's point; the audit of `Patience::ran_out`'s sites, 2026-11-11): a line refused because its only
/// seed failed to start is no answer and no run-out: no seed was slow, none came. Do: on an armed pool of one slot
/// (the line's wait 4 ms, the hang bound 8 ms), let a create wait in line, then fail the seed's start at 3 ms. Expect:
/// the create refused, its wait the 3 ms it waited; both waits as they were, the line's not following.
#[test]
fn a_failed_start_refusing_the_line_moves_no_wait() {
    let mut pool = seeds(1, 4, 4, 8);
    let Ok(Admit::Wait {
        ticket,
        until: Some(until),
    }) = pool.create(ms(0))
    else {
        panic!("a create with no seed up didn't wait armed");
    };
    assert_eq!(until, ms(4));
    let failed = pool.start().unwrap();
    assert_eq!(pool.start_failed(failed, ms(3)), Ok(true));
    assert_eq!(
        pool.claim(ticket, ms(3)),
        Ok(Claim::Refused(Refused::Waited { wait: ms(3) }))
    );
    assert_eq!(pool.deadlines(), (Some(ms(4)), Some(ms(8))));
}

/// Decision 9 (no record is read; AC-T7.1.3): before any answer of its own, a node's pool follows its seeds. Do: open
/// a pool with the node's seeds, serve a create for 1 ms, then one for 5 ms. Expect: both waits following (no bound)
/// before any answer, the busy seed's create unbounded by any clock; the hang bound then each answer, the slowest seen;
/// the line's wait, which no create waited in, still following.
#[test]
fn a_pools_waits_follow_their_seeds_until_an_answer_and_only_rise() {
    let mut pool = Seeds::new(setup(1, 4, None, None).limits);
    assert_eq!(pool.deadlines(), (None, None));
    let seed = up(&mut pool, ms(0));
    assert_eq!(pool.create(ms(0)), Ok(Admit::Seed(seed)));
    assert_eq!(pool.until(seed), Ok(None));
    assert_eq!(pool.done(seed, ms(1)), Ok(None));
    assert_eq!(pool.deadlines(), (None, Some(ms(1))));
    assert_eq!(pool.create(ms(1)), Ok(Admit::Seed(seed)));
    assert_eq!(pool.done(seed, ms(6)), Ok(None));
    assert_eq!(pool.deadlines(), (None, Some(ms(5))));
}
