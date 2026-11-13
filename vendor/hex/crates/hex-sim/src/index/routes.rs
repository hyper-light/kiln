//! AC-T7.4.1: initialized routes and real backing charges inside the existing index.
//! Provisioning IDs have no observation until the same native owner supplies one.

use super::*;
use crate::model::{Arch, Os, SharedRoutes, capacity_resources};
use hex_api::{
    Allocation, BackingKey, BackingPhase, CapacityResources, DeviceId, Execution, ImageArch,
    ImageOs, RouteOffer, RouteReceipt,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PhysicalSlot {
    pub id: Option<DeviceId>,
    pub observed: Option<Device>,
    pub routes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Holder {
    Box(WorkloadId),
    Backing(BackingKey),
    /// One physical claim during observer-to-workload handoff; never two independently free slots.
    Handoff {
        backing: BackingKey,
        workload: WorkloadId,
    },
}

impl Holder {
    pub fn workload(self) -> Option<WorkloadId> {
        match self {
            Self::Box(workload) | Self::Handoff { workload, .. } => Some(workload),
            Self::Backing(_) => None,
        }
    }
    pub fn backing(self) -> Option<BackingKey> {
        match self {
            Self::Backing(key) | Self::Handoff { backing: key, .. } => Some(key),
            Self::Box(_) => None,
        }
    }
}

pub(super) fn native_id(device: &Device) -> Option<DeviceId> {
    device.native_id()
}

pub(super) fn physical(node: &NodeSpec) -> Result<Vec<PhysicalSlot>, SimError> {
    let mut out = Vec::new();
    out.try_reserve_exact(node.devices.len())
        .map_err(|_| SimError::Resources(node.id))?;
    for (at, device) in node.devices.iter().enumerate() {
        let mask = node
            .routes
            .as_ref()
            .and_then(|routes| routes.device_routes.get(at))
            .copied()
            .unwrap_or(0);
        out.push(PhysicalSlot {
            id: native_id(device),
            observed: Some(*device),
            routes: mask,
        });
    }
    let Some(routes) = &node.routes else {
        return Ok(out);
    };
    let provisioned = routes.unobserved_claims(&node.devices, node.id)?;
    out.try_reserve_exact(provisioned.len())
        .map_err(|_| SimError::Resources(node.id))?;
    out.extend(provisioned.into_iter().map(|id| PhysicalSlot {
        id: Some(id),
        observed: None,
        routes: 0,
    }));
    let gpus = out
        .iter()
        .filter(|slot| {
            slot.id
                .is_some_and(|id| id.kind == hex_api::DeviceKind::Gpu)
        })
        .count();
    if u32::try_from(gpus) != Ok(node.capacity.gpus) {
        return Err(SimError::Config(
            "physical GPU claim budget differs from the canonical slot union",
        ));
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Choice {
    pub at: usize,
    pub offer: RouteOffer,
}
impl Choice {
    pub fn receipt(self) -> RouteReceipt {
        RouteReceipt {
            key: self.offer.key,
            backing: self.offer.backing,
        }
    }
}

#[derive(Debug)]
pub(super) struct Routes {
    pub catalog: SharedRoutes,
    pub used: Vec<Resources>,
    pub phases: Vec<BackingPhase>,
}

pub(super) fn copy<T: Copy>(values: &[T], node: NodeId) -> Result<Vec<T>, SimError> {
    let mut out = Vec::new();
    out.try_reserve_exact(values.len())
        .map_err(|_| SimError::Resources(node))?;
    out.extend_from_slice(values);
    Ok(out)
}

pub(super) fn claim_indices(
    physical: &[PhysicalSlot],
    claims: &[DeviceId],
    node: NodeId,
) -> Result<Vec<usize>, SimError> {
    let mut indices = Vec::new();
    indices
        .try_reserve_exact(claims.len())
        .map_err(|_| SimError::Resources(node))?;
    if claims.is_empty() {
        return Ok(indices);
    }
    let mut by_id = Vec::new();
    by_id
        .try_reserve_exact(physical.len())
        .map_err(|_| SimError::Resources(node))?;
    by_id.extend(
        physical
            .iter()
            .enumerate()
            .filter_map(|(at, slot)| slot.id.map(|id| (id, at))),
    );
    by_id.sort_unstable_by_key(|(id, _)| *id);
    for id in claims {
        let at = by_id
            .binary_search_by_key(id, |(id, _)| *id)
            .ok()
            .and_then(|at| by_id.get(at))
            .map(|(_, at)| *at)
            .ok_or(SimError::Config(
                "backing provision claim has no canonical physical slot",
            ))?;
        indices.push(at);
    }
    Ok(indices)
}

pub(super) fn backing_charge(
    backing: &hex_api::BackingOffer,
    node: NodeId,
) -> Result<Resources, SimError> {
    let allocation = backing
        .allocation()
        .map_err(|_| SimError::Resources(node))?;
    Ok(Resources {
        cpu_milli: allocation.resources.cpu_milli,
        mem_bytes: allocation.resources.mem_bytes,
        disk_bytes: allocation.resources.disk_bytes,
        // Physical exclusivity is counted from the SAME holder slots, not a second pool GPU debit.
        gpus: 0,
    })
}

pub(super) fn restore_backings(
    node: NodeId,
    routes: &Routes,
    physical: &[PhysicalSlot],
    holders: &mut [Option<Holder>],
    free: &mut Resources,
) -> Result<(), SimError> {
    for (backing, phase) in routes.catalog.backings.iter().zip(&routes.phases) {
        if matches!(phase, BackingPhase::Prepared | BackingPhase::Released) {
            continue;
        }
        let indices = claim_indices(physical, &backing.devices, node)?;
        if indices
            .iter()
            .any(|&at| holders.get(at).is_none_or(Option::is_some))
        {
            return Err(SimError::Config(
                "backing physical grant overlaps another owner",
            ));
        }
        *free = free
            .checked_sub(&backing_charge(backing, node)?)
            .ok_or(SimError::Resources(node))?;
        for at in indices {
            *holders.get_mut(at).ok_or(SimError::Resources(node))? =
                Some(Holder::Backing(backing.key));
        }
    }
    Ok(())
}

impl Routes {
    pub fn rank_cpu(&self) -> u64 {
        self.catalog
            .backings
            .iter()
            .zip(&self.phases)
            .filter(|(_, phase)| **phase == BackingPhase::Live)
            .filter_map(|(backing, _)| {
                let mut shared = false;
                let used = self
                    .catalog
                    .offers
                    .iter()
                    .zip(&self.used)
                    .filter(|(offer, _)| {
                        offer.backing == Some(backing.key)
                            && offer.key.execution == Execution::ShardsShared
                    })
                    .try_fold(0u64, |sum, (_, used)| {
                        shared = true;
                        sum.checked_add(used.cpu_milli)
                    })?;
                shared
                    .then(|| {
                        backing
                            .cost
                            .floor
                            .cpu_milli
                            .checked_sub(backing.cost.guest_overhead.cpu_milli)?
                            .checked_sub(used)
                    })
                    .flatten()
            })
            .max()
            .unwrap_or(0)
    }

    pub fn refreshed(&self, catalog: &SharedRoutes, node: NodeId) -> Result<Self, SimError> {
        let mut next = Self::new(catalog, node)?;
        for (offer, used) in self.catalog.offers.iter().zip(&self.used) {
            if *used == Resources::default() {
                continue;
            }
            let at = next
                .catalog
                .offers
                .iter()
                .position(|new| new.key == offer.key && new.backing == offer.backing)
                .ok_or(SimError::Config(
                    "refreshed routes remove a retained workload receipt",
                ))?;
            *next.used.get_mut(at).ok_or(SimError::Resources(node))? = *used;
        }
        for (backing, phase) in self.catalog.backings.iter().zip(&self.phases) {
            if matches!(phase, BackingPhase::Prepared | BackingPhase::Released) {
                continue;
            }
            let (new, new_phase) = next.backing(backing.key).ok_or(SimError::Config(
                "backing disappearance is not native release proof",
            ))?;
            if new.cost != backing.cost
                || new.devices != backing.devices
                || matches!(
                    (phase, new_phase),
                    (
                        BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining,
                        BackingPhase::Prepared
                    ) | (
                        BackingPhase::Live | BackingPhase::Draining,
                        BackingPhase::Granted
                    ) | (BackingPhase::Draining, BackingPhase::Live)
                )
            {
                return Err(SimError::Config(
                    "refreshed backing changes a retained grant or rolls back lifecycle",
                ));
            }
            if new_phase == BackingPhase::Released
                && next
                    .catalog
                    .offers
                    .iter()
                    .zip(&next.used)
                    .any(|(offer, used)| {
                        offer.backing == Some(backing.key)
                            && offer.key.execution == Execution::ShardsShared
                            && *used != Resources::default()
                    })
            {
                return Err(SimError::Config(
                    "released backing still has retained inner workload reservations",
                ));
            }
        }
        for (backing, phase) in next.catalog.backings.iter().zip(&next.phases) {
            if !matches!(
                phase,
                BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining
            ) {
                continue;
            }
            let used = next
                .catalog
                .offers
                .iter()
                .zip(&next.used)
                .filter(|(offer, _)| {
                    offer.backing == Some(backing.key)
                        && offer.key.execution == Execution::ShardsShared
                })
                .try_fold(CapacityResources::default(), |sum, (_, used)| {
                    sum.checked_add(capacity_resources(*used))
                })
                .ok_or(SimError::Resources(node))?;
            backing
                .check_inner(used)
                .map_err(|_| SimError::Resources(node))?;
        }
        Ok(next)
    }

    pub fn new(catalog: &SharedRoutes, node: NodeId) -> Result<Self, SimError> {
        let mut used = Vec::new();
        used.try_reserve_exact(catalog.offers.len())
            .map_err(|_| SimError::Resources(node))?;
        used.resize(catalog.offers.len(), Resources::default());
        let mut phases = Vec::new();
        phases
            .try_reserve_exact(catalog.backings.len())
            .map_err(|_| SimError::Resources(node))?;
        phases.extend(catalog.backings.iter().map(|backing| backing.phase));
        Ok(Self {
            catalog: catalog.clone(),
            used,
            phases,
        })
    }
    pub fn try_clone(&self, node: NodeId) -> Result<Self, SimError> {
        Ok(Self {
            catalog: self.catalog.clone(),
            used: copy(&self.used, node)?,
            phases: copy(&self.phases, node)?,
        })
    }
    pub fn backing(&self, key: BackingKey) -> Option<(&hex_api::BackingOffer, BackingPhase)> {
        self.catalog
            .backings
            .iter()
            .enumerate()
            .find(|(_, b)| b.key == key)
            .and_then(|(at, b)| self.phases.get(at).copied().map(|phase| (b, phase)))
    }
    pub fn chosen(&self, demand: &Demand) -> Result<Choice, Verdict> {
        if demand
            .ask
            .runtime_class
            .is_some_and(|class| class != hex_api::Class::Process)
            && (demand.ask.os.is_none() || demand.ask.arch.is_none())
        {
            return Err(Verdict::NoPlatform);
        }
        let mut found = None;
        let mut inactive = None;
        let mut unavailable_gpu = false;
        for (at, offer) in self.catalog.offers.iter().enumerate() {
            let form_matches = demand.ask.runtime_class.map_or_else(
                || match demand.class {
                    Class::Oci => matches!(
                        offer.key.class,
                        hex_api::Class::Process | hex_api::Class::Oci
                    ),
                    Class::Firecracker => offer.key.class == hex_api::Class::Firecracker,
                    Class::Shards => offer.key.class == hex_api::Class::Shards,
                },
                |class| offer.key.class == class,
            );
            if !form_matches
                || (demand.ask.runtime_class.is_none() && offer.key.execution != Execution::Native)
                || !offer.key.gpu.accepts(demand.need.gpus != 0)
                || (demand.ask.native_only && offer.key.execution != Execution::Native)
            {
                continue;
            }
            let platform = offer.key.image;
            let os = match platform.os {
                ImageOs::Linux => Os::Linux,
                ImageOs::Windows => Os::Windows,
                ImageOs::Macos => Os::Macos,
            };
            let arch = match platform.arch {
                ImageArch::Amd64 => Arch::X86_64,
                ImageArch::Arm64 => Arch::Aarch64,
            };
            if demand.ask.os.is_some_and(|asked| asked != os)
                || demand.ask.arch.is_some_and(|asked| asked != arch)
            {
                continue;
            }
            if !self.catalog.admits(at) {
                unavailable_gpu |= demand.need.gpus != 0;
                continue;
            }
            if offer.backing.is_some_and(|key| {
                matches!(
                    self.backing(key),
                    Some((_, BackingPhase::Draining | BackingPhase::Released))
                )
            }) {
                // History remains addressable by committed receipt, never a fresh alternative.
                unavailable_gpu |= demand.need.gpus != 0;
                continue;
            }
            if offer
                .backing
                .is_some_and(|key| !matches!(self.backing(key), Some((_, BackingPhase::Live))))
            {
                inactive = Some(match inactive {
                    None => Ok(Choice { at, offer: *offer }),
                    Some(_) => Err(Verdict::NoPlatform),
                });
                continue;
            }
            if found.is_some() {
                return Err(Verdict::NoPlatform);
            }
            found = Some(Choice { at, offer: *offer });
        }
        // A sole inactive route still reaches the existing pending/NoGpu admission check. An
        // inactive provisioning alternative cannot hide a fully initialized native execution.
        found
            .map(Ok)
            .or(inactive)
            .unwrap_or(Err(if unavailable_gpu {
                Verdict::NoGpu
            } else {
                Verdict::NoPlatform
            }))
    }
    pub fn committed(&self, receipt: RouteReceipt) -> Result<Choice, SimError> {
        self.catalog
            .offers
            .iter()
            .enumerate()
            .find(|(_, offer)| offer.key == receipt.key && offer.backing == receipt.backing)
            .map(|(at, offer)| Choice { at, offer: *offer })
            .ok_or(SimError::Config(
                "committed route/backing differs from the initialized offer",
            ))
    }
    pub fn allocation(
        &self,
        choice: Choice,
        demand: &Demand,
    ) -> Result<Option<Allocation>, hex_api::Refusal> {
        choice
            .offer
            .cost
            .map(|cost| cost.allocate(capacity_resources(demand.need)))
            .transpose()
    }
    pub fn charged(
        &self,
        choice: Choice,
        demand: &Demand,
        allocation: Option<Allocation>,
    ) -> Result<Resources, hex_api::Refusal> {
        crate::model::reserved_resources(demand.need, allocation, Some(choice.receipt()))
    }

    pub fn holds_inner(&self, choice: Choice, demand: &Demand) -> bool {
        let Some(key) = choice.offer.backing else {
            return true;
        };
        let Some((backing, BackingPhase::Live)) = self.backing(key) else {
            return false;
        };
        if choice.offer.key.execution != Execution::ShardsShared {
            return self.holds_dedicated_inner(key, backing, demand);
        }
        let used = self
            .catalog
            .offers
            .iter()
            .zip(&self.used)
            .filter(|(offer, _)| {
                offer.backing == Some(key) && offer.key.execution == Execution::ShardsShared
            })
            .try_fold(CapacityResources::default(), |sum, (_, used)| {
                sum.checked_add(capacity_resources(*used))
            })
            .and_then(|used| used.checked_add(capacity_resources(demand.need)));
        used.is_some_and(|used| backing.check_inner(used).is_ok())
    }

    /// Released observer history still identifies its one surviving dedicated lifetime. Its
    /// receiver retains the exact binding; a remote owner reserves only the original commitment.
    pub fn holds_recovered_inner(
        &self,
        choice: Choice,
        demand: &Demand,
        allocation: Option<Allocation>,
    ) -> bool {
        if matches!(
            choice.offer.key.execution,
            Execution::ShardsDedicated | Execution::Firecracker
        ) && let Some(key) = choice.offer.backing
            && let Some((backing, BackingPhase::Released)) = self.backing(key)
        {
            // Validate the immutable profile against its SAME retained source with the API's
            // existing topology check; this temporary value is never advertised as a fresh offer.
            let original = RouteOffer {
                cost: allocation.map(|allocation| allocation.cost),
                ..choice.offer
            };
            return original.check_backing(backing).is_ok()
                && self.holds_dedicated_inner(key, backing, demand);
        }
        self.holds_inner(choice, demand)
    }

    fn holds_dedicated_inner(
        &self,
        key: BackingKey,
        backing: &hex_api::BackingOffer,
        demand: &Demand,
    ) -> bool {
        let consumed = self
            .catalog
            .offers
            .iter()
            .zip(&self.used)
            .filter(|(offer, _)| offer.backing == Some(key))
            .any(|(_, used)| *used != Resources::default());
        let physical = backing
            .devices
            .iter()
            .filter(|id| id.kind == hex_api::DeviceKind::Gpu)
            .count();
        !consumed && u32::try_from(physical).is_ok_and(|physical| demand.need.gpus <= physical)
    }
    pub fn used_next(
        &self,
        receipt: RouteReceipt,
        demand: &Demand,
        adding: bool,
    ) -> Result<(usize, Resources), SimError> {
        let choice = self.committed(receipt)?;
        let used = self
            .used
            .get(choice.at)
            .ok_or(SimError::Config("missing route usage"))?;
        let amount = demand.need;
        let next = if adding {
            used.checked_add(&amount)
        } else {
            used.checked_sub(&amount)
        }
        .ok_or(SimError::Config(
            "route payload usage overflows or underflows",
        ))?;
        Ok((choice.at, next))
    }
}

impl FitIndex {
    pub(crate) fn backing_phase(
        &self,
        node: NodeId,
        key: BackingKey,
    ) -> Result<BackingPhase, SimError> {
        self.slot(node)?
            .routes
            .as_ref()
            .and_then(|routes| routes.backing(key))
            .map(|(_, phase)| phase)
            .ok_or(SimError::Config(
                "backing phase has no canonical initialized owner",
            ))
    }

    /// The one index grants a prepared canonical incarnation BEFORE the native owner may boot it.
    pub fn grant_backing(
        &mut self,
        node: NodeId,
        key: BackingKey,
    ) -> Result<hex_api::BackingGrant, SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let slot = self.slot(node)?;
        let routes = slot
            .routes
            .as_ref()
            .ok_or(SimError::Config("backing has no initialized route owner"))?;
        let (backing, phase) = routes
            .backing(key)
            .ok_or(SimError::Config("unknown canonical backing generation"))?;
        let offer = backing.try_clone().map_err(|_| SimError::Resources(node))?;
        let grant = hex_api::BackingGrant {
            key,
            allocation: offer.allocation().map_err(|_| SimError::Resources(node))?,
            devices: offer.devices,
        };
        if matches!(phase, BackingPhase::Granted | BackingPhase::Live) {
            return Ok(grant);
        }
        if phase != BackingPhase::Prepared {
            return Err(SimError::Config(
                "backing lifecycle cannot accept a new grant",
            ));
        }
        if !routes
            .catalog
            .offers
            .iter()
            .enumerate()
            .any(|(at, offer)| offer.backing == Some(key) && routes.catalog.admits(at))
        {
            return Err(SimError::Config(
                "unavailable backing cannot receive a fresh native grant",
            ));
        }
        let physical = self.devices.get(at).ok_or(SimError::UnknownNode(node))?;
        let holders = self.held.get(at).ok_or(SimError::UnknownNode(node))?;
        let indices = claim_indices(physical, &grant.devices, node)?;
        if indices
            .iter()
            .any(|&index| holders.get(index).is_none_or(Option::is_some))
        {
            return Err(SimError::Config(
                "backing physical grant conflicts with another owner",
            ));
        }
        let gpu_claims = u32::try_from(
            grant
                .devices
                .iter()
                .filter(|id| id.kind == hex_api::DeviceKind::Gpu)
                .count(),
        )
        .map_err(|_| SimError::Resources(node))?;
        if gpu_claims > slot.free.gpus {
            return Err(SimError::Resources(node));
        }
        let pending = self.pending_gpus(node)?;
        let charge = backing_charge(backing, node)?;
        let free = slot
            .free
            .checked_sub(&charge)
            .ok_or(SimError::Resources(node))?;
        let backing_at = routes
            .catalog
            .backings
            .iter()
            .position(|backing| backing.key == key)
            .ok_or(SimError::Resources(node))?;
        let from = bucket_slot(slot, self.last);
        let slot = self.slots.get_mut(at).ok_or(SimError::UnknownNode(node))?;
        *slot
            .routes
            .as_mut()
            .and_then(|routes| routes.phases.get_mut(backing_at))
            .ok_or(SimError::Resources(node))? = BackingPhase::Granted;
        slot.free = free;
        let to = bucket_slot(slot, self.last);
        let holders = self.held.get_mut(at).ok_or(SimError::UnknownNode(node))?;
        for index in indices {
            *holders.get_mut(index).ok_or(SimError::Resources(node))? = Some(Holder::Backing(key));
        }
        self.ix.relink(node.0, from, to);
        self.recount_gpus(node, pending)?;
        Ok(grant)
    }

    /// A native result may advance only its exact grant. Disappearance, controller exit or a
    /// different generation cannot release any resource/physical claim.
    pub fn observe_backing(
        &mut self,
        node: NodeId,
        observed: &hex_api::BackingOffer,
    ) -> Result<(), SimError> {
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let slot = self.slot(node)?;
        let routes = slot
            .routes
            .as_ref()
            .ok_or(SimError::Config("backing has no initialized route owner"))?;
        let (backing, phase) = routes
            .backing(observed.key)
            .ok_or(SimError::Config("unknown canonical backing generation"))?;
        if backing.cost != observed.cost || backing.devices != observed.devices {
            return Err(SimError::Config(
                "native result changes the exact backing grant",
            ));
        }
        if phase == observed.phase {
            return Ok(());
        }
        let legal = matches!(
            (phase, observed.phase),
            (
                BackingPhase::Granted,
                BackingPhase::Live | BackingPhase::Draining | BackingPhase::Released
            ) | (BackingPhase::Live, BackingPhase::Draining)
                | (BackingPhase::Draining, BackingPhase::Released)
        );
        if !legal {
            return Err(SimError::Config(
                "native backing result rolls back or bypasses admission",
            ));
        }
        let releasing = observed.phase == BackingPhase::Released;
        if releasing
            && routes
                .catalog
                .offers
                .iter()
                .zip(&routes.used)
                .any(|(offer, used)| {
                    offer.backing == Some(observed.key)
                        && offer.key.execution == Execution::ShardsShared
                        && *used != Resources::default()
                })
        {
            return Err(SimError::Config(
                "backing release still has retained inner reservations",
            ));
        }
        let pending = self.pending_gpus(node)?;
        let next_free = if releasing {
            slot.free
                .checked_add(&backing_charge(backing, node)?)
                .filter(|free| slot.capacity.holds(free))
                .ok_or(SimError::Resources(node))?
        } else {
            slot.free
        };
        let backing_at = routes
            .catalog
            .backings
            .iter()
            .position(|backing| backing.key == observed.key)
            .ok_or(SimError::Resources(node))?;
        let from = bucket_slot(slot, self.last);
        let slot = self.slots.get_mut(at).ok_or(SimError::UnknownNode(node))?;
        *slot
            .routes
            .as_mut()
            .and_then(|routes| routes.phases.get_mut(backing_at))
            .ok_or(SimError::Resources(node))? = observed.phase;
        slot.free = next_free;
        let to = bucket_slot(slot, self.last);
        if releasing {
            // These readings came from the ending scope. Keep retained identity/metadata and
            // Box holders, but only a subsequent actual physical refresh may grant from them.
            for physical in self
                .devices
                .get_mut(at)
                .ok_or(SimError::UnknownNode(node))?
            {
                if physical
                    .id
                    .is_some_and(|id| observed.devices.binary_search(&id).is_ok())
                    && let Some(device) = physical.observed.as_mut()
                {
                    device.available = false;
                    device.load = crate::model::GpuLoad::default();
                }
            }
            for holder in self.held.get_mut(at).ok_or(SimError::UnknownNode(node))? {
                *holder = match *holder {
                    Some(Holder::Backing(key)) if key == observed.key => None,
                    Some(Holder::Handoff { backing, workload }) if backing == observed.key => {
                        Some(Holder::Box(workload))
                    }
                    other => other,
                };
            }
        }
        self.ix.relink(node.0, from, to);
        self.recount_gpus(node, pending)
    }
}
