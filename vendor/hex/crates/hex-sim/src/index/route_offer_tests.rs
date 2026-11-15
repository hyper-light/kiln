//! AC-T7.4.1: initialized OCI route/platform/cost and cold-claim controls.
//! Original three red bodies/source remain in the immutable 40f/1740 evidence cohort.
//! These explicit initialized offers are synthetic fixtures, never measured native capability.
use super::*;
use crate::model::{Ask, Device, ModelId, Os, RuntimeRoutes};
use hex_api::{
    BackingKey, BackingOffer, BackingPhase, Execution, GpuEligibility, ImageArch, ImageOs,
    ImagePlatform, RouteKey, RouteOffer,
};

fn profile() -> hex_api::RuntimeCost {
    let guest = hex_api::CapacityResources {
        cpu_milli: 1_000,
        mem_bytes: 512 << 20,
        disk_bytes: 0,
    };
    hex_api::RuntimeCost {
        id: [0x47; 32],
        floor: guest,
        ceiling: guest,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: hex_api::CapacityResources::default(),
        overhead: hex_api::CapacityResources {
            cpu_milli: 100,
            mem_bytes: 64 << 20,
            disk_bytes: 0,
        },
    }
}
/// A Live shared backing, its guest measured as it went Live at nothing of its own (joint plan step 14, C7, pin 1).
fn backing() -> BackingOffer {
    BackingOffer {
        guest: measured(0, 0),
        key: BackingKey {
            pool: [1; 32],
            generation: [2; 16],
        },
        cost: profile(),
        phase: BackingPhase::Live,
        devices: Vec::new(),
    }
}
fn image(os: ImageOs, arm: bool) -> ImagePlatform {
    ImagePlatform {
        os,
        arch: if arm {
            ImageArch::Arm64
        } else {
            ImageArch::Amd64
        },
    }
}
fn node(os: Os) -> NodeSpec {
    let mut node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 8_000,
            mem_bytes: 8 << 30,
            disk_bytes: 8 << 30,
            gpus: 0,
        },
    );
    node.platform.os = os;
    if os == Os::Macos {
        node.platform.arch = crate::model::Arch::Aarch64;
        node.virt.kvm = false;
        node.virt.hvf = true;
    }
    node.classes = Classes::NONE.with(Class::Oci);
    let backing = backing();
    let mut offers = vec![RouteOffer {
        key: RouteKey {
            class: hex_api::Class::Oci,
            image: image(ImageOs::Linux, os == Os::Macos),
            execution: Execution::ShardsShared,
            gpu: GpuEligibility::None,
        },
        cost: None,
        backing: Some(backing.key),
    }];
    if os == Os::Windows {
        offers.push(RouteOffer {
            key: RouteKey {
                class: hex_api::Class::Oci,
                image: image(ImageOs::Windows, false),
                execution: Execution::Native,
                gpu: GpuEligibility::Any,
            },
            cost: None,
            backing: None,
        });
    }
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers,
            device_routes: Vec::new(),
            backings: vec![backing],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    node
}
fn demand(os: Option<Os>, gpus: u32) -> Demand {
    Demand {
        class: Class::Oci,
        kind: Kind::Plain,
        need: Resources {
            cpu_milli: 100,
            mem_bytes: 64 << 20,
            disk_bytes: 0,
            gpus,
        },
        ask: Ask {
            runtime_class: Some(hex_api::Class::Oci),
            os,
            arch: Some(crate::model::Arch::Aarch64),
            ..Ask::default()
        },
    }
}

#[test]
fn mac_linux_oci_is_not_compared_with_the_host_image_os() {
    let index = FitIndex::new(&[node(Os::Macos)]).unwrap();
    assert_eq!(
        index
            .verdict(NodeId(0), &demand(Some(Os::Linux), 0))
            .unwrap(),
        Verdict::Admits
    );
    assert_eq!(
        index
            .verdict(NodeId(0), &demand(Some(Os::Windows), 0))
            .unwrap(),
        Verdict::NoPlatform
    );
}

#[test]
fn windows_hcs_and_linux_oci_keep_distinct_image_routes() {
    let index = FitIndex::new(&[node(Os::Windows)]).unwrap();
    for os in [Os::Windows, Os::Linux] {
        let mut demand = demand(Some(os), 0);
        demand.ask.arch = Some(crate::model::Arch::X86_64);
        assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::Admits);
        let route = index.route(NodeId(0), &demand).unwrap().unwrap();
        assert_eq!(
            route.key.execution,
            if os == Os::Windows {
                Execution::Native
            } else {
                Execution::ShardsShared
            }
        );
    }
    assert_eq!(
        index
            .verdict(NodeId(0), &demand(Some(Os::Macos), 0))
            .unwrap(),
        Verdict::NoPlatform
    );
    assert_eq!(
        index.verdict(NodeId(0), &demand(None, 0)).unwrap(),
        Verdict::NoPlatform,
        "missing image platform cannot guess between initialized native and guest routes"
    );
}

#[test]
fn two_cpu_shared_oci_and_one_gpu_dedicated_oci_pay_two_vm_envelopes() {
    let cost = profile();
    let charge = cost
        .allocate(hex_api::CapacityResources::default())
        .unwrap();
    let mut node = node(Os::Macos);
    node.capacity.cpu_milli = charge.resources.cpu_milli * 2;
    node.capacity.mem_bytes = charge.resources.mem_bytes * 2;
    node.capacity.gpus = 1;
    let mut gpu = Device::gpu(ModelId::UNNAMED, 8 << 30);
    gpu.key = Some(7);
    node.devices = vec![gpu];
    let shared = backing();
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: vec![
                RouteOffer {
                    key: RouteKey {
                        class: hex_api::Class::Oci,
                        image: image(ImageOs::Linux, true),
                        execution: Execution::ShardsShared,
                        gpu: GpuEligibility::None,
                    },
                    cost: None,
                    backing: Some(shared.key),
                },
                RouteOffer {
                    key: RouteKey {
                        class: hex_api::Class::Oci,
                        image: image(ImageOs::Linux, true),
                        execution: Execution::ShardsDedicated,
                        gpu: GpuEligibility::Required,
                    },
                    cost: Some(cost),
                    backing: None,
                },
            ],
            device_routes: vec![2],
            backings: vec![shared],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    let mut index = FitIndex::new(&[node]).unwrap();
    let cpu = demand(Some(Os::Linux), 0);
    for id in [WorkloadId(1), WorkloadId(2)] {
        assert_eq!(index.allocation(NodeId(0), &cpu).unwrap(), None);
        assert_eq!(index.verdict(NodeId(0), &cpu).unwrap(), Verdict::Admits);
        index.take_allocated(NodeId(0), id, &cpu, None).unwrap();
    }
    let gpu = demand(Some(Os::Linux), 1);
    assert_eq!(
        index.verdict(NodeId(0), &gpu).unwrap(),
        Verdict::Admits,
        "one CPU backing plus one dedicated GPU envelope must fit"
    );
    let allocation = index.allocation(NodeId(0), &gpu).unwrap();
    let route = index.route(NodeId(0), &gpu).unwrap();
    index
        .take_routed(NodeId(0), WorkloadId(3), &gpu, allocation, route)
        .unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap().cpu_milli, 0);
    assert_eq!(index.key(NodeId(0)).unwrap().mem_bytes, 0);
    index
        .give_routed(NodeId(0), WorkloadId(3), &gpu, allocation, route)
        .unwrap();
    for id in [WorkloadId(1), WorkloadId(2)] {
        index.give_allocated(NodeId(0), id, &cpu, None).unwrap();
    }
    assert_eq!(
        index.key(NodeId(0)).unwrap().cpu_milli,
        charge.resources.cpu_milli,
        "inner deletion cannot refund its still-live backing"
    );
}

#[test]
fn shared_backing_checks_below_exact_and_over_aggregate_payload_in_the_actual_index() {
    let mut backing = backing();
    backing.cost.floor.cpu_milli = 4000;
    backing.cost.floor.disk_bytes = 4096;
    backing.cost.ceiling = backing.cost.floor;
    backing.cost.guest_overhead = hex_api::CapacityResources {
        cpu_milli: 1000,
        mem_bytes: 64 << 20,
        disk_bytes: 512,
    };
    backing.cost.overhead.disk_bytes = 128;
    let allocation = backing.allocation().unwrap();
    let mut node = node(Os::Macos);
    node.capacity = Resources {
        cpu_milli: allocation.resources.cpu_milli,
        mem_bytes: allocation.resources.mem_bytes,
        disk_bytes: allocation.resources.disk_bytes,
        gpus: 0,
    };
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: vec![RouteOffer {
                key: RouteKey {
                    class: hex_api::Class::Oci,
                    image: image(ImageOs::Linux, true),
                    execution: Execution::ShardsShared,
                    gpu: GpuEligibility::None,
                },
                cost: None,
                backing: Some(backing.key),
            }],
            device_routes: Vec::new(),
            backings: vec![backing],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    let mut index = FitIndex::new(&[node]).unwrap();
    let mut half = demand(Some(Os::Linux), 0);
    half.need = Resources {
        cpu_milli: 1500,
        mem_bytes: 224 << 20,
        disk_bytes: 1792,
        gpus: 0,
    };
    assert_eq!(index.key(NodeId(0)).unwrap().cpu_milli, 0);
    assert!(
        index.fit(&half, &Pick::default()).best.is_some(),
        "initialized guest payload room participates in the SAME bucket walk despite no extra host CPU"
    );
    index
        .take_allocated(NodeId(0), WorkloadId(1), &half, None)
        .unwrap();
    assert_eq!(
        index.verdict(NodeId(0), &half).unwrap(),
        Verdict::Admits,
        "exact aggregate payload fits after both allowances and quantization"
    );
    index
        .take_allocated(NodeId(0), WorkloadId(2), &half, None)
        .unwrap();
    for over in [
        Resources {
            cpu_milli: 1,
            ..Resources::default()
        },
        Resources {
            mem_bytes: 1,
            ..Resources::default()
        },
        Resources {
            disk_bytes: 1,
            ..Resources::default()
        },
    ] {
        let request = Demand { need: over, ..half };
        assert_eq!(index.verdict(NodeId(0), &request).unwrap(), Verdict::NoRoom);
    }
    assert_eq!(
        index.key(NodeId(0)).unwrap().cpu_milli,
        0,
        "residual clamping cannot invent a second guest budget"
    );
}

#[test]
fn cold_claims_reserve_unobserved_ids_before_boot_without_fabricating_devices() {
    let id = hex_api::DeviceId {
        kind: hex_api::DeviceKind::Gpu,
        key: 7,
    };
    let key = BackingKey {
        pool: [8; 32],
        generation: [9; 16],
    };
    let second = BackingKey {
        pool: [10; 32],
        generation: [11; 16],
    };
    let mut node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 8000,
            mem_bytes: 8 << 30,
            disk_bytes: 0,
            gpus: 0,
        },
    );
    node.capacity.gpus = 1;
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: vec![
                RouteOffer {
                    key: RouteKey {
                        class: hex_api::Class::Firecracker,
                        image: image(ImageOs::Linux, false),
                        execution: Execution::Firecracker,
                        gpu: GpuEligibility::Required,
                    },
                    cost: Some(profile()),
                    backing: Some(key),
                },
                RouteOffer {
                    key: RouteKey {
                        class: hex_api::Class::Shards,
                        image: image(ImageOs::Linux, false),
                        execution: Execution::ShardsDedicated,
                        gpu: GpuEligibility::Required,
                    },
                    cost: Some(profile()),
                    backing: Some(second),
                },
                RouteOffer {
                    key: RouteKey {
                        class: hex_api::Class::Process,
                        image: image(ImageOs::Linux, false),
                        execution: Execution::Native,
                        gpu: GpuEligibility::None,
                    },
                    cost: None,
                    backing: None,
                },
            ],
            device_routes: Vec::new(),
            backings: vec![
                BackingOffer {
                    guest: Default::default(),
                    key,
                    cost: profile(),
                    phase: BackingPhase::Prepared,
                    devices: vec![id],
                },
                BackingOffer {
                    guest: Default::default(),
                    key: second,
                    cost: profile(),
                    phase: BackingPhase::Prepared,
                    devices: vec![id],
                },
            ],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    assert!(node.devices.is_empty());
    let mut index = FitIndex::new(&[node]).unwrap();
    let grant = index.grant_backing(NodeId(0), key).unwrap();
    assert_eq!(grant.devices, vec![id]);
    assert_eq!(index.key(NodeId(0)).unwrap().gpus, 0);
    assert!(
        index
            .devices(NodeId(0))
            .unwrap()
            .iter()
            .all(|slot| slot.observed.is_none())
    );
    let before = index.key(NodeId(0)).unwrap();
    assert!(index.grant_backing(NodeId(0), second).is_err());
    assert_eq!(index.key(NodeId(0)).unwrap(), before);
    let cpu = Demand {
        need: Resources {
            cpu_milli: 1,
            ..Resources::default()
        },
        class: Class::Oci,
        kind: Kind::Plain,
        ask: Ask {
            runtime_class: Some(hex_api::Class::Process),
            os: Some(Os::Linux),
            ..Ask::default()
        },
    };
    assert_eq!(index.verdict(NodeId(0), &cpu).unwrap(), Verdict::Admits);
    let gpu = Demand {
        class: Class::Firecracker,
        ask: Ask {
            runtime_class: Some(hex_api::Class::Firecracker),
            os: Some(Os::Linux),
            arch: Some(crate::model::Arch::X86_64),
            ..Ask::default()
        },
        ..demand(Some(Os::Linux), 1)
    };
    assert_eq!(index.verdict(NodeId(0), &gpu).unwrap(), Verdict::NoGpu);
}

/// AC-T7.4.1: the owner's pending row is authority even without a receiver binding. A cold restart
/// cannot credit that reservation to a sibling or let a fixed observer grant sponsor two boxes.
/// This is a new consumer control, SOURCE FIRST; no native or Kiln defect is claimed.
#[test]
fn pending_gpu_reservations_keep_original_shape_across_refresh_and_backing_handoff() {
    let plain_node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 8000,
            mem_bytes: 8 << 30,
            disk_bytes: 0,
            gpus: 1,
        },
    );
    let request = Demand {
        need: Resources {
            cpu_milli: 100,
            mem_bytes: 64 << 20,
            disk_bytes: 0,
            gpus: 1,
        },
        kind: Kind::Plain,
        class: Class::Oci,
        ask: Ask::default(),
    };
    let mut plain = FitIndex::new(std::slice::from_ref(&plain_node)).unwrap();
    plain
        .take_pending_routed(NodeId(0), &request, None, None)
        .unwrap();
    let before = plain.verdict(NodeId(0), &request).unwrap();
    plain.refresh_node(&plain_node).unwrap();
    let refreshed = plain.verdict(NodeId(0), &request).unwrap();

    let key = BackingKey {
        pool: [21; 32],
        generation: [22; 16],
    };
    let id = hex_api::DeviceId {
        kind: hex_api::DeviceKind::Gpu,
        key: 7,
    };
    let mut node = plain_node;
    node.devices[0].key = Some(id.key);
    node.devices[0].classes = Classes::ALL;
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: vec![RouteOffer {
                key: RouteKey {
                    class: hex_api::Class::Firecracker,
                    image: image(ImageOs::Linux, false),
                    execution: Execution::Firecracker,
                    gpu: GpuEligibility::Required,
                },
                cost: Some(profile()),
                backing: Some(key),
            }],
            device_routes: vec![1],
            backings: vec![BackingOffer {
                guest: Default::default(),
                key,
                cost: profile(),
                phase: BackingPhase::Live,
                devices: vec![id],
            }],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    let request = Demand {
        class: Class::Firecracker,
        ask: Ask {
            runtime_class: Some(hex_api::Class::Firecracker),
            os: Some(Os::Linux),
            arch: Some(crate::model::Arch::X86_64),
            ..Ask::default()
        },
        ..request
    };
    let mut observed = FitIndex::new(&[node]).unwrap();
    let allocation = observed.allocation(NodeId(0), &request).unwrap();
    let route = observed.route(NodeId(0), &request).unwrap();
    observed
        .take_pending_routed(NodeId(0), &request, allocation, route)
        .unwrap();
    let backing = observed.verdict(NodeId(0), &request).unwrap();
    eprintln!(
        "pending GPU source control: before={before:?}, after_refresh={refreshed:?}, already_committed_backing={backing:?}; native_effect=false"
    );
    assert_eq!(
        (before, refreshed, backing),
        (Verdict::NoRoom, Verdict::NoRoom, Verdict::NoRoom)
    );
}

/// AC-T7.4.1: an ungranted alternative cannot hide an initialized native route with a free GPU.
/// ROOT's f3 source hypothesis; this control is source-first and has not established an actual red.
#[test]
fn a_prepared_gpu_backing_does_not_hide_an_eligible_native_oci_route() {
    let id = hex_api::DeviceId {
        kind: hex_api::DeviceKind::Gpu,
        key: 7,
    };
    let mut node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 8000,
            mem_bytes: 8 << 30,
            disk_bytes: 0,
            gpus: 1,
        },
    );
    node.virt.kvm = true;
    node.devices[0].key = Some(id.key);
    node.devices[0].classes = Classes::ALL;
    let observer = BackingOffer {
        guest: Default::default(),
        key: BackingKey {
            pool: [31; 32],
            generation: [32; 16],
        },
        cost: profile(),
        phase: BackingPhase::Prepared,
        devices: vec![id],
    };
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: vec![
                RouteOffer {
                    key: RouteKey {
                        class: hex_api::Class::Oci,
                        image: image(ImageOs::Linux, false),
                        execution: Execution::Native,
                        gpu: GpuEligibility::Any,
                    },
                    cost: None,
                    backing: None,
                },
                RouteOffer {
                    key: RouteKey {
                        class: hex_api::Class::Oci,
                        image: image(ImageOs::Linux, false),
                        execution: Execution::ShardsDedicated,
                        gpu: GpuEligibility::Required,
                    },
                    cost: Some(profile()),
                    backing: Some(observer.key),
                },
            ],
            device_routes: vec![3],
            backings: vec![observer],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    node.check_runtime_pools().unwrap();
    let mut request = demand(Some(Os::Linux), 1);
    request.ask.arch = Some(crate::model::Arch::X86_64);
    let mut cpu = request;
    cpu.need.gpus = 0;
    let mut index = FitIndex::new(&[node]).unwrap();
    let cpu_verdict = index.verdict(NodeId(0), &cpu).unwrap();
    let gpu_verdict = index.verdict(NodeId(0), &request).unwrap();
    eprintln!(
        "mixed initialized/prepared route source control: cpu={cpu_verdict:?}, gpu={gpu_verdict:?}; native_effect=false"
    );
    assert_eq!(
        (cpu_verdict, gpu_verdict),
        (Verdict::Admits, Verdict::Admits)
    );
    let route = index.route(NodeId(0), &request).unwrap();
    assert_eq!(route.unwrap().key.execution, Execution::Native);
    let before = index.key(NodeId(0)).unwrap();
    let allocation = index.allocation(NodeId(0), &request).unwrap();
    index
        .take_routed(NodeId(0), WorkloadId(1), &request, allocation, route)
        .unwrap();
    index
        .give_routed(NodeId(0), WorkloadId(1), &request, allocation, route)
        .unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap(), before);
}

/// AC-T7.4.1: a delayed Prepared report cannot revoke a locally granted physical/native envelope.
/// ROOT's f3 source hypothesis; no production remedy is included before an actual control result.
#[test]
fn a_prepared_snapshot_cannot_roll_back_a_granted_backing_or_its_claims() {
    let mut node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 8000,
            mem_bytes: 8 << 30,
            disk_bytes: 0,
            gpus: 1,
        },
    );
    node.virt.kvm = true;
    node.devices[0].key = Some(7);
    node.devices[0].classes = Classes::ALL;
    let observer = BackingOffer {
        guest: Default::default(),
        key: BackingKey {
            pool: [41; 32],
            generation: [42; 16],
        },
        cost: profile(),
        phase: BackingPhase::Prepared,
        devices: vec![hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 7,
        }],
    };
    let key = observer.key;
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: vec![RouteOffer {
                key: RouteKey {
                    class: hex_api::Class::Shards,
                    image: image(ImageOs::Linux, false),
                    execution: Execution::ShardsDedicated,
                    gpu: GpuEligibility::Required,
                },
                cost: Some(profile()),
                backing: Some(key),
            }],
            device_routes: vec![1],
            backings: vec![observer],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    node.check_runtime_pools().unwrap();
    let mut index = FitIndex::new(std::slice::from_ref(&node)).unwrap();
    let grant = index.grant_backing(NodeId(0), key).unwrap();
    let before = index.key(NodeId(0)).unwrap();
    assert_eq!(before.gpus, 0);
    let refreshed = index.refresh_node(&node);
    let after = index.key(NodeId(0)).unwrap();
    eprintln!(
        "granted/prepared rollback source control: refresh={refreshed:?}, before={before:?}, after={after:?}; native_effect=false"
    );
    assert!(
        refreshed.is_err(),
        "a stale Prepared snapshot is not release proof"
    );
    assert_eq!(after, before);
    assert_eq!(index.grant_backing(NodeId(0), key).unwrap(), grant);
}

/// AC-T7.4.1: an observer's wider provisioning set must not silently enlarge an exact GPU demand.
/// Original source-first control for the multi-GPU topology gap; no production remedy is included.
#[test]
fn one_gpu_demand_can_select_an_exact_subset_of_a_two_gpu_observer() {
    let ids = [7, 8].map(|key| hex_api::DeviceId {
        kind: hex_api::DeviceKind::Gpu,
        key,
    });
    let key = BackingKey {
        pool: [71; 32],
        generation: [72; 16],
    };
    let mut node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 8000,
            mem_bytes: 8 << 30,
            disk_bytes: 0,
            gpus: 2,
        },
    );
    node.virt.kvm = true;
    node.classes = Classes::ALL;
    for (gpu, id) in node.devices.iter_mut().zip(ids) {
        gpu.key = Some(id.key);
        gpu.classes = Classes::ALL;
    }
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: vec![RouteOffer {
                key: RouteKey {
                    class: hex_api::Class::Firecracker,
                    image: image(ImageOs::Linux, false),
                    execution: Execution::Firecracker,
                    gpu: GpuEligibility::Required,
                },
                cost: Some(profile()),
                backing: Some(key),
            }],
            device_routes: vec![1, 1],
            backings: vec![BackingOffer {
                guest: Default::default(),
                key,
                cost: profile(),
                phase: BackingPhase::Live,
                devices: ids.to_vec(),
            }],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    let index = FitIndex::new(&[node]).unwrap();
    let request = Demand {
        class: Class::Firecracker,
        ask: Ask {
            runtime_class: Some(hex_api::Class::Firecracker),
            os: Some(Os::Linux),
            arch: Some(crate::model::Arch::X86_64),
            ..Ask::default()
        },
        ..demand(Some(Os::Linux), 1)
    };
    let mut complete = request;
    complete.need.gpus = 2;
    let wider = index.verdict(NodeId(0), &complete).unwrap();
    let subset = index.verdict(NodeId(0), &request).unwrap();
    eprintln!(
        "two-GPU observer source control: complete={wider:?}, exact_one={subset:?}; native_effect=false"
    );
    assert_eq!(
        wider,
        Verdict::Admits,
        "the fixture must expose both actually observed devices"
    );
    assert_eq!(
        subset,
        Verdict::Admits,
        "one GPU demand must not be enlarged to the observer's complete provisioning set"
    );
    let route = index.route(NodeId(0), &request).unwrap().unwrap();
    assert_eq!(route.backing, Some(key));
    assert_eq!(request.need.gpus, 1);
}

/// AC-T7.4.1: transient observer failure blocks fresh work/grants, not exact durable accounting.
#[test]
fn unavailable_route_keeps_committed_recovery_refund_and_backing_charge() {
    let mut offered = node(Os::Macos);
    let request = demand(Some(Os::Linux), 0);
    let mut original = FitIndex::new(std::slice::from_ref(&offered)).unwrap();
    let route = original.route(NodeId(0), &request).unwrap();
    let allocation = original.allocation(NodeId(0), &request).unwrap();
    let charged = original.key(NodeId(0)).unwrap();
    original
        .take_routed(NodeId(0), WorkloadId(90), &request, allocation, route)
        .unwrap();
    let prior = offered.routes.as_ref().unwrap();
    offered.routes = Some(
        RuntimeRoutes {
            admit: Some(0),
            offers: prior.offers.clone(),
            device_routes: prior.device_routes.clone(),
            backings: prior
                .backings
                .iter()
                .map(|backing| backing.try_clone().unwrap())
                .collect(),
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    original.refresh_node(&offered).unwrap();
    assert_eq!(original.key(NodeId(0)).unwrap(), charged);
    assert_ne!(
        original.verdict(NodeId(0), &request).unwrap(),
        Verdict::Admits
    );
    let mut recovered = FitIndex::new(std::slice::from_ref(&offered)).unwrap();
    recovered
        .take_recovered_routed(NodeId(0), WorkloadId(90), &request, &[], allocation, route)
        .unwrap();
    recovered
        .give_routed(NodeId(0), WorkloadId(90), &request, allocation, route)
        .unwrap();
    assert_eq!(
        recovered.key(NodeId(0)).unwrap(),
        charged,
        "inner refund must keep the still-live VM's full charge"
    );
    let prior = offered.routes.as_ref().unwrap();
    let mut prepared: Vec<_> = prior
        .backings
        .iter()
        .map(|backing| backing.try_clone().unwrap())
        .collect();
    prepared[0].phase = BackingPhase::Prepared;
    let key = prepared[0].key;
    offered.routes = Some(
        RuntimeRoutes {
            admit: Some(0),
            offers: prior.offers.clone(),
            device_routes: prior.device_routes.clone(),
            backings: prepared,
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    let mut unavailable = FitIndex::new(&[offered]).unwrap();
    assert!(unavailable.grant_backing(NodeId(0), key).is_err());
}

fn subset_observer_fixture(native: bool) -> (NodeSpec, Demand, BackingOffer) {
    let ids = [7, 8].map(|key| hex_api::DeviceId {
        kind: hex_api::DeviceKind::Gpu,
        key,
    });
    let source = BackingOffer {
        guest: Default::default(),
        key: BackingKey {
            pool: [91; 32],
            generation: [92; 16],
        },
        cost: profile(),
        phase: BackingPhase::Live,
        devices: ids.to_vec(),
    };
    let mut offered = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 8000,
            mem_bytes: 8 << 30,
            disk_bytes: 0,
            gpus: 2,
        },
    );
    offered.virt.kvm = true;
    offered.classes = Classes::ALL;
    for (gpu, id) in offered.devices.iter_mut().zip(ids) {
        gpu.key = Some(id.key);
        gpu.classes = Classes::ALL;
    }
    let mut offers = vec![RouteOffer {
        key: RouteKey {
            class: hex_api::Class::Firecracker,
            image: image(ImageOs::Linux, false),
            execution: Execution::Firecracker,
            gpu: GpuEligibility::Required,
        },
        cost: Some(profile()),
        backing: Some(source.key),
    }];
    if native {
        offers.push(RouteOffer {
            key: RouteKey {
                class: hex_api::Class::Oci,
                image: image(ImageOs::Linux, false),
                execution: Execution::Native,
                gpu: GpuEligibility::Any,
            },
            cost: None,
            backing: None,
        });
    }
    offered.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers,
            device_routes: vec![if native { 3 } else { 1 }; 2],
            backings: vec![source.try_clone().unwrap()],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    let request = Demand {
        class: Class::Firecracker,
        ask: Ask {
            runtime_class: Some(hex_api::Class::Firecracker),
            os: Some(Os::Linux),
            arch: Some(crate::model::Arch::X86_64),
            ..Ask::default()
        },
        ..demand(Some(Os::Linux), 1)
    };
    (offered, request, source)
}

/// AC-T7.4.1: one subset cannot consume a second lifetime or refund the original full-P scope.
#[test]
fn exact_subset_has_one_consumer_and_cancellation_keeps_full_backing_reservation() {
    let (offered, request, _) = subset_observer_fixture(false);
    let mut index = FitIndex::new(&[offered]).unwrap();
    let before = index.key(NodeId(0)).unwrap();
    let route = index.route(NodeId(0), &request).unwrap();
    let allocation = index.allocation(NodeId(0), &request).unwrap();
    index
        .take_routed(NodeId(0), WorkloadId(91), &request, allocation, route)
        .unwrap();
    assert_eq!(
        index.held_devices(NodeId(0), WorkloadId(91)).unwrap().len(),
        1
    );
    assert_ne!(index.verdict(NodeId(0), &request).unwrap(), Verdict::Admits);
    assert!(
        index
            .take_routed(NodeId(0), WorkloadId(92), &request, allocation, route)
            .is_err()
    );
    index
        .give_routed(NodeId(0), WorkloadId(91), &request, allocation, route)
        .unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap(), before);
    assert_eq!(index.verdict(NodeId(0), &request).unwrap(), Verdict::Admits);
}

/// AC-T7.4.1 source-first control: terminal P does not make its stale sibling sample current.
/// This new lifecycle-freshness concern is UNRUN, not an accepted red or an included remedy.
#[test]
fn released_observer_siblings_require_new_observation_before_native_route_admission() {
    let (offered, request, mut source) = subset_observer_fixture(true);
    let mut index = FitIndex::new(&[offered]).unwrap();
    let route = index.route(NodeId(0), &request).unwrap();
    let allocation = index.allocation(NodeId(0), &request).unwrap();
    index
        .take_routed(NodeId(0), WorkloadId(91), &request, allocation, route)
        .unwrap();
    let native = Demand {
        class: Class::Oci,
        ask: Ask {
            runtime_class: Some(hex_api::Class::Oci),
            os: Some(Os::Linux),
            arch: Some(crate::model::Arch::X86_64),
            ..Ask::default()
        },
        ..request
    };
    assert_eq!(index.key(NodeId(0)).unwrap().gpus, 0);
    assert_eq!(index.verdict(NodeId(0), &native).unwrap(), Verdict::NoRoom);
    source.phase = BackingPhase::Draining;
    index.observe_backing(NodeId(0), &source).unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap().gpus, 0);
    assert_eq!(index.verdict(NodeId(0), &native).unwrap(), Verdict::NoRoom);
    source.phase = BackingPhase::Released;
    index.observe_backing(NodeId(0), &source).unwrap();
    assert_eq!(
        index.held_devices(NodeId(0), WorkloadId(91)).unwrap().len(),
        1
    );
    let actual = index.verdict(NodeId(0), &native).unwrap();
    eprintln!(
        "released sibling original: native={actual:?}; fresh physical observations=0; native_effect=false"
    );
    assert_eq!(
        actual,
        Verdict::NoGpu,
        "the existing catalog sample predates release; a new successful observation is required for the sibling"
    );
}

/// AC-T7.4.1: known observations do not bypass full-P charge, but an independent fresh terminal view can admit.
#[test]
fn healthy_independent_native_snapshot_distinguishes_held_full_pool_from_fresh_free_gpu() {
    let (offered, request, _) = subset_observer_fixture(true);
    let native = Demand {
        class: Class::Oci,
        ask: Ask {
            runtime_class: Some(hex_api::Class::Oci),
            os: Some(Os::Linux),
            arch: Some(crate::model::Arch::X86_64),
            ..Ask::default()
        },
        ..request
    };
    let held = FitIndex::new(&[offered]).unwrap();
    assert_eq!(held.key(NodeId(0)).unwrap().gpus, 0);
    assert_eq!(held.verdict(NodeId(0), &native).unwrap(), Verdict::NoRoom);

    // This is a separate current model snapshot with no retained Box and successful readings,
    // not a refresh that treats the preceding observer's stale samples as new observations.
    let (mut fresh, _, _) = subset_observer_fixture(true);
    let routes = triomphe::Arc::get_mut(fresh.routes.as_mut().unwrap()).unwrap();
    routes.backings.first_mut().unwrap().phase = BackingPhase::Released;
    let free = FitIndex::new(&[fresh]).unwrap();
    assert_eq!(free.key(NodeId(0)).unwrap().gpus, 2);
    assert_eq!(free.verdict(NodeId(0), &native).unwrap(), Verdict::Admits);
}

/// AC-T7.4.1: ending full P invalidates pressure without dropping S; only new observations restore grants.
#[test]
fn released_scope_keeps_selected_identity_and_charge_until_new_physical_refresh() {
    let (offered, request, mut source) = subset_observer_fixture(true);
    let mut index = FitIndex::new(&[offered]).unwrap();
    let route = index.route(NodeId(0), &request).unwrap();
    let allocation = index.allocation(NodeId(0), &request).unwrap();
    index
        .take_routed(NodeId(0), WorkloadId(91), &request, allocation, route)
        .unwrap();
    let selected: Vec<_> = index
        .held_devices(NodeId(0), WorkloadId(91))
        .unwrap()
        .iter()
        .map(Device::native_id)
        .collect();
    let charged = index.key(NodeId(0)).unwrap();
    source.phase = BackingPhase::Draining;
    index.observe_backing(NodeId(0), &source).unwrap();
    source.phase = BackingPhase::Released;
    index.observe_backing(NodeId(0), &source).unwrap();
    let after = index.key(NodeId(0)).unwrap();
    let refund = source.allocation().unwrap().resources;
    assert_eq!(
        after.cpu_milli.checked_sub(charged.cpu_milli),
        Some(refund.cpu_milli)
    );
    assert_eq!(
        after.mem_bytes.checked_sub(charged.mem_bytes),
        Some(refund.mem_bytes)
    );
    assert_eq!(
        index
            .held_devices(NodeId(0), WorkloadId(91))
            .unwrap()
            .iter()
            .map(Device::native_id)
            .collect::<Vec<_>>(),
        selected
    );
    assert!(index.devices(NodeId(0)).unwrap().iter().all(|physical| {
        physical.observed.is_some_and(|device| {
            !device.available
                && device.load.busy_per_mille.is_none()
                && device.load.memory_used.is_none()
        })
    }));
    let native = Demand {
        class: Class::Oci,
        ask: Ask {
            runtime_class: Some(hex_api::Class::Oci),
            os: Some(Os::Linux),
            arch: Some(crate::model::Arch::X86_64),
            ..Ask::default()
        },
        ..request
    };
    assert_eq!(index.verdict(NodeId(0), &native).unwrap(), Verdict::NoGpu);

    // A separately supplied actual-facts fixture refresh, not a time passage or release signal.
    let (mut current, _, _) = subset_observer_fixture(true);
    triomphe::Arc::get_mut(current.routes.as_mut().unwrap())
        .unwrap()
        .backings
        .first_mut()
        .unwrap()
        .phase = BackingPhase::Released;
    index.refresh_node(&current).unwrap();
    assert_eq!(index.verdict(NodeId(0), &native).unwrap(), Verdict::Admits);
    let fresh = index.key(NodeId(0)).unwrap();
    // A duplicate same-key terminal acknowledgment follows the fresh snapshot. It must not
    // erase new observations or refund the already-consumed backing charge a second time.
    index.observe_backing(NodeId(0), &source).unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap(), fresh);
    assert_eq!(index.verdict(NodeId(0), &native).unwrap(), Verdict::Admits);
    assert_eq!(
        index
            .held_devices(NodeId(0), WorkloadId(91))
            .unwrap()
            .iter()
            .map(Device::native_id)
            .collect::<Vec<_>>(),
        selected
    );
    index
        .give_routed(NodeId(0), WorkloadId(91), &request, allocation, route)
        .unwrap();
    assert!(index.holds(NodeId(0), WorkloadId(91)).is_empty());
}

/// AC-T7.4.1 original: recovery retains BoxS's old tuple while fresh cold R provisions independently.
/// Source hypothesis only; no committed-recovery remedy is included before ROOT's actual result.
#[test]
fn committed_subset_recovery_keeps_released_parent_and_does_not_repick_current_remainder() {
    let (mut node, request, mut parent) = subset_observer_fixture(false);
    let current = BackingOffer {
        guest: Default::default(),
        key: BackingKey {
            pool: [0x61; 32],
            generation: [0x62; 16],
        },
        cost: profile(),
        phase: BackingPhase::Prepared,
        devices: vec![parent.devices[1]],
    };
    parent.phase = BackingPhase::Released;
    let catalog = triomphe::Arc::get_mut(node.routes.as_mut().unwrap()).unwrap();
    catalog.backings[0].phase = parent.phase;
    let original = hex_api::RouteReceipt {
        key: catalog.offers[0].key,
        backing: Some(parent.key),
    };
    catalog.offers.push(RouteOffer {
        backing: Some(current.key),
        ..catalog.offers[0]
    });
    catalog.backings.push(current.try_clone().unwrap());
    // Retained P metadata can validate old S, but it does not certify R's fresh execution.
    catalog.device_routes = vec![1, 0];
    let mut index = FitIndex::new(&[node]).unwrap();
    assert_eq!(
        index.route(NodeId(0), &request).unwrap().unwrap().backing,
        Some(current.key)
    );
    assert_eq!(index.verdict(NodeId(0), &request).unwrap(), Verdict::NoGpu);
    let allocation = profile()
        .allocate(crate::model::capacity_resources(request.need))
        .unwrap();
    let before = index.key(NodeId(0)).unwrap();
    let restored = index.take_recovered_routed(
        NodeId(0),
        WorkloadId(91),
        &request,
        &[0],
        Some(allocation),
        Some(original),
    );
    eprintln!(
        "released parent recovery original: restored={restored:?}; source={:?}; current={:?}; native_effect=false",
        parent.key, current.key
    );
    assert!(
        restored.is_ok(),
        "the exact retained BoxS tuple must survive independently of a current provisioning source"
    );
    assert_eq!(
        index.held_devices(NodeId(0), WorkloadId(91)).unwrap()[0].native_id(),
        Some(parent.devices[0])
    );
    assert_eq!(
        index.key(NodeId(0)).unwrap().cpu_milli,
        before.cpu_milli - allocation.resources.cpu_milli
    );
    let grant = index.grant_backing(NodeId(0), current.key).unwrap();
    assert_eq!(grant.devices, current.devices);
    assert_eq!(index.verdict(NodeId(0), &request).unwrap(), Verdict::NoGpu);
    index
        .give_routed(
            NodeId(0),
            WorkloadId(91),
            &request,
            Some(allocation),
            Some(original),
        )
        .unwrap();
    assert!(index.holds(NodeId(0), WorkloadId(91)).is_empty());
    assert_eq!(
        index.key(NodeId(0)).unwrap().cpu_milli,
        before.cpu_milli - grant.allocation.resources.cpu_milli
    );
}

/// AC-T7.4.1: only committed recovery can retain a Released dedicated source, once per lifetime.
#[test]
fn released_source_recovery_preserves_fresh_phase_and_single_consumer_guards() {
    for phase in [
        BackingPhase::Prepared,
        BackingPhase::Granted,
        BackingPhase::Draining,
    ] {
        let (mut node, request, _) = subset_observer_fixture(false);
        let catalog = triomphe::Arc::get_mut(node.routes.as_mut().unwrap()).unwrap();
        catalog.backings[0].phase = phase;
        let route = hex_api::RouteReceipt {
            key: catalog.offers[0].key,
            backing: catalog.offers[0].backing,
        };
        let allocation = profile()
            .allocate(crate::model::capacity_resources(request.need))
            .unwrap();
        let mut index = FitIndex::new(&[node]).unwrap();
        let before = index.key(NodeId(0)).unwrap();
        assert!(
            index
                .take_recovered_routed(
                    NodeId(0),
                    WorkloadId(91),
                    &request,
                    &[0],
                    Some(allocation),
                    Some(route)
                )
                .is_err(),
            "{phase:?} is not a completed observer handoff"
        );
        assert_eq!(index.key(NodeId(0)).unwrap(), before);
    }
    let (mut node, request, _) = subset_observer_fixture(false);
    let catalog = triomphe::Arc::get_mut(node.routes.as_mut().unwrap()).unwrap();
    catalog.backings[0].phase = BackingPhase::Released;
    let route = hex_api::RouteReceipt {
        key: catalog.offers[0].key,
        backing: catalog.offers[0].backing,
    };
    let allocation = profile()
        .allocate(crate::model::capacity_resources(request.need))
        .unwrap();
    let mut index = FitIndex::new(&[node]).unwrap();
    let before = index.key(NodeId(0)).unwrap();
    assert_eq!(index.verdict(NodeId(0), &request).unwrap(), Verdict::NoGpu);
    // ROOT4f0's actual remote-owner original establishes that this is also committed recovery.
    // Keep it in a separate view so every fresh/local one-consumer oracle below stays intact.
    let mut remote_owner = index.try_clone().unwrap();
    remote_owner
        .take_pending_routed(NodeId(0), &request, Some(allocation), Some(route))
        .unwrap();
    assert_eq!(
        remote_owner.key(NodeId(0)).unwrap().cpu_milli,
        before.cpu_milli - allocation.resources.cpu_milli
    );
    assert_eq!(
        remote_owner.verdict(NodeId(0), &request).unwrap(),
        Verdict::NoGpu
    );
    assert!(remote_owner.holds(NodeId(0), WorkloadId(91)).is_empty());
    assert!(
        index
            .take_routed(
                NodeId(0),
                WorkloadId(91),
                &request,
                Some(allocation),
                Some(route)
            )
            .is_err()
    );
    assert_eq!(index.key(NodeId(0)).unwrap(), before);
    index
        .take_recovered_routed(
            NodeId(0),
            WorkloadId(91),
            &request,
            &[0],
            Some(allocation),
            Some(route),
        )
        .unwrap();
    let charged = index.key(NodeId(0)).unwrap();
    assert!(
        index
            .take_recovered_routed(
                NodeId(0),
                WorkloadId(92),
                &request,
                &[1],
                Some(allocation),
                Some(route)
            )
            .is_err()
    );
    assert_eq!(index.key(NodeId(0)).unwrap(), charged);
    assert_eq!(index.holds(NodeId(0), WorkloadId(91)), &[0]);
    assert!(index.holds(NodeId(0), WorkloadId(92)).is_empty());
    index
        .give_routed(
            NodeId(0),
            WorkloadId(91),
            &request,
            Some(allocation),
            Some(route),
        )
        .unwrap();
    assert_eq!(index.key(NodeId(0)).unwrap(), before);
    assert_eq!(index.verdict(NodeId(0), &request).unwrap(), Verdict::NoGpu);
}

/// AC-T7.4.1: committed pending recovery retains one exact paid source without fresh facts,
/// a new binding, foreign receipt/profile, oversized physical demand or host overcommit.
#[test]
fn committed_pending_recovery_keeps_receipt_charge_and_claim_bounds() {
    let (mut node, request, parent) = subset_observer_fixture(false);
    let allocation = profile()
        .allocate(crate::model::capacity_resources(request.need))
        .unwrap();
    let current = BackingOffer {
        guest: Default::default(),
        key: BackingKey {
            pool: [0x75; 32],
            generation: [0x76; 16],
        },
        cost: profile(),
        phase: BackingPhase::Prepared,
        devices: vec![parent.devices[1]],
    };
    let catalog = triomphe::Arc::get_mut(node.routes.as_mut().unwrap()).unwrap();
    catalog.backings[0].phase = BackingPhase::Released;
    let route = hex_api::RouteReceipt {
        key: catalog.offers[0].key,
        backing: Some(parent.key),
    };
    catalog.offers.push(RouteOffer {
        backing: Some(current.key),
        ..catalog.offers[0]
    });
    catalog.backings.push(current.try_clone().unwrap());
    catalog.device_routes = vec![1, 0];
    for device in &mut node.devices {
        device.available = false;
        device.load = crate::model::GpuLoad::default();
    }
    let mut malformed = allocation;
    malformed.resources.cpu_milli -= 1;
    let mut foreign_profile = profile();
    foreign_profile.id[0] ^= 1;
    let foreign_allocation = foreign_profile
        .allocate(crate::model::capacity_resources(request.need))
        .unwrap();
    let foreign_route = hex_api::RouteReceipt {
        backing: Some(BackingKey {
            generation: [0x77; 16],
            ..parent.key
        }),
        ..route
    };
    let mut oversized = request;
    oversized.need.gpus = 3;
    for (name, demand, allocation, receipt) in [
        ("missing receipt", request, Some(allocation), None),
        (
            "foreign generation",
            request,
            Some(allocation),
            Some(foreign_route),
        ),
        ("missing allocation", request, None, Some(route)),
        ("malformed charge", request, Some(malformed), Some(route)),
        (
            "foreign profile",
            request,
            Some(foreign_allocation),
            Some(route),
        ),
        (
            "too many physical GPUs",
            oversized,
            Some(allocation),
            Some(route),
        ),
    ] {
        let mut index = FitIndex::new(std::slice::from_ref(&node)).unwrap();
        let before = index.key(NodeId(0)).unwrap();
        assert!(
            index
                .take_pending_routed(NodeId(0), &demand, allocation, receipt)
                .is_err(),
            "{name}"
        );
        assert_eq!(index.key(NodeId(0)).unwrap(), before, "{name}");
    }
    let mut masked = node.try_clone().unwrap();
    let source = node.routes.as_ref().unwrap();
    masked.routes = Some(
        RuntimeRoutes {
            admit: Some(0),
            offers: source.offers.clone(),
            device_routes: source.device_routes.clone(),
            backings: source
                .backings
                .iter()
                .map(|backing| backing.try_clone().unwrap())
                .collect(),
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    let mut retained = FitIndex::new(&[masked]).unwrap();
    let before = retained.key(NodeId(0)).unwrap();
    assert_eq!(
        retained.verdict(NodeId(0), &request).unwrap(),
        Verdict::NoGpu
    );
    retained
        .take_pending_routed(NodeId(0), &request, Some(allocation), Some(route))
        .unwrap();
    let paid = retained.key(NodeId(0)).unwrap();
    assert_eq!(
        paid.cpu_milli,
        before.cpu_milli - allocation.resources.cpu_milli
    );
    assert_eq!(
        paid.mem_bytes,
        before.mem_bytes - allocation.resources.mem_bytes
    );
    assert_eq!(
        retained.verdict(NodeId(0), &request).unwrap(),
        Verdict::NoGpu
    );
    assert!(retained.holds(NodeId(0), WorkloadId(91)).is_empty());
    assert!(
        retained
            .take_pending_routed(NodeId(0), &request, Some(allocation), Some(route))
            .is_err()
    );
    assert_eq!(retained.key(NodeId(0)).unwrap(), paid);

    // The other exact canonical source can consume room before this old Box is restored.
    // Both individual envelopes fit the host; their combined charge is one CPU unit too large.
    node.capacity.cpu_milli =
        allocation.resources.cpu_milli + current.allocation().unwrap().resources.cpu_milli - 1;
    let mut short = FitIndex::new(&[node]).unwrap();
    short.grant_backing(NodeId(0), current.key).unwrap();
    let before = short.key(NodeId(0)).unwrap();
    assert!(
        short
            .take_pending_routed(NodeId(0), &request, Some(allocation), Some(route))
            .is_err()
    );
    assert_eq!(short.key(NodeId(0)).unwrap(), before);
}

/// `node(Os::Macos)` with its shared backing's guest overhead `guest`, as its runtime measured it (joint plan step 14).
fn measured_node(guest: hex_api::LearnedOverhead) -> NodeSpec {
    let mut node = node(Os::Macos);
    let mut backing = backing();
    backing.guest = guest;
    node.routes = Some(
        RuntimeRoutes {
            admit: None,
            offers: vec![RouteOffer {
                key: RouteKey {
                    class: hex_api::Class::Oci,
                    image: image(ImageOs::Linux, true),
                    execution: Execution::ShardsShared,
                    gpu: GpuEligibility::None,
                },
                cost: None,
                backing: Some(backing.key),
            }],
            device_routes: Vec::new(),
            backings: vec![backing],
            node_vm_room: None,
        }
        .share()
        .unwrap(),
    );
    node
}

/// A guest overhead measured in each dimension: `cpu_milli` and `mem_bytes`, no disk.
fn measured(cpu_milli: u64, mem_bytes: u64) -> hex_api::LearnedOverhead {
    hex_api::LearnedOverhead {
        cpu_milli: Some(cpu_milli),
        mem_bytes: Some(mem_bytes),
        disk_bytes: Some(0),
    }
}

/// Joint plan step 14 (C7; AC-T7.4.1; ROOT, 2026-11-13): a shared guest's own overhead measured after its boxes were
/// placed stops further boxes, never its node's refresh: the boxes it holds stay held. Do place a box of 0.5 CPU and
/// 256 MiB on a Live backing of a 1 CPU, 512 MiB envelope whose guest is measured at nothing; then refresh the node
/// with its guest measured at 300 MiB, past the 256 MiB its box leaves. Expect the refresh taken and a box of 1 MiB
/// more refused, no room.
#[test]
fn w14_a_shared_guests_overhead_measured_past_its_room_keeps_its_boxes_and_admits_no_more() {
    let mut index = FitIndex::new(&[measured_node(measured(0, 0))]).unwrap();
    let mut half = demand(Some(Os::Linux), 0);
    half.need = Resources {
        cpu_milli: 500,
        mem_bytes: 256 << 20,
        disk_bytes: 0,
        gpus: 0,
    };
    assert_eq!(index.verdict(NodeId(0), &half).unwrap(), Verdict::Admits);
    index
        .take_allocated(NodeId(0), WorkloadId(1), &half, None)
        .unwrap();
    index
        .refresh_node(&measured_node(measured(0, 300 << 20)))
        .unwrap();
    let more = Demand {
        need: Resources {
            mem_bytes: 1 << 20,
            ..Resources::default()
        },
        ..half
    };
    assert_eq!(index.verdict(NodeId(0), &more).unwrap(), Verdict::NoRoom);
}

/// Joint plan step 14 (C7; ROOT, 2026-11-13): a Live shared backing ranks the CPU of its envelope its guest's own, as
/// its runtime measured it, and its boxes leave; one whose guest isn't heard from ranks none, as no box fits it. Do
/// rank a node's routes with a backing of a 1 CPU envelope whose guest is cold; then measured at 0.25 CPU. Expect none,
/// then 750 milli-CPU.
#[test]
fn w14_a_shared_backing_ranks_the_cpu_its_guest_leaves_and_none_while_cold() {
    let rank = |guest| {
        let node = measured_node(guest);
        Routes::new(node.routes.as_ref().unwrap(), NodeId(0))
            .unwrap()
            .rank_cpu()
    };
    assert_eq!(rank(hex_api::LearnedOverhead::default()), 0);
    assert_eq!(rank(measured(250, 0)), 750);
}
