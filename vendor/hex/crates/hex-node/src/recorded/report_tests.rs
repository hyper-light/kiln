//! AC-T7.4.1: actual Store/transaction boundary with counted native callbacks. Selectors here are
//! pure fixture values, never evidence of live physical hardware or a native launch.
use super::*;
use hex_api::{BackingGrant, CapacityResources, DeviceKind, Execution, RuntimeCost};
use std::cell::Cell;
use std::time::{Duration, Instant};

fn cost() -> RuntimeCost {
    let guest = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: shards_api::min_memory_mib(std::env::consts::ARCH) << 20,
        disk_bytes: 0,
    };
    RuntimeCost {
        id: [1; 32],
        floor: guest,
        ceiling: guest,
        quantum: CapacityResources {
            cpu_milli: 1000,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    }
}

fn boot(selected: Vec<shards_api::PhysicalGpu>) -> shards_api::PreparedBoot {
    let root = if cfg!(windows) { "C:\\" } else { "/" };
    let root = Path::new(root);
    let spec = shards_api::GuestSpec {
        kernel: root.join("kernel"),
        init: shards_api::Init::Executable(root.join("init")),
        cmdline: String::from("console=ttyS0"),
        vcpus: 1,
        memory_mib: shards_api::min_memory_mib(std::env::consts::ARCH),
        gpu: !selected.is_empty(),
        gpu_devices: selected,
        disks: Vec::new(),
        shares: Vec::new(),
        ports: vec![1024],
        binary: None,
    };
    shards_api::prepare_boot(&spec, &root.join("shards"), &root.join("v"), 0).unwrap()
}

fn io(report: bool) -> shards_api::LaunchIo {
    let (_, stdout) = std::io::pipe().unwrap();
    let (_, stderr) = std::io::pipe().unwrap();
    let gpu_report = if report {
        let (_, writer) = std::io::pipe().unwrap();
        Some(writer)
    } else {
        None
    };
    shards_api::LaunchIo {
        stdout,
        stderr,
        gpu_report,
    }
}

#[test]
fn canonical_report_is_required_before_the_recorded_native_callback() {
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk");
    let store = Store::open(&disk, &dir.path().join("run")).unwrap();
    let devices = [
        DeviceId {
            kind: DeviceKind::Gpu,
            key: 7,
        },
        DeviceId {
            kind: DeviceKind::Gpu,
            key: 8,
        },
    ];
    let catalog = hex_runtime::devices::Catalog::parse(
        b"hex-devices-1\ngpu\t7\tfixture-a\t4096\tshards-gpu\tmetal:0000000000000007\t\ngpu\t8\tfixture-b\t4096\tshards-gpu\tmetal:0000000000000008\t\n",
    ).unwrap();
    let row = store
        .prepare_pool([0x66; 32], Execution::ShardsDedicated, cost(), &devices)
        .unwrap();
    let key = row.key;
    let calls = Cell::new(0usize);
    let launch =
        |_: NativeRecord<'_>,
         _: &hex_runtime::devices::ManagedLaunch<'_>,
         io: shards_api::LaunchIo,
         remaining: Duration,
         _: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>| {
            assert!(io.gpu_report.is_some());
            assert!(!remaining.is_zero() && remaining <= Duration::from_secs(60));
            calls.set(calls.get() + 1);
            Err(Refusal::Exhausted {
                what: "counted native launcher",
                bound: 0,
            })
        };
    let attach =
        |_: NativeRecord<'_>,
         _: Option<&shards_api::Identity>|
         -> Result<ManagedReceipt, Refusal> { panic!("report validation cannot attach") };
    let finalize = |_: NativeRecord<'_>, _: Option<&shards_api::Identity>| -> Result<(), Refusal> {
        panic!("report validation cannot finalize")
    };
    let usage = |_: NativeRecord<'_>| -> Result<Usage, Refusal> {
        panic!("report validation cannot sample native usage")
    };
    let native = Native {
        catalog: Some(&catalog),
        launch: &launch,
        attach: &attach,
        finalize: &finalize,
        usage: &usage,
        unsubmitted_pool: &|_| panic!("this control cannot check Prepared native submission"),
    };
    pool_transaction(&store, &key, &devices, &native, &mut |tx| {
        tx.grant_pool(&BackingGrant {
            key,
            allocation: cost().allocate(CapacityResources::default())?,
            devices: devices.to_vec(),
        })?;
        let path = disk
            .join(".pools")
            .join(format!("{}.json", "66".repeat(32)));
        let before = std::fs::read(&path).unwrap();
        let physical = [
            shards_api::PhysicalGpu::MetalRegistry(7),
            shards_api::PhysicalGpu::MetalRegistry(8),
        ];
        let mut foreign = key.generation;
        foreign[0] ^= 1;
        for (requested, selected, writer) in [
            (Some(foreign), physical.to_vec(), true),
            (Some(key.generation), physical.to_vec(), false),
            (None, physical.to_vec(), true),
            (None, physical.to_vec(), false),
            (Some(key.generation), vec![physical[0]], true),
            (
                Some(key.generation),
                vec![physical[0], shards_api::PhysicalGpu::MetalRegistry(9)],
                true,
            ),
        ] {
            let plan = boot(selected);
            let plan = if let Some(scope) = requested {
                plan.with_gpu_report(scope, Instant::now() + Duration::from_secs(60))
                    .unwrap()
            } else {
                plan
            };
            let result = tx.launch_managed(
                &hex_runtime::devices::ManagedLaunch::Shards(&plan),
                io(writer),
                Duration::from_secs(3600),
            );
            assert!(matches!(result, Err(Refusal::Backing { key:actual, .. }) if actual==key));
            assert_eq!(
                calls.get(),
                0,
                "malformed report reached the native callback"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
        let plan = boot(physical.to_vec())
            .with_gpu_report(key.generation, Instant::now() + Duration::from_secs(60))
            .unwrap();
        assert!(matches!(
            tx.launch_managed(
                &hex_runtime::devices::ManagedLaunch::Shards(&plan),
                io(true),
                Duration::from_secs(3600)
            ),
            Err(Refusal::Exhausted {
                what: "counted native launcher",
                bound: 0
            })
        ));
        assert_eq!(
            calls.get(),
            1,
            "exact canonical scope/selection must reach the native owner once"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        Ok(())
    })
    .unwrap();
}

#[test]
fn cpu_none_reaches_the_same_callback_without_a_catalog_or_report() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
    let row = store
        .prepare_pool([0x77; 32], Execution::ShardsShared, cost(), &[])
        .unwrap();
    let calls = Cell::new(0usize);
    let launch =
        |_: NativeRecord<'_>,
         _: &hex_runtime::devices::ManagedLaunch<'_>,
         io: shards_api::LaunchIo,
         _: Duration,
         _: &mut dyn FnMut(&shards_api::Identity) -> Result<(), Refusal>| {
            assert!(io.gpu_report.is_none());
            calls.set(calls.get() + 1);
            Err(Refusal::Exhausted {
                what: "counted native launcher",
                bound: 0,
            })
        };
    let attach = |_: NativeRecord<'_>,
                  _: Option<&shards_api::Identity>|
     -> Result<ManagedReceipt, Refusal> { panic!("CPU control cannot attach") };
    let finalize = |_: NativeRecord<'_>, _: Option<&shards_api::Identity>| -> Result<(), Refusal> {
        panic!("CPU control cannot finalize")
    };
    let usage =
        |_: NativeRecord<'_>| -> Result<Usage, Refusal> { panic!("CPU control cannot sample") };
    let native = Native {
        catalog: None,
        launch: &launch,
        attach: &attach,
        finalize: &finalize,
        usage: &usage,
        unsubmitted_pool: &|_| panic!("this control cannot check Prepared native submission"),
    };
    pool_transaction(&store, &row.key, &[], &native, &mut |tx| {
        tx.grant_pool(&BackingGrant {
            key: row.key,
            allocation: cost().allocate(CapacityResources::default())?,
            devices: Vec::new(),
        })?;
        let plan = boot(Vec::new());
        assert!(matches!(
            tx.launch_managed(
                &hex_runtime::devices::ManagedLaunch::Shards(&plan),
                io(false),
                Duration::from_secs(60)
            ),
            Err(Refusal::Exhausted {
                what: "counted native launcher",
                bound: 0
            })
        ));
        assert_eq!(calls.get(), 1);
        Ok(())
    })
    .unwrap();
}
