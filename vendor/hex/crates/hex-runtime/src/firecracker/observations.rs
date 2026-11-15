//! Current guest-native facts joined through the VMM's authoritative PCI assignment witness.
//! The result is transient observation, not another inventory cache, picker or holder authority.

use hex_api::{
    BoxId, Capacity, Class, DeviceId, DeviceInfo, DeviceKind, Execution, GpuEligibility, ImageOs,
    PhysicalDevice, PhysicalReport, Refusal,
};

use super::{exhausted, invalid};

#[derive(Clone, Copy)]
pub(super) struct Assignment {
    pub host: DeviceId,
    pub guest: DeviceId,
}

/// Called only after this VM's private readiness/connection and exact host->guest SBDF witnesses
/// have been established. Catalog/BAR values never fill absent native facts or counters.
pub(super) fn join(
    assignments: &[Assignment],
    report: &PhysicalReport,
) -> Result<Vec<PhysicalDevice>, Refusal> {
    report.check()?;
    if assignments.len() > hex_api::MAX_DEVICES {
        return Err(exhausted(
            "guest physical assignments",
            hex_api::MAX_DEVICES,
        ));
    }
    let mut hosts = Vec::new();
    let mut guests = Vec::new();
    reserve(&mut hosts, assignments.len())?;
    reserve(&mut guests, assignments.len())?;
    for assignment in assignments {
        assignment.host.check()?;
        assignment.guest.check()?;
        if assignment.host.kind != DeviceKind::Gpu || assignment.guest.kind != DeviceKind::Gpu {
            return Err(invalid("native GPU witness contains another device kind"));
        }
        hosts.push(assignment.host);
        guests.push(assignment.guest);
    }
    hosts.sort_unstable();
    guests.sort_unstable();
    if repeated(&hosts) || repeated(&guests) {
        return Err(invalid("VMM witnesses alias a selected host or guest GPU"));
    }
    let mut native = Vec::new();
    reserve(&mut native, report.devices.len())?;
    native.extend(report.devices.iter());
    native.sort_unstable_by_key(|device| device.info.id);
    let mut joined = Vec::new();
    reserve(&mut joined, assignments.len())?;
    for assignment in assignments {
        let at = native
            .binary_search_by_key(&assignment.guest, |device| device.info.id)
            .map_err(|_| invalid("assigned PCI GPU has no current guest-native observation"))?;
        let actual = native
            .get(at)
            .ok_or_else(|| invalid("guest-native index disagrees"))?;
        if actual.info.size == 0 {
            return Err(invalid(
                "assigned GPU has no observed native memory capacity",
            ));
        }
        let mut model = String::new();
        model
            .try_reserve_exact(actual.info.model.len())
            .map_err(|_| exhausted("observed GPU model bytes", actual.info.model.len()))?;
        model.push_str(&actual.info.model);
        joined.push(PhysicalDevice {
            info: DeviceInfo {
                id: assignment.host,
                model,
                size: actual.info.size,
                holder: None,
            },
            busy_per_mille: actual.busy_per_mille,
            memory_used_bytes: actual.memory_used_bytes,
        });
    }
    Ok(joined)
}

/// Driver visibility alone does not prove that the SAME guest node can bind the selected GPU to
/// its native OCI payload. Check its already validated attachment routes, without selecting again.
pub(super) fn check_bindings(
    assignments: &[Assignment],
    report: &PhysicalReport,
    capacity: &Capacity,
    owner: Option<&BoxId>,
) -> Result<(), Refusal> {
    report.check()?;
    capacity.check()?;
    if assignments.len() > hex_api::MAX_DEVICES {
        return Err(exhausted(
            "guest attachment witnesses",
            hex_api::MAX_DEVICES,
        ));
    }
    let mut guests = Vec::new();
    reserve(&mut guests, assignments.len())?;
    guests.extend(assignments.iter().map(|assignment| assignment.guest));
    guests.sort_unstable();
    if repeated(&guests)
        || report.devices.iter().any(|device| {
            device.info.id.kind == DeviceKind::Gpu && guests.binary_search(&device.info.id).is_err()
        })
    {
        return Err(invalid(
            "guest driver exposes GPUs outside the exact VMM witness set",
        ));
    }
    let mut eligible = 0u64;
    for (at, route) in capacity.routes.iter().enumerate() {
        if route.key.class == Class::Oci
            && route.key.execution == Execution::Native
            && route.key.image.os == ImageOs::Linux
            && route.key.gpu != GpuEligibility::None
        {
            eligible |= u32::try_from(at)
                .ok()
                .and_then(|at| 1u64.checked_shl(at))
                .ok_or_else(|| invalid("guest native attachment route exceeds its mask"))?;
        }
    }
    let mut attachments = Vec::new();
    reserve(&mut attachments, capacity.devices.len())?;
    attachments.extend(capacity.devices.iter().zip(&capacity.device_routes));
    attachments.sort_unstable_by_key(|(device, _)| device.id);
    for guest in guests {
        let (device, mask) = attachments
            .binary_search_by_key(&guest, |(device, _)| device.id)
            .ok()
            .and_then(|at| attachments.get(at))
            .ok_or_else(|| invalid("assigned guest GPU has no native OCI attachment"))?;
        if **mask & eligible == 0
            || device
                .holder
                .as_ref()
                .is_some_and(|holder| owner.is_none_or(|owner| holder.box_id() != Some(owner)))
        {
            return Err(invalid(
                "assigned guest GPU is not bindable by its native OCI owner",
            ));
        }
    }
    Ok(())
}

fn reserve<T>(items: &mut Vec<T>, count: usize) -> Result<(), Refusal> {
    items
        .try_reserve_exact(count)
        .map_err(|_| exhausted("guest-native observation items", count))
}

fn repeated(ids: &[DeviceId]) -> bool {
    ids.windows(2).any(|pair| pair.first() == pair.last())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(key: u64) -> DeviceId {
        DeviceId {
            kind: DeviceKind::Gpu,
            key,
        }
    }
    fn reading(key: u64, busy: Option<u16>, used: Option<u64>) -> PhysicalDevice {
        PhysicalDevice {
            info: DeviceInfo {
                id: id(key),
                model: "actual guest GPU".into(),
                size: 4096,
                holder: None,
            },
            busy_per_mille: busy,
            memory_used_bytes: used,
        }
    }
    fn report(devices: Vec<PhysicalDevice>) -> PhysicalReport {
        PhysicalReport {
            unavailable: Vec::new(),
            authority: "initialized-private-guest-node".into(),
            devices,
        }
    }

    /// AC-T7.4.1: native reorder must not exchange selected hardware or invent missing counters.
    #[test]
    fn exact_vmm_witness_preserves_native_identity_and_independent_uncertainty() {
        let assignments = [
            Assignment {
                host: id(100),
                guest: id(7),
            },
            Assignment {
                host: id(200),
                guest: id(9),
            },
        ];
        let actual = report(vec![
            reading(9, Some(400), None),
            reading(7, None, Some(6000)),
        ]);
        let joined = join(&assignments, &actual).unwrap();
        assert_eq!(
            joined
                .iter()
                .map(|device| device.info.id)
                .collect::<Vec<_>>(),
            vec![id(100), id(200)]
        );
        assert_eq!(joined[0].busy_per_mille, None);
        assert_eq!(joined[0].memory_used_bytes, Some(6000));
        assert!(joined[0].memory_used_bytes.unwrap() > joined[0].info.size);
        assert_eq!(joined[1].busy_per_mille, Some(400));
        assert_eq!(joined[1].memory_used_bytes, None);
        assert!(joined.iter().all(|device| device.info.holder.is_none()));
    }

    /// AC-T7.4.1: an assignment needs actual current observation; catalog/BAR data cannot fill it.
    #[test]
    fn missing_zero_memory_aliased_or_holder_bearing_observation_refuses() {
        let assignment = Assignment {
            host: id(100),
            guest: id(7),
        };
        assert!(join(&[assignment], &report(Vec::new())).is_err());
        let mut zero = reading(7, Some(0), Some(0));
        zero.info.size = 0;
        assert!(join(&[assignment], &report(vec![zero])).is_err());
        let same_guest = [
            assignment,
            Assignment {
                host: id(200),
                guest: id(7),
            },
        ];
        assert!(join(&same_guest, &report(vec![reading(7, Some(0), Some(0))])).is_err());
        let same_host = [
            assignment,
            Assignment {
                host: id(100),
                guest: id(9),
            },
        ];
        assert!(
            join(
                &same_host,
                &report(vec![
                    reading(7, Some(0), Some(0)),
                    reading(9, Some(0), Some(0))
                ])
            )
            .is_err()
        );
        let mut claimed = reading(7, Some(0), Some(0));
        claimed.info.holder = Some(hex_api::DeviceHolder::Box(
            hex_api::BoxId::new("foreign").unwrap(),
        ));
        assert!(join(&[assignment], &report(vec![claimed])).is_err());
    }

    /// AC-T7.4.1: initialized CPU-only native reports are valid without fabricating a GPU.
    #[test]
    fn empty_selection_accepts_initialized_empty_native_report() {
        assert!(join(&[], &report(Vec::new())).unwrap().is_empty());
        assert!(
            join(
                &[],
                &PhysicalReport {
                    unavailable: Vec::new(),
                    authority: String::new(),
                    devices: Vec::new()
                }
            )
            .is_err()
        );
    }

    /// AC-T7.4.1: a real native reading still needs the actual guest OCI attachment; renderer
    /// substitution, a foreign holder and any extra driver-visible GPU cannot grant readiness.
    #[test]
    fn observer_needs_exact_driver_set_and_real_native_container_binding() {
        use hex_api::{CapacityResources, ImageArch, ImagePlatform, RouteKey, RouteOffer};
        let assignment = Assignment {
            host: id(100),
            guest: id(7),
        };
        let actual = report(vec![reading(7, None, Some(17))]);
        let route = RouteOffer {
            key: RouteKey {
                class: Class::Oci,
                image: ImagePlatform {
                    os: ImageOs::Linux,
                    arch: ImageArch::Amd64,
                },
                execution: Execution::Native,
                gpu: GpuEligibility::Any,
            },
            cost: None,
            backing: None,
        };
        let total = CapacityResources {
            cpu_milli: 1_000,
            mem_bytes: 128 << 20,
            disk_bytes: 0,
        };
        let mut capacity = Capacity {
            own: Default::default(),
            id: "private actual guest node".into(),
            total,
            free: total,
            devices: Vec::new(),
            cost: None,
            routes: vec![route],
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
            node_vms: Vec::new(),
            unlearned: None,
        };
        assert!(check_bindings(&[assignment], &actual, &capacity, None).is_err());
        capacity
            .devices
            .push(actual.devices[0].info.try_clone().unwrap());
        capacity.device_routes.push(1);
        check_bindings(&[assignment], &actual, &capacity, None).unwrap();
        let owner = BoxId::new("actual-inner-owner").unwrap();
        capacity.devices[0].holder = Some(hex_api::DeviceHolder::Box(owner.clone()));
        assert!(check_bindings(&[assignment], &actual, &capacity, None).is_err());
        check_bindings(&[assignment], &actual, &capacity, Some(&owner)).unwrap();
        assert!(
            check_bindings(
                &[assignment],
                &report(vec![
                    reading(7, Some(0), Some(0)),
                    reading(9, Some(0), Some(0))
                ]),
                &capacity,
                Some(&owner),
            )
            .is_err()
        );
        capacity.devices[0].holder = None;
        capacity.device_routes[0] = 0;
        assert!(check_bindings(&[assignment], &actual, &capacity, None).is_err());
    }
}
