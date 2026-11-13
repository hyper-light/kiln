//! After the node agent restarts: which boxes to take back and which to remove (T-T7.1.2). A box is
//! taken back only when its record says it was made and the machine still holds it; everything else
//! the agent finds of a box (a record mid-create or mid-delete, a record it can't read, a cgroup,
//! container state or bundle that no record names, a container whose init is gone) is removed, so
//! nothing is leaked. A box's record is written once it's whole, and its parts lie at paths its id
//! fixes, so the agent finds a box caught mid-create by its parts alone ([`Parts`], goal 5 extended,
//! step 5).

use std::collections::{BTreeMap, BTreeSet};

use hex_api::{BoxId, Class};

use crate::record::{BoxRecord, Phase};

/// The parts of a box the node agent finds at the paths its id fixes: its cgroup under a box root,
/// its container's state directory under the node's runtime directory, and the bundle the node
/// rendered for it under its bundles directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Parts {
    pub cgroup: bool,
    pub state_dir: bool,
    pub bundle: bool,
}

/// What the node agent found of one box.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Seen {
    /// Its record: `Err` holds why the file there couldn't be read as one.
    pub record: Option<Result<BoxRecord, String>>,
    /// Its parts at the paths its id fixes.
    pub parts: Parts,
    /// For a record with an init, or a run's program: whether that very process still runs.
    pub init_alive: Option<bool>,
    /// For an `oci` box: a start of it had begun and never took the byte when the agent stopped (its
    /// state directory's start mark beside its exec FIFO, hex_runtime's `state::start_interrupted`).
    pub start_interrupted: bool,
    /// Its record was written in an earlier boot of the host than this one ([`crate::record::BootSession`]),
    /// whose end ended every process of it.
    pub earlier_boot: bool,
}

/// What to do with a box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Take it back as it is.
    Adopt,
    /// End what runs in it and remove it, with what's left of it.
    Remove(Why),
}

/// Why a box is removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Its record says it was being made: a node agent from before goal 5 extended step 5, which
    /// marked a box creating before making it, stopped while it did.
    MidCreate,
    /// The agent stopped while removing it.
    MidDelete,
    /// Its container's init has ended: the container is gone.
    InitGone,
    /// The program it was run for has ended, while no agent was there to remove it (decision 7).
    RunEnded,
    /// Its record exists but its cgroup doesn't: something removed it (its unit was stopped).
    NoCgroup,
    /// No record names it, and these parts of it are left: the agent stopped while making it, before
    /// its record was written.
    NoRecord(Parts),
    /// Its record can't be read, for this reason.
    Unreadable(String),
    /// The agent stopped while it started the box, as the box's startContainer hooks ran: the start never
    /// took, and its init ends when it finds the start gone. Removed whether the init has ended yet or not.
    StartInterrupted,
    /// Its record was written in an earlier boot of the host: that boot's end ended everything that ran of it,
    /// so nothing is left to end, and what is left of it on disk goes (T-T7.1.2).
    EarlierBoot,
}

/// The decision for one box.
pub fn decide(seen: &Seen) -> Decision {
    match &seen.record {
        None => Decision::Remove(Why::NoRecord(seen.parts)),
        Some(Err(why)) => Decision::Remove(Why::Unreadable(why.clone())),
        Some(Ok(_)) if seen.earlier_boot => Decision::Remove(Why::EarlierBoot),
        Some(Ok(r)) => match r.phase {
            Phase::Creating => Decision::Remove(Why::MidCreate),
            Phase::Deleting => Decision::Remove(Why::MidDelete),
            Phase::Ready
                if matches!(r.spec.class.class(), Class::Process | Class::Oci)
                    && !seen.parts.cgroup =>
            {
                Decision::Remove(Why::NoCgroup)
            }
            Phase::Ready if r.spec.class.class() != Class::Process && seen.start_interrupted => {
                Decision::Remove(Why::StartInterrupted)
            }
            Phase::Ready
                if r.spec.class.class() != Class::Process && seen.init_alive != Some(true) =>
            {
                Decision::Remove(Why::InitGone)
            }
            Phase::Ready if r.run.is_some() && seen.init_alive != Some(true) => {
                Decision::Remove(Why::RunEnded)
            }
            Phase::Ready => Decision::Adopt,
        },
    }
}

/// Decisions for every box found, in id order.
pub fn plan(seen: &BTreeMap<BoxId, Seen>) -> Vec<(BoxId, Decision)> {
    seen.iter().map(|(id, s)| (id.clone(), decide(s))).collect()
}

/// The slots found (`slots`, each a cgroup [`crate::reuse::name`] names) that no readable record names:
/// idle when the agent stopped, or a box's caught mid-create before its record was written (goal 5
/// extended, step 6). Each is removed, whatever runs in it ended first: no idle cgroup outlives the agent
/// that kept it, and a slot a record names goes with its box's own decision.
pub fn strays(seen: &BTreeMap<BoxId, Seen>, slots: &BTreeSet<u64>) -> Vec<u64> {
    let named: BTreeSet<u64> = seen
        .values()
        .filter_map(|s| match &s.record {
            Some(Ok(r)) => r.slot.map(|slot| slot.n),
            _ => None,
        })
        .collect();
    slots.difference(&named).copied().collect()
}

#[cfg(test)]
mod strays_tests {
    use std::collections::{BTreeMap, BTreeSet};

    use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec};

    use super::*;
    use crate::cgroup::Baseline;
    use crate::record::{BoxRecord, Slot};

    /// Goal 5 extended, step 6 (T-T7.1.2): after a crash, a slot no readable record names (idle, or a
    /// create's before its record) is removed; one a record names goes with that box, adopted or removed.
    #[test]
    fn a_slot_no_record_names_is_removed_and_a_named_one_goes_with_its_box() {
        let id = |s: &str| BoxId::new(s).unwrap();
        let mut seen = BTreeMap::new();
        let mut held = BoxRecord::new(
            BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id("b1"),
                bounds: Bounds::default(),
                class: ClassSpec::Process,
                owner: None,
            },
            Phase::Ready,
            Vec::new(),
        );
        held.slot = Some(Slot {
            n: 4,
            baseline: Baseline::default(),
        });
        seen.insert(
            id("b1"),
            Seen {
                record: Some(Ok(held)),
                ..Seen::default()
            },
        );
        seen.insert(
            id("torn"),
            Seen {
                record: Some(Err("EOF".to_owned())),
                ..Seen::default()
            },
        );
        let found: BTreeSet<u64> = [2, 4, 9].into_iter().collect();
        assert_eq!(strays(&seen, &found), vec![2, 9]);
        assert_eq!(
            strays(&BTreeMap::new(), &BTreeSet::new()),
            Vec::<u64>::new()
        );
    }
}

#[cfg(test)]
mod tests {
    //! A model of the machine (records, cgroups, containers' state directories, rendered bundles,
    //! inits) with the node agent's steps applied to it, crashed after every step of every generated
    //! history, then recovered as the agent recovers: from the records and from the parts it finds at
    //! the paths each id fixes. Crash injection at every write (CLAUDE.md §6). After recovery every box
    //! left is whole and taken back, every box that was whole and not being deleted survives, and
    //! nothing else is left (T-T7.1.2).

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use hex_api::{Bounds, BoxSpec, ClassSpec, Network, OciSpec, Program, Rootfs};
    use proptest::prelude::*;

    use super::*;
    use crate::record::{DELETE_STEPS, ProcId, REMOVE_STEPS, RUN_STEPS, Step, create_steps};

    #[derive(Debug, Clone, Default)]
    struct Machine {
        records: BTreeMap<BoxId, BoxRecord>,
        cgroups: BTreeSet<BoxId>,
        state_dirs: BTreeSet<BoxId>,
        bundles: BTreeSet<BoxId>,
        inits: BTreeSet<BoxId>,
        steps: usize,
    }

    /// What a box is made from, which decides its create's steps.
    #[derive(Debug, Clone, Copy)]
    enum Kind {
        Process,
        /// An `oci` box run from the caller's bundle.
        Bundle,
        /// An `oci` box run from a bundle the node renders from its `OciSpec`.
        Rendered,
        /// A `process` box made by a run, for its program's life (decision 7).
        Run,
        Firecracker,
        Shards,
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Create(u8, Kind),
        Delete(u8),
        /// A container's init exits on its own (the box stays until deleted), or a run's program
        /// ends while the agent is down (it removes the box itself while up).
        InitExits(u8),
    }

    fn id(n: u8) -> BoxId {
        BoxId::new(&format!("b{n}")).unwrap()
    }

    fn spec(id: &BoxId, kind: Kind) -> BoxSpec {
        let class = match kind {
            Kind::Process | Kind::Run => ClassSpec::Process,
            Kind::Bundle => ClassSpec::Bundle(PathBuf::from("/srv/bundle")),
            Kind::Rendered | Kind::Firecracker | Kind::Shards => {
                let guest = Box::new(OciSpec {
                    rootfs: Rootfs::Layers(vec![]),
                    init: Program::of(&["sleep", "infinity"]),
                    mounts: vec![],
                    hostname: None,
                    network: Network::Host,
                    hosts: vec![],
                });
                match kind {
                    Kind::Firecracker => ClassSpec::Firecracker(guest),
                    Kind::Shards => ClassSpec::Shards(guest),
                    _ => ClassSpec::Oci(guest),
                }
            }
        };
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class,
            owner: None,
        }
    }

    /// Applies one step; `false` when the crash budget ran out first.
    fn apply(m: &mut Machine, budget: &mut usize, id: &BoxId, spec: &BoxSpec, step: Step) -> bool {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        m.steps += 1;
        match step {
            Step::MarkCreating => {
                m.records.insert(
                    id.clone(),
                    BoxRecord::new(spec.clone(), Phase::Creating, vec![]),
                );
            }
            Step::RenderBundle => {
                m.bundles.insert(id.clone());
            }
            Step::MakeCgroup => {
                m.cgroups.insert(id.clone());
            }
            Step::MakeStateDir => {
                m.state_dirs.insert(id.clone());
            }
            Step::StartInit => {
                m.inits.insert(id.clone());
            }
            Step::MarkReady => {
                let mut r = BoxRecord::new(spec.clone(), Phase::Ready, vec![]);
                if m.inits.contains(id) {
                    let program = Some(ProcId {
                        pid: 1,
                        start_ticks: 1,
                    });
                    // A `process` box with a program started before its record is a run's.
                    match spec.class {
                        ClassSpec::Process => r.run = program,
                        _ => r.init = program,
                    }
                }
                m.records.insert(id.clone(), r);
            }
            Step::MarkDeleting => {
                if let Some(r) = m.records.get_mut(id) {
                    r.phase = Phase::Deleting;
                }
            }
            Step::KillAll => {
                m.inits.remove(id);
            }
            Step::RemoveCgroup => {
                m.cgroups.remove(id);
            }
            Step::RemoveDirs => {
                m.state_dirs.remove(id);
                m.bundles.remove(id);
            }
            Step::DropRecord => {
                m.records.remove(id);
            }
        }
        true
    }

    /// Runs `ops` until the budget runs out.
    fn run(m: &mut Machine, ops: &[Op], mut budget: usize) {
        for op in ops {
            match *op {
                Op::Create(n, kind) => {
                    let id = id(n);
                    if m.records.contains_key(&id) {
                        continue;
                    }
                    let spec = spec(&id, kind);
                    let steps = match kind {
                        Kind::Run => RUN_STEPS,
                        _ => create_steps(&spec.class),
                    };
                    for s in steps {
                        if !apply(m, &mut budget, &id, &spec, *s) {
                            return;
                        }
                    }
                }
                Op::Delete(n) => {
                    let id = id(n);
                    let Some(r) = m.records.get(&id) else {
                        continue;
                    };
                    let spec = r.spec.clone();
                    for s in DELETE_STEPS {
                        if !apply(m, &mut budget, &id, &spec, *s) {
                            return;
                        }
                    }
                }
                Op::InitExits(n) => {
                    m.inits.remove(&id(n));
                }
            }
        }
    }

    /// The boxes whole and not being deleted: each part its class has, and its record ready.
    fn whole(m: &Machine) -> BTreeSet<BoxId> {
        m.records
            .iter()
            .filter(|(id, r)| {
                let made = |parts: &BTreeSet<BoxId>| parts.contains(*id);
                r.phase == Phase::Ready
                    && match r.spec.class {
                        ClassSpec::Process => {
                            made(&m.cgroups) && (r.run.is_none() || made(&m.inits))
                        }
                        ClassSpec::Bundle(_) => {
                            made(&m.cgroups) && made(&m.inits) && made(&m.state_dirs)
                        }
                        ClassSpec::Oci(_) => {
                            made(&m.cgroups)
                                && made(&m.inits)
                                && made(&m.state_dirs)
                                && made(&m.bundles)
                        }
                        ClassSpec::Firecracker(_)
                        | ClassSpec::Shards(_)
                        | ClassSpec::ShardsImage(_) => made(&m.inits),
                    }
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// The parts of box `id` the agent finds at the paths its id fixes.
    fn parts(m: &Machine, id: &BoxId) -> Parts {
        Parts {
            cgroup: m.cgroups.contains(id),
            state_dir: m.state_dirs.contains(id),
            bundle: m.bundles.contains(id),
        }
    }

    /// What the agent sees after a restart: every record, and every id it finds a part of. An init
    /// runs in its box's cgroup, which the agent lists in its place.
    fn look(m: &Machine) -> BTreeMap<BoxId, Seen> {
        let mut seen: BTreeMap<BoxId, Seen> = BTreeMap::new();
        for (id, r) in &m.records {
            let init_alive = r.init.or(r.run).map(|_| m.inits.contains(id));
            seen.insert(
                id.clone(),
                Seen {
                    record: Some(Ok(r.clone())),
                    parts: parts(m, id),
                    init_alive,
                    start_interrupted: false,
                    earlier_boot: false,
                },
            );
        }
        for id in m.cgroups.iter().chain(&m.state_dirs).chain(&m.bundles) {
            seen.entry(id.clone()).or_insert(Seen {
                record: None,
                parts: parts(m, id),
                init_alive: None,
                start_interrupted: false,
                earlier_boot: false,
            });
        }
        seen
    }

    fn recover(m: &mut Machine) {
        for (id, d) in plan(&look(m)) {
            if let Decision::Remove(_) = d {
                let spec = spec(&id, Kind::Process);
                let mut unlimited = usize::MAX;
                for s in REMOVE_STEPS {
                    apply(m, &mut unlimited, &id, &spec, *s);
                }
            }
        }
    }

    fn op() -> impl Strategy<Value = Op> {
        let kind = prop_oneof![
            Just(Kind::Process),
            Just(Kind::Bundle),
            Just(Kind::Rendered),
            Just(Kind::Run),
            Just(Kind::Firecracker),
            Just(Kind::Shards)
        ];
        prop_oneof![
            (0u8..4, kind).prop_map(|(n, k)| Op::Create(n, k)),
            (0u8..4).prop_map(Op::Delete),
            (0u8..4).prop_map(Op::InitExits),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        #[test]
        fn a_crash_after_any_step_leaves_only_whole_boxes_and_keeps_every_whole_one(ops in proptest::collection::vec(op(), 1..12)) {
            let mut full = Machine::default();
            run(&mut full, &ops, usize::MAX);
            let total = full.steps;
            // Non-vacuity: the histories do make and remove boxes.
            prop_assume!(total > 0);
            for crash_after in 0..=total {
                let mut m = Machine::default();
                run(&mut m, &ops, crash_after);
                let survivors = whole(&m);
                recover(&mut m);
                let left: BTreeSet<BoxId> = m.records.keys().cloned().collect();
                prop_assert_eq!(&left, &survivors, "crash after step {}", crash_after);
                prop_assert_eq!(whole(&m), survivors.clone());
                let leaked: Vec<&BoxId> = m.cgroups.iter().chain(&m.state_dirs).chain(&m.bundles).chain(&m.inits).filter(|id| !survivors.contains(*id)).collect();
                prop_assert!(leaked.is_empty(), "leaked {:?} after a crash at step {}", leaked, crash_after);
            }
        }
    }

    /// T-T7.1.2, goal 5 extended step 5: kill the agent after each step of a create of each kind,
    /// then recover. Before the record is written the box is found by the parts made so far and removed,
    /// those parts named (non-vacuity: every prefix of each create is seen this way); once it's written
    /// the box is taken back whole.
    #[test]
    fn a_create_killed_after_each_step_is_removed_by_what_it_made_or_taken_back() {
        let b = id(0);
        for kind in [
            Kind::Process,
            Kind::Bundle,
            Kind::Rendered,
            Kind::Run,
            Kind::Firecracker,
            Kind::Shards,
        ] {
            let spec = spec(&b, kind);
            let steps = match kind {
                Kind::Run => RUN_STEPS,
                _ => create_steps(&spec.class),
            };
            assert_eq!(
                steps.last(),
                Some(&Step::MarkReady),
                "{kind:?}: the record is last"
            );
            for crash_after in 0..=steps.len() {
                let mut m = Machine::default();
                run(&mut m, &[Op::Create(0, kind)], crash_after);
                let made = parts(&m, &b);
                let want = match crash_after {
                    0 => None,
                    n if n == steps.len() => Some(Decision::Adopt),
                    _ if m.records.contains_key(&b) => Some(Decision::Remove(Why::MidCreate)),
                    _ => Some(Decision::Remove(Why::NoRecord(made))),
                };
                let got = plan(&look(&m))
                    .into_iter()
                    .find(|(id, _)| *id == b)
                    .map(|(_, d)| d);
                assert_eq!(got, want, "{kind:?}, killed after {crash_after} steps");
                recover(&mut m);
                let kept = usize::from(want == Some(Decision::Adopt));
                assert_eq!(
                    (m.records.len(), m.cgroups.len(), m.inits.len()),
                    (
                        kept,
                        if matches!(kind, Kind::Firecracker | Kind::Shards) {
                            0
                        } else {
                            kept
                        },
                        if matches!(kind, Kind::Process) {
                            0
                        } else {
                            kept
                        }
                    ),
                    "{kind:?}, killed after {crash_after} steps"
                );
                assert_eq!(
                    m.state_dirs.len() + m.bundles.len(),
                    match kind {
                        Kind::Process | Kind::Run | Kind::Firecracker | Kind::Shards => 0,
                        Kind::Bundle => kept,
                        Kind::Rendered => 2 * kept,
                    },
                    "{kind:?}, killed after {crash_after} steps: a directory is left"
                );
            }
        }
    }

    /// Goal 5 extended step 5: a box seen only as a cgroup, only as a container's state directory, only
    /// as a rendered bundle, or as any two or all three of them, with no record, is removed, the
    /// decision naming each part found.
    #[test]
    fn a_box_found_by_its_parts_alone_is_removed_naming_them() {
        for bits in 1u8..8 {
            let parts = Parts {
                cgroup: bits & 1 != 0,
                state_dir: bits & 2 != 0,
                bundle: bits & 4 != 0,
            };
            let seen = Seen {
                record: None,
                parts,
                init_alive: None,
                start_interrupted: false,
                earlier_boot: false,
            };
            assert_eq!(
                decide(&seen),
                Decision::Remove(Why::NoRecord(parts)),
                "{parts:?}"
            );
        }
    }

    #[test]
    fn each_finding_gets_its_decision() {
        let r = |phase, kind| {
            let mut r = BoxRecord::new(spec(&id(0), kind), phase, vec![]);
            let program = Some(ProcId {
                pid: 7,
                start_ticks: 9,
            });
            match kind {
                Kind::Bundle | Kind::Rendered | Kind::Firecracker | Kind::Shards => {
                    r.init = program
                }
                Kind::Run => r.run = program,
                Kind::Process => {}
            }
            Some(Ok(r))
        };
        let cgroup = Parts {
            cgroup: true,
            ..Parts::default()
        };
        let cases = [
            (
                Seen {
                    record: None,
                    parts: cgroup,
                    init_alive: None,
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Remove(Why::NoRecord(cgroup)),
            ),
            (
                Seen {
                    record: Some(Err("bad".into())),
                    parts: cgroup,
                    init_alive: None,
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Remove(Why::Unreadable("bad".into())),
            ),
            (
                // A record an agent from before goal 5 extended step 5 wrote before making the box.
                Seen {
                    record: r(Phase::Creating, Kind::Process),
                    parts: cgroup,
                    init_alive: None,
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Remove(Why::MidCreate),
            ),
            (
                Seen {
                    record: r(Phase::Deleting, Kind::Process),
                    parts: cgroup,
                    init_alive: None,
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Remove(Why::MidDelete),
            ),
            (
                Seen {
                    record: r(Phase::Ready, Kind::Process),
                    parts: Parts::default(),
                    init_alive: None,
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Remove(Why::NoCgroup),
            ),
            (
                Seen {
                    record: r(Phase::Ready, Kind::Rendered),
                    parts: cgroup,
                    init_alive: Some(false),
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Remove(Why::InitGone),
            ),
            (
                Seen {
                    record: r(Phase::Ready, Kind::Bundle),
                    parts: cgroup,
                    init_alive: Some(true),
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Adopt,
            ),
            // A start under way, in its startContainer hooks, when the agent stopped: removed by that
            // reason whether its init still runs or has ended at the start it found gone.
            (
                Seen {
                    record: r(Phase::Ready, Kind::Bundle),
                    parts: cgroup,
                    init_alive: Some(true),
                    start_interrupted: true,
                    earlier_boot: false,
                },
                Decision::Remove(Why::StartInterrupted),
            ),
            (
                Seen {
                    record: r(Phase::Ready, Kind::Rendered),
                    parts: cgroup,
                    init_alive: Some(false),
                    start_interrupted: true,
                    earlier_boot: false,
                },
                Decision::Remove(Why::StartInterrupted),
            ),
            (
                Seen {
                    record: r(Phase::Ready, Kind::Process),
                    parts: cgroup,
                    init_alive: None,
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Adopt,
            ),
            // Decision 7: a run whose program ended while no agent was there is removed; one whose program
            // still runs is taken back, to be removed when it ends.
            (
                Seen {
                    record: r(Phase::Ready, Kind::Run),
                    parts: cgroup,
                    init_alive: Some(false),
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Remove(Why::RunEnded),
            ),
            (
                Seen {
                    record: r(Phase::Ready, Kind::Run),
                    parts: cgroup,
                    init_alive: Some(true),
                    start_interrupted: false,
                    earlier_boot: false,
                },
                Decision::Adopt,
            ),
        ];
        for (seen, want) in cases {
            assert_eq!(decide(&seen), want, "{seen:?}");
        }
        // T-T7.1.2: a record from an earlier boot of the host names nothing that runs, whatever this boot
        // shows at its paths or under its pid: removed as that boot's, never taken back, in any phase. A
        // record no boot names is decided as before.
        for (phase, kind) in [
            (Phase::Ready, Kind::Process),
            (Phase::Ready, Kind::Bundle),
            (Phase::Ready, Kind::Run),
            (Phase::Creating, Kind::Bundle),
            (Phase::Deleting, Kind::Process),
        ] {
            let seen = Seen {
                record: r(phase, kind),
                parts: cgroup,
                init_alive: Some(true),
                start_interrupted: false,
                earlier_boot: true,
            };
            assert_eq!(
                decide(&seen),
                Decision::Remove(Why::EarlierBoot),
                "{seen:?}"
            );
            let this_boot = Seen {
                earlier_boot: false,
                ..seen
            };
            assert_ne!(
                decide(&this_boot),
                Decision::Remove(Why::EarlierBoot),
                "{this_boot:?}"
            );
        }
    }
}
