//! What a simulated cluster is made of: nodes with a capacity, a platform and devices, workloads with
//! a need and an ask of their node.

use serde::{Deserialize, Serialize};

/// Virtual nanoseconds since the run began.
pub type Time = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkloadId(pub u64);

/// What a node reserves for a workload until it is deleted: CPU, memory, disk and GPUs (kiln's needs,
/// `crates/kiln-ci/src/sched/needs.rs`: `cpus=`, `mem=`, `disk=`, `gpus=`). Ordered field by field, so
/// a need can key an equivalence class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
pub struct Resources {
    pub cpu_milli: u64,
    pub mem_bytes: u64,
    pub gpus: u32,
    pub disk_bytes: u64,
}

impl Resources {
    /// Whether `need` fits in what this is.
    pub fn holds(&self, need: &Resources) -> bool {
        need.cpu_milli <= self.cpu_milli
            && need.mem_bytes <= self.mem_bytes
            && need.gpus <= self.gpus
            && need.disk_bytes <= self.disk_bytes
    }

    pub fn checked_sub(&self, other: &Resources) -> Option<Resources> {
        Some(Resources {
            cpu_milli: self.cpu_milli.checked_sub(other.cpu_milli)?,
            mem_bytes: self.mem_bytes.checked_sub(other.mem_bytes)?,
            gpus: self.gpus.checked_sub(other.gpus)?,
            disk_bytes: self.disk_bytes.checked_sub(other.disk_bytes)?,
        })
    }

    pub fn checked_add(&self, other: &Resources) -> Option<Resources> {
        Some(Resources {
            cpu_milli: self.cpu_milli.checked_add(other.cpu_milli)?,
            mem_bytes: self.mem_bytes.checked_add(other.mem_bytes)?,
            gpus: self.gpus.checked_add(other.gpus)?,
            disk_bytes: self.disk_bytes.checked_add(other.disk_bytes)?,
        })
    }
}

/// An operating system, as Rust names it (kiln `crates/kiln-ci/src/sched/needs.rs` `os_name`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum Os {
    #[default]
    Linux,
    Macos,
    Windows,
    Freebsd,
}

impl Os {
    /// The names people and tools use for it (kiln's `os_name`).
    pub fn named(s: &str) -> Option<Os> {
        Some(match s.to_ascii_lowercase().as_str() {
            "linux" | "gnu" => Os::Linux,
            "macos" | "mac" | "darwin" | "osx" | "apple" => Os::Macos,
            "windows" | "win" | "win32" | "win64" => Os::Windows,
            "freebsd" => Os::Freebsd,
            _ => return None,
        })
    }

    /// The classes a node of this OS runs with `virt` (H10, `huawei_trace.py`): Linux OCI containers,
    /// and Firecracker and Shards microVMs where KVM is usable (both VMMs run on it); Windows OCI
    /// containers and Shards microVMs (on the Windows Hypervisor Platform, which kiln's facts don't
    /// probe); macOS Shards microVMs where Hypervisor.framework is supported; FreeBSD none Hex places.
    pub fn classes(self, virt: Virt) -> Classes {
        match self {
            Os::Linux if virt.kvm => Classes::ALL,
            Os::Linux => Classes::NONE.with(Class::Oci),
            Os::Windows => Classes::NONE.with(Class::Oci).with(Class::Shards),
            Os::Macos if virt.hvf => Classes::NONE.with(Class::Shards),
            Os::Macos | Os::Freebsd => Classes::NONE,
        }
    }
}

/// A processor architecture, as Rust names it (kiln's `arch_name`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum Arch {
    #[default]
    X86_64,
    Aarch64,
    X86,
    Arm,
    Riscv64,
}

impl Arch {
    /// The names people and tools use for it (kiln's `arch_name`).
    pub fn named(s: &str) -> Option<Arch> {
        Some(match s.to_ascii_lowercase().as_str() {
            "x86_64" | "amd64" | "x64" | "x86-64" | "intel" => Arch::X86_64,
            "aarch64" | "arm64" | "armv8" | "apple-silicon" => Arch::Aarch64,
            "x86" | "i386" | "i686" => Arch::X86,
            "arm" | "armv7" | "armhf" => Arch::Arm,
            "riscv64" => Arch::Riscv64,
            _ => return None,
        })
    }
}

/// A node's OS and architecture. The default is EC2 m5d.metal's, G2's main node model: Linux on
/// x86_64.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

/// The virtualization a node offers (kiln's facts' `virt`): `/dev/kvm` usable, nested virtualization
/// on (its guests may use KVM themselves), and macOS's Hypervisor.framework supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Virt {
    pub kvm: bool,
    pub nested: bool,
    pub hvf: bool,
}

/// A device a box holds alone, from its start until it is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DeviceKind {
    Gpu,
    /// A PCI function bound to `vfio-pci`, given whole.
    Vfio,
    /// A block device given whole.
    Block,
    /// A network device given whole (an SR-IOV function, a NIC).
    Net,
}

/// A GPU's load from what isn't Hex's (hex-gpu's `sample()`): its utilisation in per mille, and its
/// memory in use. Injected in the simulator; a node's agent reads it from hex-gpu.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct GpuLoad {
    /// None is unread, never an idle sample. Independent counters retain independent quality.
    pub busy_per_mille: Option<u16>,
    pub memory_used: Option<u64>,
}

/// A device model's name, interned in its cluster ([`Cluster::model`]): GPU models are compared by
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ModelId(pub u32);

impl ModelId {
    /// A device whose model its source doesn't say (a trace that counts GPUs): no ask names it.
    pub const UNNAMED: ModelId = ModelId(u32::MAX);
}

/// One of a node's devices: its kind, its model, its size (a GPU's memory, a block device's bytes) and,
/// for a GPU, its load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Device {
    pub kind: DeviceKind,
    /// Stable physical identity within `kind`; `None` denotes a synthetic indexed device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<u64>,
    /// Live backend room for a fresh holder; exact existing reservations remain authoritative.
    #[serde(default = "device_available")]
    pub available: bool,
    /// Initialized attachment adapters for this exact identity. One physical slot and holder are
    /// shared by all listed classes; an adapter never creates another slot for the same hardware.
    #[serde(default = "legacy_device_classes")]
    pub classes: Classes,
    pub model: ModelId,
    pub size: u64,
    pub load: GpuLoad,
}

fn device_available() -> bool {
    true
}

fn legacy_device_classes() -> Classes {
    Classes::NONE.with(Class::Oci)
}

impl Device {
    pub fn native_id(&self) -> Option<hex_api::DeviceId> {
        Some(hex_api::DeviceId {
            kind: match self.kind {
                DeviceKind::Gpu => hex_api::DeviceKind::Gpu,
                DeviceKind::Vfio => hex_api::DeviceKind::Vfio,
                DeviceKind::Block => hex_api::DeviceKind::Block,
                DeviceKind::Net => hex_api::DeviceKind::Net,
            },
            key: self.key?,
        })
    }

    /// A GPU of `model` with `memory` bytes, idle.
    pub fn gpu(model: ModelId, memory: u64) -> Self {
        Self {
            kind: DeviceKind::Gpu,
            key: None,
            available: true,
            classes: legacy_device_classes(),
            model,
            size: memory,
            // This constructor declares a synthetic idle device. Native observations supply
            // explicit independent counters instead; their Default is unknown.
            load: GpuLoad {
                busy_per_mille: Some(0),
                memory_used: Some(0),
            },
        }
    }

    /// A GPU's memory free of what isn't Hex's.
    pub fn memory_free(&self) -> Option<u64> {
        self.load
            .memory_used
            .map(|used| self.size.saturating_sub(used))
    }
}

/// The most devices a node has: one for each PCI function address of a segment (bus, device and
/// function: 8, 5 and 3 bits), so a device's place on its node is a u16.
pub const DEVICES: usize = 1 << 16;

/// What a workload asks of its node beyond room (kiln's needs, adapted): its image's OS and
/// architecture, and the devices it is given. Its GPUs' count is its need's
/// ([`Resources::gpus`]); `gpu_model` and `gpu_memory` constrain each of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Ask {
    /// Original payload form. Process keeps host-platform semantics; OCI names its image platform.
    /// None preserves legacy traces, but cannot disambiguate multiple initialized payload routes.
    pub runtime_class: Option<hex_api::Class>,
    /// Opaque native OCI bundles cannot be rewritten as guest OciSpec payloads. The driver derives
    /// this constraint from that actual payload before candidate admission.
    pub native_only: bool,
    /// Image OS for routed OCI/VMs; host OS for Process. Never inferred from a rootfs path.
    pub os: Option<Os>,
    pub arch: Option<Arch>,
    pub gpu_model: Option<ModelId>,
    /// Each GPU's memory free, at least.
    pub gpu_memory: u64,
    pub vfio: u16,
    pub block: u16,
    /// Each block device's size, at least.
    pub block_size: u64,
    pub net: u16,
    /// `/dev/kvm` in the box: shared, not held.
    pub kvm: bool,
}

impl Ask {
    /// Whether it asks a device of any kind, or KVM.
    pub fn devices(&self) -> bool {
        self.vfio > 0 || self.block > 0 || self.net > 0 || self.kvm
    }
}

/// The devices a workload holds on its node, by their places in the node's devices.
pub type Held = Vec<u16>;

fn device_needs(gpus: u32, ask: &Ask) -> impl Iterator<Item = (DeviceKind, u64)> {
    [
        (DeviceKind::Gpu, u64::from(gpus)),
        (DeviceKind::Vfio, u64::from(ask.vfio)),
        (DeviceKind::Block, u64::from(ask.block)),
        (DeviceKind::Net, u64::from(ask.net)),
    ]
    .into_iter()
}

/// The admission half of bind, without selecting/sorting identities or allocating a Held vector.
/// Device kinds are disjoint, so enough matching free devices of every kind is sufficient.
pub fn fits_devices(
    devices: &[Device],
    free: impl Fn(usize) -> bool,
    gpus: u32,
    ask: &Ask,
) -> bool {
    fits_observed(devices.len(), |at| devices.get(at), free, gpus, ask)
}

/// The same predicate over the canonical physical slots, whose provisioning-only observations
/// are None. Such slots can be reserved by a backing but can never match fresh workload demand.
pub(crate) fn fits_observed<'a>(
    count: usize,
    observed: impl Fn(usize) -> Option<&'a Device>,
    free: impl Fn(usize) -> bool,
    gpus: u32,
    ask: &Ask,
) -> bool {
    device_needs(gpus, ask).all(|(kind, n)| {
        let Ok(n) = usize::try_from(n) else {
            return false;
        };
        (0..count)
            .filter(|&at| {
                free(at) && observed(at).is_some_and(|d| d.kind == kind && matches(d, ask))
            })
            .take(n)
            .count()
            == n
    })
}

/// The sole picker for both ordinary observed catalogs and nullable provisioning slots. Device
/// order breaks equal native loads; an unavailable observation never receives an idle rank.
pub fn bind(
    devices: &[Device],
    free: impl Fn(usize) -> bool,
    gpus: u32,
    ask: &Ask,
) -> Option<Held> {
    bind_observed(devices.len(), |at| devices.get(at), free, gpus, ask)
}

pub(crate) fn bind_observed<'a>(
    count: usize,
    observed: impl Fn(usize) -> Option<&'a Device>,
    free: impl Fn(usize) -> bool,
    gpus: u32,
    ask: &Ask,
) -> Option<Held> {
    let total = device_needs(gpus, ask).try_fold(0usize, |total, (_, n)| {
        total.checked_add(usize::try_from(n).ok()?)
    })?;
    if total > count || count > DEVICES {
        return None;
    }
    let mut held = Held::new();
    held.try_reserve_exact(total).ok()?;
    if total == 0 {
        return Some(held);
    }
    let mut fit = Vec::new();
    fit.try_reserve_exact(count).ok()?;
    for (kind, n) in device_needs(gpus, ask) {
        if n == 0 {
            continue;
        }
        fit.clear();
        for at in 0..count {
            let Some(device) =
                observed(at).filter(|d| d.kind == kind && free(at) && matches(d, ask))
            else {
                continue;
            };
            let rank = match kind {
                DeviceKind::Gpu => (
                    u64::from(device.load.busy_per_mille?),
                    device.load.memory_used?,
                ),
                DeviceKind::Block => (device.size, 0),
                DeviceKind::Vfio | DeviceKind::Net => (0, 0),
            };
            fit.push((rank.0, rank.1, at));
        }
        fit.sort_unstable();
        let n = usize::try_from(n).ok()?;
        if fit.len() < n {
            return None;
        }
        for &(_, _, at) in fit.iter().take(n) {
            held.push(u16::try_from(at).ok()?);
        }
    }
    Some(held)
}

/// Whether device `d` is one `ask` can be given, by its model and size.
pub fn matches(d: &Device, ask: &Ask) -> bool {
    match d.kind {
        DeviceKind::Gpu => {
            ask.gpu_model.is_none_or(|m| m == d.model)
                && d.load.busy_per_mille.is_some_and(|busy| busy <= 1000)
                && d.memory_free().is_some_and(|free| free >= ask.gpu_memory)
        }
        DeviceKind::Block => d.size >= ask.block_size,
        DeviceKind::Vfio | DeviceKind::Net => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimePool {
    pub classes: Classes,
    pub capacity: Resources,
}

/// One immutable initialized offer view; masks pair with NodeSpec's existing device list.
/// Backing claims may name unobserved physical IDs and never supply measured Device metadata.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRoutes {
    /// Current route permission: a cold permit may provision its canonical backing, while
    /// workload admission still requires Live and observed matching. Retained committed receipts
    /// survive missing observations; None preserves all-offers behavior without inventing readings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admit: Option<u64>,
    #[serde(deserialize_with = "hex_api::Capacity::deserialize_routes")]
    pub offers: Vec<hex_api::RouteOffer>,
    #[serde(deserialize_with = "hex_api::Capacity::deserialize_device_routes")]
    pub device_routes: Vec<u64>,
    #[serde(deserialize_with = "hex_api::Capacity::deserialize_backings")]
    pub backings: Vec<hex_api::BackingOffer>,
}

pub type SharedRoutes = triomphe::Arc<RuntimeRoutes>;

impl RuntimeRoutes {
    fn check_admit(&self) -> Result<(), crate::SimError> {
        let bits = if self.offers.len() == hex_api::MAX_RUNTIME_ROUTES {
            u64::MAX
        } else {
            u32::try_from(self.offers.len())
                .ok()
                .and_then(|count| 1u64.checked_shl(count))
                .and_then(|value| value.checked_sub(1))
                .ok_or(crate::SimError::Config(
                    "runtime offer admission mask exceeds its word",
                ))?
        };
        if self.admit.is_some_and(|mask| mask & !bits != 0) {
            return Err(crate::SimError::Config(
                "runtime admission mask references an absent offer",
            ));
        }
        Ok(())
    }

    pub fn admits(&self, at: usize) -> bool {
        at < self.offers.len()
            && self.admit.is_none_or(|mask| {
                u32::try_from(at)
                    .ok()
                    .and_then(|at| 1u64.checked_shl(at))
                    .is_some_and(|bit| mask & bit != 0)
            })
    }

    /// Provisioning authority contributes identities only, after the observed catalog's slots.
    /// This is the SAME union consumed by Index; it supplies no model, size, load or fresh offer.
    pub fn unobserved_claims(
        &self,
        devices: &[Device],
        node: NodeId,
    ) -> Result<Vec<hex_api::DeviceId>, crate::SimError> {
        if devices.len() > DEVICES
            || self.backings.len() > hex_api::MAX_RUNTIME_ROUTES
            || self
                .backings
                .iter()
                .any(|backing| backing.devices.len() > DEVICES)
        {
            return Err(crate::SimError::Resources(node));
        }
        let mut known = Vec::new();
        known
            .try_reserve_exact(devices.len())
            .map_err(|_| crate::SimError::Resources(node))?;
        for device in devices {
            known.push(device.native_id().ok_or(crate::SimError::Config(
                "initialized routes require stable physical IDs",
            ))?);
        }
        known.sort_unstable();
        let count = self
            .backings
            .iter()
            .try_fold(0usize, |n, backing| n.checked_add(backing.devices.len()))
            .ok_or(crate::SimError::Resources(node))?;
        let mut provisioned = Vec::new();
        provisioned
            .try_reserve_exact(count)
            .map_err(|_| crate::SimError::Resources(node))?;
        for backing in &self.backings {
            provisioned.extend_from_slice(&backing.devices);
        }
        provisioned.sort_unstable();
        provisioned.dedup();
        provisioned.retain(|id| known.binary_search(id).is_err());
        if devices
            .len()
            .checked_add(provisioned.len())
            .is_none_or(|total| total > DEVICES)
        {
            return Err(crate::SimError::Full {
                what: "canonical physical slots",
                bound: DEVICES,
            });
        }
        Ok(provisioned)
    }

    pub fn share(self) -> Result<SharedRoutes, crate::SimError> {
        if self.offers.len() > hex_api::MAX_RUNTIME_ROUTES
            || self.backings.len() > hex_api::MAX_RUNTIME_ROUTES
            || self.device_routes.len() > DEVICES
        {
            return Err(crate::SimError::Config(
                "initialized routes exceed their address space",
            ));
        }
        self.check_admit()?;
        triomphe::Arc::try_new(self).map_err(|_| crate::SimError::Full {
            what: "immutable initialized runtime routes",
            bound: 1,
        })
    }

    pub fn check(&self, node: &NodeSpec) -> Result<(), crate::SimError> {
        self.check_admit()?;
        for (at, offer) in self.offers.iter().enumerate() {
            let arch = match offer.key.image.arch {
                hex_api::ImageArch::Amd64 => Arch::X86_64,
                hex_api::ImageArch::Arm64 => Arch::Aarch64,
            };
            let native_os = match offer.key.image.os {
                hex_api::ImageOs::Linux => Os::Linux,
                hex_api::ImageOs::Windows => Os::Windows,
                hex_api::ImageOs::Macos => Os::Macos,
            };
            let supported = match offer.key.backend_class() {
                hex_api::Class::Firecracker => {
                    node.platform.os == Os::Linux && (!self.admits(at) || node.virt.kvm)
                }
                hex_api::Class::Shards => match node.platform.os {
                    Os::Linux => !self.admits(at) || node.virt.kvm,
                    Os::Macos => {
                        (!self.admits(at) || node.virt.hvf) && node.platform.arch == Arch::Aarch64
                    }
                    Os::Windows => true, // The initialized native endpoint supplies WHP authority.
                    Os::Freebsd => false,
                },
                hex_api::Class::Process | hex_api::Class::Oci => native_os == node.platform.os,
            };
            if arch != node.platform.arch || !supported {
                return Err(crate::SimError::Config(
                    "initialized execution disagrees with its native host/platform",
                ));
            }
        }
        hex_api::Capacity::check_route_parts(
            capacity_resources(node.capacity),
            node.devices.len(),
            &self.offers,
            &self.device_routes,
            &self.backings,
        )
        .map_err(|_| crate::SimError::Config("invalid initialized route/backing offer"))
    }
}

mod shared_routes {
    use super::*;

    pub fn serialize<S: serde::Serializer>(
        value: &Option<SharedRoutes>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.as_deref().serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<SharedRoutes>, D::Error> {
        Option::<RuntimeRoutes>::deserialize(deserializer)?
            .map(RuntimeRoutes::share)
            .transpose()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub id: NodeId,
    pub capacity: Resources,
    /// CPU in use by what runs there outside Hex's placements (co-tenants, the host itself), which
    /// the 90% CPU rule counts beside what Hex placed.
    pub background_milli: u64,
    /// The classes it can run.
    pub classes: Classes,
    pub platform: Platform,
    pub virt: Virt,
    /// What boxes may be given whole; its GPUs are [`Resources::gpus`] of its capacity.
    pub devices: Vec<Device>,
    /// Additional capacity of a shared runtime (e.g. one fixed guest). Host capacity still applies.
    /// Missing pools retain the host-backed model used by earlier records/configuration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_pools: Vec<RuntimePool>,
    /// Initialized, evidence-pinned per-box charges. Real peer/Service offers require a known
    /// profile for each VM class; absent entries retain native identity-cost semantics only.
    #[serde(default, skip_serializing_if = "no_runtime_costs")]
    pub costs: [Option<hex_api::RuntimeCost>; Class::COUNT],
    /// What a new VM of each class is booked at here, learned (joint plan step 14, C2); absent, its whole
    /// allocation.
    #[serde(default, skip_serializing_if = "no_priors")]
    pub priors: [Option<Prior>; Class::COUNT],
    /// Absent retains unambiguous legacy admission. The owner is allocated fallibly once, then
    /// borrowed/shared across views; cloning a node never clones a backing's physical claim Vec.
    #[serde(
        default,
        with = "shared_routes",
        skip_serializing_if = "Option::is_none"
    )]
    pub routes: Option<SharedRoutes>,
}

fn no_runtime_costs(costs: &[Option<hex_api::RuntimeCost>; Class::COUNT]) -> bool {
    costs.iter().all(Option::is_none)
}

fn no_priors(priors: &[Option<Prior>; Class::COUNT]) -> bool {
    priors.iter().all(Option::is_none)
}

/// A class's learned booking on a node (joint plan step 14, `docs/design/learned-charge.md` C2): for VMs of the cost
/// profile `profile` and allocation `ceiling`, a class, what a new one is booked at, at most its allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prior {
    pub profile: [u8; 32],
    pub booking: hex_api::CapacityResources,
    /// The class's allocation, appended: a prior recorded before it reads as none's, booking no receipt.
    #[serde(default)]
    pub ceiling: hex_api::CapacityResources,
}

/// A receipt `allocation` booked at `prior` where it is of the prior's class, its cost profile and its allocation
/// (C2); as it was otherwise, so a VM of a size none has warmed for is booked whole.
pub fn booked_at(
    allocation: Option<hex_api::Allocation>,
    prior: Option<Prior>,
) -> Option<hex_api::Allocation> {
    match (allocation, prior) {
        (Some(allocation), Some(prior))
            if allocation.cost.id == prior.profile && allocation.resources == prior.ceiling =>
        {
            Some(allocation.booked_at(prior.booking))
        }
        _ => allocation,
    }
}

/// Whether two receipts charge alike, what each booked aside: a receiver books the owner's receipt as it was
/// booked, whatever it would book a new one at now (C2, C6).
pub fn same_charge(a: Option<hex_api::Allocation>, b: Option<hex_api::Allocation>) -> bool {
    let unbooked =
        |a: Option<hex_api::Allocation>| a.map(|a| hex_api::Allocation { booked: None, ..a });
    unbooked(a) == unbooked(b)
}

/// Physical resource units shared with the native allocation API; GPU counts stay in the sole
/// existing device/holder authority and are never inferred from a runtime's resource profile.
pub fn capacity_resources(need: Resources) -> hex_api::CapacityResources {
    hex_api::CapacityResources {
        cpu_milli: need.cpu_milli,
        mem_bytes: need.mem_bytes,
        disk_bytes: need.disk_bytes,
    }
}

/// Validate the immutable receipt against the original demand, including on recovery. This calls
/// the API's single cost formula; it does not re-derive the charge using today's runtime profile.
/// The ledger holds what the receipt booked (joint plan step 14), its whole charge where it booked none.
pub fn allocated_resources(
    need: Resources,
    allocation: Option<hex_api::Allocation>,
) -> Result<Resources, hex_api::Refusal> {
    let Some(allocation) = allocation else {
        return Ok(need);
    };
    allocation.check(capacity_resources(need))?;
    let booked = allocation.booking();
    Ok(Resources {
        cpu_milli: booked.cpu_milli,
        mem_bytes: booked.mem_bytes,
        disk_bytes: booked.disk_bytes,
        gpus: need.gpus,
    })
}

pub fn reserved_resources(
    need: Resources,
    allocation: Option<hex_api::Allocation>,
    route: Option<hex_api::RouteReceipt>,
) -> Result<Resources, hex_api::Refusal> {
    let Some(route) = route else {
        return allocated_resources(need, allocation);
    };
    route.check(route.key.class, need.gpus != 0)?;
    let charged = route.charged(capacity_resources(need), allocation)?;
    Ok(Resources {
        cpu_milli: charged.cpu_milli,
        mem_bytes: charged.mem_bytes,
        disk_bytes: charged.disk_bytes,
        // A linked observer already owns this physical grant; the same holder transitions.
        gpus: if route.backing.is_some() {
            0
        } else {
            need.gpus
        },
    })
}

impl NodeSpec {
    /// Copy bounded node facts fallibly; the initialized route/claim catalog keeps the SAME owner.
    /// Structural and capability validation remains the caller's explicit operation.
    pub fn try_clone(&self) -> Result<Self, crate::SimError> {
        if self.devices.len() > DEVICES || self.runtime_pools.len() > Class::COUNT {
            return Err(crate::SimError::Resources(self.id));
        }
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(self.devices.len())
            .map_err(|_| crate::SimError::Resources(self.id))?;
        devices.extend_from_slice(&self.devices);
        let mut runtime_pools = Vec::new();
        runtime_pools
            .try_reserve_exact(self.runtime_pools.len())
            .map_err(|_| crate::SimError::Resources(self.id))?;
        runtime_pools.extend_from_slice(&self.runtime_pools);
        Ok(Self {
            id: self.id,
            capacity: self.capacity,
            background_milli: self.background_milli,
            classes: self.classes,
            platform: self.platform,
            virt: self.virt,
            devices,
            runtime_pools,
            costs: self.costs,
            priors: self.priors,
            routes: self.routes.clone(),
        })
    }

    /// Runtime pools are bounded by the runtime class set: each class belongs to at most one pool.
    pub fn check_runtime_pools(&self) -> Result<(), crate::SimError> {
        if let Some(routes) = &self.routes {
            routes.check(self)?;
        }
        for cost in self.costs.iter().flatten() {
            cost.check().map_err(|_| {
                crate::SimError::Config("node runtime cost has an invalid identity, bound or unit")
            })?;
        }
        if self.runtime_pools.len() > Class::COUNT {
            return Err(crate::SimError::Full {
                what: "a node's runtime pools",
                bound: Class::COUNT,
            });
        }
        let mut covered = 0;
        for pool in &self.runtime_pools {
            let mask = pool.classes.0;
            if mask == 0
                || mask & !Classes::ALL.0 != 0
                || mask & !self.classes.0 != 0
                || covered & mask != 0
            {
                return Err(crate::SimError::Config(
                    "runtime pool masks must be nonempty, disjoint and within the node's classes",
                ));
            }
            covered |= mask;
        }
        Ok(())
    }
    /// A Linux x86_64 node with KVM and `capacity`, running every class, its GPUs unnamed and idle,
    /// with nothing outside Hex.
    pub fn new(id: NodeId, capacity: Resources) -> Self {
        Self {
            id,
            capacity,
            background_milli: 0,
            classes: Classes::ALL,
            platform: Platform::default(),
            virt: Virt {
                kvm: true,
                ..Virt::default()
            },
            devices: unnamed_gpus(capacity.gpus),
            runtime_pools: Vec::new(),
            costs: [None; Class::COUNT],
            priors: [None; Class::COUNT],
            routes: None,
        }
    }

    /// Whether its devices agree with its capacity: as many GPUs listed as it has, and no more
    /// devices than [`DEVICES`].
    pub fn devices_agree(&self) -> bool {
        let gpus = self
            .devices
            .iter()
            .filter(|d| d.kind == DeviceKind::Gpu)
            .count();
        self.devices.len() <= DEVICES
            // A routed index includes canonical unobserved provisioning IDs in its physical
            // claim budget. Its constructor checks that union, without inventing Device facts.
            && (self.routes.is_some() || u32::try_from(gpus) == Ok(self.capacity.gpus))
            && self
                .devices
                .iter()
                .all(|device| device.classes.0 != 0 && device.classes.0 & !Classes::ALL.0 == 0)
    }
}

/// `n` GPUs whose model and memory a source doesn't say, idle.
pub fn unnamed_gpus(n: u32) -> Vec<Device> {
    (0..n).map(|_| Device::gpu(ModelId::UNNAMED, 0)).collect()
}

/// What runs a workload. Hex places three: OCI containers, Firecracker microVMs and Shards
/// microVMs (Ada, 2026-11-06), each only on a node that can run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(usize)]
pub enum Class {
    #[default]
    Oci,
    Firecracker,
    Shards,
}

impl Class {
    pub const COUNT: usize = Self::Shards as usize + 1;
    pub const ALL: [Self; Self::COUNT] = [Self::Oci, Self::Firecracker, Self::Shards];
    /// Whether it can be given a device of `kind` on a node of `os`
    /// (docs/research/scheduler/agentic-workloads.md §6 and finding 8: device access is a property of
    /// the node and the class). An OCI container on Linux can be given each kind: its runtime passes
    /// the device in by CDI, as Kubernetes' device plugins do ([21][55] there), a VFIO group, a block
    /// device or a network device moved into its namespace. On Windows, a process-isolated container
    /// is given a GPU by its device interface class (the class Hex's Windows `oci` grants), and no
    /// device whole. Firecracker's pinned VFIO adapter runs on Linux KVM; Shards' exact renderer
    /// adapter also works on macOS and Windows. These are physical ceilings only: each Device's
    /// classes mask must additionally name the initialized adapter for that identity.
    pub fn exposes(self, os: Os, kind: DeviceKind) -> bool {
        match (self, os) {
            (Class::Oci, Os::Linux) => true,
            (Class::Oci, Os::Windows) => kind == DeviceKind::Gpu,
            (Class::Oci, Os::Macos) => kind == DeviceKind::Gpu,
            (Class::Firecracker, Os::Linux) => matches!(kind, DeviceKind::Gpu | DeviceKind::Vfio),
            (Class::Shards, Os::Linux) => matches!(kind, DeviceKind::Gpu | DeviceKind::Vfio),
            (Class::Shards, Os::Macos | Os::Windows) => kind == DeviceKind::Gpu,
            _ => false,
        }
    }

    /// Whether it can be given `/dev/kvm` on a node of `os`: an OCI container on Linux, by the device
    /// node. A microVM would need its VMM to pass virtualization through to its guest, which neither
    /// Firecracker's mainline nor shards-api does.
    pub fn gives_kvm(self, os: Os) -> bool {
        matches!((self, os), (Class::Oci, Os::Linux))
    }

    /// Its place among the three, as reports and per-class arrays give them: OCI containers,
    /// Firecracker microVMs, Shards microVMs.
    pub fn index(self) -> usize {
        match self {
            Class::Oci => 0,
            Class::Firecracker => 1,
            Class::Shards => 2,
        }
    }
}

/// The classes a node can run, a bit each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Classes(u8);

impl Classes {
    pub const ALL: Self = Self(0b111);
    pub const NONE: Self = Self(0);

    pub fn with(self, class: Class) -> Self {
        Self(self.0 | Self::bit(class))
    }

    pub fn has(self, class: Class) -> bool {
        self.0 & Self::bit(class) != 0
    }

    /// Whether the two share a class.
    pub fn meets(self, other: Classes) -> bool {
        self.0 & other.0 != 0
    }

    /// How many classes it holds.
    pub fn count(self) -> u32 {
        self.0.count_ones()
    }

    fn bit(class: Class) -> u8 {
        match class {
            Class::Oci => 1,
            Class::Firecracker => 2,
            Class::Shards => 4,
        }
    }
}

/// What each class costs its node beside what a workload asks, in bytes of memory: its runtime's own
/// (a container's shim, a microVM's VMM and the kernel's share of it). A workload's need includes its
/// class's.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Overheads([Overhead; 3]);

/// One class's overhead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overhead {
    /// The same for every workload of the class.
    Flat(u64),
    /// By the workload's own memory, where it was measured by guest size: (its memory, the overhead),
    /// ascending. A trace's rows bound it (`replay::Limits`).
    BySize(Vec<(u64, u64)>),
}

impl Default for Overhead {
    fn default() -> Self {
        Self::Flat(0)
    }
}

impl Overheads {
    /// A workload's overhead, `class`'s for its own memory `mem`; none where the class's overhead goes
    /// by size and names no row for `mem`.
    pub fn of(&self, class: Class, mem: u64) -> Option<u64> {
        match self.0.get(class.index())? {
            Overhead::Flat(bytes) => Some(*bytes),
            Overhead::BySize(sizes) => sizes
                .binary_search_by_key(&mem, |&(m, _)| m)
                .ok()
                .and_then(|i| sizes.get(i))
                .map(|&(_, bytes)| bytes),
        }
    }

    /// A workload's own memory, from its need as `class`: what [`Overheads::of`] added taken off.
    pub fn own(&self, class: Class, need: u64) -> Option<u64> {
        match self.0.get(class.index())? {
            Overhead::Flat(bytes) => need.checked_sub(*bytes),
            Overhead::BySize(sizes) => sizes
                .iter()
                .find(|&&(m, bytes)| m.checked_add(bytes) == Some(need))
                .map(|&(m, _)| m),
        }
    }

    pub fn set(&mut self, class: Class, bytes: u64) {
        if let Some(slot) = self.0.get_mut(class.index()) {
            *slot = Overhead::Flat(bytes);
        }
    }

    /// Adds `class`'s overhead for workloads whose own memory is `mem`, making its overhead go by size;
    /// refused, with why, where `mem` has one already.
    pub fn set_size(&mut self, class: Class, mem: u64, bytes: u64) -> Result<(), &'static str> {
        let slot = self
            .0
            .get_mut(class.index())
            .ok_or("a class with no slot")?;
        if let Overhead::Flat(_) = slot {
            *slot = Overhead::BySize(Vec::new());
        }
        let Overhead::BySize(sizes) = slot else {
            return Err("a class's overhead not by size");
        };
        match sizes.binary_search_by_key(&mem, |&(m, _)| m) {
            Ok(_) => Err("a class's overhead given twice for one size"),
            Err(at) => {
                sizes.insert(at, (mem, bytes));
                Ok(())
            }
        }
    }
}

impl Default for Classes {
    /// A node that says nothing runs every class, so a trace written before classes reads as it did.
    fn default() -> Self {
        Self::ALL
    }
}

/// What a workload is, for the filters that keep work off hosts it would harm or be harmed by
/// (`scheduler-architecture.md` S3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Kind {
    #[default]
    Plain,
    /// Changes its host while it runs (a build): never beside a benchmark.
    Build,
    /// Interference-sensitive (a benchmark): never beside a build.
    Benchmark,
    /// An agent session: never on a node above 90% CPU (`AC-T7.6.3`).
    Session,
}

/// A workload's equivalence class: what it needs, what it is, what runs it and what it asks of its
/// node. Workloads of one class are placed alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Demand {
    pub need: Resources,
    pub kind: Kind,
    pub class: Class,
    pub ask: Ask,
}

/// Who a workload's capacity is charged to: the tenant or app an OCI container, a Firecracker microVM
/// or a Shards microVM runs for (`AC-T7.5.2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct OwnerId(pub u32);

/// Workloads admitted whole or not at all: workloads that only work together (`AC-T7.5.1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GangId(pub u64);

/// Workloads that share one disruption budget (`AC-T7.5.3`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId(pub u32);

/// How a workload competes for capacity (`docs/design/scheduler-fairness.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tenancy {
    pub owner: OwnerId,
    /// Its priority band: a higher band goes first, and only a strictly higher one preempts it.
    pub band: u8,
    pub gang: Option<GangId>,
    pub group: Option<GroupId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkloadSpec {
    pub id: WorkloadId,
    pub need: Resources,
    pub kind: Kind,
    /// When its client first submits it.
    pub arrives: Time,
    /// How long it runs once started; `None` runs until the run ends.
    pub runs_for: Option<Time>,
    pub tenancy: Tenancy,
    pub class: Class,
    pub ask: Ask,
}

impl WorkloadSpec {
    pub fn demand(&self) -> Demand {
        Demand {
            need: self.need,
            kind: self.kind,
            class: self.class,
            ask: self.ask,
        }
    }
}

/// An owner's weight in weighted DRF: its fair share is its weight over the sum of the weights of
/// the owners with work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub weight: u32,
}

impl Default for Owner {
    fn default() -> Self {
        Self { weight: 1 }
    }
}

/// A disruption budget: at most `budget` of the group's members preempted and not yet running
/// again at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    pub budget: u32,
}

/// A run's input: its nodes (ids dense from 0), its workloads (ids dense from 0, by arrival), its
/// owners and its disruption groups (ids by position).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cluster {
    pub nodes: Vec<NodeSpec>,
    pub workloads: Vec<WorkloadSpec>,
    pub owners: Vec<Owner>,
    pub groups: Vec<Group>,
    /// Already counted in each workload's need; kept to change a workload's class.
    pub overheads: Overheads,
    /// Device models' names, by [`ModelId`]: lowercase, words joined by `-`.
    pub models: Vec<String>,
}

impl Cluster {
    /// One owner of weight 1, no groups and no class overheads: every workload's default tenancy.
    pub fn single_owner(nodes: Vec<NodeSpec>, workloads: Vec<WorkloadSpec>) -> Self {
        Self {
            nodes,
            workloads,
            owners: vec![Owner::default()],
            groups: Vec::new(),
            overheads: Overheads::default(),
            models: Vec::new(),
        }
    }

    /// The id of the device model `name`, as its cluster spells it ([`model_name`]), added if new.
    pub fn model(&mut self, name: &str) -> Result<ModelId, crate::SimError> {
        let name = model_name(name);
        if let Some(at) = self.models.iter().position(|m| *m == name) {
            return u32::try_from(at)
                .map(ModelId)
                .map_err(|_| crate::SimError::Config("a model id past u32"));
        }
        let id = u32::try_from(self.models.len())
            .ok()
            .filter(|&at| at != ModelId::UNNAMED.0)
            .map(ModelId)
            .ok_or(crate::SimError::Full {
                what: "a cluster's device models",
                bound: usize::try_from(ModelId::UNNAMED.0).unwrap_or(usize::MAX),
            })?;
        self.models.push(name);
        Ok(id)
    }
}

/// A device model's name as a needs word can spell it (words are split at commas and spaces):
/// lowercase, its words joined by `-`. `NVIDIA GeForce RTX 5060 Ti` is `nvidia-geforce-rtx-5060-ti`.
pub fn model_name(name: &str) -> String {
    name.split(|c: char| c.is_whitespace() || c == '_' || c == '-')
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod route_decode_tests {
    use super::*;

    /// AC-T7.4.1/R01: the SAME bounded API visitors run before immutable-owner allocation.
    #[test]
    fn initialized_route_json_bounds_precede_shared_owner_construction() {
        #[derive(Deserialize)]
        struct View {
            #[serde(with = "shared_routes")]
            routes: Option<SharedRoutes>,
        }
        assert!(
            serde_json::from_str::<View>(r#"{"routes":null}"#)
                .unwrap()
                .routes
                .is_none()
        );
        let route = serde_json::json!({
            "key": {"class":"oci","image":{"os":"linux","arch":"amd64"},"execution":"native","gpu":"any"},
            "cost":null,"backing":null
        });
        let mut input =
            serde_json::json!({"routes":{"offers":[],"device_routes":[],"backings":[]}});
        let empty: View = serde_json::from_value(input.clone()).unwrap();
        assert!(empty.routes.unwrap().offers.is_empty());
        input["routes"]["offers"] =
            serde_json::Value::Array(vec![route; hex_api::MAX_RUNTIME_ROUTES + 1]);
        let error = serde_json::from_value::<View>(input).err().unwrap();
        assert!(error.to_string().contains("route count"));
        let input =
            serde_json::json!({"offers":[],"device_routes":vec![0u64; DEVICES+1],"backings":[]});
        let error = serde_json::from_value::<RuntimeRoutes>(input).unwrap_err();
        assert!(error.to_string().contains("eligibility count"));
    }
}
