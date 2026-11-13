//! The pool's tests (decision 4, goal 4 plan step 3: HEX-NETNS-1 to 5). Against an oracle under random takes,
//! made and failed makes, creates the pool can't serve and shutdowns, the pool never holds more than its bound,
//! hands each namespace out once and the oldest first, answers each event as the oracle does and counts what it
//! counts, with every path taken over the run (non-vacuity counters); and each case the plan names, a dry take,
//! a make past the bound and a take after shutdown, is answered as the design says, as are a failed make and a
//! create the pool can't serve.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::path::Path;

use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};

use super::*;

/// What a shell does to the pool.
#[derive(Debug, Clone, Copy)]
enum Event {
    /// A create takes a namespace.
    Take,
    /// The refill thread hands in a namespace it made.
    Made,
    /// The refill thread's make failed at a step, with an errno.
    Failed(Step, i32),
    /// A create the pool can't serve makes its namespace on the path.
    OnPath,
    /// The node shuts the pool down.
    ShutDown,
}

/// The pool as plainly as it can be said: the namespaces it holds as a set, whether it is open and asking, and
/// what it has counted.
struct Oracle {
    bound: usize,
    held: BTreeSet<u64>,
    open: bool,
    asking: bool,
    counts: Counts,
}

impl Oracle {
    fn new(bound: usize) -> Self {
        Self {
            bound,
            held: BTreeSet::new(),
            open: true,
            asking: true,
            counts: Counts::default(),
        }
    }

    fn wanted(&self) -> usize {
        if self.open && self.asking {
            self.bound - self.held.len()
        } else {
            0
        }
    }
}

/// How many times each path was taken over a run: one taken by no case is one the run proves nothing of.
#[derive(Debug, Clone, Copy, Default)]
struct Paths {
    pooled: usize,
    dry: usize,
    full: usize,
    take_refused: usize,
    made_refused: usize,
    discarded: usize,
    stopped_asking: usize,
    asked_again: usize,
    on_path: usize,
    on_path_paused: usize,
    on_path_refused: usize,
}

/// Plays `events` on a pool of `bound` and on the oracle, and checks the pool against it after each.
fn play(bound: usize, events: &[Event], paths: &Cell<Paths>) -> Result<(), TestCaseError> {
    let mut pool = Pool::new(bound);
    let mut oracle = Oracle::new(bound);
    let mut seen = paths.get();
    // The test's shell gives each namespace it makes the next id, so the oldest held is the least; `gone` holds
    // every id that left the pool, taken or discarded, so one that comes out again is caught.
    let mut next = 0u64;
    let mut gone = BTreeSet::new();
    for event in events {
        match event {
            Event::Take => {
                let got = pool.take();
                if !oracle.open {
                    prop_assert_eq!(got, Err(ShutDown));
                    seen.take_refused += 1;
                } else {
                    if !oracle.asking {
                        seen.asked_again += 1;
                    }
                    oracle.asking = true;
                    match got {
                        Ok(Take::Pooled(id)) => {
                            prop_assert_eq!(
                                oracle.held.pop_first(),
                                Some(id),
                                "not the oldest held"
                            );
                            prop_assert!(gone.insert(id), "handed out {} again", id);
                            oracle.counts.taken += 1;
                            seen.pooled += 1;
                        }
                        Ok(Take::MakeOnPath) => {
                            prop_assert!(oracle.held.is_empty(), "dry holding {:?}", oracle.held);
                            oracle.counts.made_on_path += 1;
                            seen.dry += 1;
                        }
                        Err(ShutDown) => prop_assert!(false, "an open pool refused a take"),
                    }
                }
            }
            Event::Made => {
                let id = next;
                next += 1;
                let want = if !oracle.open {
                    seen.made_refused += 1;
                    Err(Refused::ShutDown { ns: id })
                } else if oracle.held.len() == bound {
                    seen.full += 1;
                    Err(Refused::Full { bound, ns: id })
                } else {
                    oracle.held.insert(id);
                    oracle.counts.made_ahead += 1;
                    Ok(())
                };
                prop_assert_eq!(pool.made(id), want);
            }
            Event::Failed(step, errno) => {
                if oracle.wanted() > 0 {
                    seen.stopped_asking += 1;
                }
                let reason = Reason::new(Some(*errno), &format!("make {errno}"));
                pool.failed(*step, reason.clone());
                oracle.counts.failed += 1;
                let by = &mut oracle.counts.failed_by;
                let last = &mut oracle.counts.last_failed;
                let (n, kept) = match step {
                    Step::Home => (&mut by.home, &mut last.home),
                    Step::Unshare => (&mut by.unshare, &mut last.unshare),
                    Step::Open => (&mut by.open, &mut last.open),
                    Step::Loopback => (&mut by.loopback, &mut last.loopback),
                    Step::Back => (&mut by.back, &mut last.back),
                };
                *n += 1;
                *kept = Some(reason);
                oracle.asking = false;
            }
            Event::OnPath => {
                let got = pool.on_path();
                if oracle.open {
                    prop_assert_eq!(got, Ok(()));
                    oracle.counts.made_on_path += 1;
                    seen.on_path += 1;
                    if !oracle.asking {
                        seen.on_path_paused += 1;
                    }
                } else {
                    prop_assert_eq!(got, Err(ShutDown));
                    seen.on_path_refused += 1;
                }
            }
            Event::ShutDown => {
                let back = pool.shut_down();
                let set: BTreeSet<u64> = back.iter().copied().collect();
                prop_assert_eq!(set.len(), back.len(), "handed back {:?}, some twice", back);
                prop_assert_eq!(&set, &oracle.held);
                for id in set {
                    prop_assert!(gone.insert(id), "discarded {}, which had left the pool", id);
                }
                if !oracle.held.is_empty() {
                    seen.discarded += 1;
                }
                oracle.counts.discarded += oracle.held.len() as u64;
                oracle.held.clear();
                oracle.open = false;
            }
        }
        prop_assert!(
            pool.held() <= bound,
            "{} held past a bound of {}",
            pool.held(),
            bound
        );
        prop_assert_eq!(pool.held(), oracle.held.len());
        prop_assert_eq!(pool.wanted(), oracle.wanted());
        prop_assert_eq!(pool.is_shut_down(), !oracle.open);
        let counts = pool.counts();
        prop_assert_eq!(&counts, &oracle.counts);
        prop_assert_eq!(
            counts.made_ahead,
            counts.taken + pool.held() as u64 + counts.discarded
        );
        let by = counts.failed_by;
        prop_assert_eq!(
            by.home + by.unshare + by.open + by.loopback + by.back,
            counts.failed
        );
    }
    paths.set(seen);
    Ok(())
}

/// Takes and makes alike, failures and creates the pool can't serve rarer, a shutdown rarer still, so most
/// histories run a while before one and go on after it.
fn event() -> impl Strategy<Value = Event> {
    prop_oneof![
        8 => Just(Event::Take),
        8 => Just(Event::Made),
        2 => (
            prop_oneof![
                Just(Step::Home),
                Just(Step::Unshare),
                Just(Step::Open),
                Just(Step::Loopback),
                Just(Step::Back),
            ],
            1i32..140
        )
            .prop_map(|(step, errno)| Event::Failed(step, errno)),
        2 => Just(Event::OnPath),
        1 => Just(Event::ShutDown),
    ]
}

/// The bench's concurrency, the size decision 4's pool had before it was learned and starts at on fedora.
const BENCH: usize = 32;

/// Small bounds, which histories fill and drain often, 0 (a pool that holds none), and the bench's width.
fn bound() -> impl Strategy<Value = usize> {
    prop_oneof![0usize..=6, Just(BENCH)]
}

/// HEX-NETNS-1 to 5, against the oracle: under random takes, made and failed makes, creates the pool can't
/// serve and shutdowns, the pool never holds more than its bound, never hands a namespace out twice, hands out
/// the oldest it holds, answers every event as the oracle does (its dry takes and refusals among them) and
/// counts what the oracle counts, and no event panics. Every path is taken over the run.
#[test]
fn against_an_oracle_the_pool_keeps_its_bound_hands_each_namespace_out_once_and_counts_true() {
    let paths = Cell::new(Paths::default());
    let mut runner = TestRunner::new(Config {
        cases: 1024,
        source_file: Some(file!()),
        ..Config::default()
    });
    let histories = (bound(), proptest::collection::vec(event(), 0..200));
    if let Err(e) = runner.run(&histories, |(bound, events)| play(bound, &events, &paths)) {
        panic!("{e}");
    }
    let p = paths.get();
    for (path, n) in [
        ("a take from the pool", p.pooled),
        ("a dry take", p.dry),
        ("a make past the bound", p.full),
        ("a take after shutdown", p.take_refused),
        ("a make after shutdown", p.made_refused),
        ("a shutdown that discards", p.discarded),
        ("a failed make that stops the asking", p.stopped_asking),
        ("a take that asks again", p.asked_again),
        ("a create the pool can't serve", p.on_path),
        ("one while the refill is paused", p.on_path_paused),
        ("one after shutdown", p.on_path_refused),
    ] {
        assert!(n > 0, "no history took {path}: {p:?}");
    }
}

/// HEX-NETNS-3: a take from an empty pool is answered with a namespace made on the path, and counted; one made
/// ahead is taken before the pool runs dry again.
#[test]
fn a_take_from_an_empty_pool_is_made_on_the_path_and_counted() {
    let mut pool = Pool::new(BENCH);
    assert_eq!(pool.take(), Ok(Take::MakeOnPath));
    assert_eq!(
        pool.counts(),
        Counts {
            made_on_path: 1,
            ..Counts::default()
        }
    );
    pool.made(7).unwrap();
    assert_eq!(pool.take(), Ok(Take::Pooled(7)));
    assert_eq!(pool.take(), Ok(Take::MakeOnPath));
    assert_eq!(
        pool.counts(),
        Counts {
            made_ahead: 1,
            taken: 1,
            made_on_path: 2,
            ..Counts::default()
        }
    );
}

/// HEX-NETNS-3: a create the pool can't serve (a container with a user namespace of its own) is counted as made
/// on the path, as a dry take is, and takes nothing: the pool holds what it held, and a refill paused by a failed
/// make stays paused. After shutdown it is refused by type, as a take is.
#[test]
fn a_create_the_pool_cant_serve_is_counted_on_the_path_and_takes_nothing() {
    let mut pool = Pool::new(2);
    pool.made(1).unwrap();
    pool.failed(Step::Unshare, Reason::new(Some(1), "unshare failed"));
    assert_eq!(pool.on_path(), Ok(()));
    assert_eq!(pool.held(), 1);
    assert_eq!(pool.wanted(), 0);
    assert_eq!(
        pool.counts(),
        Counts {
            made_ahead: 1,
            made_on_path: 1,
            failed: 1,
            failed_by: FailedBy {
                unshare: 1,
                ..FailedBy::default()
            },
            last_failed: LastFailed {
                unshare: Some(Reason::new(Some(1), "unshare failed")),
                ..LastFailed::default()
            },
            ..Counts::default()
        }
    );
    assert_eq!(pool.shut_down(), [1]);
    assert_eq!(pool.on_path(), Err(ShutDown));
    assert_eq!(pool.counts().made_on_path, 1);
}

/// HEX-NETNS-1: a namespace made past the bound is refused by type and handed back, uncounted, and the pool
/// holds its bound still. A pool of bound 0, the drained pool step 4's lab runs, asks for none, refuses every
/// make and is dry for every take.
#[test]
fn a_namespace_made_past_the_bound_is_refused_and_handed_back() {
    let mut pool = Pool::new(2);
    pool.made(1).unwrap();
    pool.made(2).unwrap();
    assert_eq!(pool.wanted(), 0);
    assert_eq!(pool.made(3), Err(Refused::Full { bound: 2, ns: 3 }));
    assert_eq!(pool.held(), 2);
    assert_eq!(pool.counts().made_ahead, 2);

    let mut none = Pool::new(0);
    assert_eq!(none.wanted(), 0);
    assert_eq!(none.made(1), Err(Refused::Full { bound: 0, ns: 1 }));
    assert_eq!(none.take(), Ok(Take::MakeOnPath));
    assert_eq!(none.wanted(), 0);
}

/// HEX-NETNS-4: after shutdown a take is refused by type, what the pool held is handed back and counted as
/// discarded, a make that was under way is handed back, and nothing more is asked for.
#[test]
fn a_take_after_shutdown_is_refused_and_what_the_pool_held_is_handed_back() {
    let mut pool = Pool::new(BENCH);
    pool.made(1).unwrap();
    pool.made(2).unwrap();
    let mut back = pool.shut_down();
    back.sort_unstable();
    assert_eq!(back, [1, 2]);
    assert_eq!(pool.take(), Err(ShutDown));
    assert_eq!(pool.made(3), Err(Refused::ShutDown { ns: 3 }));
    assert_eq!(pool.wanted(), 0);
    assert!(pool.is_shut_down());
    assert!(pool.shut_down().is_empty());
    assert_eq!(
        pool.counts(),
        Counts {
            made_ahead: 2,
            discarded: 2,
            ..Counts::default()
        }
    );
}

/// HEX-NETNS-5: a new pool asks for its whole bound, and for the room a take leaves; a failed make stops the
/// asking, with room left, until the next take, so a kernel refusing every make is asked once a take. A make
/// already under way still lands, and the oldest held goes first.
#[test]
fn a_failed_make_stops_the_asking_until_the_next_take() {
    let mut pool = Pool::new(3);
    assert_eq!(pool.wanted(), 3);
    pool.made(1).unwrap();
    assert_eq!(pool.wanted(), 2);
    pool.failed(Step::Unshare, Reason::new(Some(1), "unshare failed"));
    assert_eq!(pool.wanted(), 0);
    pool.made(2).unwrap();
    assert_eq!(pool.wanted(), 0);
    assert_eq!(pool.take(), Ok(Take::Pooled(1)));
    assert_eq!(pool.wanted(), 2);
    assert_eq!(pool.counts().failed, 1);
}

/// HEX-NETNS-12: each failed make is counted by the step that failed, the steps' counts summing to the failed
/// makes, so the reason the kernel gave is kept (CLAUDE.md §3). Do: fail a make at each step, the unshare twice.
/// Expect: each step counted once, the unshare twice, six failed in all.
#[test]
fn each_failed_make_is_counted_by_the_step_that_failed() {
    let mut pool = Pool::<u64>::new(2);
    for step in [
        Step::Home,
        Step::Unshare,
        Step::Unshare,
        Step::Open,
        Step::Loopback,
        Step::Back,
    ] {
        pool.failed(step, Reason::new(None, "failed"));
    }
    let counts = pool.counts();
    assert_eq!(counts.failed, 6);
    assert_eq!(
        counts.failed_by,
        FailedBy {
            home: 1,
            unshare: 2,
            open: 1,
            loopback: 1,
            back: 1,
        }
    );
}

/// HEX-NETNS-14: the latest reason a make failed for is kept at each step, errno and text, one a step, so the
/// OS error isn't dropped once counted (CLAUDE.md §3); a later failure at a step replaces its reason, and a
/// step that never failed keeps none. Do: fail the unshare with EPERM, then with ENOSPC, and the loopback with
/// no errno. Expect: the unshare's reason is ENOSPC's, the loopback's has no errno, the rest none.
#[test]
fn the_latest_reason_is_kept_at_each_step() {
    let mut pool = Pool::<u64>::new(2);
    pool.failed(Step::Unshare, Reason::new(Some(1), "unshare failed: EPERM"));
    pool.failed(
        Step::Unshare,
        Reason::new(Some(28), "unshare failed: ENOSPC"),
    );
    pool.failed(Step::Loopback, Reason::new(None, "not an ack"));
    let last = pool.counts().last_failed;
    assert_eq!(
        last,
        LastFailed {
            unshare: Some(Reason::new(Some(28), "unshare failed: ENOSPC")),
            loopback: Some(Reason::new(None, "not an ack")),
            ..LastFailed::default()
        }
    );
    assert_eq!(
        last.unshare.unwrap().to_string(),
        "errno 28: unshare failed: ENOSPC"
    );
    assert_eq!(last.loopback.unwrap().to_string(), "not an ack");
}

/// HEX-NETNS-14: a reason's text is cut to MAX_REASON bytes at a character boundary, the cut marked, so what a
/// step keeps is bounded whatever the error says. Do: a text of 1,000 two-byte characters, and one of exactly
/// the bound. Expect: the first within the bound, ending in the mark, a prefix of the text; the second whole.
#[test]
fn a_reasons_text_is_cut_to_its_bound() {
    let long = "é".repeat(1_000);
    let cut = Reason::new(Some(1), &long);
    assert!(cut.text().len() <= MAX_REASON, "{}", cut.text().len());
    assert!(cut.text().len() > MAX_REASON - 4);
    assert!(long.starts_with(cut.text().strip_suffix('…').unwrap()));
    assert_eq!(cut.errno(), Some(1));
    let exact = "x".repeat(MAX_REASON);
    assert_eq!(Reason::new(None, &exact).text(), exact);
}

/// HEX-NETNS-13: a pool refused for what the node lacks asks for nothing and holds nothing; every take is made
/// on the path, counted under the refusal, which the counts name; a create the pool can't serve is counted on
/// the path but not under it. Do: refuse a pool for CAP_NET_ADMIN, take twice, count one create on the path.
/// Expect: nothing wanted, a make refused, three made on the path, two of them refused ones, the refusal named.
#[test]
fn a_refused_pool_makes_every_take_on_the_path_counted_under_its_reason() {
    let missing = Missing::of(true, false).unwrap();
    let mut pool = Pool::refused(missing);
    assert_eq!(pool.wanted(), 0);
    assert_eq!(pool.made(1), Err(Refused::Full { bound: 0, ns: 1 }));
    assert_eq!(pool.take(), Ok(Take::MakeOnPath));
    assert_eq!(pool.take(), Ok(Take::MakeOnPath));
    assert_eq!(pool.on_path(), Ok(()));
    assert_eq!(pool.wanted(), 0);
    assert_eq!(
        pool.counts(),
        Counts {
            made_on_path: 3,
            refused: Some(missing),
            refused_on_path: 2,
            ..Counts::default()
        }
    );
    assert_eq!(pool.counts().refused.unwrap().to_string(), "CAP_NET_ADMIN");
}

/// HEX-NETNS-13: what a node lacks is what it doesn't hold of CAP_SYS_ADMIN and CAP_NET_ADMIN, named so; a node
/// that holds both lacks nothing.
#[test]
fn what_a_node_lacks_is_named() {
    assert_eq!(Missing::of(true, true), None);
    let named = |s, n| Missing::of(s, n).unwrap().to_string();
    assert_eq!(named(false, true), "CAP_SYS_ADMIN");
    assert_eq!(named(true, false), "CAP_NET_ADMIN");
    assert_eq!(named(false, false), "CAP_SYS_ADMIN and CAP_NET_ADMIN");
    let both = Missing::of(false, false).unwrap();
    assert!(both.sys_admin() && both.net_admin());
}

/// Doc-truth (Ada's rule, 2026-11-06): decision 4 says the pool's size is learned from its draws, as the node's
/// default is (`Pool::learning`), and names no picked bound.
#[test]
fn decision_4_says_the_pools_size_is_learned() {
    let design =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/runtime-architecture.md");
    let design = std::fs::read_to_string(design).unwrap();
    assert!(design.contains("The pool's size is **learned from its draws**"));
    assert!(!design.contains("The pool holds at most **32**"));
}

/// Ada's rule, 2026-11-06 (AC-T7.1.3; goal 5 extended step 4's IPC_UTS_BOUND, a picked 32): a pool made ahead learns
/// its size from its draws. Do: a pool starting at 2 of at most 10, filled, then five takes with no make between.
/// Expect: two pooled and three made on the path, the size raised to the five drawn plus a take for each of the
/// three short, 8; filled again, eight takes are all pooled, none made on the path. The size never falls, and stops
/// at its bound.
#[test]
fn a_pool_made_ahead_learns_the_deepest_draw_and_never_runs_short_at_it_again() {
    let mut pool = Pool::learning(2, 10);
    assert_eq!(pool.wanted(), 2);
    let mut n = 0;
    let mut fill = |pool: &mut Pool<u32>| {
        while pool.wanted() > 0 {
            n += 1;
            pool.made(n).unwrap();
        }
    };
    fill(&mut pool);
    let dry = (0..5)
        .filter(|_| pool.take() == Ok(Take::MakeOnPath))
        .count();
    assert_eq!(dry, 3);
    assert_eq!(pool.bound(), 8);
    fill(&mut pool);
    assert_eq!(pool.held(), 8);
    for _ in 0..8 {
        assert!(matches!(pool.take(), Ok(Take::Pooled(_))));
    }
    assert_eq!(pool.counts().made_on_path, 3, "no cold take once learned");
    fill(&mut pool);
    for _ in 0..30 {
        let _ = pool.take();
    }
    assert_eq!(pool.bound(), 10, "held at its bound");
}
