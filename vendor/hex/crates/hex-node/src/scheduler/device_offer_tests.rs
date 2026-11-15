//! AC-T7.4.1: Service's real capacity/device bridge with pure Facts/Capacity inputs.
//! These are synthetic measurements in production types, never hardware/native evidence.

use super::*;
use hex_api::{Capacity, CapacityResources, DeviceId, DeviceInfo, DeviceKind as NativeKind};
use hex_sim::facts::{Cpu, Facts, Gpu};
use hex_sim::model::{Ask, Class as Route, Device, DeviceKind, GpuLoad, Os, bind};

const NAMESPACE: &str = "00112233445566778899aabbccddeeff";
const FOREIGN: &str = "ffeeddccbbaa99887766554433221100";

type Reports = Vec<(Route, Option<Capacity>)>;

fn copy_reports(reports: &Reports) -> Reports {
    reports
        .iter()
        .map(|(class, report)| {
            (
                *class,
                report.as_ref().map(|report| report.try_clone().unwrap()),
            )
        })
        .collect()
}

fn inputs() -> (Facts, Reports) {
    let memory = hex_api::MIN_MEMORY;
    let facts = Facts {
        os: Os::Linux,
        cpu: Cpu {
            threads: 1,
            ..Cpu::default()
        },
        classes: Classes::NONE.with(Route::Oci),
        gpus: vec![
            Gpu {
                vendor: "nvidia".into(),
                model: "NVIDIA GeForce RTX 5060 Ti".into(),
                memory,
                pci: Some("12345678:01:02.3".into()),
                load: GpuLoad {
                    busy_per_mille: Some(1000 / 4),
                    memory_used: Some(memory / 4),
                },
                ..Gpu::default()
            },
            Gpu {
                vendor: "amd".into(),
                model: "AMD Radeon 610M".into(),
                memory: memory * 2,
                pci: Some("00000000:01:02.3".into()),
                load: GpuLoad {
                    busy_per_mille: Some(1000 / 2),
                    memory_used: Some(memory),
                },
                ..Gpu::default()
            },
        ],
        ..Facts::default()
    };
    let devices = vec![
        // Fixed PCI domain32:bus8:device5:function3 fixtures; same BDF in distinct domains.
        DeviceInfo {
            id: DeviceId {
                kind: NativeKind::Gpu,
                key: 0x1234_5678_0113,
            },
            model: "nvidia_geforce-rtx-5060-ti".into(),
            size: memory,
            holder: None,
        },
        DeviceInfo {
            id: DeviceId {
                kind: NativeKind::Gpu,
                key: 0x0113,
            },
            model: "AMD Radeon 610M".into(),
            size: memory * 2,
            holder: None,
        },
    ];
    (facts, vec![(Route::Oci, Some(report(devices)))])
}

fn report(devices: Vec<DeviceInfo>) -> Capacity {
    let room = CapacityResources {
        cpu_milli: u64::from(hex_api::MIN_CPU_MILLI),
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    Capacity {
        own: Default::default(),
        id: "fixture-native-pool".into(),
        total: room,
        free: room,
        devices,
        cost: None,
        routes: Vec::new(),
        device_routes: Vec::new(),
        backings: Vec::new(),
        unavailable: Vec::new(),
        learned: None,
        node_vms: Vec::new(),
        unlearned: None,
    }
}

/// AC-T7.4.1: source control at the actual Facts/offers/bind seam. Unread is uncertainty,
/// independent known counter metadata and physical identity remain, and fresh picking refuses.
#[test]
fn unread_native_counters_never_become_a_fresh_idle_free_offer() {
    let (mut facts, reports) = inputs();
    let inventory = facts
        .gpus
        .iter()
        .map(|gpu| hex_gpu::Gpu {
            vendor: hex_gpu::Vendor::Nvidia,
            model: gpu.model.clone(),
            memory_total_bytes: gpu.memory,
            identity: hex_gpu::Identity::Pci(
                hex_gpu::PciAddress::parse(gpu.pci.as_deref().unwrap()).unwrap(),
            ),
            driver_version: "pure-fixture".into(),
        })
        .collect::<Vec<_>>();
    let native = [
        hex_gpu::Load {
            busy_percent: Err(hex_gpu::Unread::Unsupported),
            memory_used_bytes: Ok(hex_api::MIN_MEMORY / 4),
        },
        hex_gpu::Load {
            busy_percent: Ok(50),
            memory_used_bytes: Err(hex_gpu::Unread::NoSample),
        },
    ];
    facts.gpus = crate::facts::gpu_facts(&inventory, &native);
    assert_eq!(facts.gpus.len(), 2);
    assert_eq!(
        facts.gpus[0].key,
        Some(reports[0].1.as_ref().unwrap().devices[0].id.key)
    );
    assert_eq!(
        facts.gpus[0].load.memory_used,
        Some(hex_api::MIN_MEMORY / 4)
    );
    assert_eq!(facts.gpus[1].load.busy_per_mille, Some(500));
    assert_eq!(facts.gpus[0].load.busy_per_mille, None);
    assert_eq!(facts.gpus[1].load.memory_used, None);
    let mut cluster = service_cluster(&facts);
    let offers = offered(&reports, &facts, &mut cluster);
    let selected = bind(
        &offers,
        |index| offers.get(index).is_some_and(|device| device.available),
        1,
        &hex_sim::model::Ask::default(),
    );
    eprintln!("pure unread Facts/offers selected={selected:?}; hardware_effect=false");
    assert!(
        selected.is_none(),
        "Unread native counters must not become a fresh known-idle/free offer"
    );
}

/// AC-T7.4.1: split Service consumes actual native report keys/counters, with no vendor/ordinal
/// reconstruction. An initialized empty snapshot retains unavailable identities and CPU progress.
#[test]
fn physical_rpc_facts_keep_unknowns_and_empty_native_inventory_cannot_grant() {
    let (mut facts, reports) = inputs();
    let native = hex_api::PhysicalReport {
        unavailable: Vec::new(),
        authority: "native-store-owner".into(),
        devices: reports[0]
            .1
            .as_ref()
            .unwrap()
            .devices
            .iter()
            .map(|info| hex_api::PhysicalDevice {
                info: info.try_clone().unwrap(),
                busy_per_mille: None,
                memory_used_bytes: Some(info.size / 2),
            })
            .collect(),
    };
    facts.gpus = crate::facts::reported_gpus(&native).unwrap();
    assert!(
        facts
            .gpus
            .iter()
            .all(|gpu| gpu.pci.is_none() && gpu.driver.is_none() && gpu.vendor.is_empty())
    );
    let mut cluster = service_cluster(&facts);
    let actual = offered(&reports, &facts, &mut cluster);
    assert_eq!(actual.len(), 2);
    assert_eq!(actual[0].key, Some(native.devices[0].info.id.key));
    assert_eq!(actual[0].load.busy_per_mille, None);
    assert!(!actual[0].available);
    assert_eq!(
        actual[0].load.memory_used,
        native.devices[0].memory_used_bytes
    );
    assert!(bind(&actual, |i| actual[i].available, 1, &Ask::default()).is_none());
    let empty = hex_api::PhysicalReport {
        unavailable: Vec::new(),
        authority: native.authority,
        devices: Vec::new(),
    };
    facts.gpus = crate::facts::reported_gpus(&empty).unwrap();
    let actual = offered(&reports, &facts, &mut cluster);
    assert_eq!(actual.len(), 2);
    assert!(
        actual
            .iter()
            .all(|device| !device.available && device.load == GpuLoad::default())
    );
    assert!(bind(&actual, |i| actual[i].available, 1, &Ask::default()).is_none());
    assert_eq!(
        bind(&actual, |i| actual[i].available, 0, &Ask::default()),
        Some(Vec::new())
    );
}

/// Mirror Service::open/refresh: Facts::node interns measured GPU models before offers runs.
fn service_cluster(facts: &Facts) -> Cluster {
    let mut cluster = Cluster::single_owner(Vec::new(), Vec::new());
    cluster.model("existing-model").unwrap();
    let local = facts.node(NodeId(0), &mut cluster).unwrap();
    cluster.nodes.push(local);
    cluster
}

fn offered(reports: &Reports, facts: &Facts, cluster: &mut Cluster) -> Vec<Device> {
    capacity::devices::offers(reports, facts, cluster, NAMESPACE).unwrap()
}

#[test]
fn exact_measured_pci_metadata_load_and_report_order_reach_offers() {
    let (mut facts, mut reports) = inputs();
    facts.gpus.push(Gpu {
        model: "Telemetry Only".into(),
        memory: hex_api::MIN_MEMORY,
        pci: Some("0000:02:00.0".into()),
        ..Gpu::default()
    });
    let devices = &mut reports[0].1.as_mut().unwrap().devices;
    devices.reverse(); // Native order, not telemetry order, is the scheduler's device slot order.
    let before_inputs = (facts.clone(), copy_reports(&reports));
    let mut cluster = service_cluster(&facts);
    let before_cluster = cluster.clone();
    let amd = cluster.model(&facts.gpus[1].model).unwrap();
    let nvidia = cluster.model(&facts.gpus[0].model).unwrap();
    let actual = offered(&reports, &facts, &mut cluster);
    assert_eq!(
        actual,
        vec![
            Device {
                kind: DeviceKind::Gpu,
                key: Some(0x0113),
                available: true,
                classes: hex_sim::model::Classes::NONE.with(hex_sim::model::Class::Oci),
                model: amd,
                size: facts.gpus[1].memory,
                load: facts.gpus[1].load
            },
            Device {
                kind: DeviceKind::Gpu,
                key: Some(0x1234_5678_0113),
                available: true,
                classes: hex_sim::model::Classes::NONE.with(hex_sim::model::Class::Oci),
                model: nvidia,
                size: facts.gpus[0].memory,
                load: facts.gpus[0].load
            },
        ]
    );
    assert_eq!(
        actual[0].memory_free(),
        facts.gpus[1]
            .load
            .memory_used
            .map(|used| facts.gpus[1].memory.saturating_sub(used))
    );
    assert_eq!((facts, reports), before_inputs);
    assert_eq!(cluster, before_cluster);
    eprintln!(
        "DEVICE_OFFER exact actual={actual:?}; inputs/cluster unchanged; telemetry-only GPU omitted"
    );
}

#[test]
fn complete_telemetry_without_backend_capability_offers_no_devices() {
    let (facts, _) = inputs();
    for reports in [
        Vec::new(),
        vec![(Route::Oci, None)],
        vec![(Route::Oci, Some(report(Vec::new())))],
    ] {
        let before_inputs = (facts.clone(), copy_reports(&reports));
        let mut cluster = service_cluster(&facts);
        let before_cluster = cluster.clone();
        assert!(
            !cluster.nodes[0].devices.is_empty(),
            "positive measured telemetry control"
        );
        assert!(offered(&reports, &facts, &mut cluster).is_empty());
        assert_eq!((facts.clone(), reports), before_inputs);
        assert_eq!(cluster, before_cluster);
    }
    eprintln!(
        "DEVICE_OFFER absent backend/unknown capacity/empty capability: 3 exact empty offers"
    );
}

#[derive(Debug, Clone, Copy)]
enum BadInput {
    Model,
    Memory,
    Duplicate,
    PoolRoom,
}

/// AC-T7.4.1: a current native observation that disagrees with its route report (model, memory),
/// a duplicate report entry, or free room above its pool's total is a typed refusal that changes
/// neither its inputs nor the cluster.
#[test]
fn mismatched_model_memory_and_capacity_refuse_without_mutating_inputs() {
    let cases = [
        BadInput::Model,
        BadInput::Memory,
        BadInput::Duplicate,
        BadInput::PoolRoom,
    ];
    for case in cases {
        let (facts, mut reports) = inputs();
        let report = reports[0].1.as_mut().unwrap();
        match case {
            BadInput::Model => report.devices[0].model = "other-model".into(),
            BadInput::Memory => report.devices[0].size += 1,
            BadInput::Duplicate => report.devices.push(report.devices[0].try_clone().unwrap()),
            BadInput::PoolRoom => report.free.cpu_milli = report.total.cpu_milli + 1,
        }
        let before_inputs = (facts.clone(), copy_reports(&reports));
        let mut cluster = service_cluster(&facts);
        let before_cluster = cluster.clone();
        let refused =
            capacity::devices::offers(&reports, &facts, &mut cluster, NAMESPACE).unwrap_err();
        eprintln!(
            "DEVICE_OFFER refusal {case:?}: {refused:?}; facts={facts:?}; reports={reports:?}; cluster_before={before_cluster:?}; cluster_after={cluster:?}"
        );
        assert!(matches!(refused, Refusal::Invalid { .. }));
        assert_eq!((facts, reports), before_inputs);
        assert_eq!(cluster, before_cluster);
        let (repaired_facts, repaired_reports) = inputs();
        let mut repaired_cluster = service_cluster(&repaired_facts);
        assert_eq!(
            offered(&repaired_reports, &repaired_facts, &mut repaired_cluster).len(),
            2
        );
    }
    eprintln!(
        "DEVICE_OFFER 4 typed refusals/input+cluster snapshots preserved; 4 repaired positive controls"
    );
}

#[derive(Debug, Clone, Copy)]
enum Unmatched {
    MissingPci,
    MalformedPci,
    WrongDomain,
    MissingGpu,
}

/// AC-T7.4.1: a reported GPU whose identity no current native observation has (no, a malformed or
/// another PCI address; no measured GPU) keeps its physical key as an unavailable placeholder of
/// unknown load that is never bound, beside the measured GPU, which still is. Inputs are unchanged.
#[test]
fn unmatched_gpu_identity_is_an_unavailable_placeholder_without_mutating_inputs() {
    let cases = [
        Unmatched::MissingPci,
        Unmatched::MalformedPci,
        Unmatched::WrongDomain,
        Unmatched::MissingGpu,
    ];
    for case in cases {
        let (mut facts, reports) = inputs();
        match case {
            Unmatched::MissingPci => facts.gpus[0].pci = None,
            Unmatched::MalformedPci => facts.gpus[0].pci = Some("12345678:01:20.3".into()),
            Unmatched::WrongDomain => facts.gpus[0].pci = Some("12345679:01:02.3".into()),
            Unmatched::MissingGpu => {
                facts.gpus.remove(0);
            }
        }
        let before_inputs = (facts.clone(), copy_reports(&reports));
        let mut cluster = service_cluster(&facts);
        let unmatched = &reports[0].1.as_ref().unwrap().devices[0];
        let measured = facts
            .gpus
            .iter()
            .find(|gpu| gpu.pci.as_deref() == Some("00000000:01:02.3"))
            .unwrap();
        // The offer publishes only the placeholder's catalog name, where new, to the model table.
        let mut expected_cluster = cluster.clone();
        let placeholder = expected_cluster.model(&unmatched.model).unwrap();
        let amd = expected_cluster.model(&measured.model).unwrap();
        let actual = offered(&reports, &facts, &mut cluster);
        eprintln!("DEVICE_OFFER unmatched {case:?}: {actual:?}");
        assert_eq!(
            actual,
            vec![
                Device {
                    kind: DeviceKind::Gpu,
                    key: Some(unmatched.id.key),
                    available: false,
                    classes: Classes::NONE.with(Route::Oci),
                    model: placeholder,
                    size: unmatched.size,
                    load: GpuLoad::default()
                },
                Device {
                    kind: DeviceKind::Gpu,
                    key: Some(0x0113),
                    available: true,
                    classes: Classes::NONE.with(Route::Oci),
                    model: amd,
                    size: measured.memory,
                    load: measured.load
                },
            ],
            "{case:?}"
        );
        assert_eq!(
            bind(&actual, |i| actual[i].available, 1, &Ask::default()),
            Some(vec![1]),
            "{case:?}: only the measured GPU is bound"
        );
        assert!(
            bind(&actual, |i| actual[i].available, 2, &Ask::default()).is_none(),
            "{case:?}: the placeholder is never bound"
        );
        assert_eq!(cluster, expected_cluster, "{case:?}");
        assert_eq!((facts, reports), before_inputs, "{case:?}");
    }
    eprintln!("DEVICE_OFFER 4 unmatched identities: unavailable placeholders, never bound");
}

#[test]
fn foreign_holders_are_unavailable_but_current_namespace_survives_for_recovery() {
    let key = Key {
        owner: 0,
        epoch: 1,
        seq: 0,
    };
    let own = crate::placement::physical_id(NAMESPACE, key).unwrap();
    let foreign = crate::placement::physical_id(FOREIGN, key).unwrap();
    let near = BoxId::new(&format!("hex-place-{NAMESPACE}0-0-1-0")).unwrap();
    let cases = [
        (None, true),
        (Some(own), true),
        (Some(foreign), false),
        (Some(near), false),
        (Some(BoxId::new("caller-id").unwrap()), false),
    ];
    for (holder, available) in cases {
        let (facts, mut reports) = inputs();
        let report = reports[0].1.as_mut().unwrap();
        report.devices.truncate(1);
        report.devices[0].holder = holder
            .as_ref()
            .map(|id| hex_api::DeviceHolder::Box(id.try_clone().unwrap()));
        let before_inputs = (facts.clone(), copy_reports(&reports));
        let mut cluster = service_cluster(&facts);
        let before_cluster = cluster.clone();
        let actual = offered(&reports, &facts, &mut cluster);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].available, available);
        assert_eq!(actual[0].key, Some(report_key(&reports)));
        let choice = bind(&actual, |index| actual[index].available, 1, &Ask::default());
        assert_eq!(
            choice.is_some(),
            available,
            "existing binder sees bridge availability"
        );
        assert_eq!((facts, reports), before_inputs);
        assert_eq!(cluster, before_cluster);
        eprintln!("DEVICE_OFFER holder={holder:?}; available={available}; bound={choice:?}");
    }
}

/// The lead's ruling of 2026-11-11 (AC-T7.4.1; original): a device a runtime lists but won't offer, with why (a VFIO
/// function no reset clears, spent on this boot), read as free, so placement chose it and only its create refused it.
/// Do: offer a report whose one measured GPU, unheld, the report lists unavailable with its reason. Expect it offered
/// unavailable and never bound; the same report without that listing offers it free and binds it.
#[test]
fn original_a_device_the_runtime_lists_unavailable_is_never_bound() {
    for listed in [false, true] {
        let (facts, mut reports) = inputs();
        let report = reports[0].1.as_mut().unwrap();
        report.devices.truncate(1);
        report.device_routes.truncate(1);
        if listed {
            report.unavailable = vec![hex_api::DeviceUnavailable {
                id: report.devices[0].id,
                why: "0000:01:02.3 has no reset method that clears it between VMs, and a VM had it on this boot; \
                      the host must reboot"
                    .into(),
            }];
        }
        let mut cluster = service_cluster(&facts);
        let actual = offered(&reports, &facts, &mut cluster);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].available, !listed, "listed unavailable: {listed}");
        let choice = bind(&actual, |index| actual[index].available, 1, &Ask::default());
        assert_eq!(choice.is_some(), !listed, "placement binds it: {choice:?}");
    }
}

fn report_key(reports: &Reports) -> u64 {
    reports[0].1.as_ref().unwrap().devices[0].id.key
}

#[test]
fn initialized_native_oci_routes_devices_and_uninitialized_class_reports_refuse() {
    for os in [Os::Linux, Os::Windows] {
        let (mut facts, reports) = inputs();
        facts.os = os;
        let mut cluster = service_cluster(&facts);
        let actual = offered(&reports, &facts, &mut cluster);
        assert_eq!(actual.len(), 2);
        assert!(actual.iter().all(|device| device.kind == DeviceKind::Gpu));
    }
    // These are misrouted native backend reports, not a claim that a valid VM/device request
    // may remain unsupported. ROOT owns the actual guest and macOS attachment paths.
    for route in [Route::Firecracker, Route::Shards] {
        let (facts, mut reports) = inputs();
        reports[0].0 = route;
        let before_inputs = (facts.clone(), copy_reports(&reports));
        let mut cluster = service_cluster(&facts);
        let before_cluster = cluster.clone();
        let refused =
            capacity::devices::offers(&reports, &facts, &mut cluster, NAMESPACE).unwrap_err();
        assert!(matches!(refused, Refusal::Invalid { .. }));
        assert_eq!((facts, reports), before_inputs);
        assert_eq!(cluster, before_cluster);
    }
    eprintln!(
        "DEVICE_OFFER Linux/Windows OCI inputs accepted; 2 misrouted native reports typed-refused; no hardware/platform completion claim"
    );
}

#[test]
fn late_refusal_does_not_publish_partial_model_interning() {
    let (mut facts, _) = inputs();
    facts.gpus.truncate(1);
    let block = DeviceInfo {
        id: DeviceId {
            kind: NativeKind::Block,
            key: (1u64 << 32) | 3,
        },
        model: "new-block-model".into(),
        size: hex_api::MIN_MEMORY,
        holder: None,
    };
    let (_, mut gpu_reports) = inputs();
    let mut gpu = gpu_reports[0].1.as_mut().unwrap().devices.remove(0);
    gpu.size += 1; // One later mismatching metadata field after a valid named native device.
    let reports = vec![(Route::Oci, Some(report(vec![block, gpu])))];
    let before_inputs = (facts.clone(), copy_reports(&reports));
    let mut cluster = service_cluster(&facts);
    let before_cluster = cluster.clone();
    let refused = capacity::devices::offers(&reports, &facts, &mut cluster, NAMESPACE).unwrap_err();
    let after_cluster = cluster.clone();
    eprintln!(
        "DEVICE_OFFER candidate late-refusal={refused:?}; facts={facts:?}; reports={reports:?}; cluster_before={before_cluster:?}; cluster_after={after_cluster:?}"
    );
    assert!(matches!(refused, Refusal::Invalid { .. }));
    assert_eq!((facts.clone(), copy_reports(&reports)), before_inputs);
    // Preserve the failed snapshots intact. A separate repaired working copy proves that the
    // same ordinary bridge still accepts the intended devices; it does not erase a candidate red.
    let mut repaired_reports = copy_reports(&reports);
    repaired_reports[0].1.as_mut().unwrap().devices[1].size = facts.gpus[0].memory;
    let mut repaired_cluster = before_cluster.clone();
    let repaired = offered(&repaired_reports, &facts, &mut repaired_cluster);
    assert_eq!(repaired.len(), 2);
    assert_eq!(repaired[0].kind, DeviceKind::Block);
    assert_eq!(repaired[1].kind, DeviceKind::Gpu);
    eprintln!(
        "DEVICE_OFFER candidate repair accepted={repaired:?}; repaired_models={:?}; failed snapshot retained",
        repaired_cluster.models
    );
    assert_eq!(
        after_cluster, before_cluster,
        "requested candidate invariant: an error publishes no partial model table; ROOT classifies/fixes separately after actual result"
    );
}

/// AC-T7.4.1: three initialized adapters alias one GPU slot, so one physical holder excludes
/// all three backends and a class without an actual offer cannot take that slot.
#[test]
fn backend_aliases_share_one_slot_and_require_exact_class_capability() {
    use hex_sim::index::{FitIndex, Verdict};
    use hex_sim::model::{Demand, Kind, WorkloadId};
    let (mut facts, mut reports) = inputs();
    facts.classes = Classes::ALL;
    facts.virt.kvm = true;
    let mut native = reports[0].1.as_ref().unwrap().try_clone().unwrap();
    native.devices.truncate(1);
    reports = Route::ALL
        .into_iter()
        .map(|class| (class, Some(native.try_clone().unwrap())))
        .collect();
    let mut cluster = service_cluster(&facts);
    let devices = offered(&reports, &facts, &mut cluster);
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].classes, Classes::ALL);
    let mut node = cluster.nodes[0].clone();
    node.devices = devices;
    node.capacity.gpus = 1;
    let mut index = FitIndex::new(&[node.clone()]).unwrap();
    let mut demand = Demand {
        need: node.capacity,
        ask: Ask::default(),
        class: Route::Firecracker,
        kind: Kind::Plain,
    };
    assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::Admits);
    index.take(NodeId(0), WorkloadId(0), &demand).unwrap();
    for class in Route::ALL {
        demand.class = class;
        assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::NoRoom);
    }
    // Same identity survives; only the initialized adapter mask changes in this independent node.
    node.devices[0].classes = Classes::NONE.with(Route::Oci);
    let index = FitIndex::new(&[node]).unwrap();
    demand.class = Route::Firecracker;
    assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::NoGpu);
    demand.class = Route::Oci;
    assert_eq!(index.verdict(NodeId(0), &demand).unwrap(), Verdict::Admits);
}

/// AC-T7.4.1: backend aliases cannot disagree about a holder or redefine physical metadata.
#[test]
fn conflicting_backend_aliases_refuse_before_publishing_any_offer_or_model() {
    let (mut facts, reports) = inputs();
    facts.classes = Classes::ALL;
    facts.virt.kvm = true;
    for alter in [0, 1, 2] {
        let mut alias = reports[0].1.as_ref().unwrap().try_clone().unwrap();
        match alter {
            0 => {
                alias.devices[0].holder = Some(hex_api::DeviceHolder::Box(
                    BoxId::new("foreign-holder").unwrap(),
                ))
            }
            1 => alias.devices[0].model = "wrong-model".into(),
            _ => alias.devices[0].size += 1,
        }
        let mut both = copy_reports(&reports);
        both.push((Route::Firecracker, Some(alias)));
        let mut cluster = service_cluster(&facts);
        let before = cluster.clone();
        assert!(capacity::devices::offers(&both, &facts, &mut cluster, NAMESPACE).is_err());
        assert_eq!(cluster, before);
    }
}
