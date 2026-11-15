//! The real Driver/peer grant seam, with pure runtime callbacks and private regular-file rows.
use super::*;
use hex_api::{
    BackingGrant, BackingKey, BackingOffer, BackingPhase, Capacity, CapacityResources, DeviceId,
    DeviceKind, Execution, GpuEligibility, ImageArch, ImageOs, ImagePlatform, LifecycleCause, Proc,
    Program, RouteKey, RouteOffer, RuntimeCost, Signal, Stdio, Usage,
};
use hex_sim::model::{Classes, RuntimeRoutes};
use std::sync::{
    Weak,
    atomic::{AtomicUsize, Ordering},
};

fn fixture() -> (Capacity, NodeSpec, BackingKey) {
    let key = BackingKey {
        pool: [41; 32],
        generation: [42; 16],
    };
    let guest = CapacityResources {
        cpu_milli: 1_000,
        mem_bytes: 128 << 20,
        disk_bytes: 0,
    };
    let cost = RuntimeCost {
        id: [43; 32],
        floor: guest,
        ceiling: guest,
        quantum: CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    };
    let total = CapacityResources {
        cpu_milli: 4_000,
        mem_bytes: 512 << 20,
        disk_bytes: 0,
    };
    let image = ImagePlatform {
        os: ImageOs::Linux,
        arch: ImageArch::Amd64,
    };
    let routes = vec![
        RouteOffer {
            key: RouteKey {
                class: hex_api::Class::Process,
                image,
                execution: Execution::Native,
                gpu: GpuEligibility::None,
            },
            cost: None,
            backing: None,
        },
        RouteOffer {
            key: RouteKey {
                class: hex_api::Class::Firecracker,
                image,
                execution: Execution::Firecracker,
                gpu: GpuEligibility::Required,
            },
            cost: Some(cost),
            backing: Some(key),
        },
    ];
    let report = Capacity {
        own: Default::default(),
        id: "same native owner".into(),
        total,
        free: total,
        devices: Vec::new(),
        cost: None,
        routes: routes.clone(),
        device_routes: Vec::new(),
        backings: vec![BackingOffer {
            guest: Default::default(),
            key,
            cost,
            phase: BackingPhase::Prepared,
            devices: vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 7,
            }],
        }],
        unavailable: Vec::new(),
        learned: None,
        node_vms: Vec::new(),
        unlearned: None,
    };
    report.check().unwrap();
    let mut node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: total.cpu_milli,
            mem_bytes: total.mem_bytes,
            disk_bytes: 0,
            gpus: 1,
        },
    );
    // Only a provisioning ID exists here; no measured model/VRAM/load is fabricated.
    node.devices.clear();
    node.classes = Classes::NONE.with(Class::Oci).with(Class::Firecracker);
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: routes,
            device_routes: Vec::new(),
            backings: vec![report.backings[0].try_clone().unwrap()],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    (report, node, key)
}

#[derive(Clone, Copy)]
enum ReleaseMode {
    AckOnly,
    Cleanup(CleanupReply),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CleanupReply {
    Released,
    Uncertain,
    WrongGeneration,
    WrongSelection,
    StillDraining,
}

struct Callback {
    sim: hex_api::sim::SimRuntime,
    report: Capacity,
    node: NodeSpec,
    driver: Mutex<Weak<Driver>>,
    calls: AtomicUsize,
    releases: AtomicUsize,
    release_mode: ReleaseMode,
    wrong_generation: bool,
    exposes_firecracker: bool,
}

impl Runtime for Callback {
    fn classes(&self) -> Vec<hex_api::Class> {
        if self.exposes_firecracker {
            vec![hex_api::Class::Process, hex_api::Class::Firecracker]
        } else {
            vec![hex_api::Class::Process]
        }
    }
    fn capacity(&self, _: hex_api::Class) -> Result<Option<Capacity>, Refusal> {
        Ok(Some(self.report.try_clone()?))
    }
    fn initialize_backing(&self, grant: BackingGrant) -> Result<BackingOffer, Refusal> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let driver = self.driver.lock().unwrap().upgrade().unwrap();
        assert!(
            driver.operation.try_lock().is_err(),
            "same operation owns native effects"
        );
        let mut core = driver
            .state
            .try_lock()
            .expect("native callback must not hold core lock");
        assert!(
            core.node
                .refresh_node(self.node.try_clone().unwrap())
                .is_err(),
            "Prepared must already be durably reserved in the same Index before callback"
        );
        drop(core);
        grant.check()?;
        assert_eq!(grant.key, self.report.backings[0].key);
        assert_eq!(grant.devices, self.report.backings[0].devices);
        assert_eq!(grant.allocation, self.report.backings[0].allocation()?);
        let mut observed = self.report.backings[0].try_clone()?;
        observed.phase = BackingPhase::Live;
        if self.wrong_generation {
            observed.key.generation = [99; 16];
        }
        Ok(observed)
    }
    fn release_backing(&self, key: BackingKey) -> Result<BackingOffer, Refusal> {
        let call = self.releases.fetch_add(1, Ordering::SeqCst);
        let driver = self.driver.lock().unwrap().upgrade().unwrap();
        assert!(driver.operation.try_lock().is_err());
        let core = driver
            .state
            .try_lock()
            .expect("release RPC cannot retain core mutex");
        let mut observed = self.report.backings[0].try_clone()?;
        observed.phase = BackingPhase::Released;
        if let ReleaseMode::Cleanup(reply) = self.release_mode
            && call == 0
        {
            assert!(
                !core.node.backing_released(key).unwrap(),
                "native cleanup must finish before either Index refund or ACK"
            );
            match reply {
                CleanupReply::Released => {}
                CleanupReply::Uncertain => {
                    return Err(Refusal::Backing {
                        key,
                        cause: LifecycleCause::Uncertain,
                        why: None,
                    });
                }
                CleanupReply::WrongGeneration => observed.key.generation = [99; 16],
                CleanupReply::WrongSelection => observed.devices.clear(),
                CleanupReply::StillDraining => observed.phase = BackingPhase::Draining,
            }
        } else {
            assert!(
                core.node.backing_released(key).unwrap(),
                "old Released receipt must precede native rearm acknowledgement"
            );
        }
        Ok(observed)
    }
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.sim.create(spec)
    }
    fn start(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.start(id)
    }
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.exec(id, program, stdio)
    }
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal> {
        self.sim.run(spec, program, stdio)
    }
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal> {
        self.sim.signal(id, signal)
    }
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal> {
        self.sim.inspect(id)
    }
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal> {
        self.sim.usage(id)
    }
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal> {
        self.sim.list()
    }
    fn delete(&self, id: &BoxId) -> Result<(), Refusal> {
        self.sim.delete(id)
    }
}

/// AC-T7.4.1: reserve real provisioning IDs before native boot, without calling a second picker,
/// and retain the exact grant when a native reply supplies a different generation.
#[test]
fn driver_reserves_the_exact_backing_before_unlocked_native_callback() {
    for wrong_generation in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (report, node, key) = fixture();
        let runtime = Arc::new(Callback {
            sim: hex_api::sim::SimRuntime::default(),
            report,
            node: node.try_clone().unwrap(),
            driver: Mutex::new(Weak::new()),
            calls: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
            release_mode: ReleaseMode::AckOnly,
            wrong_generation,
            exposes_firecracker: true,
        });
        let driver = Arc::new(
            Driver::boot_in(
                Identity {
                    namespace: "00112233445566778899aabbccddeeff",
                    node: NodeId(0),
                },
                std::slice::from_ref(&node),
                &[],
                Budget { owned: 4, ran: 4 },
                Pick::default(),
                dir.path(),
                Runtimes::new([(Class::Firecracker, runtime.clone() as Arc<dyn Runtime>)]).unwrap(),
            )
            .unwrap(),
        );
        *runtime.driver.lock().unwrap() = Arc::downgrade(&driver);
        let result = driver.initialize_backing(Class::Firecracker, key);
        if wrong_generation {
            assert!(matches!(result, Err(Error::Runtime(Refusal::Backing {
                key: actual, cause: LifecycleCause::Uncertain,
                why: Some(_),
            })) if actual == key));
            assert!(driver.state.lock().unwrap().admission.is_some());
        } else {
            assert!(result.unwrap());
        }
        assert_eq!(runtime.calls.load(Ordering::SeqCst), 1);
        let mut core = driver.state.lock().unwrap();
        assert!(
            core.node.refresh_node(node.try_clone().unwrap()).is_err(),
            "native mismatch/absence cannot roll back held charge or physical claims"
        );
        assert_eq!(
            core.node.grant_backing(key).unwrap().devices,
            vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 7
            }]
        );
    }
}

/// AC-T7.4.1: an OCI/native hello cannot smuggle a Firecracker route through a composite catalog.
#[test]
fn route_snapshot_requires_the_real_backend_in_the_originating_endpoint() {
    let (report, node, _) = fixture();
    let runtime = Arc::new(Callback {
        sim: hex_api::sim::SimRuntime::default(),
        report,
        node,
        driver: Mutex::new(Weak::new()),
        calls: AtomicUsize::new(0),
        releases: AtomicUsize::new(0),
        release_mode: ReleaseMode::AckOnly,
        wrong_generation: false,
        exposes_firecracker: false,
    });
    let runtimes = Runtimes::new([(Class::Oci, runtime.clone() as Arc<dyn Runtime>)]).unwrap();
    assert!(runtimes.capacities().is_err());
    assert_eq!(runtime.calls.load(Ordering::SeqCst), 0);
}

/// AC-T7.4.1: a new generation cannot hide the old positive release fact from either existing
/// view. Native rearm is acknowledged only after that exact terminal snapshot was consumed.
#[test]
fn native_rearm_waits_for_the_original_released_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let (mut report, mut node, _) = fixture();
    report.backings[0].phase = BackingPhase::Live;
    let catalog = node.routes.as_ref().unwrap();
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: catalog.offers.clone(),
            device_routes: catalog.device_routes.clone(),
            backings: vec![report.backings[0].try_clone().unwrap()],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    let runtime = Arc::new(Callback {
        sim: hex_api::sim::SimRuntime::default(),
        report,
        node: node.try_clone().unwrap(),
        driver: Mutex::new(Weak::new()),
        calls: AtomicUsize::new(0),
        releases: AtomicUsize::new(0),
        release_mode: ReleaseMode::AckOnly,
        wrong_generation: false,
        exposes_firecracker: true,
    });
    let driver = Arc::new(
        Driver::boot_in(
            Identity {
                namespace: "00112233445566778899aabbccddeeff",
                node: NodeId(0),
            },
            std::slice::from_ref(&node),
            &[],
            Budget { owned: 4, ran: 4 },
            Pick::default(),
            dir.path(),
            Runtimes::new([(Class::Firecracker, runtime.clone() as Arc<dyn Runtime>)]).unwrap(),
        )
        .unwrap(),
    );
    *runtime.driver.lock().unwrap() = Arc::downgrade(&driver);
    let mut released = runtime.report.backings[0].try_clone().unwrap();
    released.phase = BackingPhase::Released;
    assert!(
        driver
            .release_backing(Class::Firecracker, &released)
            .is_err()
    );
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
    let catalog = node.routes.as_ref().unwrap();
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: catalog.offers.clone(),
            device_routes: catalog.device_routes.clone(),
            backings: vec![released.try_clone().unwrap()],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    driver.refresh_node(node.try_clone().unwrap()).unwrap();
    driver
        .release_backing(Class::Firecracker, &released)
        .unwrap();
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 1);
    let mut prepared = released;
    prepared.key.generation = [44; 16];
    prepared.phase = BackingPhase::Prepared;
    let catalog = node.routes.as_ref().unwrap();
    let mut offers = catalog.offers.clone();
    offers[1].backing = Some(prepared.key);
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers,
            device_routes: catalog.device_routes.clone(),
            backings: vec![prepared],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    driver.refresh_node(node).unwrap();
    assert!(
        driver
            .release_backing(Class::Firecracker, &runtime.report.backings[0])
            .is_err()
    );
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 1);
}

/// AC-T7.4.1: the actual Driver retains its grant through native cleanup, validates the full
/// receipt, consumes Released in both Index views, and only then makes a separate explicit ACK.
/// These are pure runtime callbacks/private file rows, not native VM termination witnesses.
#[test]
fn canonical_draining_cleanup_refunds_only_exact_release_before_separate_ack() {
    for reply in [
        CleanupReply::Released,
        CleanupReply::Uncertain,
        CleanupReply::WrongGeneration,
        CleanupReply::WrongSelection,
        CleanupReply::StillDraining,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (report, node, key) = fixture();
        let runtime = Arc::new(Callback {
            sim: hex_api::sim::SimRuntime::default(),
            report,
            node: node.try_clone().unwrap(),
            driver: Mutex::new(Weak::new()),
            calls: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
            release_mode: ReleaseMode::Cleanup(reply),
            wrong_generation: false,
            exposes_firecracker: true,
        });
        let driver = Arc::new(
            Driver::boot_in(
                Identity {
                    namespace: "00112233445566778899aabbccddeeff",
                    node: NodeId(0),
                },
                std::slice::from_ref(&node),
                &[],
                Budget { owned: 4, ran: 4 },
                Pick::default(),
                dir.path(),
                Runtimes::new([(Class::Firecracker, runtime.clone() as Arc<dyn Runtime>)]).unwrap(),
            )
            .unwrap(),
        );
        *runtime.driver.lock().unwrap() = Arc::downgrade(&driver);
        assert!(driver.initialize_backing(Class::Firecracker, key).unwrap());
        let mut draining = runtime.report.backings[0].try_clone().unwrap();
        draining.phase = BackingPhase::Draining;
        let result = driver.drain_backing(Class::Firecracker, &draining);
        assert_eq!(runtime.releases.load(Ordering::SeqCst), 1);
        if reply == CleanupReply::Released {
            let released = result.unwrap();
            assert_eq!(released.key, key);
            assert_eq!(released.devices, draining.devices);
            assert_eq!(released.cost, draining.cost);
            assert_eq!(released.phase, BackingPhase::Released);
            assert!(
                driver
                    .state
                    .lock()
                    .unwrap()
                    .node
                    .backing_released(key)
                    .unwrap()
            );
            driver
                .release_backing(Class::Firecracker, &released)
                .unwrap();
            assert_eq!(runtime.releases.load(Ordering::SeqCst), 2);
        } else {
            assert!(
                result.is_err(),
                "{reply:?} cannot prove original full-scope release"
            );
            assert!(
                !driver
                    .state
                    .lock()
                    .unwrap()
                    .node
                    .backing_released(key)
                    .unwrap()
            );
            let mut unproved = draining;
            unproved.phase = BackingPhase::Released;
            assert!(
                driver
                    .release_backing(Class::Firecracker, &unproved)
                    .is_err()
            );
            assert_eq!(
                runtime.releases.load(Ordering::SeqCst),
                1,
                "no native ACK after a failed cleanup"
            );
        }
    }
}
