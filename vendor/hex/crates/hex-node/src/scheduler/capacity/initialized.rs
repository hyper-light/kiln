//! Normalize actual initialized endpoints into the existing host/index resource plane.

use super::{amounts, least};
use hex_api::{Capacity, Refusal};
use hex_sim::model::{Class, Classes, Cluster, NodeSpec, Resources, RuntimeRoutes};

fn invalid(why: &'static str) -> Refusal {
    let mut text = String::new();
    if text.try_reserve_exact(why.len()).is_err() {
        return Refusal::Exhausted {
            what: "initialized capacity refusal bytes",
            bound: why.len(),
        };
    }
    text.push_str(why);
    Refusal::Invalid {
        what: "initialized runtime capacity",
        why: text,
    }
}

fn reserve<T>(values: &mut Vec<T>, count: usize, what: &'static str) -> Result<(), Refusal> {
    values
        .try_reserve_exact(count)
        .map_err(|_| Refusal::Exhausted { what, bound: count })
}

fn bit(at: usize) -> Result<u64, Refusal> {
    u32::try_from(at)
        .ok()
        .and_then(|at| 1u64.checked_shl(at))
        .ok_or_else(|| invalid("route eligibility exceeds its 64-bit address space"))
}

fn class(form: hex_api::Class) -> Class {
    match form {
        hex_api::Class::Process | hex_api::Class::Oci => Class::Oci,
        hex_api::Class::Shards => Class::Shards,
        hex_api::Class::Firecracker => Class::Firecracker,
    }
}

/// No native calls, boot or grants occur here. Reports are moved into one immutable view after
/// physical identity/metadata and the common resource-plane identity have been checked.
pub(super) fn apply(
    reports: Vec<(Class, Option<Capacity>)>,
    facts: &hex_sim::facts::Facts,
    cluster: &mut Cluster,
    namespace: &str,
    node: &mut NodeSpec,
    owned: Resources,
) -> Result<(), Refusal> {
    let devices = super::devices::offers(&reports, facts, cluster, namespace)?;
    let mut offers: Vec<hex_api::RouteOffer> = Vec::new();
    reserve(
        &mut offers,
        hex_api::MAX_RUNTIME_ROUTES,
        "initialized route catalog",
    )?;
    let mut plane = None;
    let mut room = None;
    let mut seen = Classes::NONE;
    for (backend, report) in &reports {
        if seen.has(*backend) {
            return Err(invalid("duplicate initialized endpoint class"));
        }
        seen = seen.with(*backend);
        let Some(report) = report else { continue };
        report.check()?;
        if report.routes.is_empty() {
            return Err(invalid(
                "legacy and initialized route reports cannot share admission",
            ));
        }
        if let Some((identity, total)) = plane {
            if identity != report.id.as_str() || total != report.total {
                return Err(invalid(
                    "initialized routes disagree about their physical host resource plane",
                ));
            }
        } else {
            plane = Some((report.id.as_str(), report.total));
        }
        let free = amounts(&report.free, 0);
        room = Some(room.map_or(free, |previous| least(previous, free)));
        for offer in &report.routes {
            if let Some(previous) = offers
                .iter()
                .find(|previous| previous.key == offer.key && previous.backing == offer.backing)
            {
                if previous != offer {
                    return Err(invalid(
                        "route aliases disagree about cost or canonical backing",
                    ));
                }
            } else {
                if offers.len() >= hex_api::MAX_RUNTIME_ROUTES {
                    return Err(invalid("combined routes exceed their eligibility mask"));
                }
                offers.push(*offer);
            }
        }
    }
    let (_, total) = plane.ok_or_else(|| invalid("no initialized host resource plane"))?;
    let credited = room
        .ok_or_else(|| invalid("no initialized host free-room observation"))?
        .checked_add(&Resources {
            mem_bytes: owned.mem_bytes,
            disk_bytes: owned.disk_bytes,
            ..Resources::default()
        })
        .ok_or_else(|| invalid("measured owned resources plus host room overflow"))?;
    let host = least(amounts(&total, 0), credited);
    let mut by_id = Vec::new();
    reserve(
        &mut by_id,
        devices.len(),
        "normalized device identity index",
    )?;
    for (at, device) in devices.iter().enumerate() {
        by_id.push((
            device
                .native_id()
                .ok_or_else(|| invalid("initialized device lacks native identity"))?,
            at,
        ));
    }
    by_id.sort_unstable_by_key(|(id, _)| *id);
    let mut masks = Vec::new();
    reserve(&mut masks, devices.len(), "normalized device route masks")?;
    masks.resize(devices.len(), 0u64);
    // The room its NodeVm routes admit within (joint plan step 39, V2): its Ready node VM's guest, the one guest that
    // serves its oci class.
    let mut node_vm_room = None;
    for receipt in reports
        .iter()
        .filter_map(|(_, report)| report.as_ref())
        .flat_map(|report| &report.node_vms)
        .filter(|receipt| receipt.phase == hex_api::NodeVmPhase::Ready)
    {
        if node_vm_room.replace(receipt.guest).is_some() {
            return Err(invalid(
                "two node VMs Ready in one node's answers: one guest serves its oci class",
            ));
        }
    }
    let mut backings: Vec<hex_api::BackingOffer> = Vec::new();
    reserve(
        &mut backings,
        hex_api::MAX_RUNTIME_ROUTES,
        "initialized backing catalog",
    )?;
    for (_, report) in reports {
        let Some(report) = report else { continue };
        for (device, local_mask) in report.devices.iter().zip(&report.device_routes) {
            let at = by_id
                .binary_search_by_key(&device.id, |(id, _)| *id)
                .ok()
                .and_then(|at| by_id.get(at))
                .map(|(_, at)| *at)
                .ok_or_else(|| invalid("route mask has no canonical physical device"))?;
            let mask = masks
                .get_mut(at)
                .ok_or_else(|| invalid("canonical device index is absent"))?;
            for (local_at, offer) in report.routes.iter().enumerate() {
                if local_mask & bit(local_at)? == 0 {
                    continue;
                }
                let global_at = offers
                    .iter()
                    .position(|global| global.key == offer.key && global.backing == offer.backing)
                    .ok_or_else(|| invalid("local route is absent from normalized catalog"))?;
                *mask |= bit(global_at)?;
            }
        }
        for backing in report.backings {
            if let Some(previous) = backings
                .iter()
                .find(|previous| previous.key.pool == backing.key.pool)
            {
                if previous != &backing {
                    return Err(invalid(
                        "backing aliases disagree about generation, profile, phase or complete claims",
                    ));
                }
            } else {
                if backings.len() >= hex_api::MAX_RUNTIME_ROUTES {
                    return Err(invalid(
                        "combined backing catalog exceeds route address space",
                    ));
                }
                backings.push(backing);
            }
        }
    }
    let routes = RuntimeRoutes {
        admit: None,
        offers,
        device_routes: masks,
        backings,
        node_vm_room,
    };
    let provisioned = routes
        .unobserved_claims(&devices, node.id)
        .map_err(|_| invalid("canonical physical claim union exceeds its address space"))?;
    let gpus = devices
        .iter()
        .filter(|device| device.kind == hex_sim::model::DeviceKind::Gpu)
        .count()
        .checked_add(
            provisioned
                .iter()
                .filter(|id| id.kind == hex_api::DeviceKind::Gpu)
                .count(),
        )
        .and_then(|count| u32::try_from(count).ok())
        .ok_or_else(|| invalid("canonical GPU claim count overflows"))?;
    // Every route remains constrained by the host plane. Native HCS/OCI and CPU-only process
    // routes never inherit another backend's fixed guest ceiling or per-box VM cost.
    node.capacity = Resources { gpus, ..host };
    node.classes = routes.offers.iter().fold(Classes::NONE, |classes, offer| {
        let payload = class(offer.key.class);
        if facts.classes.has(payload) {
            classes.with(payload)
        } else {
            classes
        }
    });
    node.devices = devices;
    node.runtime_pools.clear();
    node.costs = [None; Class::COUNT];
    let routes = routes.share().map_err(|_| Refusal::Exhausted {
        what: "immutable initialized runtime routes",
        bound: 1,
    })?;
    routes
        .check(node)
        .map_err(|_| invalid("normalized initialized route topology is invalid"))?;
    node.routes = Some(routes);
    Ok(())
}

/// Keep a failed observer's immutable receipts in the same catalog for accounting/recovery.
/// No retained metadata is a fresh reading, and only successful current offers keep admit bits.
pub(super) fn retain_unavailable(
    node: &mut NodeSpec,
    previous: Option<&NodeSpec>,
    unavailable: &[hex_api::BackendUnavailable],
) -> Result<(), Refusal> {
    if unavailable.is_empty() {
        return Ok(());
    }
    for observation in unavailable {
        observation.check()?;
    }
    let failed = |offer: &hex_api::RouteOffer| {
        unavailable.iter().any(|value| {
            value.backend == offer.key.backend_class()
                && matches!(&value.cause, hex_api::Refusal::Backing { key, .. }
                    if offer.backing == Some(*key))
        })
    };
    let prior = previous.and_then(|node| node.routes.as_deref());
    let Some(shared) = node.routes.as_mut() else {
        if prior.is_some_and(|prior| prior.offers.iter().any(failed)) {
            return Err(invalid(
                "retained initialized routes require the actual native route plane",
            ));
        }
        node.classes = Class::ALL
            .into_iter()
            .filter(|&payload| {
                node.classes.has(payload)
                    && !unavailable
                        .iter()
                        .any(|value| class(value.backend) == payload)
            })
            .fold(Classes::NONE, Classes::with);
        return Ok(());
    };
    let current = triomphe::Arc::get_mut(shared)
        .ok_or_else(|| invalid("fresh normalized route view was already published"))?;
    let mut admit = 0u64;
    for (at, offer) in current.offers.iter().enumerate() {
        let backing = offer
            .backing
            .and_then(|key| current.backings.iter().find(|backing| backing.key == key));
        let mut blocked = false;
        for observation in unavailable {
            blocked |= observation.blocks_route(offer, backing)?;
        }
        if current.admits(at) && !blocked {
            admit |= bit(at)?;
        }
    }
    if let (Some(previous), Some(prior)) = (previous, prior) {
        for offer in prior.offers.iter().filter(|offer| failed(offer)) {
            if let Some(existing) = current
                .offers
                .iter()
                .find(|existing| existing.key == offer.key && existing.backing == offer.backing)
            {
                if existing != offer {
                    return Err(invalid(
                        "retained qualified route changed its immutable cost",
                    ));
                }
            } else {
                if current.offers.len() >= hex_api::MAX_RUNTIME_ROUTES {
                    return Err(invalid("retained routes exceed their mask address space"));
                }
                reserve(&mut current.offers, 1, "retained unavailable routes")?;
                current.offers.push(*offer);
            }
            node.classes = node.classes.with(class(offer.key.class));
            if let Some(key) = offer.backing {
                let backing = prior
                    .backings
                    .iter()
                    .find(|backing| backing.key == key)
                    .ok_or_else(|| invalid("retained route has no canonical backing"))?;
                if let Some(existing) = current
                    .backings
                    .iter()
                    .find(|existing| existing.key.pool == key.pool)
                {
                    if existing.cost != backing.cost
                        || existing.devices != backing.devices
                        || existing.key != backing.key
                    {
                        return Err(invalid(
                            "unavailable backing changed generation/profile/full claims",
                        ));
                    }
                } else {
                    if current.backings.len() >= hex_api::MAX_RUNTIME_ROUTES {
                        return Err(invalid("retained backings exceed their route bound"));
                    }
                    reserve(&mut current.backings, 1, "retained unavailable backings")?;
                    current.backings.push(backing.try_clone()?);
                }
            }
        }
        let mut by_id = Vec::new();
        reserve(
            &mut by_id,
            node.devices.len(),
            "current physical identity index",
        )?;
        for (at, device) in node.devices.iter().enumerate() {
            by_id.push((
                device
                    .native_id()
                    .ok_or_else(|| invalid("current device has no stable identity"))?,
                at,
            ));
        }
        by_id.sort_unstable_by_key(|(id, _)| *id);
        for (old_device, old_mask) in previous.devices.iter().zip(&prior.device_routes) {
            let mut mask = 0u64;
            for (at, offer) in prior.offers.iter().enumerate() {
                if old_mask & bit(at)? != 0 && failed(offer) {
                    let Some(at) = current.offers.iter().position(|current| current == offer)
                    else {
                        continue;
                    };
                    mask |= bit(at)?;
                }
            }
            if mask == 0 {
                continue;
            }
            let id = old_device
                .native_id()
                .ok_or_else(|| invalid("retained device has no stable identity"))?;
            // Prior catalog identities are unique. Newly appended retained entries cannot alias
            // another prior entry, so only the immutable fresh prefix needs this sorted lookup.
            let at = if let Some(at) = by_id
                .binary_search_by_key(&id, |(id, _)| *id)
                .ok()
                .and_then(|at| by_id.get(at))
                .map(|(_, at)| *at)
            {
                let actual = node
                    .devices
                    .get_mut(at)
                    .ok_or_else(|| invalid("retained device index is absent"))?;
                if actual.kind != old_device.kind
                    || actual.model != old_device.model
                    || actual.size != old_device.size
                {
                    return Err(invalid("retained physical identity changed metadata"));
                }
                for class in Class::ALL {
                    if old_device.classes.has(class) {
                        actual.classes = actual.classes.with(class);
                    }
                }
                at
            } else {
                if node.devices.len() >= hex_api::MAX_DEVICES {
                    return Err(invalid(
                        "retained devices exceed the physical address space",
                    ));
                }
                reserve(&mut node.devices, 1, "retained unavailable physical slots")?;
                reserve(
                    &mut current.device_routes,
                    1,
                    "retained unavailable device masks",
                )?;
                let mut retained = *old_device;
                retained.available = false;
                retained.load = hex_sim::model::GpuLoad::default();
                let at = node.devices.len();
                node.devices.push(retained);
                current.device_routes.push(0);
                at
            };
            *current
                .device_routes
                .get_mut(at)
                .ok_or_else(|| invalid("retained physical route mask is absent"))? |= mask;
        }
    }
    current.admit = Some(admit);
    let unknown = current
        .unobserved_claims(&node.devices, node.id)
        .map_err(|_| invalid("retained physical claim union is invalid"))?;
    node.capacity.gpus = node
        .devices
        .iter()
        .filter(|device| device.kind == hex_sim::model::DeviceKind::Gpu)
        .count()
        .checked_add(
            unknown
                .iter()
                .filter(|id| id.kind == hex_api::DeviceKind::Gpu)
                .count(),
        )
        .and_then(|count| u32::try_from(count).ok())
        .ok_or_else(|| invalid("retained physical GPU count overflows"))?;
    node.check_runtime_pools()
        .map_err(|_| invalid("retained initialized route topology is invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::{
        BackingKey, BackingOffer, BackingPhase, CapacityResources, DeviceId, DeviceInfo,
        DeviceKind, Execution, GpuEligibility, ImageArch, ImageOs, ImagePlatform, RouteKey,
        RouteOffer, RuntimeCost,
    };
    use hex_sim::facts::{Cpu, Facts, Gpu};
    use hex_sim::model::{GpuLoad, NodeId, Os, Virt};

    fn room() -> CapacityResources {
        CapacityResources {
            cpu_milli: 8_000,
            mem_bytes: 8 << 30,
            disk_bytes: 8 << 30,
        }
    }
    fn profile() -> RuntimeCost {
        let guest = CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 512 << 20,
            disk_bytes: 0,
        };
        RuntimeCost {
            id: [0x72; 32],
            floor: guest,
            ceiling: guest,
            quantum: CapacityResources {
                cpu_milli: 1_000,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources {
                cpu_milli: 100,
                mem_bytes: 64 << 20,
                disk_bytes: 0,
            },
        }
    }
    fn route(class: hex_api::Class, execution: Execution, gpu: GpuEligibility) -> RouteOffer {
        RouteOffer {
            key: RouteKey {
                class,
                execution,
                gpu,
                image: ImagePlatform {
                    os: ImageOs::Linux,
                    arch: ImageArch::Amd64,
                },
            },
            cost: (execution == Execution::Firecracker).then(profile),
            backing: None,
        }
    }
    fn report(
        routes: Vec<RouteOffer>,
        devices: Vec<DeviceInfo>,
        masks: Vec<u64>,
        backings: Vec<BackingOffer>,
    ) -> Capacity {
        Capacity {
            own: Default::default(),
            id: "same-native-host".into(),
            total: room(),
            free: room(),
            cost: None,
            routes,
            devices,
            device_routes: masks,
            backings,
            unavailable: Vec::new(),
            learned: None,
            node_vms: Vec::new(),
            unlearned: None,
        }
    }
    fn facts() -> Facts {
        Facts {
            os: Os::Linux,
            cpu: Cpu {
                threads: 8,
                ..Cpu::default()
            },
            classes: Classes::ALL,
            virt: Virt {
                kvm: true,
                ..Virt::default()
            },
            gpus: vec![Gpu {
                key: Some(1),
                model: "actual fixture GPU".into(),
                memory: 2 << 30,
                load: GpuLoad {
                    busy_per_mille: Some(0),
                    memory_used: Some(0),
                },
                ..Gpu::default()
            }],
            ..Facts::default()
        }
    }
    fn device() -> DeviceInfo {
        DeviceInfo {
            id: DeviceId {
                kind: DeviceKind::Gpu,
                key: 1,
            },
            model: "actual fixture GPU".into(),
            size: 2 << 30,
            holder: None,
        }
    }
    fn normalize(reports: Vec<(Class, Option<Capacity>)>) -> Result<NodeSpec, Refusal> {
        let facts = facts();
        let mut node = NodeSpec::new(NodeId(0), Resources::default());
        let mut cluster = Cluster::single_owner(Vec::new(), Vec::new());
        apply(
            reports,
            &facts,
            &mut cluster,
            "00112233445566778899aabbccddeeff",
            &mut node,
            Resources::default(),
        )?;
        Ok(node)
    }

    /// AC-T7.4.1: aliases reorder local bits, but one physical device and one host plane survive.
    #[test]
    fn service_route_normalization_remaps_alias_bits_and_refuses_conflicting_plane() {
        let native = route(hex_api::Class::Oci, Execution::Native, GpuEligibility::Any);
        let process = route(
            hex_api::Class::Process,
            Execution::Native,
            GpuEligibility::None,
        );
        let input = || {
            vec![
                (
                    Class::Oci,
                    Some(report(
                        vec![native, process],
                        vec![device()],
                        vec![1],
                        Vec::new(),
                    )),
                ),
                (
                    Class::Shards,
                    Some(report(
                        vec![process, native],
                        vec![device()],
                        vec![2],
                        Vec::new(),
                    )),
                ),
            ]
        };
        let node = normalize(input()).unwrap();
        assert_eq!(node.capacity.cpu_milli, room().cpu_milli);
        assert_eq!(node.capacity.mem_bytes, room().mem_bytes);
        assert_eq!(node.devices.len(), 1);
        assert_eq!(node.routes.as_ref().unwrap().device_routes, vec![1]);
        assert_eq!(node.routes.as_ref().unwrap().offers.len(), 2);
        assert!(node.costs.iter().all(Option::is_none));
        assert!(node.runtime_pools.is_empty());
        let mut bad = input();
        bad.get_mut(1).unwrap().1.as_mut().unwrap().id = "unrelated-guest-budget".into();
        assert!(normalize(bad).is_err());
    }

    /// AC-T7.4.1: an unobserved cold GPU is a reservable physical claim, never invented telemetry.
    #[test]
    fn service_cold_claim_union_has_no_fabricated_device_or_classwide_vm_cap() {
        let key = BackingKey {
            pool: [0x24; 32],
            generation: [0x25; 16],
        };
        let mut vm = route(
            hex_api::Class::Firecracker,
            Execution::Firecracker,
            GpuEligibility::Required,
        );
        vm.backing = Some(key);
        let backing = BackingOffer {
            guest: Default::default(),
            key,
            cost: profile(),
            phase: BackingPhase::Prepared,
            devices: vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 2,
            }],
        };
        let node = normalize(vec![
            (
                Class::Oci,
                Some(report(
                    vec![route(
                        hex_api::Class::Oci,
                        Execution::Native,
                        GpuEligibility::Any,
                    )],
                    vec![device()],
                    vec![1],
                    Vec::new(),
                )),
            ),
            (
                Class::Firecracker,
                Some(report(vec![vm], Vec::new(), Vec::new(), vec![backing])),
            ),
        ])
        .unwrap();
        assert_eq!(node.capacity.gpus, 2);
        assert_eq!(node.devices.len(), 1);
        assert_eq!(node.devices.first().unwrap().key, Some(1));
        assert_eq!(node.capacity.mem_bytes, room().mem_bytes);
        assert_eq!(
            node.routes
                .as_ref()
                .unwrap()
                .unobserved_claims(&node.devices, node.id)
                .unwrap(),
            vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 2
            }]
        );
        hex_sim::index::FitIndex::new(&[node]).unwrap();
    }
    fn pending_gpu_and_shared_cpu_fixture(
        phase: BackingPhase,
        cause: hex_api::LifecycleCause,
    ) -> (NodeSpec, BackingKey, hex_api::DeviceId) {
        let cold = BackingKey {
            pool: [0x34; 32],
            generation: [0x35; 16],
        };
        let warm = BackingKey {
            pool: [0x36; 32],
            generation: [0x37; 16],
        };
        let id = DeviceId {
            kind: DeviceKind::Gpu,
            key: 2,
        };
        let mut gpu = route(
            hex_api::Class::Shards,
            Execution::ShardsDedicated,
            GpuEligibility::Required,
        );
        gpu.cost = Some(profile());
        gpu.backing = Some(cold);
        let mut shared = route(
            hex_api::Class::Oci,
            Execution::ShardsShared,
            GpuEligibility::None,
        );
        shared.backing = Some(warm);
        let reports = vec![
            (
                Class::Oci,
                Some(report(
                    vec![route(
                        hex_api::Class::Process,
                        Execution::Native,
                        GpuEligibility::None,
                    )],
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )),
            ),
            (
                Class::Shards,
                Some(report(
                    vec![gpu, shared],
                    Vec::new(),
                    Vec::new(),
                    vec![
                        BackingOffer {
                            guest: Default::default(),
                            key: cold,
                            cost: profile(),
                            phase,
                            devices: vec![id],
                        },
                        BackingOffer {
                            // Measured as it went Live (joint plan step 14, C7, pin 1): nothing of its own.
                            guest: hex_api::LearnedOverhead {
                                cpu_milli: Some(0),
                                mem_bytes: Some(0),
                                disk_bytes: Some(0),
                            },
                            key: warm,
                            cost: profile(),
                            phase: BackingPhase::Live,
                            devices: Vec::new(),
                        },
                    ],
                )),
            ),
        ];
        let mut measured = facts();
        measured.gpus.clear();
        let mut cluster = Cluster::single_owner(Vec::new(), Vec::new());
        let mut node = NodeSpec::new(NodeId(0), Resources::default());
        node.platform.os = Os::Linux;
        node.platform.arch = hex_sim::model::Arch::X86_64;
        node.virt.kvm = true;
        apply(
            reports,
            &measured,
            &mut cluster,
            "00112233445566778899aabbccddeeff",
            &mut node,
            Resources::default(),
        )
        .unwrap();
        retain_unavailable(
            &mut node,
            None,
            &[hex_api::BackendUnavailable::from_refusal(
                hex_api::Class::Shards,
                Refusal::Backing {
                    key: cold,
                    cause,
                    why: None,
                },
            )
            .unwrap()],
        )
        .unwrap();
        (node, cold, id)
    }

    fn original_pending_request(gpu: bool) -> hex_sim::model::Demand {
        hex_sim::model::Demand {
            class: if gpu { Class::Shards } else { Class::Oci },
            kind: hex_sim::model::Kind::Plain,
            need: Resources {
                cpu_milli: 100,
                mem_bytes: hex_api::MIN_MEMORY,
                disk_bytes: 0,
                gpus: u32::from(gpu),
            },
            ask: hex_sim::model::Ask {
                runtime_class: Some(if gpu {
                    hex_api::Class::Shards
                } else {
                    hex_api::Class::Oci
                }),
                os: Some(Os::Linux),
                arch: Some(hex_sim::model::Arch::X86_64),
                ..hex_sim::model::Ask::default()
            },
        }
    }

    /// AC-T7.4.1 source-first: a typed cold Pending is not fresh GPU observation or lost provisioning authority.
    #[test]
    fn cold_pending_gpu_source_keeps_exact_charged_provisioning_and_unrelated_shared_cpu() {
        use hex_sim::index::{FitIndex, Verdict};
        let (node, key, id) = pending_gpu_and_shared_cpu_fixture(
            BackingPhase::Prepared,
            hex_api::LifecycleCause::Pending,
        );
        assert!(
            node.devices.is_empty(),
            "provisioning must not fabricate measured metadata"
        );
        let mut index = FitIndex::new(&[node]).unwrap();
        let fresh = index
            .verdict(NodeId(0), &original_pending_request(true))
            .unwrap();
        let cpu = index
            .verdict(NodeId(0), &original_pending_request(false))
            .unwrap();
        let before = index.key(NodeId(0)).unwrap();
        let grant = index.grant_backing(NodeId(0), key);
        eprintln!(
            "cold pending original: fresh={fresh:?}; unrelated_shared_cpu={cpu:?}; exact_grant={grant:?}; physical_observations=0; native_effect=false"
        );
        let grant = grant.expect(
            "canonical Prepared full claims/profile must remain provisionable by the SAME Index",
        );
        assert_eq!(grant.key, key);
        assert_eq!(grant.devices, vec![id]);
        assert_eq!(
            grant.allocation,
            profile()
                .allocate(hex_api::CapacityResources::default())
                .unwrap()
        );
        assert_eq!(fresh, Verdict::NoGpu);
        assert_eq!(cpu, Verdict::Admits);
        let after = index.key(NodeId(0)).unwrap();
        assert_eq!(
            before.cpu_milli.checked_sub(after.cpu_milli),
            Some(grant.allocation.resources.cpu_milli)
        );
        assert_eq!(
            before.mem_bytes.checked_sub(after.mem_bytes),
            Some(grant.allocation.resources.mem_bytes)
        );
    }

    /// AC-T7.4.1 source-first: an unavailable Live GPU key is accounting-only, not failure of a separate shared CPU key.
    #[test]
    fn live_uncertain_gpu_key_does_not_suppress_unrelated_shared_cpu_or_release_claims() {
        use hex_sim::index::{FitIndex, Verdict};
        let (node, key, id) = pending_gpu_and_shared_cpu_fixture(
            BackingPhase::Live,
            hex_api::LifecycleCause::Uncertain,
        );
        let mut index = FitIndex::new(&[node]).unwrap();
        let before = index.key(NodeId(0)).unwrap();
        let fresh = index
            .verdict(NodeId(0), &original_pending_request(true))
            .unwrap();
        let cpu = index
            .verdict(NodeId(0), &original_pending_request(false))
            .unwrap();
        let retained = index.grant_backing(NodeId(0), key).unwrap();
        eprintln!(
            "live uncertain original: fresh={fresh:?}; unrelated_shared_cpu={cpu:?}; retained={retained:?}; physical_observations=0; native_effect=false"
        );
        assert_eq!(retained.key, key);
        assert_eq!(retained.devices, vec![id]);
        assert_eq!(
            index.key(NodeId(0)).unwrap(),
            before,
            "a retained grant lookup cannot refund resources"
        );
        assert_eq!(fresh, Verdict::NoGpu);
        assert_eq!(cpu, Verdict::Admits);
    }
    /// AC-T7.4.1: healthy report normalization fixes the expected taxonomy/authority independently of diagnostics.
    #[test]
    fn healthy_normalized_cold_and_live_sources_keep_no_gpu_cpu_room_and_exact_grants() {
        use hex_sim::index::{FitIndex, Verdict};
        let key = BackingKey {
            pool: [0x34; 32],
            generation: [0x35; 16],
        };
        let warm = BackingKey {
            pool: [0x36; 32],
            generation: [0x37; 16],
        };
        let id = DeviceId {
            kind: DeviceKind::Gpu,
            key: 2,
        };
        for phase in [BackingPhase::Prepared, BackingPhase::Live] {
            let mut gpu = route(
                hex_api::Class::Shards,
                Execution::ShardsDedicated,
                GpuEligibility::Required,
            );
            gpu.cost = Some(profile());
            gpu.backing = Some(key);
            let mut shared = route(
                hex_api::Class::Oci,
                Execution::ShardsShared,
                GpuEligibility::None,
            );
            shared.backing = Some(warm);
            let node = normalize(vec![
                (
                    Class::Oci,
                    Some(report(
                        vec![route(
                            hex_api::Class::Process,
                            Execution::Native,
                            GpuEligibility::None,
                        )],
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                    )),
                ),
                (
                    Class::Shards,
                    Some(report(
                        vec![gpu, shared],
                        Vec::new(),
                        Vec::new(),
                        vec![
                            BackingOffer {
                                guest: Default::default(),
                                key,
                                cost: profile(),
                                phase,
                                devices: vec![id],
                            },
                            BackingOffer {
                                // Measured as it went Live (joint plan step 14, C7, pin 1): nothing of its own.
                                guest: hex_api::LearnedOverhead {
                                    cpu_milli: Some(0),
                                    mem_bytes: Some(0),
                                    disk_bytes: Some(0),
                                },
                                key: warm,
                                cost: profile(),
                                phase: BackingPhase::Live,
                                devices: Vec::new(),
                            },
                        ],
                    )),
                ),
            ])
            .unwrap();
            assert!(node.devices.is_empty());
            let mut index = FitIndex::new(&[node]).unwrap();
            assert_eq!(
                index
                    .verdict(NodeId(0), &original_pending_request(true))
                    .unwrap(),
                Verdict::NoGpu
            );
            assert_eq!(
                index
                    .verdict(NodeId(0), &original_pending_request(false))
                    .unwrap(),
                Verdict::Admits
            );
            let before = index.key(NodeId(0)).unwrap();
            let grant = index.grant_backing(NodeId(0), key).unwrap();
            assert_eq!(grant.devices, vec![id]);
            assert_eq!(
                grant.allocation,
                profile()
                    .allocate(hex_api::CapacityResources::default())
                    .unwrap()
            );
            let after = index.key(NodeId(0)).unwrap();
            if phase == BackingPhase::Prepared {
                assert_eq!(
                    before.cpu_milli.checked_sub(after.cpu_milli),
                    Some(grant.allocation.resources.cpu_milli)
                );
            } else {
                assert_eq!(after, before);
            }
        }
    }
    /// AC-T7.4.1: reversed local masks map by full tuple while Released history remains addressable.
    #[test]
    fn normalized_historical_and_current_tuples_keep_exact_masks_and_current_choice() {
        let parent = BackingKey {
            pool: [0x41; 32],
            generation: [0x42; 16],
        };
        let current = BackingKey {
            pool: [0x43; 32],
            generation: [0x44; 16],
        };
        let id = device().id;
        let mut historical = route(
            hex_api::Class::Shards,
            Execution::ShardsDedicated,
            GpuEligibility::Required,
        );
        historical.cost = Some(profile());
        historical.backing = Some(parent);
        let fresh = RouteOffer {
            backing: Some(current),
            ..historical
        };
        let rows = || {
            vec![
                BackingOffer {
                    guest: Default::default(),
                    key: parent,
                    cost: profile(),
                    phase: BackingPhase::Released,
                    devices: vec![id, DeviceId { key: 2, ..id }],
                },
                BackingOffer {
                    guest: Default::default(),
                    key: current,
                    cost: profile(),
                    phase: BackingPhase::Live,
                    devices: vec![id],
                },
            ]
        };
        let actual = || {
            let mut actual = device();
            actual.holder = Some(hex_api::DeviceHolder::Backing(current));
            actual
        };
        let node = normalize(vec![
            (
                Class::Oci,
                Some(report(
                    vec![historical, fresh],
                    vec![actual()],
                    vec![2],
                    rows(),
                )),
            ),
            (
                Class::Shards,
                Some(report(
                    vec![fresh, historical],
                    vec![actual()],
                    vec![1],
                    rows(),
                )),
            ),
        ])
        .unwrap();
        let routes = node.routes.as_ref().unwrap();
        assert_eq!(routes.offers.len(), 2);
        assert_eq!(routes.backings.len(), 2);
        assert_eq!(routes.device_routes, vec![2]);
        assert_eq!(node.devices.len(), 1);
        let index = hex_sim::index::FitIndex::new(&[node]).unwrap();
        let demand = original_pending_request(true);
        assert_eq!(
            index.verdict(NodeId(0), &demand).unwrap(),
            hex_sim::index::Verdict::Admits
        );
        assert_eq!(
            index.route(NodeId(0), &demand).unwrap().unwrap().backing,
            Some(current)
        );
    }
}
