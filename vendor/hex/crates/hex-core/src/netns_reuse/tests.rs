//! The idle namespaces against an oracle (CLAUDE.md §6: a model-based test with non-vacuity counters; goal
//! 5 extended, step 6, Ada's call of 2026-11-03): creates and deletes of boxes of random owners and
//! eligibility, with the check of a deleted box's namespace failing at random, applied to the pool as the
//! node's shell applies them and to a plain model of what it must keep. After every step the bounds hold, an
//! idle namespace is handed only to a box of its own owner, eligible, and only if its check passed, the
//! owner's oldest first, a fresh one is never made while the box's owner has one idle, and no namespace is two
//! boxes' or a box's and idle at once. Over a long run every path is taken: handed on, fresh, each refusal and
//! each eviction.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::*;

/// The check's reasons, which a delete's injected failure picks from.
const CHECKS: [Refused; 8] = [
    Refused::Sockets,
    Refused::TimeWait,
    Refused::Links,
    Refused::Addresses,
    Refused::Routes,
    Refused::Rules,
    Refused::Firewall,
    Refused::Unreadable,
];

/// What a box's spec says of it, and the refusal it is counted under when it may not share.
const INELIGIBLE: [Refused; 3] = [
    Refused::NetAdmin,
    Refused::ProcSysWritable,
    Refused::OwnUserNamespace,
];

#[derive(Debug, Clone, Copy)]
enum Op {
    /// A box of `owner` (none when `None`), reuse on unless `opted_out`, its spec eligible unless
    /// `ineligible` names why not.
    Create {
        owner: Option<u8>,
        opted_out: bool,
        ineligible: Option<usize>,
    },
    /// The `n`th live box (modulo how many live) is deleted; the check of its namespace, if kept, fails
    /// with `CHECKS[fail]` when `fail` is set, and the shell closes it.
    Delete { n: usize, fail: Option<usize> },
    /// The node lets every idle namespace go.
    Drain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ns {
    /// One the node keeps for the box's owner when the box is deleted.
    Kept(u64),
    /// One of its own, never kept.
    Own,
}

#[derive(Debug)]
struct Live {
    owner: Option<u8>,
    ns: Ns,
}

/// What the pool must keep: each owner's idle namespaces in the order they went idle, with when.
#[derive(Debug, Default)]
struct Model {
    live: Vec<Live>,
    idle: BTreeMap<u8, Vec<(u64, u64)>>,
    seq: u64,
    next: u64,
    /// The per-owner bound, learned as the pool must learn it (Ada's rule, 2026-11-06).
    ghosts: Option<crate::demand::Ghosts>,
}

impl Model {
    fn total(&self) -> usize {
        self.idle.values().map(Vec::len).sum()
    }
}

fn owner_name(o: u8) -> String {
    format!("o{o}")
}

/// Applies `ops` to a pool bounded `per_owner` and `per_node` and to the model, checking both agree and every
/// property holds after each op; returns the pool's counts.
fn run(ops: &[Op], per_owner: usize, per_node: usize) -> Counts {
    let mut pool = Idle::<u64>::new(per_owner, per_node);
    let mut m = Model {
        ghosts: Some(crate::demand::Ghosts::new(per_owner, per_node)),
        ..Model::default()
    };
    for op in ops {
        match *op {
            Op::Create {
                owner,
                opted_out,
                ineligible,
            } => {
                let name = owner.map(owner_name);
                let eligible = match ineligible {
                    Some(i) => Err(INELIGIBLE[i % INELIGIBLE.len()]),
                    None => Ok(()),
                };
                let whose = Whose {
                    owner: name.as_deref(),
                    reuse: !opted_out,
                    eligible,
                };
                // The shell's take: an idle namespace was checked as its box went, so it's handed on.
                let ns = match pool.take(whose) {
                    Take::Idle(n) => {
                        let o = owner.expect("an idle namespace handed to a box with no owner");
                        assert!(!opted_out && eligible.is_ok(), "{n} offered to {whose:?}");
                        let list = m
                            .idle
                            .get_mut(&o)
                            .expect("handed an owner's namespace it lacks");
                        assert_eq!(list[0].0, n, "not the owner's oldest idle namespace");
                        list.remove(0);
                        if list.is_empty() {
                            m.idle.remove(&o);
                        }
                        pool.hand_on();
                        Ns::Kept(n)
                    }
                    Take::Fresh => {
                        let o = owner.expect("a kept namespace for a box with no owner");
                        assert!(!opted_out && eligible.is_ok(), "fresh for {whose:?}");
                        assert!(
                            !m.idle.contains_key(&o),
                            "a fresh namespace made while the owner had one idle"
                        );
                        m.ghosts.as_mut().unwrap().missed(&owner_name(o));
                        m.next += 1;
                        Ns::Kept(m.next << 8)
                    }
                    Take::Own(why) => {
                        let want = match (owner, opted_out, eligible) {
                            (None, _, _) => Refused::NoOwner,
                            (Some(_), true, _) => Refused::OptedOut,
                            (Some(_), false, Err(why)) => why,
                            (Some(_), false, Ok(())) => {
                                panic!("a namespace of its own for a box that may share")
                            }
                        };
                        assert_eq!(why, want);
                        Ns::Own
                    }
                };
                m.live.push(Live { owner, ns });
            }
            Op::Delete { n: i, fail } => {
                if m.live.is_empty() {
                    continue;
                }
                let at = i % m.live.len();
                let gone = m.live.remove(at);
                let Ns::Kept(n) = gone.ns else { continue };
                let o = gone.owner.expect("a kept namespace of a box with no owner");
                // The shell's check of the namespace given back: refused, it's closed, never idle.
                if let Some(f) = fail {
                    pool.refuse(CHECKS[f % CHECKS.len()]);
                } else {
                    let got = pool.give(&owner_name(o), n);
                    m.seq += 1;
                    m.idle.entry(o).or_default().push((n, m.seq));
                    let mut want = Vec::new();
                    let per_owner = m.ghosts.as_ref().unwrap().bound();
                    if let Some(list) = m.idle.get_mut(&o)
                        && list.len() > per_owner
                    {
                        m.ghosts.as_mut().unwrap().evicted(&owner_name(o));
                        want.push(list.remove(0).0);
                        if list.is_empty() {
                            m.idle.remove(&o);
                        }
                    }
                    while m.total() > per_node {
                        let (oldest, _) = m
                            .idle
                            .iter()
                            .map(|(o, l)| (*o, l[0].1))
                            .min_by_key(|(_, seq)| *seq)
                            .unwrap();
                        let list = m.idle.get_mut(&oldest).unwrap();
                        want.push(list.remove(0).0);
                        if list.is_empty() {
                            m.idle.remove(&oldest);
                        }
                    }
                    assert_eq!(got, want, "evicted by the bounds");
                }
            }
            Op::Drain => {
                let mut got = pool.drain();
                got.sort_unstable();
                let mut want: Vec<u64> = m.idle.values().flatten().map(|(n, _)| *n).collect();
                want.sort_unstable();
                assert_eq!(got, want);
                m.idle.clear();
            }
        }
        let per_owner_now = m.ghosts.as_ref().unwrap().bound();
        assert_eq!(pool.counts().per_owner, per_owner_now, "the learned bound");
        for (o, list) in &m.idle {
            assert!(
                list.len() <= per_owner_now,
                "owner {o} keeps {} idle",
                list.len()
            );
            assert_eq!(pool.idle_of(&owner_name(*o)), list.len());
        }
        assert!(m.total() <= per_node, "the node keeps {} idle", m.total());
        assert_eq!(pool.counts().idle, m.total());
        let mut seen = BTreeSet::new();
        for b in &m.live {
            if let Ns::Kept(n) = b.ns {
                assert!(seen.insert(n), "{n} is two live boxes'");
            }
        }
        for (n, _) in m.idle.values().flatten() {
            assert!(seen.insert(*n), "{n} is a live box's and idle");
        }
    }
    pool.counts()
}

fn any_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (
            proptest::option::weighted(0.9, 0u8..4),
            proptest::bool::weighted(0.1),
            proptest::option::weighted(0.15, 0usize..INELIGIBLE.len()),
        )
            .prop_map(|(owner, opted_out, ineligible)| Op::Create {
                owner,
                opted_out,
                ineligible,
            }),
        4 => (any::<usize>(), proptest::option::weighted(0.2, 0usize..CHECKS.len()))
            .prop_map(|(n, fail)| Op::Delete { n, fail }),
        1 => Just(Op::Drain),
    ]
}

proptest! {
    #[test]
    fn the_pool_keeps_its_bounds_and_hands_a_namespace_only_to_its_owner_when_clean(
        ops in proptest::collection::vec(any_op(), 0..300),
        per_owner in 1usize..5,
        per_node in 1usize..9,
    ) {
        run(&ops, per_owner, per_node);
    }
}

/// xorshift64*, so the long run below is the same every time.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Non-vacuity: a long run takes every path the pool has, each counted, so the property test above can't
/// pass by never reaching one.
#[test]
fn a_long_run_takes_every_path_and_counts_it() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut ops = Vec::new();
    for _ in 0..20_000 {
        let op = match rng.below(10) {
            0..=4 => Op::Create {
                owner: (rng.below(11) != 0).then(|| u8::try_from(rng.below(4)).unwrap()),
                opted_out: rng.below(10) == 0,
                ineligible: (rng.below(7) == 0).then(|| usize::try_from(rng.below(3)).unwrap()),
            },
            5..=8 => Op::Delete {
                n: usize::try_from(rng.below(1 << 16)).unwrap(),
                fail: (rng.below(5) == 0).then(|| usize::try_from(rng.below(8)).unwrap()),
            },
            _ => Op::Drain,
        };
        ops.push(op);
    }
    let c = run(&ops, 3, 7);
    assert!(c.handed_on > 0, "{c:?}");
    assert!(c.fresh > 0, "{c:?}");
    assert!(c.evicted_owner_bound > 0, "{c:?}");
    assert!(c.evicted_node_bound > 0, "{c:?}");
    assert!(c.drained > 0, "{c:?}");
    for reason in Refused::ALL {
        assert!(
            c.refused(reason) > 0,
            "{} never counted: {c:?}",
            reason.name()
        );
    }
}

/// A box of another owner gets a fresh namespace, counted Owner, and the idle one stays its owner's for
/// that owner's next box (Ada's call, 2026-11-03, as for cgroups).
#[test]
fn another_owners_box_is_counted_and_the_idle_one_kept() {
    let mut pool = Idle::<u64>::new(4, 8);
    assert!(pool.give("o", 7).is_empty());
    let p = Whose {
        owner: Some("p"),
        reuse: true,
        eligible: Ok(()),
    };
    assert_eq!(pool.take(p), Take::Fresh);
    assert_eq!(pool.counts().refused(Refused::Owner), 1);
    assert_eq!(pool.idle_of("o"), 1, "kept for o");
    let o = Whose {
        owner: Some("o"),
        ..p
    };
    assert_eq!(pool.take(o), Take::Idle(7));
}

/// Ada's rule, 2026-11-06 (AC-T7.1.3): an owner's burst that its bound cut one round is kept whole the next. Do:
/// four boxes of one owner taken and given back at a bound of 2, three rounds. Expect: two closed the first round,
/// the second round's two fresh takes raise the bound to 4, and the third takes four idle, none fresh.
#[test]
fn a_burst_the_owner_bound_cut_is_kept_whole_the_next_round() {
    let mut pool = Idle::<u64>::new(2, 100);
    let o = Whose {
        owner: Some("o"),
        reuse: true,
        eligible: Ok(()),
    };
    let mut next = 0;
    let mut round = |pool: &mut Idle<u64>| {
        let taken: Vec<u64> = (0..4)
            .map(|_| match pool.take(o) {
                Take::Idle(n) => {
                    pool.hand_on();
                    n
                }
                Take::Fresh => {
                    next += 1;
                    next
                }
                Take::Own(why) => panic!("{why:?}"),
            })
            .collect();
        taken.into_iter().flat_map(|n| pool.give("o", n)).count()
    };
    assert_eq!(
        round(&mut pool),
        2,
        "four fresh, two past the bound given back"
    );
    assert_eq!(round(&mut pool), 0, "two taken fresh raised the bound to 4");
    assert_eq!(pool.counts().per_owner, 4);
    let fresh_learned = pool.counts().fresh;
    assert_eq!(round(&mut pool), 0);
    assert_eq!(
        pool.counts().fresh,
        fresh_learned,
        "no cold take once learned"
    );
}

/// Every reason has a name of its own, and `ALL` lists each once.
#[test]
fn every_reason_is_named_once() {
    let names: BTreeSet<&str> = Refused::ALL.iter().map(|r| r.name()).collect();
    assert_eq!(names.len(), Refused::ALL.len());
}
