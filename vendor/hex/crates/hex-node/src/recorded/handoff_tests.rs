//! AC-T7.4.1: SAME Store/Turns/Box transaction controls. Native callbacks are counted stand-ins;
//! injected positive Pool results do not establish actual host/VM termination or resource return.
use super::*;
use hex_api::{
    BackingGrant, BackingPhase, Bounds, CapacityResources, ClassSpec, DeviceKind, Execution,
    GpuEligibility, ImageArch, ImageOs, ImagePlatform, Network, OciSpec, Rootfs, RouteKey,
    RouteReceipt, RuntimeCost,
};
use std::cell::Cell;
use std::time::{Duration, Instant};

fn devices() -> [DeviceId; 2] {
    [
        DeviceId {
            kind: DeviceKind::Gpu,
            key: 7,
        },
        DeviceId {
            kind: DeviceKind::Gpu,
            key: 8,
        },
    ]
}
fn cost() -> RuntimeCost {
    let fixed = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: shards_api::min_memory_mib(std::env::consts::ARCH) << 20,
        disk_bytes: 0,
    };
    RuntimeCost {
        id: [3; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        overhead: CapacityResources::default(),
        guest_overhead: CapacityResources::default(),
    }
}
fn catalog() -> hex_runtime::devices::Catalog {
    hex_runtime::devices::Catalog::parse(b"hex-devices-1\ngpu\t7\tfixture-a\t4096\tshards-gpu\tmetal:0000000000000007\t\ngpu\t8\tfixture-b\t4096\tshards-gpu\tmetal:0000000000000008\t\n").unwrap()
}
fn live(store: &Store) -> PoolRecord {
    let pool = store
        .prepare_pool([0x77; 32], Execution::ShardsDedicated, cost(), &devices())
        .unwrap();
    let mut pool = store
        .grant_pool(&BackingGrant {
            key: pool.key,
            allocation: cost().allocate(CapacityResources::default()).unwrap(),
            devices: devices().to_vec(),
        })
        .unwrap();
    pool.init = Some(ProcId {
        pid: 11,
        start_ticks: 13,
    });
    store.put_pool(&pool).unwrap(); // Held native identity is canonical while still Granted.
    pool.phase = BackingPhase::Live;
    store.put_pool(&pool).unwrap();
    pool
}
fn box_spec(directory: &Path, pool: &PoolRecord, id: &str) -> BoxSpec {
    let spec = BoxSpec {
        id: BoxId::new(id).unwrap(),
        class: ClassSpec::Shards(Box::new(OciSpec {
            rootfs: Rootfs::Dir(directory.to_path_buf()),
            init: Program::of(&["/bin/true"]),
            mounts: Vec::new(),
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        })),
        bounds: Bounds {
            cpu_max_milli: Some(1000),
            memory_max: Some(hex_api::MIN_MEMORY),
            ..Bounds::default()
        },
        owner: None,
        devices: vec![devices()[0]],
        allocation: pool.allocation,
        route: Some(RouteReceipt {
            key: RouteKey {
                class: Class::Shards,
                image: ImagePlatform {
                    os: ImageOs::Linux,
                    arch: ImageArch::Amd64,
                },
                execution: Execution::ShardsDedicated,
                gpu: GpuEligibility::Any,
            },
            backing: Some(pool.key),
        }),
        band: 0,
    };
    spec.check().unwrap();
    spec
}
fn released(store: &Store, pool: &mut PoolRecord) {
    pool.phase = BackingPhase::Draining;
    pool.exit = Some(hex_api::Exit::Code(17));
    store.put_pool(pool).unwrap();
    pool.phase = BackingPhase::Released;
    store.put_pool(pool).unwrap();
}
fn no_launch(
    _: NativeRecord<'_>,
    _: &hex_runtime::devices::ManagedLaunch<'_>,
    _: shards_api::LaunchIo,
    _: Duration,
    _: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>,
) -> Result<ManagedReceipt, Refusal> {
    panic!("metadata/cancellation control cannot launch")
}
fn no_attach(
    _: NativeRecord<'_>,
    _: Option<&shards_api::Identity>,
) -> Result<ManagedReceipt, Refusal> {
    panic!("metadata/cancellation control cannot attach")
}
fn no_finalize(_: NativeRecord<'_>, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
    panic!("metadata/cancellation control cannot finalize")
}

/// Reserve S while full P remains Live, reject missing full keys/second consumer, cancel without
/// any native callback, then require exact Released before even native usage is permitted.
#[test]
fn subset_handoff_reservation_and_cancel_keep_full_source_and_native_gate() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let mut pool = live(&store);
    let catalog = catalog();
    let keys = catalog.conflict_ids(&pool.devices).unwrap();
    let original_pool = pool.encode().unwrap();
    let calls = Cell::new(0);
    let usage = |record: NativeRecord<'_>| {
        assert!(matches!(record, NativeRecord::Box(_)));
        calls.set(calls.get() + 1);
        Ok(Usage::default())
    };
    let native = Native {
        catalog: Some(&catalog),
        launch: &no_launch,
        attach: &no_attach,
        finalize: &no_finalize,
        usage: &usage,
        unsubmitted_pool: &|_| panic!("this control cannot check Prepared native submission"),
    };
    let spec = box_spec(directory.path(), &pool, "subset-reserved");
    let id = spec.id.try_clone().unwrap();
    handoff_transaction(&store,&id,&pool.key,&keys,&native,&mut |tx| {
        tx.put(&BoxRecord::new(spec.clone(),Phase::Creating,Vec::new()))?;
        let canonical=tx.read(&id)?.unwrap(); assert!(canonical.native_scope.is_some());
        assert_eq!(canonical.spec.devices,[devices()[0]]);
        assert!(matches!(tx.managed_usage(),Err(Refusal::Backing {key,cause:hex_api::LifecycleCause::Pending}) if key==pool.key));
        assert_eq!(calls.get(),0);
        let source=tx.read_pool(&pool.key)?.unwrap();
        assert_eq!(source.devices,devices());
        assert!(tx.put_pool(&source).is_err(),"Box transaction cannot mutate its source");
        let mut cancelled=canonical; cancelled.phase=Phase::Deleting; tx.put(&cancelled)?;
        assert!(tx.managed_usage().is_err());
        Ok(())
    }).unwrap();
    assert_eq!(
        store
            .read_pool(&pool.key.pool)
            .unwrap()
            .unwrap()
            .encode()
            .unwrap(),
        original_pool
    );
    assert_eq!(calls.get(), 0);
    let callbacks = Cell::new(0);
    assert!(
        handoff_transaction(&store, &id, &pool.key, &keys[..1], &native, &mut |_| {
            callbacks.set(1);
            Ok(())
        })
        .is_err()
    );
    let sibling = BoxId::new("second-consumer").unwrap();
    assert!(
        handoff_transaction(&store, &sibling, &pool.key, &keys, &native, &mut |_| {
            callbacks.set(1);
            Ok(())
        })
        .is_err()
    );
    assert_eq!(callbacks.get(), 0);
    let sibling = box_spec(directory.path(), &pool, "second-consumer");
    assert!(
        store
            .put_lifecycle(&BoxRecord::new(sibling, Phase::Creating, Vec::new()))
            .is_err()
    );
    assert!(
        store
            .read_lifecycle(&BoxId::new("second-consumer").unwrap())
            .unwrap()
            .is_none()
    );
    let canonical = store.read_lifecycle(&id).unwrap().unwrap();
    assert_eq!(canonical.phase, Phase::Deleting);
    check_backing_reference(&canonical, &pool).unwrap(); // structural only; adapter owes exact no-Intent proof
    let turns = Turns::new(
        "handoff",
        "test methods",
        8,
        hex_core::patience::Patience::measuring("test.handoff.preflight"),
    );
    let guard = preflight_handoff_turns(&turns, &store, &canonical, Some(&catalog))
        .unwrap()
        .unwrap();
    assert!(turns.try_take_pool(&pool.key, &[]).is_err());
    drop(guard);
    released(&store, &mut pool);
    assert_eq!(pool.devices, devices());
    assert_eq!(
        pool.allocation,
        Some(cost().allocate(CapacityResources::default()).unwrap())
    );
    handoff_transaction(&store, &id, &pool.key, &keys, &native, &mut |tx| {
        tx.managed_usage()?;
        Ok(())
    })
    .unwrap();
    assert_eq!(
        calls.get(),
        1,
        "positive post-release callback is nonvacuous, still not native proof"
    );
}

/// Whole-owner metadata accepts one exact subset reservation and rejects duplicate references,
/// stale source generation, forged profile/selected set, early native identity, or missing source.
#[test]
fn retained_handoff_preflight_joins_one_exact_source_before_any_effect() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let mut pool = live(&store);
    let catalog = catalog();
    let mut record = BoxRecord::new(
        box_spec(directory.path(), &pool, "retained-subset"),
        Phase::Creating,
        Vec::new(),
    );
    store.put_lifecycle(&record).unwrap();
    record = store.read_lifecycle(&record.spec.id).unwrap().unwrap();
    let mut records = store.retained_records().unwrap();
    preflight_records(&records, std::slice::from_ref(&pool), Some(&catalog)).unwrap();
    let before = std::fs::read(store.directory().join("retained-subset.json")).unwrap();
    let mut duplicate = record.clone();
    duplicate.spec.id = BoxId::new("duplicate-subset").unwrap();
    duplicate.native_scope = Some(hex_core::record::NativeScope::from_bytes([61; 16]));
    records.insert(duplicate.spec.id.try_clone().unwrap(), Ok(duplicate));
    assert!(preflight_records(&records, std::slice::from_ref(&pool), Some(&catalog)).is_err());
    records = store.retained_records().unwrap();
    for change in 0..4 {
        let mut bad = record.clone();
        match change {
            0 => {
                bad.spec
                    .route
                    .as_mut()
                    .unwrap()
                    .backing
                    .as_mut()
                    .unwrap()
                    .generation = [67; 16]
            }
            1 => bad.spec.allocation.as_mut().unwrap().cost.id = [71; 32],
            2 => {
                bad.spec.devices = vec![DeviceId {
                    kind: DeviceKind::Gpu,
                    key: 9,
                }]
            }
            _ => {
                bad.init = Some(ProcId {
                    pid: 73,
                    start_ticks: 79,
                })
            }
        }
        assert!(check_backing_reference(&bad, &pool).is_err());
    }
    assert!(preflight_records(&records, &[], Some(&catalog)).is_err());
    let mut cancelled = record.clone();
    cancelled.phase = Phase::Deleting;
    check_backing_reference(&cancelled, &pool).unwrap(); // no native/absence permission
    cancelled.init = Some(ProcId {
        pid: 73,
        start_ticks: 79,
    });
    assert!(check_backing_reference(&cancelled, &pool).is_err());
    released(&store, &mut pool);
    check_backing_reference(&cancelled, &pool).unwrap(); // Released delegates real Intent/identity proof to adapter
    assert_eq!(
        std::fs::read(store.directory().join("retained-subset.json")).unwrap(),
        before
    );
}

fn boot(scope: [u8; 16]) -> shards_api::PreparedBoot {
    let root = if cfg!(windows) {
        Path::new("C:\\")
    } else {
        Path::new("/")
    };
    let guest = shards_api::GuestSpec {
        kernel: root.join("kernel"),
        init: shards_api::Init::Executable(root.join("init")),
        cmdline: String::from("console=ttyS0"),
        vcpus: 1,
        memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH),
        gpu: true,
        gpu_devices: vec![shards_api::PhysicalGpu::MetalRegistry(7)],
        disks: Vec::new(),
        shares: Vec::new(),
        ports: vec![1024],
        binary: None,
    };
    shards_api::prepare_boot(&guest, &root.join("shards"), &root.join("v"), 0)
        .unwrap()
        .with_gpu_report(scope, Instant::now() + Duration::from_secs(60))
        .unwrap()
}
fn io() -> shards_api::LaunchIo {
    let (_, stdout) = std::io::pipe().unwrap();
    let (_, stderr) = std::io::pipe().unwrap();
    let (_, report) = std::io::pipe().unwrap();
    shards_api::LaunchIo {
        stdout,
        stderr,
        gpu_report: Some(report),
    }
}
/// The same concrete native bridge sees only S after full-P Released. Before that positive fact,
/// even a valid prepared report and exact Box scope cannot reach the counted native callback.
#[test]
fn subset_native_launch_waits_for_full_source_release_then_passes_only_selected_devices() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let mut pool = live(&store);
    let catalog = catalog();
    let keys = catalog.conflict_ids(&pool.devices).unwrap();
    let calls = Cell::new(0);
    let source = pool.key;
    let launch = |record: NativeRecord<'_>,
                  plan: &hex_runtime::devices::ManagedLaunch<'_>,
                  _: shards_api::LaunchIo,
                  _: Duration,
                  _: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>|
     -> Result<ManagedReceipt, Refusal> {
        assert_eq!(record.devices(), [devices()[0]]);
        let hex_runtime::devices::ManagedLaunch::Shards(plan) = plan else {
            panic!("Shards fixture")
        };
        assert_eq!(
            plan.gpu_report_request().unwrap().selected(),
            [shards_api::PhysicalGpu::MetalRegistry(7)]
        );
        assert_eq!(
            store.read_pool(&source.pool).unwrap().unwrap().phase,
            BackingPhase::Released
        );
        calls.set(calls.get() + 1);
        Err(Refusal::Exhausted {
            what: "counted subset native launch",
            bound: 0,
        })
    };
    let usage = |_: NativeRecord<'_>| -> Result<Usage, Refusal> {
        panic!("launch control cannot sample native usage")
    };
    let native = Native {
        catalog: Some(&catalog),
        launch: &launch,
        attach: &no_attach,
        finalize: &no_finalize,
        usage: &usage,
        unsubmitted_pool: &|_| panic!("this control cannot check Prepared native submission"),
    };
    let spec = box_spec(directory.path(), &pool, "launch-subset");
    let id = spec.id.try_clone().unwrap();
    handoff_transaction(&store, &id, &pool.key, &keys, &native, &mut |tx| {
        tx.put(&BoxRecord::new(spec.clone(), Phase::Creating, Vec::new()))?;
        let record = tx.read(&id)?.unwrap();
        let plan = boot(*record.native_scope.unwrap().bytes());
        assert!(matches!(
            tx.launch_managed(
                &hex_runtime::devices::ManagedLaunch::Shards(&plan),
                io(),
                Duration::from_secs(60)
            ),
            Err(Refusal::Backing {
                cause: hex_api::LifecycleCause::Pending,
                ..
            })
        ));
        Ok(())
    })
    .unwrap();
    assert_eq!(calls.get(), 0);
    released(&store, &mut pool);
    handoff_transaction(&store, &id, &pool.key, &keys, &native, &mut |tx| {
        let record = tx.read(&id)?.unwrap();
        let plan = boot(*record.native_scope.unwrap().bytes());
        assert!(matches!(
            tx.launch_managed(
                &hex_runtime::devices::ManagedLaunch::Shards(&plan),
                io(),
                Duration::from_secs(60)
            ),
            Err(Refusal::Exhausted {
                what: "counted subset native launch",
                bound: 0
            })
        ));
        Ok(())
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(pool.devices, devices());
}

/// The source metadata turn serializes a new ordinary put_lifecycle reference against removal.
/// Either publication wins and retains P, or removal wins and publication refuses; no orphan Box.
#[test]
fn concurrent_source_removal_and_reference_publication_keep_a_canonical_join() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let mut pool = live(&store);
    released(&store, &mut pool);
    let row = BoxRecord::new(
        box_spec(directory.path(), &pool, "concurrent-reference"),
        Phase::Creating,
        Vec::new(),
    );
    let start = std::sync::Barrier::new(2);
    let (published, removed) = std::thread::scope(|scope| {
        let publish = scope.spawn(|| {
            start.wait();
            store.put_lifecycle(&row)
        });
        let remove = scope.spawn(|| {
            start.wait();
            store.remove_pool(&pool.key)
        });
        (publish.join().unwrap(), remove.join().unwrap())
    });
    let retained_box = store.read_lifecycle(&row.spec.id).unwrap();
    let retained_pool = store.read_pool(&pool.key.pool).unwrap();
    match (published, removed) {
        (Ok(()), Err(_)) => {
            assert!(retained_box.is_some());
            assert_eq!(retained_pool.unwrap().key, pool.key);
        }
        (Err(_), Ok(())) => {
            assert!(retained_box.is_none());
            assert!(retained_pool.is_none());
        }
        other => panic!("one serialized transition must succeed: {other:?}"),
    }
}

/// A malformed/incomplete retained owner cannot be treated as proof of no references. The same
/// strict reader refuses removal, preserves both files, and permits it only after owned repair.
#[test]
fn malformed_retained_owner_never_frees_a_released_backing_reference() {
    let directory = tempfile::tempdir().unwrap();
    let disk = directory.path().join("disk");
    let store = Store::open(&disk, &directory.path().join("run")).unwrap();
    let mut pool = live(&store);
    released(&store, &mut pool);
    let pool_path = disk
        .join(".pools")
        .join(format!("{}.json", "77".repeat(32)));
    let before = std::fs::read(&pool_path).unwrap();
    let damaged = disk.join("uncertain-reference.json");
    let bytes = b"{incomplete immutable route reference\n";
    std::fs::write(&damaged, bytes).unwrap();
    assert!(store.remove_pool(&pool.key).is_err());
    assert_eq!(std::fs::read(&damaged).unwrap(), bytes);
    assert_eq!(std::fs::read(&pool_path).unwrap(), before);
    std::fs::remove_file(&damaged).unwrap();
    store.remove_pool(&pool.key).unwrap();
    assert!(store.read_pool(&pool.key.pool).unwrap().is_none());
}

/// Existing update callers change init/exit/abandoned, not admission provenance. A Box-only update
/// cannot add/drop a source reference outside source-before-Box publication ordering.
#[test]
fn ordinary_accounting_update_cannot_add_or_erase_a_backing_reference() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let mut pool = live(&store);
    released(&store, &mut pool);
    let mut cpu = box_spec(directory.path(), &pool, "ordinary-accounting");
    let route = cpu.route;
    cpu.class = ClassSpec::Process;
    cpu.devices.clear();
    cpu.allocation = None;
    cpu.route = None;
    cpu.check().unwrap();
    let id = cpu.id.try_clone().unwrap();
    store
        .put(&BoxRecord::new(cpu, Phase::Ready, Vec::new()))
        .unwrap();
    assert!(
        store
            .update(&id, |row| {
                row.abandoned += 1;
                true
            })
            .unwrap()
    );
    let before = store.get(&id).unwrap().unwrap().unwrap();
    assert!(
        store
            .update(&id, |row| {
                row.spec.route = route;
                true
            })
            .is_err()
    );
    assert_eq!(store.get(&id).unwrap().unwrap().unwrap(), before);
    let row = BoxRecord::new(
        box_spec(directory.path(), &pool, "immutable-reference"),
        Phase::Creating,
        Vec::new(),
    );
    store.put_lifecycle(&row).unwrap();
    let canonical = store.read_lifecycle(&row.spec.id).unwrap().unwrap();
    let before = std::fs::read(store.directory().join("immutable-reference.json")).unwrap();
    assert!(
        store
            .update(&row.spec.id, |row| {
                row.spec.route = None;
                true
            })
            .is_err()
    );
    let mut stripped = canonical;
    stripped.spec.route = None;
    assert!(store.put(&stripped).is_err());
    assert_eq!(
        std::fs::read(store.directory().join("immutable-reference.json")).unwrap(),
        before
    );
}

/// Legacy missing-source damage cannot authorize a fresh generation while a canonical Box still
/// names the configuration. This private-file fault leaves the Box bytes and absent source intact.
#[test]
fn missing_source_rearm_refuses_a_retained_immutable_reference() {
    let directory = tempfile::tempdir().unwrap();
    let disk = directory.path().join("disk");
    let store = Store::open(&disk, &directory.path().join("run")).unwrap();
    let mut pool = live(&store);
    released(&store, &mut pool);
    let record = BoxRecord::new(
        box_spec(directory.path(), &pool, "missing-source-reference"),
        Phase::Creating,
        Vec::new(),
    );
    store.put_lifecycle(&record).unwrap();
    let path = disk.join("missing-source-reference.json");
    let before = std::fs::read(&path).unwrap();
    let pool_path = disk
        .join(".pools")
        .join(format!("{}.json", "77".repeat(32)));
    std::fs::remove_file(&pool_path).unwrap(); // explicit owned fault, not a positive native release
    assert!(
        store
            .prepare_pool(pool.key.pool, pool.execution, pool.cost, &pool.devices)
            .is_err()
    );
    assert!(!pool_path.exists());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// Original control for lazy coalescing: a no-ref Draining observer cannot acquire its first
/// consumer after draining began. An already-canonical consumer still reenters/cancels normally.
/// This is an UNRUN source hypothesis, distinct from the reproduced c9b removal failure.
#[test]
fn draining_observer_refuses_first_reference_but_retains_existing_consumer() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(
        &directory.path().join("disk"),
        &directory.path().join("run"),
    )
    .unwrap();
    let mut source = live(&store);
    let original = BoxRecord::new(
        box_spec(directory.path(), &source, "existing-before-drain"),
        Phase::Creating,
        Vec::new(),
    );
    store.put_lifecycle(&original).unwrap();
    let mut canonical = store.read_lifecycle(&original.spec.id).unwrap().unwrap();
    source.phase = BackingPhase::Draining;
    store.put_pool(&source).unwrap();
    store.put_lifecycle(&canonical).unwrap();
    canonical.phase = Phase::Deleting;
    store.put_lifecycle(&canonical).unwrap();
    store.remove_lifecycle(&canonical.spec.id).unwrap();
    assert!(store.read_lifecycle(&canonical.spec.id).unwrap().is_none());

    // The same canonical Draining scope now has no reference. The old proposal must refuse
    // before generating a Box scope or publishing any bytes, even on the ordinary Store path.
    let proposal = BoxRecord::new(
        box_spec(directory.path(), &source, "stale-new-after-drain"),
        Phase::Creating,
        Vec::new(),
    );
    proposal.spec.check().unwrap();
    source.check().unwrap();
    preflight_records(
        &store.retained_records().unwrap(),
        std::slice::from_ref(&source),
        Some(&catalog()),
    )
    .unwrap();
    let before = source.encode().unwrap();
    let result = store.put_lifecycle(&proposal);
    assert!(
        result.is_err(),
        "Draining no-ref observer admitted a new consuming reference"
    );
    assert!(store.read_lifecycle(&proposal.spec.id).unwrap().is_none());
    assert_eq!(
        store
            .read_pool(&source.key.pool)
            .unwrap()
            .unwrap()
            .encode()
            .unwrap(),
        before
    );
}
