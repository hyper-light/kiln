//! AC-T7.4.1: native attachment offers intersect measured hardware; telemetry alone grants nothing.

use super::*;
use hex_sim::model::{Classes, Cluster, Device, DeviceKind, GpuLoad, ModelId};

pub(crate) fn offers(
    reports: &[(Class, Option<Capacity>)],
    facts: &hex_sim::facts::Facts,
    cluster: &mut Cluster,
    namespace: &str,
) -> Result<Vec<Device>, Refusal> {
    // Cluster::model only appends. This exclusive call publishes either the entire offer's
    // interned names or none, without copying the cluster or maintaining another model table.
    let before = cluster.models.len();
    let result = collect_offers(reports, facts, cluster, namespace);
    if result.is_err() {
        cluster.models.truncate(before);
    }
    result
}

fn collect_offers(
    reports: &[(Class, Option<Capacity>)],
    facts: &hex_sim::facts::Facts,
    cluster: &mut Cluster,
    namespace: &str,
) -> Result<Vec<Device>, Refusal> {
    if reports.len() > Class::COUNT {
        return Err(invalid("more attachment reports than placement classes"));
    }
    struct Candidate<'a> {
        id: hex_api::DeviceId,
        device: Device,
        holder: Option<&'a hex_api::DeviceHolder>,
        order: usize,
    }
    let mut candidates: Vec<Candidate<'_>> = Vec::new();
    let mut classes = Classes::NONE;
    let own_prefix = format!("hex-place-{namespace}-");
    for (class, report) in reports {
        if classes.has(*class) {
            return Err(invalid("duplicate attachment class report"));
        }
        classes = classes.with(*class);
        let Some(report) = report else {
            continue;
        };
        report.check()?;
        if !report.devices.is_empty() && !facts.classes.has(*class) {
            return Err(invalid(
                "device offers require an initialized native attachment adapter",
            ));
        }
        candidates
            .try_reserve_exact(report.devices.len())
            .map_err(|_| Refusal::Exhausted {
                what: "runtime device facts",
                bound: report.devices.len(),
            })?;
        for (at, offer) in report.devices.iter().enumerate() {
            let kind = match offer.id.kind {
                hex_api::DeviceKind::Gpu => DeviceKind::Gpu,
                hex_api::DeviceKind::Vfio => DeviceKind::Vfio,
                hex_api::DeviceKind::Block => DeviceKind::Block,
                hex_api::DeviceKind::Net => DeviceKind::Net,
            };
            let mut eligible = Classes::NONE;
            if let Some(mask) = report.device_routes.get(at) {
                for (at, route) in report.routes.iter().enumerate() {
                    let bit = u32::try_from(at)
                        .ok()
                        .and_then(|at| 1u64.checked_shl(at))
                        .ok_or_else(|| invalid("device route mask exceeds its address space"))?;
                    if mask & bit != 0 {
                        eligible = eligible.with(match route.key.class {
                            hex_api::Class::Process | hex_api::Class::Oci => Class::Oci,
                            hex_api::Class::Firecracker => Class::Firecracker,
                            hex_api::Class::Shards => Class::Shards,
                        });
                    }
                }
            }
            if eligible == Classes::NONE {
                eligible = eligible.with(*class);
            }
            if Class::ALL
                .into_iter()
                .any(|class| eligible.has(class) && !class.exposes(facts.os, kind))
            {
                return Err(invalid(
                    "runtime offers a device its native class cannot expose",
                ));
            }
            let (model, size, load, observed) = if kind == DeviceKind::Gpu {
                let gpu = facts
                    .gpus
                    .iter()
                    .find(|gpu| native_key(gpu) == Some(offer.id.key));
                if let Some(gpu) = gpu {
                    if offer.size != gpu.memory
                        || hex_sim::model::model_name(&offer.model)
                            != hex_sim::model::model_name(&gpu.model)
                    {
                        return Err(invalid(
                            "native GPU metadata disagrees with measured hardware",
                        ));
                    }
                    (
                        cluster
                            .model(&gpu.model)
                            .map_err(|error| invalid(error.to_string()))?,
                        gpu.memory,
                        gpu.load,
                        gpu.memory > 0
                            && gpu.load.busy_per_mille.is_some()
                            && gpu.load.memory_used.is_some(),
                    )
                } else {
                    // Preserve the physical slot/key and retained holders across missing native
                    // observation. Configured metadata is only an unavailable placeholder here:
                    // it never becomes a measured GPU or grants a new assignment. CPU can proceed.
                    (
                        cluster
                            .model(&offer.model)
                            .map_err(|error| invalid(error.to_string()))?,
                        offer.size,
                        GpuLoad::default(),
                        false,
                    )
                }
            } else {
                let model = if offer.model.is_empty() {
                    ModelId::UNNAMED
                } else {
                    cluster
                        .model(&offer.model)
                        .map_err(|error| invalid(error.to_string()))?
                };
                (model, offer.size, GpuLoad::default(), true)
            };
            candidates.push(Candidate {
                id: offer.id,
                holder: offer.holder.as_ref(),
                order: candidates.len(),
                device: Device {
                    kind,
                    key: Some(offer.id.key),
                    // A device the runtime lists unavailable, with why, is never offered (AC-T7.4.1).
                    available: observed
                        && !report
                            .unavailable
                            .iter()
                            .any(|unavailable| unavailable.id == offer.id)
                        && offer.holder.as_ref().is_none_or(|holder| match holder {
                            hex_api::DeviceHolder::Box(id) => id.as_str().starts_with(&own_prefix),
                            hex_api::DeviceHolder::Backing(key) => {
                                reports.iter().any(|(_, report)| {
                                    report.as_ref().is_some_and(|report| {
                                        report.backings.iter().any(|backing| {
                                            backing.key == *key
                                                && backing.devices.binary_search(&offer.id).is_ok()
                                                && matches!(
                                                    backing.phase,
                                                    hex_api::BackingPhase::Granted
                                                        | hex_api::BackingPhase::Live
                                                        | hex_api::BackingPhase::Draining
                                                )
                                        })
                                    })
                                })
                            }
                        }),
                    classes: eligible,
                    model,
                    size,
                    load,
                },
            });
        }
    }
    // Sort once and coalesce linearly: insertion into a sorted Vec would shift every following
    // identity for an adversarial catalog order. Total candidates are bounded by three reports
    // of at most MAX_DEVICES entries. These temporary sorts allocate no additional storage.
    candidates.sort_unstable_by_key(|candidate| candidate.id);
    let mut aliases: Vec<Candidate<'_>> = Vec::new();
    aliases
        .try_reserve_exact(candidates.len().min(hex_api::MAX_DEVICES))
        .map_err(|_| Refusal::Exhausted {
            what: "runtime device aliases",
            bound: candidates.len().min(hex_api::MAX_DEVICES),
        })?;
    for candidate in candidates {
        if let Some(previous) = aliases
            .last_mut()
            .filter(|previous| previous.id == candidate.id)
        {
            if previous.device.model != candidate.device.model
                || previous.device.size != candidate.device.size
                || previous.holder != candidate.holder
            {
                return Err(invalid(
                    "backend aliases disagree on physical metadata or holder",
                ));
            }
            for class in Class::ALL {
                if candidate.device.classes.has(class) {
                    previous.device.classes = previous.device.classes.with(class);
                }
            }
            // Unavailable where any backend listing it says so.
            previous.device.available &= candidate.device.available;
            previous.order = previous.order.min(candidate.order);
        } else {
            if aliases.len() >= hex_api::MAX_DEVICES {
                return Err(invalid(
                    "combined physical devices exceed the scheduler index space",
                ));
            }
            aliases.push(candidate);
        }
    }
    aliases.sort_unstable_by_key(|candidate| candidate.order);
    let mut devices = Vec::new();
    devices
        .try_reserve_exact(aliases.len())
        .map_err(|_| Refusal::Exhausted {
            what: "runtime device facts",
            bound: aliases.len(),
        })?;
    devices.extend(aliases.into_iter().map(|candidate| candidate.device));
    Ok(devices)
}

fn native_key(gpu: &hex_sim::facts::Gpu) -> Option<u64> {
    match gpu.pci.as_deref() {
        Some(text) => {
            let pci = hex_gpu::PciAddress::parse(text)?;
            let key = hex_gpu::Identity::Pci(pci).key();
            if gpu.key.is_some_and(|native| native != key) {
                None
            } else {
                Some(key)
            }
        }
        // Native PhysicalReport carries the provider's already checked opaque DeviceId key.
        // Missing vendor/PCI presentation text never forces a guessed ordinal or new parser.
        None => gpu.key,
    }
}
