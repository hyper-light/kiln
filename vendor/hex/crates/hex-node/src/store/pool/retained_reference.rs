//! Original-source control for the existing Store's last backing-reference fence.
//! These are injected pure record facts; no native scope/controller/GPU is created or released.
use super::*;
use hex_api::{
    Bounds, BoxId, BoxSpec, Class, ClassSpec, GpuEligibility, ImageArch, ImageOs, ImagePlatform,
    Network, OciSpec, Program, Rootfs, RouteKey, RouteReceipt,
};
use hex_core::record::{BoxRecord, NativeScope, Phase, ProcId};

fn cost() -> RuntimeCost {
    let fixed = hex_api::CapacityResources {
        cpu_milli: 1000,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    RuntimeCost {
        id: [3; 32],
        floor: fixed,
        ceiling: fixed,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: hex_api::CapacityResources::default(),
        overhead: hex_api::CapacityResources::default(),
    }
}

/// AC-T7.4.1: a Creating workload's immutable handoff receipt keeps the exact Released backing
/// generation present until that canonical workload is gone, including through prepare retries.
#[test]
fn a_referenced_backing_generation_cannot_be_removed_or_rearmed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
    let device = DeviceId {
        kind: hex_api::DeviceKind::Gpu,
        key: 7,
    };
    let row = store
        .prepare_pool_with(
            [0x55; 32],
            Execution::ShardsDedicated,
            cost(),
            &[device],
            || Ok(NativeScope::from_bytes([2; 16])),
        )
        .unwrap();
    let mut row = store
        .grant_pool(&BackingGrant {
            key: row.key,
            allocation: row
                .cost
                .allocate(hex_api::CapacityResources::default())
                .unwrap(),
            devices: vec![device],
        })
        .unwrap();
    row.init = Some(ProcId {
        pid: 7,
        start_ticks: 11,
    });
    store.put_pool(&row).unwrap();
    row.phase = BackingPhase::Draining;
    row.exit = Some(hex_api::Exit::Code(7));
    store.put_pool(&row).unwrap();
    row.phase = BackingPhase::Released;
    store.put_pool(&row).unwrap();
    let spec = BoxSpec {
        id: BoxId::new("creating-reference").unwrap(),
        class: ClassSpec::Shards(Box::new(OciSpec {
            rootfs: Rootfs::Dir(dir.path().to_path_buf()),
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
        devices: vec![device],
        allocation: row.allocation,
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
            backing: Some(row.key),
        }),
    };
    spec.check().unwrap();
    store
        .put_lifecycle(&BoxRecord::new(spec, Phase::Creating, Vec::new()))
        .unwrap();
    let id = BoxId::new("creating-reference").unwrap();
    let canonical = store.read_lifecycle(&id).unwrap().unwrap();
    assert!(canonical.native_scope.is_some());
    let (_, pool_path) = scoped_paths(
        &store.pool_directory().unwrap(),
        &name(&row.key.pool).unwrap(),
    )
    .unwrap();
    let box_path = store.directory().join("creating-reference.json");
    let pool_bytes = fs::read(&pool_path).unwrap();
    let box_bytes = fs::read(&box_path).unwrap();
    assert!(
        store.remove_pool(&row.key).is_err(),
        "removal erased a Creating Box's immutable backing reference"
    );
    assert_eq!(fs::read(&pool_path).unwrap(), pool_bytes);
    assert_eq!(fs::read(&box_path).unwrap(), box_bytes);
    let fills = std::cell::Cell::new(0);
    let same = store
        .prepare_pool_with(
            row.key.pool,
            Execution::ShardsDedicated,
            cost(),
            &[device],
            || {
                fills.set(fills.get() + 1);
                Ok(NativeScope::from_bytes([9; 16]))
            },
        )
        .unwrap();
    assert_eq!(same.key, row.key);
    assert_eq!(fills.get(), 0);
    let mut deleting = canonical;
    deleting.phase = Phase::Deleting;
    store.put_lifecycle(&deleting).unwrap();
    assert!(
        store.remove_pool(&row.key).is_err(),
        "Deleting retains the same reference until actual row removal"
    );
    store.remove_lifecycle(&id).unwrap();
    store.remove_pool(&row.key).unwrap();
    let next = store
        .prepare_pool_with(
            row.key.pool,
            Execution::ShardsDedicated,
            cost(),
            &[device],
            || Ok(NativeScope::from_bytes([9; 16])),
        )
        .unwrap();
    assert_ne!(next.key, row.key);
}
