//! AC-T7.3.4/AC-T7.4.1/LB03: retained device identity across peer recovery.
//! All assignments below are pure fixture observations, not native GPU attachments.

use super::*;
use crate::model::{Classes, Device, GpuLoad, Held};
use std::cell::RefCell;

fn deliver_one(
    node: &mut Node,
    records: &mut Memory,
    words: Vec<(NodeId, Msg)>,
) -> Vec<(NodeId, Msg)> {
    let [(to, word)] = words.as_slice() else {
        panic!("expected exactly one local peer message: {words:?}")
    };
    assert_eq!(*to, NodeId(0));
    node.hear(NodeId(0), *word, records).unwrap()
}

fn held_by(node: &Node, key: Key) -> Held {
    let runner = node.ran.get(&key).unwrap();
    node.admission.holds(NodeId(0), runner.workload)
}

fn create_confirmed(node: &mut Node, records: &mut Memory, demand: Demand) -> (Key, Held) {
    let (key, start) = node.submit(demand, records).unwrap();
    assert_eq!(
        start,
        [(
            NodeId(0),
            Msg::Start {
                allocation: None,
                key,
                demand,
                route: None,
            }
        )]
    );
    assert!(deliver_one(node, records, start).is_empty());
    assert_eq!(std::mem::take(&mut node.told), [Told::Create(key, demand)]);
    let assigned = held_by(node, key);
    let started = node.created(key, records).unwrap();
    assert_eq!(started, [(NodeId(0), Msg::Started { key })]);
    assert!(deliver_one(node, records, started).is_empty());
    assert_eq!(
        std::mem::take(&mut node.told),
        [Told::Started(key, NodeId(0))]
    );
    (key, assigned)
}

fn delete_confirmed(
    node: &mut Node,
    records: &mut Memory,
    assignments: &mut BTreeMap<Key, Held>,
    key: Key,
) {
    let demand = node.demand(key).unwrap();
    let delete = node.delete(key, records).unwrap();
    assert_eq!(delete, [(NodeId(0), Msg::Delete { key, demand })]);
    assert!(deliver_one(node, records, delete).is_empty());
    assert_eq!(std::mem::take(&mut node.told), [Told::Delete(key, demand)]);
    // The fixture removes its exact observed assignment before reporting actual model absence.
    assert!(assignments.remove(&key).is_some());
    let ended = node.end(key, records).unwrap();
    assert_eq!(ended, [(NodeId(0), Msg::Ended { key })]);
    let forget = deliver_one(node, records, ended);
    assert_eq!(std::mem::take(&mut node.told), [Told::Ended(key)]);
    assert_eq!(forget, [(NodeId(0), Msg::Forget { key })]);
    assert!(deliver_one(node, records, forget).is_empty());
    assert!(node.told.is_empty());
}

/// The devices retain index/kind/model/size; only load swaps. A surviving box must keep
/// GPU0, leaving GPU1 for the next box, even when a fresh binding would prefer GPU1.
#[test]
fn reboot_with_swapped_gpu_load_keeps_surviving_assignment_and_never_grants_it_twice() {
    let capacity = Resources {
        cpu_milli: 4_000,
        mem_bytes: 4_096,
        disk_bytes: 0,
        gpus: 2,
    };
    let models = ["fixture-gpu".to_owned()];
    let mut spec = NodeSpec::new(NodeId(0), capacity);
    spec.classes = Classes::NONE.with(Class::Oci);
    spec.devices = vec![Device::gpu(ModelId(0), 4_096); 2];
    spec.devices[1].load.busy_per_mille = Some(1_000);
    let budget = Budget { owned: 4, ran: 4 };
    let gpu = Demand {
        kind: Kind::Session,
        class: Class::Oci,
        need: Resources {
            cpu_milli: 100,
            mem_bytes: 512,
            disk_bytes: 0,
            gpus: 1,
        },
        ask: Ask {
            os: Some(Os::Linux),
            arch: Some(Arch::X86_64),
            gpu_model: Some(ModelId(0)),
            gpu_memory: 1_024,
            ..Ask::default()
        },
    };
    let cpu = Demand {
        need: Resources {
            gpus: 0,
            ..gpu.need
        },
        ask: Ask::default(),
        ..gpu
    };
    let mut records = Memory::default();
    let mut node = Node::boot(
        NodeId(0),
        &[spec.clone()],
        &models,
        budget,
        Pick::default(),
        &mut records,
        |_| Boxed::Absent,
    )
    .unwrap();
    eprintln!(
        "device-recovery phase=initial-create load={:?}",
        spec.devices
            .iter()
            .map(|device| device.load)
            .collect::<Vec<_>>()
    );
    let (old, before_held) = create_confirmed(&mut node, &mut records, gpu);
    assert_eq!(before_held, [0]);
    let (sibling, sibling_assignment) = create_confirmed(&mut node, &mut records, cpu);
    assert!(sibling_assignment.is_empty());
    // This observer outlives Node; recovery cannot silently change a running box's attachment.
    let mut assignments =
        BTreeMap::from([(old, before_held.clone()), (sibling, sibling_assignment)]);
    let before_assignments = assignments.clone();
    let old_runner_record = records.rows.get(&old.name("ran")).unwrap().clone();
    let mut before_records = records.rows.clone();
    before_records.remove("epoch");
    drop(node);

    let mut swapped = spec.clone();
    swapped.devices[0].load = GpuLoad {
        busy_per_mille: Some(1_000),
        memory_used: Some(0),
    };
    // This fixture swaps two KNOWN readings; Default intentionally denotes unread native data.
    swapped.devices[1].load = GpuLoad {
        busy_per_mille: Some(0),
        memory_used: Some(0),
    };
    for (before, after) in spec.devices.iter().zip(&swapped.devices) {
        assert_eq!(
            (before.kind, before.model, before.size),
            (after.kind, after.model, after.size)
        );
    }
    assert_eq!(spec.capacity, swapped.capacity);
    assert_eq!(spec.classes, swapped.classes);
    assert_eq!(spec.platform, swapped.platform);
    let runtime_observations = RefCell::new(Vec::new());
    let mut node = Node::boot(
        NodeId(0),
        &[swapped],
        &models,
        budget,
        Pick::default(),
        &mut records,
        |key| match assignments.get(&key) {
            Some(held) => {
                runtime_observations.borrow_mut().push((key, held.clone()));
                Boxed::Running
            }
            None => Boxed::Absent,
        },
    )
    .unwrap();
    let recovered_held = held_by(&node, old);
    let mut after_records = records.rows.clone();
    after_records.remove("epoch");
    let reconfirm = node.link_up(NodeId(0), &mut records).unwrap();
    assert_eq!(reconfirm.len(), 2);
    for word in reconfirm {
        let held = deliver_one(&mut node, &mut records, vec![word]);
        assert!(matches!(
            held.as_slice(),
            [(
                _,
                Msg::Held {
                    holds: true,
                    created: true,
                    ended: false,
                    ..
                }
            )]
        ));
        assert!(deliver_one(&mut node, &mut records, held).is_empty());
    }
    let reconfirmed = std::mem::take(&mut node.told);
    let sibling_survived = reconfirmed.contains(&Told::Started(sibling, NodeId(0)))
        && assignments.get(&sibling) == before_assignments.get(&sibling);
    eprintln!("device-recovery phase=recovered-fresh-create old={old:?} held={recovered_held:?}");
    let (fresh, fresh_held) = create_confirmed(&mut node, &mut records, gpu);
    assert!(assignments.insert(fresh, fresh_held.clone()).is_none());
    let collisions: Held = before_held
        .iter()
        .copied()
        .filter(|slot| fresh_held.contains(slot))
        .collect();
    let logical_old_after_fresh = held_by(&node, old);
    let (progress, progress_held) = create_confirmed(&mut node, &mut records, cpu);
    assert!(progress_held.is_empty());
    assert!(assignments.insert(progress, progress_held).is_none());
    let after_assignments = assignments.clone();
    let admitted_before_cleanup = node.admitted();

    // Preserve all witnesses, then complete ordinary delete/end/forget before a red assertion.
    for key in [old, sibling, fresh, progress] {
        delete_confirmed(&mut node, &mut records, &mut assignments, key);
    }
    assert!(assignments.is_empty());
    assert_eq!(node.owned(), 0);
    assert_eq!(node.records(), 0);
    assert_eq!(node.admitted(), [0; lb::DIMS]);
    assert_eq!(records.rows.len(), 1);
    assert!(records.rows.contains_key("epoch"));
    assert_eq!(node.admission.verdict(NodeId(0), &gpu), Ok(Verdict::Admits));
    assert_eq!(node.view.verdict(NodeId(0), &gpu), Ok(Verdict::Admits));
    eprintln!(
        "device-recovery PURE expected_old=[0] expected_fresh=[1] old={old:?} fresh={fresh:?} sibling={sibling:?} progress={progress:?} before_held={before_held:?} recovered_held={recovered_held:?} old_after_fresh={logical_old_after_fresh:?} fresh_held={fresh_held:?} collisions={collisions:?} before_assignments={before_assignments:?} after_assignments={after_assignments:?} runtime_observations={:?} sibling_survived={sibling_survived} progress_confirmed=true admitted_before_cleanup={admitted_before_cleanup:?} creates=4 deletes=4 cleanup_assignments=0 cleanup_own=0 cleanup_ran=0 cleanup_charge=0 cleanup_records=epoch_only old_runner_record={old_runner_record:?} native_gpu_effect=false",
        runtime_observations.into_inner()
    );

    assert!(
        collisions.is_empty(),
        "recovery re-granted a surviving fixture assignment: {collisions:?}"
    );
    assert_eq!(recovered_held, before_held);
    assert_eq!(logical_old_after_fresh, before_held);
    assert_eq!(fresh_held, [1]);
    assert_eq!(before_records, after_records);
    assert!(sibling_survived);
    assert_eq!(admitted_before_cleanup, [400, 2_048, 0, 2]);
}

/// Existing holders remain controllable when fresh placement would reject transient facts.
/// Both the owner's view and a runner-only recovery must obey that lifetime distinction.
#[test]
fn retained_gpu_recovery_ignores_transient_memory_and_session_hotness() {
    let mut outcomes = Vec::new();
    for runner_only in [false, true] {
        for memory_pressure in [true, false] {
            let models = ["fixture-gpu".to_owned()];
            let mut spec = NodeSpec::new(
                NodeId(0),
                Resources {
                    cpu_milli: 4_000,
                    mem_bytes: 4_096,
                    disk_bytes: 0,
                    gpus: 2,
                },
            );
            spec.classes = Classes::NONE.with(Class::Oci);
            spec.devices = vec![Device::gpu(ModelId(0), 4_096); 2];
            let gpu = Demand {
                kind: Kind::Session,
                class: Class::Oci,
                need: Resources {
                    cpu_milli: 100,
                    mem_bytes: 512,
                    disk_bytes: 0,
                    gpus: 1,
                },
                ask: Ask {
                    gpu_model: Some(ModelId(0)),
                    gpu_memory: 1_024,
                    ..Ask::default()
                },
            };
            let budget = Budget { owned: 2, ran: 2 };
            let mut records = Memory::default();
            let mut node = Node::boot(
                NodeId(0),
                &[spec.clone()],
                &models,
                budget,
                Pick::default(),
                &mut records,
                |_| Boxed::Absent,
            )
            .unwrap();
            let (key, held) = create_confirmed(&mut node, &mut records, gpu);
            assert_eq!(held, [0]);
            let mut assignments = BTreeMap::from([(key, held.clone())]);
            if runner_only {
                records.remove(&key.name("own")).unwrap();
            }
            drop(node);
            let before = records.rows.clone();
            let mut changed = spec.clone();
            if memory_pressure {
                for device in &mut changed.devices {
                    device.load.memory_used = Some(device.size);
                }
            } else {
                changed.background_milli =
                    changed.capacity.cpu_milli * (crate::index::HOT_PER_MILLE + 1) / 1_000;
            }
            let attempt = Node::boot(
                NodeId(0),
                &[changed],
                &models,
                budget,
                Pick::default(),
                &mut records,
                |seen| {
                    if seen == key {
                        Boxed::Running
                    } else {
                        Boxed::Absent
                    }
                },
            );
            let (mut recovered, refusal) = match attempt {
                Ok(node) => (node, None),
                Err(why) => {
                    assert_eq!(records.rows, before);
                    let node = Node::boot(
                        NodeId(0),
                        &[spec],
                        &models,
                        budget,
                        Pick::default(),
                        &mut records,
                        |seen| {
                            if seen == key {
                                Boxed::Running
                            } else {
                                Boxed::Absent
                            }
                        },
                    )
                    .unwrap();
                    (node, Some(why))
                }
            };
            let recovered_held = held_by(&recovered, key);
            // Check fresh policy only on the recovered transient facts, never on repaired facts.
            let fresh_verdict = if refusal.is_none() {
                Some(recovered.admission.verdict(NodeId(0), &gpu).unwrap())
            } else {
                None
            };
            if runner_only {
                assert!(assignments.remove(&key).is_some());
                let ended = recovered.end(key, &mut records).unwrap();
                let forget = deliver_one(&mut recovered, &mut records, ended);
                assert_eq!(forget, [(NodeId(0), Msg::Forget { key })]);
                assert!(deliver_one(&mut recovered, &mut records, forget).is_empty());
            } else {
                delete_confirmed(&mut recovered, &mut records, &mut assignments, key);
            }
            assert!(assignments.is_empty());
            assert_eq!(recovered.admitted(), [0; lb::DIMS]);
            assert_eq!(recovered.owned(), 0);
            assert_eq!(recovered.records(), 0);
            assert_eq!(records.rows.len(), 1);
            eprintln!(
                "retained-transient runner_only={runner_only} memory_pressure={memory_pressure} expected=bootOk/held0 observed_refusal={refusal:?} recovered_held={recovered_held:?} fresh_verdict={fresh_verdict:?} cleanup_assignments=0 cleanup_rows=epoch_only cleanup_charge=0"
            );
            outcomes.push((
                runner_only,
                memory_pressure,
                refusal,
                recovered_held,
                fresh_verdict,
            ));
        }
    }
    for (runner_only, memory_pressure, refusal, held, fresh) in outcomes {
        assert!(
            refusal.is_none(),
            "retained box lost recovery control: runner_only={runner_only} memory_pressure={memory_pressure} refusal={refusal:?}"
        );
        assert_eq!(held, [0]);
        assert_eq!(
            fresh,
            Some(if memory_pressure {
                Verdict::NoGpu
            } else {
                Verdict::Hot
            })
        );
    }
}

fn recovery_inputs() -> (NodeSpec, Vec<String>, Demand, Budget) {
    let mut spec = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 4_000,
            mem_bytes: 4_096,
            disk_bytes: 0,
            gpus: 2,
        },
    );
    spec.classes = Classes::NONE.with(Class::Oci);
    spec.devices = vec![Device::gpu(ModelId(0), 4_096); 2];
    spec.devices[1].load.busy_per_mille = Some(1_000);
    let demand = Demand {
        kind: Kind::Session,
        class: Class::Oci,
        need: Resources {
            cpu_milli: 100,
            mem_bytes: 512,
            disk_bytes: 0,
            gpus: 1,
        },
        ask: Ask {
            os: Some(Os::Linux),
            arch: Some(Arch::X86_64),
            gpu_model: Some(ModelId(0)),
            gpu_memory: 1_024,
            ..Ask::default()
        },
    };
    (
        spec,
        vec!["fixture-gpu".into(), "other-gpu".into()],
        demand,
        Budget { owned: 4, ran: 4 },
    )
}

fn boot_local(
    spec: &NodeSpec,
    models: &[String],
    budget: Budget,
    records: &mut Memory,
    actual: impl Fn(Key) -> Boxed,
) -> Result<Node, Stop> {
    Node::boot(
        NodeId(0),
        std::slice::from_ref(spec),
        models,
        budget,
        Pick::default(),
        records,
        actual,
    )
}

fn assert_empty(node: &Node, records: &Memory) {
    assert_eq!(node.admitted(), [0; lb::DIMS]);
    assert_eq!(node.owned(), 0);
    assert_eq!(node.records(), 0);
    assert_eq!(records.rows.len(), 1);
    assert!(records.rows.contains_key("epoch"));
}

/// AC-T7.3.4: the same one runner put contains demand and chosen physical identity before Create.
#[test]
fn chosen_binding_is_atomic_with_demand_before_create_even_at_the_record_write() {
    for when in [None, Some(When::Before), Some(When::After)] {
        let (mut spec, models, demand, budget) = recovery_inputs();
        spec.devices[0].key = Some(101);
        spec.devices[1].key = Some(102);
        let mut records = Memory::default();
        let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Absent).unwrap();
        let (key, start) = node.submit(demand, &mut records).unwrap();
        let mut crash = Crashing::new(records);
        crash.armed = when;
        let result = node.hear(NodeId(0), start[0].1, &mut crash);
        assert_eq!(crash.writes, 1);
        let persisted = crash.inner.rows.get(&key.name("ran")).cloned();
        assert_eq!(persisted.is_some(), when != Some(When::Before));
        if let Some(row) = &persisted {
            assert_eq!(
                row,
                &format!(
                    "bindings1:0:1:p:gpu:101 creating {}",
                    demand_text(&demand, &models).unwrap()
                )
            );
            // The shell's existing state/d1 split remains unchanged.
            let (_, rest) = row.split_once(' ').unwrap();
            let (state, encoded) = rest.split_once(' ').unwrap();
            assert_eq!(state, "creating");
            assert_eq!(decode_demand(encoded, &models).unwrap(), demand);
        }
        match when {
            None => {
                assert_eq!(result.unwrap(), []);
                assert_eq!(node.told, [Told::Create(key, demand)]);
                assert_eq!(node.assigned_devices(key), Some(vec![0]));
                assert_eq!(
                    node.assigned_device_specs(key).unwrap(),
                    Some(vec![spec.devices[0]])
                );
            }
            Some(_) => {
                assert_eq!(result, Err(Stop::Crashed));
                assert!(node.told.is_empty());
            }
        }
        drop(node);
        let mut records = crash.inner;
        // No fixture runtime effect was released: recovery proves absence and drops any ran row.
        let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Absent).unwrap();
        assert_eq!(node.assigned_devices(key), None);
        let delete = node.delete(key, &mut records).unwrap();
        assert!(deliver_one(&mut node, &mut records, delete).is_empty());
        assert_eq!(std::mem::take(&mut node.told), [Told::Delete(key, demand)]);
        let ended = node.end(key, &mut records).unwrap();
        let forget = deliver_one(&mut node, &mut records, ended);
        assert!(deliver_one(&mut node, &mut records, forget).is_empty());
        assert_empty(&node, &records);
        eprintln!(
            "binding-put when={when:?} puts=1 row_present={} released_create={} actual_boxes=0 cleanup_rows=epoch_only cleanup_charge=0",
            persisted.is_some(),
            when.is_none()
        );
    }
}

/// AC-T7.3.4/AC-T7.4.1: malformed or legacy device claims never guess or rewrite any bytes.
#[test]
fn missing_duplicate_out_of_range_and_overflow_claims_refuse_before_runtime_observation() {
    let (spec, models, demand, budget) = recovery_inputs();
    let mut records = Memory::default();
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Absent).unwrap();
    let (key, held) = create_confirmed(&mut node, &mut records, demand);
    let (_, suffix) = records.rows[&key.name("ran")].split_once(' ').unwrap();
    let suffix = suffix.to_owned();
    let original = records.rows.clone();
    drop(node);
    let cases = [
        ("legacy-device", "0"),
        ("missing", "bindings1:0:0"),
        ("duplicate", "bindings1:0:2:s:0:s:0"),
        ("range", "bindings1:0:1:s:2"),
        ("u16-overflow", "bindings1:0:1:s:65536"),
        ("count-overflow", "bindings1:0:18446744073709551616"),
        ("excess-count", "bindings1:0:3:s:0:s:1:s:0"),
        ("trailing", "bindings1:0:1:s:0:s:1"),
        ("version", "bindings2:0:1:s:0"),
        ("representation", "bindings1:0:1:q:0"),
        ("physical-missing", "bindings1:0:1:p:gpu:101"),
        (
            "physical-overflow",
            "bindings1:0:1:p:gpu:18446744073709551616",
        ),
    ];
    for (case, atom) in cases {
        records.rows = original.clone();
        records
            .rows
            .insert(key.name("ran"), format!("{atom} {suffix}"));
        let before = records.rows.clone();
        let observed = RefCell::new(Vec::new());
        let result = boot_local(&spec, &models, budget, &mut records, |key| {
            observed.borrow_mut().push(key);
            Boxed::Running
        });
        assert!(
            matches!(&result, Err(Stop::Damaged(_))),
            "{case}: {result:?}"
        );
        assert_eq!(records.rows, before, "{case}");
        assert!(observed.borrow().is_empty(), "{case}");
        eprintln!(
            "binding-refusal case={case} typed_damaged=true bytes_unchanged=true runtime_observations=0 result={:?}",
            result.unwrap_err()
        );
    }
    records.rows = original;
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Running).unwrap();
    assert_eq!(node.assigned_devices(key), Some(held.clone()));
    let mut assignments = BTreeMap::from([(key, held)]);
    delete_confirmed(&mut node, &mut records, &mut assignments, key);
    assert_empty(&node, &records);
}

/// AC-T7.3.4: an invalid later overlap cannot commit an earlier creating->running row rewrite.
#[test]
fn overlapping_recovery_claims_preserve_all_rows_until_the_whole_restore_is_valid() {
    let (spec, models, demand, budget) = recovery_inputs();
    let mut records = Memory::default();
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Absent).unwrap();
    let (first, first_held) = create_confirmed(&mut node, &mut records, demand);
    let (second, second_held) = create_confirmed(&mut node, &mut records, demand);
    assert_eq!(
        (first_held.clone(), second_held.clone()),
        (vec![0], vec![1])
    );
    drop(node);
    records.rows.remove(&first.name("own"));
    records.rows.remove(&second.name("own"));
    let good_second = records.rows[&second.name("ran")].clone();
    *records.rows.get_mut(&first.name("ran")).unwrap() =
        records.rows[&first.name("ran")].replace(" running ", " creating ");
    *records.rows.get_mut(&second.name("ran")).unwrap() = good_second.replacen(":s:1 ", ":s:0 ", 1);
    let before = records.rows.clone();
    let observed = RefCell::new(Vec::new());
    let result = boot_local(&spec, &models, budget, &mut records, |key| {
        observed.borrow_mut().push(key);
        Boxed::Running
    });
    assert!(matches!(result, Err(Stop::Damaged(_))));
    assert_eq!(records.rows, before);
    assert_eq!(*observed.borrow(), [first, second]);
    records.rows.insert(second.name("ran"), good_second);
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Running).unwrap();
    assert_eq!(node.assigned_devices(first), Some(first_held));
    assert_eq!(node.assigned_devices(second), Some(second_held));
    assert!(records.rows[&first.name("ran")].contains(" running "));
    for key in [first, second] {
        let replay = node
            .hear(
                NodeId(0),
                Msg::Start {
                    allocation: None,
                    key,
                    demand,
                    route: None,
                },
                &mut records,
            )
            .unwrap();
        assert_eq!(replay, [(NodeId(0), Msg::Started { key })]);
        assert!(node.told.is_empty());
        let ended = node.end(key, &mut records).unwrap();
        assert_eq!(node.assigned_devices(key), None);
        assert_eq!(node.assigned_device_specs(key).unwrap(), None);
        let forget = deliver_one(&mut node, &mut records, ended);
        assert!(deliver_one(&mut node, &mut records, forget).is_empty());
    }
    assert_empty(&node, &records);
    eprintln!(
        "binding-overlap typed_damaged=true bytes_unchanged=true observed=2 repaired_holds=[0]/[1] replay_creates=0 cleanup_rows=epoch_only cleanup_charge=0"
    );
}

/// AC-T7.3.4: old rows without exclusive devices still recover and retain replay/delete control.
#[test]
fn legacy_cpu_only_row_recovers_with_empty_assignments_and_replay_does_not_create() {
    let (spec, models, gpu, budget) = recovery_inputs();
    let demand = Demand {
        need: Resources {
            gpus: 0,
            ..gpu.need
        },
        ask: Ask::default(),
        ..gpu
    };
    let mut records = Memory::default();
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Absent).unwrap();
    let (key, held) = create_confirmed(&mut node, &mut records, demand);
    assert!(held.is_empty());
    let (_, suffix) = records.rows[&key.name("ran")].split_once(' ').unwrap();
    let legacy = format!("0 {suffix}");
    records.rows.insert(key.name("ran"), legacy.clone());
    drop(node);
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Running).unwrap();
    assert_eq!(records.rows[&key.name("ran")], legacy);
    assert_eq!(node.assigned_devices(key), Some(vec![]));
    assert_eq!(node.assigned_device_specs(key).unwrap(), Some(vec![]));
    let unknown = Key {
        seq: u64::MAX,
        ..key
    };
    assert_eq!(node.assigned_devices(unknown), None);
    assert_eq!(node.assigned_device_specs(unknown).unwrap(), None);
    let before = records.rows.clone();
    assert_eq!(
        node.hear(
            NodeId(0),
            Msg::Start {
                allocation: None,
                key,
                demand,
                route: None,
            },
            &mut records
        )
        .unwrap(),
        [(NodeId(0), Msg::Started { key })]
    );
    assert_eq!(records.rows, before);
    assert!(node.told.is_empty());
    let mut assignments = BTreeMap::from([(key, held)]);
    delete_confirmed(&mut node, &mut records, &mut assignments, key);
    assert_empty(&node, &records);
    eprintln!(
        "legacy-cpu restored=true held=[] replay_creates=0 cleanup_rows=epoch_only cleanup_charge=0"
    );
}

/// AC-T7.3.4/AC-T7.4.1: physical identity follows a harmless catalog shift, not its former index.
#[test]
fn physical_binding_survives_unheld_device_removal_and_reindex_without_reselection() {
    let (mut spec, models, demand, budget) = recovery_inputs();
    spec.devices[0].key = Some(101);
    spec.devices[1].key = Some(102);
    spec.devices.insert(
        0,
        Device {
            kind: DeviceKind::Net,
            key: Some(101),
            available: true,
            classes: crate::model::Classes::NONE.with(crate::model::Class::Oci),
            model: ModelId::UNNAMED,
            size: 0,
            load: GpuLoad::default(),
        },
    );
    // Equal numeric keys in different DeviceKind namespaces are distinct physical identities.
    let mut records = Memory::default();
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Absent).unwrap();
    let (old, before) = create_confirmed(&mut node, &mut records, demand);
    assert_eq!(before, [1]);
    let old_specs = node.assigned_device_specs(old).unwrap().unwrap();
    assert_eq!(old_specs[0].key, Some(101));
    let old_row = records.rows[&old.name("ran")].clone();
    assert!(old_row.starts_with("bindings1:0:1:p:gpu:101 "));
    let mut physical = BTreeMap::from([(old, (DeviceKind::Gpu, 101))]);
    drop(node);
    spec.devices.remove(0);
    spec.devices[0].load.busy_per_mille = Some(1_000);
    spec.devices[1].load.busy_per_mille = Some(0);
    let mut node = boot_local(&spec, &models, budget, &mut records, |key| {
        if physical.contains_key(&key) {
            Boxed::Running
        } else {
            Boxed::Absent
        }
    })
    .unwrap();
    let restored = node.assigned_devices(old).unwrap();
    assert_eq!(restored, [0]);
    assert_eq!(records.rows[&old.name("ran")], old_row);
    assert_eq!(
        node.assigned_device_specs(old).unwrap().unwrap()[0].key,
        Some(101)
    );
    let (fresh, fresh_held) = create_confirmed(&mut node, &mut records, demand);
    assert_eq!(fresh_held, [1]);
    let fresh_specs = node.assigned_device_specs(fresh).unwrap().unwrap();
    assert_eq!(fresh_specs[0].key, Some(102));
    assert!(
        physical
            .insert(fresh, (fresh_specs[0].kind, fresh_specs[0].key.unwrap()))
            .is_none()
    );
    assert_ne!(physical[&old], physical[&fresh]);
    assert_eq!(node.assigned_devices(old), Some(vec![0]));
    let mut current = BTreeMap::from([(old, restored.clone()), (fresh, fresh_held.clone())]);
    for key in [old, fresh] {
        assert!(physical.remove(&key).is_some());
        delete_confirmed(&mut node, &mut records, &mut current, key);
    }
    assert!(physical.is_empty());
    assert_empty(&node, &records);
    eprintln!(
        "physical-reindex old={old:?} fresh={fresh:?} old_before={before:?} old_restored={restored:?} fresh_held={fresh_held:?} physical_old=(Gpu,101) physical_fresh=(Gpu,102) collision=false cleanup_physical=0 cleanup_rows=epoch_only cleanup_charge=0 native_gpu_effect=false"
    );
}

/// AC-T7.4.1: neither refresh nor reboot may substitute a same-shaped physical device.
#[test]
fn physical_replacement_wrong_shape_and_synthetic_downgrade_preserve_exact_state() {
    let (mut spec, models, demand, budget) = recovery_inputs();
    spec.devices[0].key = Some(101);
    spec.devices[1].key = Some(102);
    let mut records = Memory::default();
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Absent).unwrap();
    let (key, held) = create_confirmed(&mut node, &mut records, demand);
    let original = records.rows.clone();
    let charge = node.admitted();
    let selected = node.assigned_device_specs(key).unwrap();
    let mut replacement = spec.clone();
    replacement.devices[0].key = Some(999);
    assert!(matches!(
        node.refresh_node(replacement.clone()),
        Err(Stop::Damaged(_))
    ));
    assert_eq!(node.admitted(), charge);
    assert_eq!(node.assigned_device_specs(key).unwrap(), selected);
    assert_eq!(records.rows, original);
    drop(node);
    let mut model = spec.clone();
    model.devices[0].model = ModelId(1);
    let mut size = spec.clone();
    size.devices[0].size = demand.ask.gpu_memory - 1;
    let mut kind = spec.clone();
    kind.devices[0].kind = DeviceKind::Vfio;
    kind.capacity.gpus = 1;
    let mut duplicate = spec.clone();
    duplicate.devices[1].key = Some(101);
    let mut downgraded = spec.clone();
    downgraded.devices[0].key = None;
    for (case, changed) in [
        ("same-shape-replacement", replacement),
        ("model", model),
        ("total-size", size),
        ("kind", kind),
        ("duplicate-catalog", duplicate),
        ("lost-physical-key", downgraded),
    ] {
        records.rows = original.clone();
        let observed = RefCell::new(Vec::new());
        let result = boot_local(&changed, &models, budget, &mut records, |key| {
            observed.borrow_mut().push(key);
            Boxed::Running
        });
        assert!(
            matches!(&result, Err(Stop::Damaged(_))),
            "{case}: {result:?}"
        );
        assert_eq!(records.rows, original, "{case}");
        assert!(observed.borrow().is_empty(), "{case}");
        eprintln!(
            "physical-refusal case={case} typed_damaged=true bytes_unchanged=true observations=0 result={:?}",
            result.unwrap_err()
        );
    }
    records.rows = original.clone();
    *records.rows.get_mut(&key.name("ran")).unwrap() =
        original[&key.name("ran")].replacen(":p:gpu:101 ", ":s:0 ", 1);
    let before = records.rows.clone();
    assert!(matches!(
        boot_local(&spec, &models, budget, &mut records, |_| panic!(
            "synthetic claim may not observe runtime"
        )),
        Err(Stop::Damaged(_))
    ));
    assert_eq!(records.rows, before);
    records.rows = original;
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Running).unwrap();
    let mut assignments = BTreeMap::from([(key, held)]);
    delete_confirmed(&mut node, &mut records, &mut assignments, key);
    assert_empty(&node, &records);
}

/// AC-T7.4.1: exact restoration checks stable filters/room and overlap without changing a refused index.
#[test]
fn exact_recovery_reserve_validates_stable_authority_and_is_atomic_on_refusal() {
    let (mut spec, _, demand, _) = recovery_inputs();
    spec.classes = Classes::ALL;
    for kind in [DeviceKind::Vfio, DeviceKind::Block, DeviceKind::Net] {
        spec.devices.push(Device {
            kind,
            key: None,
            available: true,
            classes: crate::model::Classes::NONE.with(crate::model::Class::Oci),
            model: ModelId::UNNAMED,
            size: 512,
            load: GpuLoad::default(),
        });
    }
    let cases = [
        ("missing", demand, vec![]),
        ("range", demand, vec![5]),
        (
            "duplicate",
            Demand {
                need: Resources {
                    gpus: 2,
                    ..demand.need
                },
                ..demand
            },
            vec![0, 0],
        ),
        ("kind", demand, vec![2]),
        (
            "model",
            Demand {
                ask: Ask {
                    gpu_model: Some(ModelId(1)),
                    ..demand.ask
                },
                ..demand
            },
            vec![0],
        ),
        (
            "total-size",
            Demand {
                ask: Ask {
                    gpu_memory: 4_097,
                    ..demand.ask
                },
                ..demand
            },
            vec![0],
        ),
        (
            "class-exposure",
            Demand {
                class: Class::Firecracker,
                ..demand
            },
            vec![0],
        ),
        (
            "os",
            Demand {
                ask: Ask {
                    os: Some(Os::Windows),
                    ..demand.ask
                },
                ..demand
            },
            vec![0],
        ),
        (
            "arch",
            Demand {
                ask: Ask {
                    arch: Some(Arch::Aarch64),
                    ..demand.ask
                },
                ..demand
            },
            vec![0],
        ),
        (
            "resource-room",
            Demand {
                need: Resources {
                    cpu_milli: 4_001,
                    ..demand.need
                },
                ..demand
            },
            vec![0],
        ),
        (
            "block-size",
            Demand {
                need: Resources {
                    gpus: 0,
                    ..demand.need
                },
                ask: Ask {
                    block: 1,
                    block_size: 513,
                    ..Ask::default()
                },
                ..demand
            },
            vec![3],
        ),
    ];
    for (case, changed, held) in cases {
        let mut index = FitIndex::new(std::slice::from_ref(&spec)).unwrap();
        let before = index.key(NodeId(0)).unwrap();
        let result =
            index.take_recovered_allocated(NodeId(0), WorkloadId(0), &changed, &held, None);
        assert!(
            matches!(
                result,
                Err(crate::SimError::Config(_) | crate::SimError::Resources(_))
            ),
            "{case}: {result:?}"
        );
        assert_eq!(index.key(NodeId(0)).unwrap(), before, "{case}");
        assert!(index.holds(NodeId(0), WorkloadId(0)).is_empty(), "{case}");
        eprintln!("reserve-refusal case={case} typed=true charge_unchanged=true holders=0");
    }
    let mut index = FitIndex::new(std::slice::from_ref(&spec)).unwrap();
    let baseline = index.key(NodeId(0)).unwrap();
    index
        .take_recovered_allocated(NodeId(0), WorkloadId(0), &demand, &[0], None)
        .unwrap();
    let before = index.key(NodeId(0)).unwrap();
    assert!(matches!(
        index.take_recovered_allocated(NodeId(0), WorkloadId(1), &demand, &[0], None),
        Err(crate::SimError::Config(_))
    ));
    assert_eq!(index.key(NodeId(0)).unwrap(), before);
    assert_eq!(index.holds(NodeId(0), WorkloadId(0)), [0]);
    assert!(index.holds(NodeId(0), WorkloadId(1)).is_empty());
    index
        .take_recovered_allocated(NodeId(0), WorkloadId(1), &demand, &[1], None)
        .unwrap();
    for workload in [WorkloadId(0), WorkloadId(1)] {
        index.give(NodeId(0), workload, &demand).unwrap();
    }
    assert_eq!(index.key(NodeId(0)).unwrap(), baseline);
    eprintln!(
        "reserve-overlap typed=true charge_unchanged=true sibling_hold=[0] independent_hold=[1] cleanup_holders=0 cleanup_charge=0"
    );
}

/// AC-T7.4.1: the existing u16 schema includes slot65535 in a 65,536-device catalog.
#[test]
fn last_legal_device_index_roundtrips_and_releases_without_iterator_overflow() {
    let (mut spec, models, demand, _) = recovery_inputs();
    spec.devices = vec![Device::gpu(ModelId(0), 4_096); crate::model::DEVICES];
    spec.capacity.gpus = u32::try_from(spec.devices.len()).unwrap();
    let last = u16::MAX;
    let encoded = format!("bindings1:0:1:s:{last}");
    assert_eq!(
        runner_owner(&encoded, &spec.devices).unwrap(),
        (NodeId(0), Some(vec![last]))
    );
    let mut index = FitIndex::new(std::slice::from_ref(&spec)).unwrap();
    let baseline = index.key(NodeId(0)).unwrap();
    index
        .take_recovered_allocated(NodeId(0), WorkloadId(0), &demand, &[last], None)
        .unwrap();
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        index.holds(NodeId(0), WorkloadId(0))
    }));
    assert_eq!(
        index.held_devices(NodeId(0), WorkloadId(0)).unwrap(),
        [spec.devices[usize::from(last)]]
    );
    index.give(NodeId(0), WorkloadId(0), &demand).unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap(), baseline);
    assert!(
        index
            .held_devices(NodeId(0), WorkloadId(0))
            .unwrap()
            .is_empty()
    );
    eprintln!(
        "last-index devices={} chosen={last} held_query_ok={} cleanup_holders=0 cleanup_charge=0 native_gpu_effect=false",
        spec.devices.len(),
        observed.is_ok()
    );
    assert_eq!(
        observed.expect("Index::holds must support every legal device index"),
        [last]
    );
    let runner = Ran {
        owner: NodeId(0),
        demand,
        allocation: None,
        workload: WorkloadId(0),
        created: true,
        at: None,
        ended: false,
        route: None,
    };
    let row = ran_text(&runner, &models, &spec.devices, &[last]).unwrap();
    assert!(row.starts_with(&format!("{encoded} running ")));
}

/// AC-T7.3.4: native preflight shares boot's binding parser before any recovery write/effect.
#[test]
fn recorded_device_specs_shares_valid_legacy_and_bad_binding_semantics() {
    let (mut spec, models, demand, _) = recovery_inputs();
    spec.devices[1].key = Some(101);
    let row = |atom: &str| format!("{atom} running {}", demand_text(&demand, &models).unwrap());
    let cpu = Demand {
        need: Resources {
            gpus: 0,
            ..demand.need
        },
        ask: Ask::default(),
        ..demand
    };
    let cpu_row = |atom: &str| format!("{atom} running {}", demand_text(&cpu, &models).unwrap());
    assert_eq!(
        recorded_device_specs(&cpu_row("0"), &spec.devices).unwrap(),
        None
    );
    assert_eq!(
        recorded_device_specs(&cpu_row("bindings1:0:0"), &spec.devices).unwrap(),
        Some(vec![])
    );
    let physical = row("bindings1:0:1:p:gpu:101");
    assert_eq!(
        recorded_device_specs(&physical, &spec.devices).unwrap(),
        Some(vec![spec.devices[1]])
    );
    assert_eq!(
        recorded_device_specs(&row("bindings1:0:1:s:0"), &spec.devices).unwrap(),
        Some(vec![spec.devices[0]])
    );
    let mut shifted = spec.devices.clone();
    shifted.swap(0, 1);
    shifted[0].load.memory_used = Some(shifted[0].size);
    assert_eq!(
        recorded_device_specs(&physical, &shifted).unwrap(),
        Some(vec![shifted[0]])
    );
    for atom in [
        "",
        "bindings2:0:0",
        "bindings1:0:1",
        "bindings1:0:3:s:0:s:0:s:0",
        "bindings1:0:1:s:1",
        "bindings1:0:1:s:2",
        "bindings1:0:1:s:65536",
        "bindings1:0:1:p:gpu:102",
        "bindings1:0:1:p:net:101",
        "bindings1:0:1:p:gpu:18446744073709551616",
        "bindings1:0:0:s:0",
    ] {
        let encoded = row(atom);
        assert!(
            matches!(
                recorded_device_specs(&encoded, &spec.devices),
                Err(Stop::Damaged(_))
            ),
            "{atom}"
        );
    }
    assert_eq!(spec.devices[1].load.memory_used, Some(0));
    assert_eq!(spec.devices[0].key, None);
    eprintln!(
        "recorded-specs legacy=None explicit_zero=Some_empty physical_key=101 reindex_preserved=true synthetic_none=true bad_claims=typed_Damaged catalog_unchanged=true records_writes=0 runtime_effects=0"
    );
}

/// AC-T7.4.1: live foreign-holder availability filters only new grants, preserving stable slots.
#[test]
fn unavailable_device_refuses_fresh_work_while_sibling_and_retained_binding_progress() {
    let (mut spec, models, demand, budget) = recovery_inputs();
    assert!(spec.devices.iter().all(|device| device.available));
    spec.devices[0].key = Some(101);
    spec.devices[1].key = Some(102);
    let mut unavailable = spec.clone();
    for device in &mut unavailable.devices {
        device.available = false;
    }
    let mut index = FitIndex::new(std::slice::from_ref(&unavailable)).unwrap();
    let baseline = index.key(NodeId(0)).unwrap();
    let refused = index.verdict(NodeId(0), &demand).unwrap();
    assert_ne!(refused, Verdict::Admits);
    assert!(index.fit(&demand, &Pick::default()).best.is_none());
    assert!(matches!(
        index.take(NodeId(0), WorkloadId(0), &demand),
        Err(crate::SimError::Resources(_))
    ));
    assert_eq!(index.key(NodeId(0)).unwrap(), baseline);
    assert!(index.holds(NodeId(0), WorkloadId(0)).is_empty());

    // GPU0 is idle but foreign-held; fresh work must choose available GPU1.
    spec.devices[0].available = false;
    let mut records = Memory::default();
    let mut node = boot_local(&spec, &models, budget, &mut records, |_| Boxed::Absent).unwrap();
    let (old, old_held) = create_confirmed(&mut node, &mut records, demand);
    assert_eq!(old_held, [1]);
    assert_eq!(
        node.assigned_device_specs(old).unwrap().unwrap()[0].key,
        Some(102)
    );
    let mut assignments = BTreeMap::from([(old, old_held.clone())]);
    // Availability alone may change at refresh; stable identity/shape and holds remain intact.
    spec.devices[0].available = true;
    spec.devices[1].available = false;
    let before = records.rows.clone();
    node.refresh_node(spec.clone()).unwrap();
    assert_eq!(records.rows, before);
    assert_eq!(node.assigned_devices(old), Some(old_held.clone()));
    assert!(!node.assigned_device_specs(old).unwrap().unwrap()[0].available);
    drop(node);
    let mut node = boot_local(&spec, &models, budget, &mut records, |key| {
        if assignments.contains_key(&key) {
            Boxed::Running
        } else {
            Boxed::Absent
        }
    })
    .unwrap();
    assert_eq!(node.assigned_devices(old), Some(old_held));
    let (fresh, fresh_held) = create_confirmed(&mut node, &mut records, demand);
    assert_eq!(fresh_held, [0]);
    assert_eq!(
        node.assigned_device_specs(fresh).unwrap().unwrap()[0].key,
        Some(101)
    );
    assert!(assignments.insert(fresh, fresh_held).is_none());
    assert_ne!(assignments[&old], assignments[&fresh]);
    for key in [old, fresh] {
        delete_confirmed(&mut node, &mut records, &mut assignments, key);
    }
    assert!(assignments.is_empty());
    assert_empty(&node, &records);
    eprintln!(
        "availability all_false_verdict={refused:?} all_false_grants=0 foreign_idle_gpu=0 first_grant=[1] refreshed_old=[1] reboot_old=[1] fresh_progress=[0] physical_old=(Gpu,102) physical_fresh=(Gpu,101) cleanup_holders=0 cleanup_charge=0 cleanup_rows=epoch_only native_gpu_effect=false"
    );
}
