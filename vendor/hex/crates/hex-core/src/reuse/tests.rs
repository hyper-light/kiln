//! The pool against an oracle (CLAUDE.md §6: a model-based test with non-vacuity counters; goal 5
//! extended, step 6): creates and deletes of boxes of random owners, device policies and privileges, with
//! checks failing at random, applied to the pool as the node's shell applies them and to a plain model of
//! what it must keep. After every step the bounds hold, an idle cgroup is handed only to a box of its own
//! owner under its own device policy and only once its check passed, an idle cgroup the box could have had
//! is never passed over for a fresh one, no slot is two boxes' or a box's and idle at once, and no slot is
//! made twice. Over a long run every path is taken: handed on, fresh, missed, each refusal and each
//! eviction.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::*;

/// The check's reasons, which a create's injected failure picks from.
const CHECKS: [Refused; 9] = [
    Refused::Populated,
    Refused::Descendants,
    Refused::Dying,
    Refused::SubtreeControl,
    Refused::NotDomain,
    Refused::Frozen,
    Refused::OomGroup,
    Refused::Charged,
    Refused::Unreadable,
];

#[derive(Debug, Clone, Copy)]
enum Op {
    /// A box of `owner` (none when `None`) under device policy `policy`; the first idle cgroup it's offered
    /// fails its check with `CHECKS[fail]` when `fail` is set.
    Create {
        owner: Option<u8>,
        reuse: bool,
        privileged: bool,
        policy: u8,
        fail: Option<usize>,
    },
    /// The `n`th live box (modulo how many live) is deleted, its cgroup left empty.
    Delete(usize),
    /// The node lets every idle cgroup go.
    Drain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cg {
    Slot(u64),
    Own,
}

#[derive(Debug)]
struct Live {
    owner: Option<u8>,
    policy: u8,
    cg: Cg,
}

/// What the pool must keep: each owner's idle slots with their policy and the order they went idle in.
#[derive(Debug, Default)]
struct Model {
    live: Vec<Live>,
    idle: BTreeMap<u8, Vec<(u64, u8, u64)>>,
    seq: u64,
    made: BTreeSet<u64>,
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

/// Applies `ops` to a pool bounded `per_owner` and `per_node` and to the model, checking both agree and
/// every property holds after each op; returns the pool's counts.
fn run(ops: &[Op], per_owner: usize, per_node: usize) -> Counts {
    let mut pool = Pool::<u8>::new(per_owner, per_node, 0);
    let mut m = Model {
        ghosts: Some(crate::demand::Ghosts::new(per_owner, per_node)),
        ..Model::default()
    };
    for op in ops {
        match *op {
            Op::Create {
                owner,
                reuse,
                privileged,
                policy,
                fail,
            } => {
                let name = owner.map(owner_name);
                let whose = Whose {
                    owner: name.as_deref(),
                    reuse,
                    privileged,
                };
                let mut fail = fail;
                let mut takes = 0;
                // The shell's loop: each refused idle cgroup is removed and the box takes again.
                let cg = loop {
                    takes += 1;
                    assert!(takes <= per_node + 2, "a create took {takes} times");
                    match pool.take(whose, &policy) {
                        Take::Idle(n) => {
                            let o = owner.expect("an idle cgroup handed to a box with no owner");
                            assert!(reuse && !privileged, "slot {n} offered to {whose:?}");
                            let list = m
                                .idle
                                .get_mut(&o)
                                .expect("handed an owner's cgroup it lacks");
                            let at = list
                                .iter()
                                .rposition(|(_, p, _)| *p == policy)
                                .expect("handed an idle cgroup under another device policy");
                            assert_eq!(
                                list[at].0, n,
                                "not the owner's idle cgroup that went idle last"
                            );
                            list.remove(at);
                            if list.is_empty() {
                                m.idle.remove(&o);
                            }
                            if let Some(i) = fail.take() {
                                pool.refuse(CHECKS[i % CHECKS.len()]);
                                continue;
                            }
                            pool.hand_on();
                            break Cg::Slot(n);
                        }
                        Take::Fresh(n) => {
                            let o = owner.expect("a fresh slot for a box with no owner");
                            if !m.idle.contains_key(&o) {
                                m.ghosts.as_mut().unwrap().missed(&owner_name(o));
                            }
                            assert!(reuse && !privileged, "slot {n} made for {whose:?}");
                            let passed_over = m
                                .idle
                                .get(&o)
                                .is_some_and(|l| l.iter().any(|(_, p, _)| *p == policy));
                            assert!(
                                !passed_over,
                                "a fresh slot made while the owner had one idle"
                            );
                            assert!(m.made.insert(n), "slot {n} made twice");
                            break Cg::Slot(n);
                        }
                        Take::Own(why) => {
                            let want = match (owner, reuse, privileged) {
                                (None, _, _) => Refused::NoOwner,
                                (Some(_), false, _) => Refused::OptedOut,
                                (Some(_), true, true) => Refused::Privileged,
                                (Some(_), true, false) => {
                                    panic!("own cgroup for a box that may reuse")
                                }
                            };
                            assert_eq!(why, want);
                            break Cg::Own;
                        }
                    }
                };
                m.live.push(Live { owner, policy, cg });
            }
            Op::Delete(i) => {
                if m.live.is_empty() {
                    continue;
                }
                let at = i % m.live.len();
                let gone = m.live.remove(at);
                let Cg::Slot(n) = gone.cg else { continue };
                let o = gone.owner.expect("a slot of a box with no owner");
                let got = pool.give(&owner_name(o), n, gone.policy);
                m.seq += 1;
                m.idle.entry(o).or_default().push((n, gone.policy, m.seq));
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
                        .map(|(o, l)| (*o, l[0].2))
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
            Op::Drain => {
                let mut got = pool.drain();
                got.sort_unstable();
                let mut want: Vec<u64> = m.idle.values().flatten().map(|(n, _, _)| *n).collect();
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
        let mut slots = BTreeSet::new();
        for b in &m.live {
            if let Cg::Slot(n) = b.cg {
                assert!(slots.insert(n), "slot {n} is two live boxes'");
            }
        }
        for (n, _, _) in m.idle.values().flatten() {
            assert!(slots.insert(*n), "slot {n} is a live box's and idle");
        }
    }
    pool.counts()
}

fn any_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        5 => (
            proptest::option::weighted(0.9, 0u8..4),
            proptest::bool::weighted(0.9),
            proptest::bool::weighted(0.1),
            0u8..3,
            proptest::option::weighted(0.2, 0usize..CHECKS.len()),
        )
            .prop_map(|(owner, reuse, privileged, policy, fail)| Op::Create {
                owner,
                reuse,
                privileged,
                policy,
                fail,
            }),
        4 => any::<usize>().prop_map(Op::Delete),
        1 => Just(Op::Drain),
    ]
}

proptest! {
    #[test]
    fn the_pool_keeps_its_bounds_and_hands_a_cgroup_only_to_its_owner_when_clean(
        ops in proptest::collection::vec(any_op(), 0..300),
        per_owner in 0usize..4,
        per_node in 0usize..7,
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
                owner: (rng.below(10) != 0).then(|| u8::try_from(rng.below(5)).unwrap()),
                reuse: rng.below(10) != 0,
                privileged: rng.below(10) == 0,
                policy: u8::try_from(rng.below(3)).unwrap(),
                fail: (rng.below(5) == 0).then(|| usize::try_from(rng.below(9)).unwrap()),
            },
            5..=8 => Op::Delete(usize::try_from(rng.below(1 << 16)).unwrap()),
            _ => Op::Drain,
        };
        ops.push(op);
    }
    let counts = run(&ops, 3, 8);
    assert!(counts.handed_on > 0, "{counts:?}");
    assert!(counts.fresh > 0, "{counts:?}");
    assert!(counts.missed > 0, "{counts:?}");
    for reason in Refused::ALL {
        if reason == Refused::Slots {
            continue;
        }
        assert!(
            counts.refused(reason) > 0,
            "{} never counted: {counts:?}",
            reason.name()
        );
    }
    assert!(counts.evicted_owner_bound > 0, "{counts:?}");
    assert!(counts.evicted_node_bound > 0, "{counts:?}");
    assert!(counts.evicted_refused > 0, "{counts:?}");
    assert!(counts.drained > 0, "{counts:?}");
}

/// Ada's rule, 2026-11-06 (AC-T7.1.3): an owner's burst that its bound cut one round is kept whole the next. Do:
/// four boxes of one owner made and deleted at a bound of 2, then four again. Expect: two put out the first round,
/// the owner's two misses raise its bound to 4, and the third round is handed all four idle, none made fresh.
#[test]
fn a_burst_the_owner_bound_cut_is_kept_whole_the_next_round() {
    let mut pool = Pool::<u8>::new(2, 100, 0);
    let whose = Whose {
        owner: Some("o"),
        reuse: true,
        privileged: false,
    };
    let round = |pool: &mut Pool<u8>| {
        let slots: Vec<u64> = (0..4)
            .map(|_| match pool.take(whose, &0) {
                Take::Idle(n) => {
                    pool.hand_on();
                    n
                }
                Take::Fresh(n) => n,
                Take::Own(why) => panic!("{why:?}"),
            })
            .collect();
        slots.into_iter().flat_map(|n| pool.give("o", n, 0)).count()
    };
    assert_eq!(round(&mut pool), 2, "the first round's two past the bound");
    assert_eq!(round(&mut pool), 0, "two misses raised the bound to 4");
    assert_eq!(pool.counts().per_owner, 4);
    let missed = pool.counts().missed;
    let fresh = pool.counts().fresh;
    assert_eq!(round(&mut pool), 0);
    assert_eq!(pool.counts().missed, missed, "no cold take once learned");
    assert_eq!(pool.counts().fresh, fresh);
}

/// A node that has numbered every slot a `u64` holds makes a box a cgroup of its own, by type.
#[test]
fn the_last_slot_number_is_never_wrapped() {
    let mut pool = Pool::<u8>::new(1, 1, u64::MAX);
    let whose = Whose {
        owner: Some("o"),
        reuse: true,
        privileged: false,
    };
    assert_eq!(pool.take(whose, &0), Take::Own(Refused::Slots));
    assert_eq!(pool.counts().refused(Refused::Slots), 1);
}

#[test]
fn a_slots_name_is_its_number_after_a_dot_and_nothing_else_is_a_slot() {
    assert_eq!(name(0), ".0");
    assert_eq!(slot_of(".0"), Some(0));
    assert_eq!(slot_of(&name(u64::MAX)), Some(u64::MAX));
    for not in [
        ".oci",
        ".",
        "",
        "7",
        ".07",
        ".1a",
        "..1",
        ".-1",
        ".18446744073709551616",
    ] {
        assert_eq!(slot_of(not), None, "{not:?}");
    }
    assert!(
        hex_api::BoxId::new(&name(3)).is_err(),
        "a slot's name is a box id"
    );
}

fn clean() -> Files {
    Files {
        events: "populated 0\nfrozen 0\n".into(),
        stat: "nr_descendants 0\nnr_subsys_cpu 0\nnr_dying_descendants 0\n".into(),
        subtree_control: "\n".into(),
        kind: "domain\n".into(),
        freeze: "0\n".into(),
        oom_group: Some("0\n".into()),
        memory_current: Some("544768\n".into()),
    }
}

/// Decision 8, "The p99.9 that stayed" (AC-T7.1.3): a slot made ahead takes the next fresh number, counted
/// fresh, and is never one a box was given or will be.
#[test]
fn a_slot_made_ahead_takes_the_next_fresh_number() {
    let mut pool: Pool<u8> = Pool::new(4, 8, 7);
    let whose = Whose {
        owner: Some("o"),
        reuse: true,
        privileged: false,
    };
    assert_eq!(pool.ahead(), Some(7));
    assert_eq!(pool.take(whose, &1), Take::Fresh(8));
    assert_eq!(pool.ahead(), Some(9));
    assert_eq!(pool.counts().fresh, 3);
}

/// Decision 8, "The p99.9 that stayed" (AC-T7.1.3): an idle cgroup an init was parked in is handed on with that
/// one process in it and its charge set apart, and refused when it holds none (the init is gone) or when what is
/// charged past the init's own goes over the threshold; every other guard holds as for an empty one. A cgroup
/// with an init in it is refused by the plain check, so one is never handed on unseen.
#[test]
fn a_parked_inits_cgroup_is_checked_with_the_init_and_its_charge_set_apart() {
    let parked = Files {
        events: "populated 1\nfrozen 0\n".into(),
        memory_current: Some(format!("{}\n", 544_768 + (3 << 20))),
        ..clean()
    };
    assert_eq!(
        check_parked(&parked, Some(64 << 20), 1 << 20, 3 << 20),
        Ok(())
    );
    assert_eq!(
        check(&parked, Some(64 << 20), 1 << 20),
        Err(Refused::Populated)
    );
    assert_eq!(
        check_parked(&clean(), Some(64 << 20), 1 << 20, 3 << 20),
        Err(Refused::Populated),
        "an idle cgroup whose parked init is gone"
    );
    assert_eq!(
        check_parked(&parked, Some(64 << 20), 1 << 20, 1 << 20),
        Err(Refused::Charged),
        "more charged than the init's own and the threshold"
    );
    let frozen = Files {
        freeze: "1\n".into(),
        ..parked.clone()
    };
    assert_eq!(
        check_parked(&frozen, Some(64 << 20), 1 << 20, 3 << 20),
        Err(Refused::Frozen)
    );
}

/// Each guard: a file that says the last box left something refuses the cgroup by that file's reason
/// (research f27-f30, f61, f81); a cgroup whose files read clean is handed on.
#[test]
fn each_file_that_says_something_was_left_refuses_by_its_reason() {
    assert_eq!(check(&clean(), Some(256 << 20), 1 << 20), Ok(()));
    assert_eq!(check(&clean(), None, 1 << 20), Ok(()));
    type Case = (fn(&mut Files), Refused);
    let cases: [Case; 13] = [
        (
            |f| f.events = "populated 1\nfrozen 0\n".into(),
            Refused::Populated,
        ),
        (
            |f| f.events = "populated 0\nfrozen 1\n".into(),
            Refused::Frozen,
        ),
        (
            |f| f.stat = "nr_descendants 1\nnr_dying_descendants 0\n".into(),
            Refused::Descendants,
        ),
        (
            |f| f.stat = "nr_descendants 0\nnr_dying_descendants 2\n".into(),
            Refused::Dying,
        ),
        (
            |f| f.subtree_control = "memory pids\n".into(),
            Refused::SubtreeControl,
        ),
        (|f| f.kind = "domain threaded\n".into(), Refused::NotDomain),
        (|f| f.kind = "threaded\n".into(), Refused::NotDomain),
        (|f| f.freeze = "1\n".into(), Refused::Frozen),
        (|f| f.oom_group = Some("1\n".into()), Refused::OomGroup),
        (
            |f| f.memory_current = Some(format!("{}\n", 64 << 20)),
            Refused::Charged,
        ),
        (|f| f.events = "frozen 0\n".into(), Refused::Unreadable),
        (
            |f| f.memory_current = Some("lots\n".into()),
            Refused::Unreadable,
        ),
        (|f| f.freeze = "2\n".into(), Refused::Unreadable),
    ];
    for (change, want) in cases {
        let mut f = clean();
        change(&mut f);
        assert_eq!(check(&f, Some(256 << 20), 1 << 20), Err(want), "{f:?}");
    }
    // The threshold must sit under the next box's bound: a box bounded at or under it gets a fresh one.
    assert_eq!(
        check(&clean(), Some(1 << 20), 1 << 20),
        Err(Refused::Charged)
    );
    // No memory controller handed down: nothing is charged, so nothing is checked.
    let mut f = clean();
    f.oom_group = None;
    f.memory_current = None;
    assert_eq!(check(&f, Some(1 << 20), 1 << 20), Ok(()));
}

proptest! {
    // Hostile input (CLAUDE.md §6): whatever the files hold, the check is a decision, never a panic, and
    // a cgroup is handed on only when every file reads exactly as a clean one's does.
    #[test]
    fn any_files_are_a_decision(
        events in "\\PC*", stat in "\\PC*", sub in "\\PC*", kind in "\\PC*", freeze in "\\PC*",
        group in proptest::option::of("\\PC*"), current in proptest::option::of("\\PC*"),
        max in proptest::option::of(any::<u64>()), threshold in any::<u64>(),
    ) {
        let f = Files { events, stat, subtree_control: sub, kind, freeze, oom_group: group, memory_current: current };
        if check(&f, max, threshold).is_ok() {
            prop_assert_eq!(flat(&f.events, "populated"), Some(0));
            prop_assert_eq!(flat(&f.stat, "nr_descendants"), Some(0));
            prop_assert_eq!(f.kind.trim(), "domain");
            prop_assert!(f.subtree_control.trim().is_empty());
        }
    }
}
