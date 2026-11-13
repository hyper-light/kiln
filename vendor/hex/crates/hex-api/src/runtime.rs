use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{Bounds, BoxId, BoxSpec, Class, DeviceInfo, Program, Stdio};

/// A runtime: boxes of one or more classes on one node. Calls are synchronous and short (a create
/// takes milliseconds); waiting for a program is [`Proc`]'s, with a deadline. Every refusal is
/// typed and says what and why (CLAUDE.md §3).
pub trait Runtime: Send + Sync {
    /// Direct native envelope counters from the SAME canonical backing owner. Inner guest usage
    /// cannot credit host room; an unread or uninitialized native scope remains a typed refusal.
    fn backing_usage(&self, key: &BackingKey) -> Result<Usage, Refusal> {
        Err(Refusal::Backing {
            key: *key,
            cause: LifecycleCause::Pending,
        })
    }

    /// Admit and initialize this exact real pool backing through the same native owner. A capacity
    /// report is not a grant, and the default uninitialized adapter performs no effects.
    fn initialize_backing(&self, grant: BackingGrant) -> Result<BackingOffer, Refusal> {
        Err(Refusal::Backing {
            key: grant.key,
            cause: LifecycleCause::Pending,
        })
    }
    fn release_backing(&self, key: BackingKey) -> Result<BackingOffer, Refusal> {
        Err(Refusal::Backing {
            key,
            cause: LifecycleCause::Pending,
        })
    }

    /// The classes it runs.
    fn classes(&self) -> Vec<Class>;

    /// One factual snapshot from the native Store/turn owner over the existing client/wire seam.
    /// An uninitialized observation API refuses explicitly and grants no physical capability.
    fn physical_report(&self) -> Result<crate::devices::PhysicalReport, Refusal> {
        Err(Refusal::Platform {
            what: "native physical report",
            platform: std::env::consts::OS,
            why: "this runtime has no initialized native facts provider".into(),
        })
    }

    /// AC-T7.4.1: the resource pool backing this class. `None` explicitly selects the caller's
    /// same-host physical budget; a guest must report its own pool rather than host capacity.
    fn capacity(&self, _class: Class) -> Result<Option<Capacity>, Refusal> {
        Ok(None)
    }

    /// Makes a box with its bounds in force. A `process` box is empty when this returns; an `oci`
    /// box's init is made and waits for [`Runtime::start`], [`BoxState::Created`].
    fn create(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal>;

    /// Lets a created `oci` box's init run its program: the runtime-spec's `start`, which `create`
    /// leaves to a later call so a box can be made before it's needed. A `process` box has no init to
    /// start, and an `oci` box that has started already can't start again: each is refused.
    fn start(&self, id: &BoxId) -> Result<(), Refusal>;

    /// Runs `program` in box `id` with `stdio`: a host process in a `process` box, a process in the
    /// container's namespaces in an `oci` box.
    fn exec(&self, id: &BoxId, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal>;

    /// Makes a `process` box from `spec` and runs `program` in it with `stdio`, as one call: the box a
    /// program runs in for its life, with no process beside it (decision 7 of
    /// docs/design/runtime-architecture.md). The runtime removes the box once the program has ended,
    /// ending anything it left running there, whether or not the caller still holds the [`Proc`]; the
    /// `Proc` reports the end only once the box is gone. Until then the box is any box: listed,
    /// signalled, inspected, deleted. An `oci` spec is refused ([`BoxSpec::check_run`]): its box has an
    /// init to create and start. A class whose node has nothing that removes a box when its program ends
    /// refuses the run as [`Refusal::Platform`], naming its platform.
    fn run(&self, spec: BoxSpec, program: Program, stdio: Stdio) -> Result<Box<dyn Proc>, Refusal>;

    /// Sends `signal` to every process in the box.
    fn signal(&self, id: &BoxId, signal: Signal) -> Result<(), Refusal>;

    /// The box as it is now.
    fn inspect(&self, id: &BoxId) -> Result<BoxInfo, Refusal>;

    /// What the box has used and holds now.
    fn usage(&self, id: &BoxId) -> Result<Usage, Refusal>;

    /// Every box on the node.
    fn list(&self) -> Result<Vec<BoxInfo>, Refusal>;

    /// Ends every process in the box and removes it, with any writable layer it owns.
    fn delete(&self, id: &BoxId) -> Result<(), Refusal>;

    /// A member of running Shards image box `id` that its spec says its caller drives
    /// ([`crate::ShardsImage::members`]; joint plan step 17), as a program: its end, and the caller's ends of
    /// its stdin, stdout and stderr, given once ([`Proc::streams`]). The member is the image's, in its guest:
    /// no process of this host. A class whose boxes have no such members refuses it, naming the class.
    fn member(&self, id: &BoxId, member: &crate::Member) -> Result<Box<dyn Proc>, Refusal> {
        let _ = member;
        Err(Refusal::Invalid {
            what: "member",
            why: format!(
                "box {id} isn't a Shards image box: only one has members its caller drives"
            ),
        })
    }

    /// Tells box `id`'s guest the room its caller's budget leaves it (D188; Shards' D241; joint plan step 14, C7): the
    /// caller's budget-left for the box, everything the VM holds coming out of it, in bytes past what the box holds now,
    /// negative where the budget is short of that (its guest then reclaims its file pages); none lifts the hold.
    /// Answers the bytes the guest holds itself within past what it uses, none where no budget holds it, waiting for
    /// that answer while the box runs. A class whose boxes have no guest to tell refuses it, naming the class.
    fn room(&self, id: &BoxId, room: Option<i64>) -> Result<Option<u64>, Refusal> {
        let _ = room;
        Err(Refusal::Invalid {
            what: "room",
            why: format!(
                "box {id} isn't a Shards image box: only its guest holds itself within a room"
            ),
        })
    }

    /// Holds box `id`'s vCPUs together to `rate` milli-CPU of the host's time (Shards' D247; joint plan step 14, C4),
    /// none lifting the hold. Answers what its VM holds them to, none where nothing does, waiting for that answer while
    /// the box runs. A class whose boxes have no vCPUs to hold refuses it, naming the class.
    fn rate(
        &self,
        id: &BoxId,
        rate: Option<std::num::NonZeroU64>,
    ) -> Result<Option<std::num::NonZeroU64>, Refusal> {
        let _ = rate;
        Err(Refusal::Invalid {
            what: "rate",
            why: format!(
                "box {id} isn't a Shards image box: only its VM holds its vCPUs to a rate"
            ),
        })
    }

    /// What box `id`'s VM costs its host beside its guest, as its run host reads its VM process (Shards' D241; joint
    /// plan steps 12 and 14, C1): counters since the VM booted, so a reader's window is between two of its reads.
    /// Refused, typed, for a part its run host didn't read, never read as none, for a box not running, and by a class
    /// whose boxes run no VM process of their own, naming the class.
    /// Deletes box `id` as [`Runtime::delete`] does, answering what its VM cost its host over its whole run, read as its
    /// run ended (goal 54's change 3: a Shards image VM's final [`HostUse`], its window the whole run): `None` where the
    /// box has no VM process of its own, or where none could be read at its end (its run had ended before its delete, or
    /// its last read failed). A box already gone is refused `NoSuchBox`, never answered with a use made up.
    fn delete_with_host_use(&self, id: &BoxId) -> Result<Option<HostUse>, Refusal> {
        self.delete(id).map(|()| None)
    }

    fn host_use(&self, id: &BoxId) -> Result<HostUse, Refusal> {
        Err(Refusal::Invalid {
            what: "host_use",
            why: format!(
                "box {id} runs no VM process of its own: only a Shards image VM's run host reads its cost"
            ),
        })
    }
}

/// What a box's VM costs its host beside its guest (Shards' D241, `Stats::microvm`; joint plan steps 12 and 14), each
/// a counter since the VM booted, as its run host read its VM process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostUse {
    /// Its VM process's CPU time, user and system together, in nanoseconds: its vCPUs' guest time among it, read just
    /// before it, so this less their sum is its VMM's own.
    pub cpu_ns: u64,
    /// Its own memory, as memory per VM is counted: macOS's physical footprint, Linux's anonymous resident pages,
    /// Windows' private working set.
    pub footprint_bytes: u64,
    pub resident_bytes: u64,
    /// Nanoseconds each vCPU has run the guest, by vCPU index, one for each vCPU of the VM's size.
    pub guest_ns: Vec<u64>,
    /// The time these counters span: from the VM's run's start, as its boot answered, to this read (goal 54's change
    /// 3: kiln learns the overhead a VM costs its host over its window, at usage and at its delete).
    pub window: Duration,
}

/// CPU milli, memory bytes and writable-volume bytes; devices are not inferred from a pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityResources {
    pub cpu_milli: u64,
    pub mem_bytes: u64,
    pub disk_bytes: u64,
}

impl CapacityResources {
    pub fn holds(self, need: Self) -> bool {
        self.cpu_milli >= need.cpu_milli
            && self.mem_bytes >= need.mem_bytes
            && self.disk_bytes >= need.disk_bytes
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            cpu_milli: self.cpu_milli.checked_add(other.cpu_milli)?,
            mem_bytes: self.mem_bytes.checked_add(other.mem_bytes)?,
            disk_bytes: self.disk_bytes.checked_add(other.disk_bytes)?,
        })
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            cpu_milli: self.cpu_milli.checked_sub(other.cpu_milli)?,
            mem_bytes: self.mem_bytes.checked_sub(other.mem_bytes)?,
            disk_bytes: self.disk_bytes.checked_sub(other.disk_bytes)?,
        })
    }
}

/// Initialized per-box cost, fixed to reviewed source/config/measurement evidence by `id`.
/// Bounds describe the actual guest envelope. `guest_overhead` pays kernel/relay resources inside
/// that envelope; `overhead` pays the VMM on its host, outside it. Neither has an invented default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeCost {
    pub id: [u8; 32],
    pub floor: CapacityResources,
    pub ceiling: CapacityResources,
    /// Explicit positive physical/API unit in every dimension, including unchanged units of one.
    pub quantum: CapacityResources,
    pub guest_overhead: CapacityResources,
    pub overhead: CapacityResources,
}

/// The immutable host charge admitted for the original workload, with its exact cost profile.
/// Persisted in BoxSpec and its existing BoxRecord, never a second resource ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub resources: CapacityResources,
    pub cost: RuntimeCost,
    /// What the ledger booked at admission, at most `resources`: the VM's class's learned prior (joint plan step
    /// 14, `docs/design/learned-charge.md` C2). `resources` stays its configuration and ceiling. None books it
    /// all, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub booked: Option<CapacityResources>,
}

/// Snapshot route eligibility is a fixed u64 word, not a tuning or an inventory cache.
pub const MAX_RUNTIME_ROUTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageOs {
    Linux,
    Windows,
    Macos,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageArch {
    Amd64,
    Arm64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImagePlatform {
    pub os: ImageOs,
    pub arch: ImageArch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Execution {
    Native,
    ShardsShared,
    ShardsDedicated,
    Firecracker,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuEligibility {
    Any,
    None,
    Required,
}
impl GpuEligibility {
    pub fn accepts(self, selected: bool) -> bool {
        match self {
            Self::Any => true,
            Self::None => !selected,
            Self::Required => selected,
        }
    }
    fn overlaps(self, other: Self) -> bool {
        [false, true]
            .into_iter()
            .any(|selected| self.accepts(selected) && other.accepts(selected))
    }
}

/// The original workload form and image platform, with its actual initialized execution path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteKey {
    pub class: crate::Class,
    pub image: ImagePlatform,
    pub execution: Execution,
    pub gpu: GpuEligibility,
}
impl RouteKey {
    pub fn backend_class(self) -> crate::Class {
        match self.execution {
            Execution::Native => self.class,
            Execution::ShardsShared | Execution::ShardsDedicated => crate::Class::Shards,
            Execution::Firecracker => crate::Class::Firecracker,
        }
    }

    pub fn check(self) -> Result<(), Refusal> {
        let valid = match (self.class, self.execution) {
            (crate::Class::Process, Execution::Native) => self.gpu == GpuEligibility::None,
            (crate::Class::Oci, Execution::Native) => self.image.os != ImageOs::Macos,
            (crate::Class::Oci, Execution::ShardsShared) => {
                self.image.os == ImageOs::Linux && self.gpu == GpuEligibility::None
            }
            (crate::Class::Oci, Execution::ShardsDedicated) => {
                self.image.os == ImageOs::Linux && self.gpu == GpuEligibility::Required
            }
            (crate::Class::Shards, Execution::ShardsDedicated)
            | (crate::Class::Firecracker, Execution::Firecracker) => {
                self.image.os == ImageOs::Linux
            }
            _ => false,
        };
        if !valid {
            return Err(invalid_route(
                "route class, image platform, execution or device mode disagree",
            ));
        }
        Ok(())
    }
}

/// Pool configuration identity and the Store-generated native incarnation; neither PID nor path
/// supplies this authority. The original profile remains in the same durable backing record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackingKey {
    pub pool: [u8; 32],
    pub generation: [u8; 16],
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackingPhase {
    Prepared,
    Granted,
    Live,
    Draining,
    Released,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackingOffer {
    pub key: BackingKey,
    pub cost: RuntimeCost,
    pub phase: BackingPhase,
    /// Exact administrator-provisioned claims. These IDs supply no model, memory or load facts.
    #[serde(deserialize_with = "deserialize_provisioning")]
    pub devices: Vec<crate::DeviceId>,
    /// Its guest's own overhead as its runtime measured it (joint plan step 14, C7; ROOT, 2026-11-13): what its guest
    /// agent says it uses past what its boxes hold, the most seen in each dimension, only rising, each none until the
    /// backing's first capacity answer measures it (its cold start: its boxes' sum alone). Never the census's
    /// `guest_overhead`, which a census-free cost carries as none. A shared backing's payload fits its envelope with it
    /// ([`BackingOffer::check_inner`]). Absent from an older JSON report, read as cold.
    #[serde(default, skip_serializing_if = "LearnedOverhead::cold")]
    pub guest: LearnedOverhead,
}
impl BackingOffer {
    /// A workload's exact selection is separate from its source observer's full grant. This
    /// validates identity membership only; it never releases P or invents a fresh observation.
    pub fn check_selection_parts(
        provisioned: &[crate::DeviceId],
        selected: &[crate::DeviceId],
    ) -> Result<(), Refusal> {
        check_provisioning(provisioned)?;
        check_provisioning(selected)?;
        if selected.is_empty()
            || selected
                .iter()
                .any(|id| provisioned.binary_search(id).is_err())
        {
            return Err(invalid_route(
                "dedicated workload selection must be a nonempty subset of its source grant",
            ));
        }
        Ok(())
    }

    pub fn try_clone(&self) -> Result<Self, Refusal> {
        self.allocation()?;
        Ok(Self {
            key: self.key,
            cost: self.cost,
            phase: self.phase,
            devices: copy_provisioning(&self.devices)?,
            guest: self.guest,
        })
    }

    /// Aggregate inner reservations must fit the SAME fixed profile, including its kernel/relay
    /// and host allowances and quantization. A residual host debit alone is not guest admission.
    /// With them, the guest's own overhead as its runtime measured it, in each dimension it has
    /// measured (joint plan step 14, C7): cold, the boxes' sum alone.
    pub fn check_inner(&self, used: CapacityResources) -> Result<(), Refusal> {
        let granted = self.allocation()?;
        let guest = CapacityResources {
            cpu_milli: self.guest.cpu_milli.unwrap_or(0),
            mem_bytes: self.guest.mem_bytes.unwrap_or(0),
            disk_bytes: self.guest.disk_bytes.unwrap_or(0),
        };
        let used = used.checked_add(guest).ok_or_else(|| {
            invalid_route("aggregate payload and the guest's own overhead overflow")
        })?;
        if self.cost.allocate(used)? != granted {
            return Err(invalid_route(
                "aggregate payload differs from its fixed backing grant",
            ));
        }
        Ok(())
    }

    pub fn allocation(&self) -> Result<Allocation, Refusal> {
        check_provisioning(&self.devices)?;
        self.cost.check()?;
        if self.cost.floor != self.cost.ceiling {
            return Err(invalid_route(
                "a backing must name its exact configured native envelope",
            ));
        }
        self.cost.allocate(CapacityResources::default())
    }
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackingGrant {
    pub key: BackingKey,
    pub allocation: Allocation,
    #[serde(deserialize_with = "deserialize_provisioning")]
    pub devices: Vec<crate::DeviceId>,
}

fn deserialize_provisioning<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<crate::DeviceId>, D::Error> {
    bounded_values(
        deserializer,
        crate::MAX_DEVICES,
        "physical claim count exceeds its address space",
    )
}

/// Count before decoding another value; an excessive next element is never materialized. These
/// vectors have protocol/address-space bounds, not input-selected allocation hints. Doubling a
/// checked capacity up to that exact bound gives amortized growth; an empty sequence allocates none.
pub(crate) fn bounded_values<'de, D, T>(
    deserializer: D,
    bound: usize,
    refused: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    use serde::de::{DeserializeSeed, Error, SeqAccess, Visitor};
    struct EndOnly(&'static str);
    impl<'de> DeserializeSeed<'de> for EndOnly {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, _: D) -> Result<(), D::Error> {
            Err(D::Error::custom(self.0))
        }
    }
    struct Values<T> {
        bound: usize,
        refused: &'static str,
        element: std::marker::PhantomData<T>,
    }
    impl<'de, T: serde::Deserialize<'de>> Visitor<'de> for Values<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a bounded runtime route or physical-claim sequence")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            if seq.size_hint().is_some_and(|count| count > self.bound) {
                return Err(A::Error::custom(self.refused));
            }
            let mut values: Vec<T> = Vec::new();
            loop {
                if values.len() == self.bound {
                    if seq.next_element_seed(EndOnly(self.refused))?.is_some() {
                        return Err(A::Error::custom(self.refused));
                    }
                    break;
                }
                let Some(value) = seq.next_element::<T>()? else {
                    break;
                };
                if values.len() == values.capacity() {
                    let capacity = values
                        .capacity()
                        .checked_mul(2)
                        .unwrap_or(self.bound)
                        .max(1)
                        .min(self.bound);
                    let additional = capacity
                        .checked_sub(values.len())
                        .ok_or_else(|| A::Error::custom(self.refused))?;
                    values
                        .try_reserve_exact(additional)
                        .map_err(|_| A::Error::custom("runtime sequence allocation refused"))?;
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Values::<T> {
        bound,
        refused,
        element: std::marker::PhantomData,
    })
}
impl BackingGrant {
    pub fn check(&self) -> Result<(), Refusal> {
        check_provisioning(&self.devices)?;
        self.allocation.check(CapacityResources::default())?;
        if self.allocation.cost.floor != self.allocation.cost.ceiling
            || self.allocation.booked.is_some()
        {
            return Err(invalid_route(
                "a backing grant must name its exact configured native envelope",
            ));
        }
        Ok(())
    }
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        self.check()?;
        Ok(Self {
            key: self.key,
            allocation: self.allocation,
            devices: copy_provisioning(&self.devices)?,
        })
    }
}

fn check_provisioning(devices: &[crate::DeviceId]) -> Result<(), Refusal> {
    if devices.len() > crate::MAX_DEVICES {
        return Err(Refusal::Exhausted {
            what: "native backing physical claims",
            bound: crate::MAX_DEVICES,
        });
    }
    for id in devices {
        id.check()?;
    }
    if devices
        .windows(2)
        .any(|pair| matches!(pair, [a,b] if a >= b))
    {
        return Err(invalid_route(
            "backing physical claims must be canonical sorted unique identities",
        ));
    }
    Ok(())
}

fn copy_provisioning(devices: &[crate::DeviceId]) -> Result<Vec<crate::DeviceId>, Refusal> {
    let mut copied = Vec::new();
    copied
        .try_reserve_exact(devices.len())
        .map_err(|_| Refusal::Exhausted {
            what: "native backing physical claims",
            bound: crate::MAX_DEVICES,
        })?;
    copied.extend_from_slice(devices);
    Ok(copied)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteOffer {
    pub key: RouteKey,
    pub cost: Option<RuntimeCost>,
    pub backing: Option<BackingKey>,
}
impl RouteOffer {
    /// A dedicated route may hand off its observer's exact physical grant. Its dynamic envelope
    /// extends the SAME evidence-pinned profile; a different pool/profile is not a substitute.
    pub fn check_backing(self, backing: &BackingOffer) -> Result<(), Refusal> {
        self.check_backing_parts(backing.key, backing.cost, &backing.devices)
    }

    pub fn check_backing_parts(
        self,
        key: BackingKey,
        profile: RuntimeCost,
        devices: &[crate::DeviceId],
    ) -> Result<(), Refusal> {
        self.check()?;
        if matches!(
            self.key.execution,
            Execution::ShardsDedicated | Execution::Firecracker
        ) && self.key.gpu != GpuEligibility::Required
        {
            return Err(invalid_route(
                "initialized backing route requires explicit selected-GPU eligibility",
            ));
        }
        check_backing_profile(
            self.key.execution,
            self.cost,
            self.backing,
            key,
            profile,
            devices,
        )
    }

    pub fn check(self) -> Result<(), Refusal> {
        self.key.check()?;
        match (self.key.execution, self.cost, self.backing) {
            (Execution::Native, None, None) | (Execution::ShardsShared, None, Some(_)) => Ok(()),
            (Execution::ShardsDedicated | Execution::Firecracker, Some(cost), _) => cost.check(),
            _ => Err(invalid_route(
                "route charge differs from its actual execution topology",
            )),
        }
    }
}

fn check_backing_profile(
    execution: Execution,
    cost: Option<RuntimeCost>,
    backing: Option<BackingKey>,
    key: BackingKey,
    profile: RuntimeCost,
    devices: &[crate::DeviceId],
) -> Result<(), Refusal> {
    if backing != Some(key) {
        return Err(invalid_route("route references another backing generation"));
    }
    check_provisioning(devices)?;
    profile.check()?;
    if let Some(cost) = cost {
        cost.check()?;
    }
    if profile.floor != profile.ceiling {
        return Err(invalid_route(
            "backing profile is not its fixed configured envelope",
        ));
    }
    match (execution, cost) {
        (Execution::ShardsShared, None) if devices.is_empty() => Ok(()),
        (Execution::ShardsDedicated | Execution::Firecracker, Some(cost))
            if !devices.is_empty()
                && cost.id == profile.id
                && cost.floor == profile.floor
                && cost.quantum == profile.quantum
                && cost.overhead == profile.overhead
                && cost.guest_overhead == profile.guest_overhead
                && cost.ceiling.holds(profile.ceiling) =>
        {
            Ok(())
        }
        _ => Err(invalid_route(
            "route and backing disagree about topology, profile or physical grant",
        )),
    }
}

/// Immutable chosen route beside the original BoxSpec. This factual receipt never relabels the
/// workload class and cannot alone initialize a backing, assert native readiness or grant devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteReceipt {
    pub key: RouteKey,
    pub backing: Option<BackingKey>,
}
impl RouteReceipt {
    /// Validate a factual retained receipt against its original source; this does not advertise
    /// a fresh initialized offer or turn a legacy Any eligibility key into new GPU capability.
    pub fn check_backing_parts(
        self,
        cost: Option<RuntimeCost>,
        key: BackingKey,
        profile: RuntimeCost,
        provisioned: &[crate::DeviceId],
        selected: &[crate::DeviceId],
    ) -> Result<(), Refusal> {
        self.check(
            self.key.class,
            selected.iter().any(|id| id.kind == crate::DeviceKind::Gpu),
        )?;
        check_backing_profile(
            self.key.execution,
            cost,
            self.backing,
            key,
            profile,
            provisioned,
        )?;
        if self.key.execution == Execution::ShardsShared {
            check_provisioning(selected)?;
            if !selected.is_empty() {
                return Err(invalid_route(
                    "shared backing has no selected physical grant",
                ));
            }
            Ok(())
        } else {
            BackingOffer::check_selection_parts(provisioned, selected)
        }
    }

    /// One formula for candidate, durable admission and recovery. Shared payload limits are
    /// unchanged; its separately granted backing pays the host envelope exactly once.
    pub fn charged(
        self,
        workload: CapacityResources,
        allocation: Option<Allocation>,
    ) -> Result<CapacityResources, Refusal> {
        self.key.check()?;
        match (self.key.execution, allocation) {
            (Execution::Native, None) => Ok(workload),
            (Execution::ShardsShared, None) if self.backing.is_some() => {
                Ok(CapacityResources::default())
            }
            (Execution::ShardsDedicated | Execution::Firecracker, Some(allocation)) => {
                allocation.check(workload)?;
                Ok(allocation.booking())
            }
            _ => Err(invalid_route(
                "committed route and immutable charge disagree",
            )),
        }
    }

    pub fn check(self, class: crate::Class, selected_gpu: bool) -> Result<(), Refusal> {
        self.key.check()?;
        if self.key.class != class || !self.key.gpu.accepts(selected_gpu) {
            return Err(invalid_route(
                "committed route differs from the original workload or selected devices",
            ));
        }
        match (self.key.execution, self.backing) {
            (Execution::Native, Some(_)) | (Execution::ShardsShared, None) => Err(invalid_route(
                "committed route has an invalid backing relationship",
            )),
            _ => Ok(()),
        }
    }
}

pub(crate) fn invalid_route(why: &'static str) -> Refusal {
    let mut owned = String::new();
    if owned.try_reserve_exact(why.len()).is_err() {
        return Refusal::Exhausted {
            what: "runtime route refusal bytes",
            bound: why.len(),
        };
    }
    owned.push_str(why);
    Refusal::Invalid {
        what: "initialized runtime route",
        why: owned,
    }
}

fn invalid_cost(why: &'static str) -> Refusal {
    Refusal::Invalid {
        what: "runtime allocation",
        why: why.into(),
    }
}

impl RuntimeCost {
    /// The profile as a node takes it (joint plan step 14; ROOT, 2026-11-13, by decision 9's rule: a census row is
    /// evidence only, never a starting number): its envelope alone, the VMM overhead and guest allowance its census
    /// measured left in its evidence, which its id still names. What a VM takes past its envelope is learned from its
    /// windows (`docs/design/learned-charge.md` C1); what a guest's own takes, from its backing's (C7).
    pub fn census_free(self) -> Self {
        Self {
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
            ..self
        }
    }

    pub fn check(self) -> Result<(), Refusal> {
        if self.id == [0; 32]
            || !self.ceiling.holds(self.floor)
            || !self.ceiling.holds(self.guest_overhead)
            || self.quantum.cpu_milli == 0
            || self.quantum.mem_bytes == 0
            || self.quantum.disk_bytes == 0
        {
            return Err(invalid_cost(
                "missing cost identity, inconsistent bounds or zero quantum",
            ));
        }
        self.ceiling
            .checked_add(self.overhead)
            .ok_or_else(|| invalid_cost("host ceiling overflows"))?;
        Ok(())
    }

    /// One formula for candidate fit, receiver admission, durable recovery and runtime checks.
    pub fn allocate(self, workload: CapacityResources) -> Result<Allocation, Refusal> {
        self.check()?;
        let inside = workload
            .checked_add(self.guest_overhead)
            .ok_or_else(|| invalid_cost("workload plus guest allowance overflows"))?;
        let round = |value: u64, floor: u64, quantum: u64| {
            let value = value.max(floor);
            let remainder = value.checked_rem(quantum)?;
            if remainder == 0 {
                Some(value)
            } else {
                value.checked_add(quantum.checked_sub(remainder)?)
            }
        };
        let guest = CapacityResources {
            cpu_milli: round(
                inside.cpu_milli,
                self.floor.cpu_milli,
                self.quantum.cpu_milli,
            )
            .ok_or_else(|| invalid_cost("CPU quantization overflows"))?,
            mem_bytes: round(
                inside.mem_bytes,
                self.floor.mem_bytes,
                self.quantum.mem_bytes,
            )
            .ok_or_else(|| invalid_cost("memory quantization overflows"))?,
            disk_bytes: round(
                inside.disk_bytes,
                self.floor.disk_bytes,
                self.quantum.disk_bytes,
            )
            .ok_or_else(|| invalid_cost("disk quantization overflows"))?,
        };
        if !self.ceiling.holds(guest) {
            return Err(invalid_cost(
                "workload and guest allowance exceed the initialized runtime ceiling",
            ));
        }
        Ok(Allocation {
            resources: guest
                .checked_add(self.overhead)
                .ok_or_else(|| invalid_cost("host charge overflows"))?,
            cost: self,
            booked: None,
        })
    }
}

impl Allocation {
    /// Recheck the original durable demand rather than recalculating against a newer cost profile. What was
    /// booked is no part of the formula, but is never past what it charges.
    pub fn check(self, workload: CapacityResources) -> Result<(), Refusal> {
        let derived = Allocation {
            booked: self.booked,
            ..self.cost.allocate(workload)?
        };
        if derived != self {
            return Err(invalid_cost(
                "charge disagrees with its immutable workload and cost profile",
            ));
        }
        if self
            .booked
            .is_some_and(|booked| !self.resources.holds(booked))
        {
            return Err(invalid_cost("booked past the admitted charge"));
        }
        Ok(())
    }

    /// What the ledger holds of it (joint plan step 14, C2): what was booked, else the whole charge.
    pub fn booking(self) -> CapacityResources {
        self.booked.unwrap_or(self.resources)
    }

    /// The same charge booked at `prior` where that is less, dimension by dimension; none where it books it all.
    pub fn booked_at(self, prior: CapacityResources) -> Self {
        let booked = CapacityResources {
            cpu_milli: self.resources.cpu_milli.min(prior.cpu_milli),
            mem_bytes: self.resources.mem_bytes.min(prior.mem_bytes),
            disk_bytes: self.resources.disk_bytes.min(prior.disk_bytes),
        };
        Self {
            booked: if booked == self.resources {
                None
            } else {
                Some(booked)
            },
            ..self
        }
    }

    /// Actual VM configuration is derived from the admitted host charge, paying host overhead once.
    pub fn guest(self) -> Result<CapacityResources, Refusal> {
        self.cost.check()?;
        let guest = self
            .resources
            .checked_sub(self.cost.overhead)
            .ok_or_else(|| invalid_cost("host charge cannot hold its VMM allowance"))?;
        if !guest.holds(self.cost.floor)
            || !self.cost.ceiling.holds(guest)
            || guest.cpu_milli.checked_rem(self.cost.quantum.cpu_milli) != Some(0)
            || guest.mem_bytes.checked_rem(self.cost.quantum.mem_bytes) != Some(0)
            || guest.disk_bytes.checked_rem(self.cost.quantum.disk_bytes) != Some(0)
        {
            return Err(invalid_cost(
                "admitted guest envelope violates the initialized bounds or units",
            ));
        }
        Ok(guest)
    }

    /// The microVM this charge sizes: its guest envelope ([`Allocation::guest`]) in whole vCPUs and
    /// MiB, unit conversions (1,000 CPU milli a vCPU, 2^20 bytes a MiB) as the dedicated Shards guest's
    /// receipt makes them, never an estimate. Refused where the envelope holds no whole vCPU or MiB, or
    /// a part of one.
    pub fn vm_size(self) -> Result<VmSize, Refusal> {
        let guest = self.guest()?;
        let whole = |value: u64, unit: u64, what: &'static str| match (
            value.checked_div(unit),
            value.checked_rem(unit),
        ) {
            (Some(units), Some(0)) if units > 0 => Ok(units),
            _ => Err(invalid_cost(what)),
        };
        let vcpus = whole(
            guest.cpu_milli,
            1000,
            "guest CPU is no whole, non-zero number of vCPUs",
        )?;
        Ok(VmSize {
            vcpus: u32::try_from(vcpus)
                .map_err(|_| invalid_cost("guest vCPUs exceed their native representation"))?,
            memory_mib: whole(
                guest.mem_bytes,
                1 << 20,
                "guest memory is no whole, non-zero number of MiB",
            )?,
        })
    }

    /// BoxSpec has workload caps, while its scheduler row also retains the full original demand.
    /// A reservation may exceed those caps; it may never shrink them to fit the VM.
    pub fn covers(self, workload: CapacityResources) -> Result<(), Refusal> {
        let inside = workload
            .checked_add(self.cost.guest_overhead)
            .ok_or_else(|| invalid_cost("workload plus guest allowance overflows"))?;
        if !self.guest()?.holds(inside) {
            return Err(invalid_cost(
                "admitted guest envelope does not cover unchanged workload caps",
            ));
        }
        Ok(())
    }
}

/// A microVM's size ([`Allocation::vm_size`]): its vCPUs and its memory in MiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmSize {
    pub vcpus: u32,
    pub memory_mib: u64,
}

/// The most priors one class's capacity answer carries: one for its own cost profile and one for each route's
/// (joint plan step 14).
pub const MAX_LEARNED_PRIORS: usize = MAX_RUNTIME_ROUTES.saturating_add(1);

/// Joint plan step 14 (`docs/design/learned-charge.md`, C3 and C8): what microVMs are learned to take, set against
/// what their receipts booked. The ledger books the receipts and the node learns the rest, so Hex's reservation on
/// its machine is the ledger's less `booked`, plus `reservation`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnedUse {
    /// What the receipts of the VMs the node learns booked.
    pub booked: CapacityResources,
    /// What those VMs are learned to take: their bookings, and the memory and disk they hold past them.
    pub reservation: CapacityResources,
}

impl LearnedUse {
    /// Two learned uses together; `None` past what a u64 counts.
    pub fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            booked: self.booked.checked_add(other.booked)?,
            reservation: self.reservation.checked_add(other.reservation)?,
        })
    }
}

/// Joint plan step 14 (C2): what a new microVM of one cost profile and ceiling is booked at, its class's learned
/// prior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnedPrior {
    /// The cost profile's identity ([`RuntimeCost::id`]).
    pub profile: [u8; 32],
    /// The allocation its VMs are charged at most.
    pub ceiling: CapacityResources,
    /// What a new VM of it is booked at, within the ceiling.
    pub booking: CapacityResources,
}

/// The most learned holdings one class's capacity answer carries: a node's most boxes (`hex_runtime::MAX_BOXES`, held
/// equal by a test in hex-node), so a node never has more running VMs to report than this.
pub const MAX_LEARNED_HOLDINGS: usize = 1 << u16::BITS;

/// Joint plan step 14 (C3, C8): one running microVM's holding, by its box: its booking and the memory and disk it
/// holds past it. The scheduler books the VM at it in the machine's capacity view (goal 13's `Driver::rebook`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnedHolding {
    pub id: BoxId,
    pub holding: CapacityResources,
}

/// Joint plan step 14 (C5): a microVM its node's last resort ended, and why. Its fleet's VMs held, with the fastest
/// growth of what they hold on record in one window, more memory than the fleet's budget, and it held the most past
/// its own budget, lowest band first: no VM within its budget is ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnedEnd {
    pub id: BoxId,
    /// What it held, and its memory budget, in bytes.
    pub held: u64,
    pub budget: u64,
    /// Its priority band: a lower one is ended first.
    pub band: u8,
    /// Its fleet's: what its VMs held, the fastest growth of that on record in one window, and its memory budget, in
    /// bytes.
    pub fleet_held: u64,
    pub fleet_growth: u64,
    pub fleet_budget: u64,
    /// What moving it to another machine instead of ending it awaits, where that move is another product's: a Shards
    /// VM's is Shards' D176 ([`Upstream::LiveMove`]). After the fleet's, so absent means none (append-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmoved: Option<Upstream>,
}

impl LearnedEnd {
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        Ok(Self {
            id: self.id.try_clone()?,
            ..*self
        })
    }
}

/// Joint plan step 14 (C1; ROOT, 2026-11-13): a profile's VMM overhead past its envelope as its node's learner has seen
/// it, each dimension the most its VMs' windows showed past the envelope, or none yet: its cold start, typed, never a
/// zero. A window in which a VM ran into its CPU cap is no CPU sample (Karn's point, decision 9).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnedOverhead {
    pub cpu_milli: Option<u64>,
    pub mem_bytes: Option<u64>,
    pub disk_bytes: Option<u64>,
}

impl LearnedOverhead {
    /// Whether no window has shown any of it yet: its profile's cold start.
    pub fn cold(&self) -> bool {
        self.cpu_milli.is_none() && self.mem_bytes.is_none() && self.disk_bytes.is_none()
    }
}

/// Joint plan step 14 (C1): one profile's ceiling on a class's capacity answer: what a VM of it is admitted at, its
/// receipt's guest envelope ([`Allocation::guest`]), and the VMM overhead its node has learned past it. A VM's ceiling
/// is the envelope and the overhead, the envelope alone in a dimension still cold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearnedCeiling {
    pub profile: [u8; 32],
    pub envelope: CapacityResources,
    pub overhead: LearnedOverhead,
}

/// Joint plan step 14 (C2, C8): what a node learned of one class's microVMs, carried on its capacity answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Learned {
    /// The priors of the profiles it offers whose VMs have warmed, at most one per profile and ceiling.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "Learned::deserialize_priors"
    )]
    pub priors: Vec<LearnedPrior>,
    /// What its VMs are learned to take against their receipts, for placement across nodes.
    pub used: LearnedUse,
    /// Each running VM's holding, those most past their receipts first: as many as the answer has room for, so a VM
    /// left out is one whose holding past its receipt is least, counted at its receipt.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "Learned::deserialize_holdings"
    )]
    pub holdings: Vec<LearnedHolding>,
    /// The VMs of the class its last resort ended, and why (C5), latest first: as many as the answer has room for
    /// once the holdings are in. Added after the holdings, so absent means none (append-only).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "Learned::deserialize_ends"
    )]
    pub ended: Vec<LearnedEnd>,
    /// Each offered profile's ceiling (C1), at most one per profile and envelope. Added after the ends, so absent
    /// means none (append-only).
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "Learned::deserialize_ceilings"
    )]
    pub ceilings: Vec<LearnedCeiling>,
}

impl Learned {
    /// At most [`MAX_LEARNED_PRIORS`]: one for each profile it offers.
    pub fn deserialize_ceilings<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<LearnedCeiling>, D::Error> {
        bounded_values(
            deserializer,
            MAX_LEARNED_PRIORS,
            "learned ceilings exceed a class's profiles",
        )
    }

    /// At most [`MAX_LEARNED_PRIORS`].
    pub fn deserialize_priors<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<LearnedPrior>, D::Error> {
        bounded_values(
            deserializer,
            MAX_LEARNED_PRIORS,
            "learned priors exceed a class's profiles",
        )
    }

    /// At most [`MAX_LEARNED_HOLDINGS`]: a node ends no more VMs than it holds boxes before it forgets the oldest.
    pub fn deserialize_ends<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<LearnedEnd>, D::Error> {
        bounded_values(
            deserializer,
            MAX_LEARNED_HOLDINGS,
            "learned ends exceed a node's boxes",
        )
    }

    /// At most [`MAX_LEARNED_HOLDINGS`].
    pub fn deserialize_holdings<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<LearnedHolding>, D::Error> {
        bounded_values(
            deserializer,
            MAX_LEARNED_HOLDINGS,
            "learned holdings exceed a node's boxes",
        )
    }

    /// At most [`MAX_LEARNED_PRIORS`] priors, each booking within its ceiling, no profile and ceiling twice; at most
    /// [`MAX_LEARNED_HOLDINGS`] holdings, no box twice; at most as many ends; at most [`MAX_LEARNED_PRIORS`]
    /// ceilings, no profile and envelope twice.
    pub fn check(&self) -> Result<(), Refusal> {
        if self.ceilings.len() > MAX_LEARNED_PRIORS {
            return Err(Refusal::Exhausted {
                what: "learned ceilings",
                bound: MAX_LEARNED_PRIORS,
            });
        }
        for (at, ceiling) in self.ceilings.iter().enumerate() {
            if self.ceilings.iter().take(at).any(|earlier| {
                earlier.profile == ceiling.profile && earlier.envelope == ceiling.envelope
            }) {
                return Err(Refusal::Invalid {
                    what: "learned ceilings",
                    why: "a profile and envelope repeat".to_owned(),
                });
            }
        }
        if self.ended.len() > MAX_LEARNED_HOLDINGS {
            return Err(Refusal::Exhausted {
                what: "learned ends",
                bound: MAX_LEARNED_HOLDINGS,
            });
        }
        if self.holdings.len() > MAX_LEARNED_HOLDINGS {
            return Err(Refusal::Exhausted {
                what: "learned holdings",
                bound: MAX_LEARNED_HOLDINGS,
            });
        }
        let mut ids = std::collections::BTreeSet::new();
        for holding in &self.holdings {
            if !ids.insert(holding.id.as_str()) {
                return Err(Refusal::Invalid {
                    what: "learned holdings",
                    why: "a box's holding repeats".to_owned(),
                });
            }
        }
        if self.priors.len() > MAX_LEARNED_PRIORS {
            return Err(Refusal::Exhausted {
                what: "learned priors",
                bound: MAX_LEARNED_PRIORS,
            });
        }
        for (at, prior) in self.priors.iter().enumerate() {
            let repeated = self.priors.iter().take(at).any(|earlier| {
                earlier.profile == prior.profile && earlier.ceiling == prior.ceiling
            });
            if repeated || !prior.ceiling.holds(prior.booking) {
                return Err(Refusal::Invalid {
                    what: "learned priors",
                    why: "a prior books past its ceiling, or a profile and ceiling repeat"
                        .to_owned(),
                });
            }
        }
        Ok(())
    }

    pub fn try_clone(&self) -> Result<Self, Refusal> {
        let mut holdings = Vec::new();
        holdings
            .try_reserve_exact(self.holdings.len())
            .map_err(|_| Refusal::Exhausted {
                what: "learned holdings",
                bound: self.holdings.len(),
            })?;
        for holding in &self.holdings {
            holdings.push(LearnedHolding {
                id: holding.id.try_clone()?,
                holding: holding.holding,
            });
        }
        let mut ended = Vec::new();
        ended
            .try_reserve_exact(self.ended.len())
            .map_err(|_| Refusal::Exhausted {
                what: "learned ends",
                bound: self.ended.len(),
            })?;
        for end in &self.ended {
            ended.push(end.try_clone()?);
        }
        Ok(Self {
            priors: copy_capacity_values(&self.priors, "learned priors")?,
            used: self.used,
            holdings,
            ended,
            ceilings: copy_capacity_values(&self.ceilings, "learned ceilings")?,
        })
    }
}

/// A stable resource-pool identity and its physical ceiling/current room. Reservations remain the
/// scheduler's authority; these facts do not grant resources or guess unread measurements.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capacity {
    pub id: String,
    pub total: CapacityResources,
    pub free: CapacityResources,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceInfo>,
    /// None is the legacy native identity cost. A VM adapter must supply its verified profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<RuntimeCost>,
    /// Actual initialized execution routes. Empty retains only unambiguous legacy native behavior.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "Capacity::deserialize_routes"
    )]
    pub routes: Vec<RouteOffer>,
    /// One eligibility word paired with each canonical DeviceInfo above. Bit positions are local
    /// to this snapshot's routes; persisted physical identity is ALWAYS the DeviceId, never a bit.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "Capacity::deserialize_device_routes"
    )]
    pub device_routes: Vec<u64>,
    /// Real pool backing offers/receipts from the same native owner, not synthetic workload rows.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "Capacity::deserialize_backings"
    )]
    pub backings: Vec<BackingOffer>,
    /// Devices listed above that this runtime won't offer now, each with why (AC-T7.4.1): never free while listed
    /// here. None from an older peer's report.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "Capacity::deserialize_unavailable"
    )]
    pub unavailable: Vec<crate::DeviceUnavailable>,
    /// What the node learned of this class's microVMs (joint plan step 14, C2 and C8): its priors and learned use.
    /// None from a node that learns none: no window on its platform, or an older build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned: Option<Learned>,
}

impl Capacity {
    /// Serde field entrypoint also used by the scheduler's SAME immutable route view.
    pub fn deserialize_routes<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<RouteOffer>, D::Error> {
        bounded_values(
            deserializer,
            MAX_RUNTIME_ROUTES,
            "runtime route count exceeds the 64-bit mask",
        )
    }

    /// One word per existing canonical device; no second inventory is decoded here.
    pub fn deserialize_device_routes<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u64>, D::Error> {
        bounded_values(
            deserializer,
            crate::MAX_DEVICES,
            "device eligibility count exceeds its address space",
        )
    }

    /// A backing must have a referencing initialized route, so the SAME route bound applies.
    pub fn deserialize_backings<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<BackingOffer>, D::Error> {
        bounded_values(
            deserializer,
            MAX_RUNTIME_ROUTES,
            "runtime backing count exceeds the route mask",
        )
    }

    /// At most one per device, as each names a device listed in the same report.
    pub fn deserialize_unavailable<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<crate::DeviceUnavailable>, D::Error> {
        bounded_values(
            deserializer,
            crate::MAX_DEVICES,
            "unavailable device count exceeds its address space",
        )
    }

    /// Copy an owned snapshot without unchecked allocation. Validation remains explicit: copying
    /// scalar facts cannot turn a malformed report into an initialized capability.
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        let mut id = String::new();
        id.try_reserve_exact(self.id.len())
            .map_err(|_| Refusal::Exhausted {
                what: "runtime capacity identity",
                bound: self.id.len(),
            })?;
        id.push_str(&self.id);
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(self.devices.len())
            .map_err(|_| Refusal::Exhausted {
                what: "runtime capacity devices",
                bound: self.devices.len(),
            })?;
        for device in &self.devices {
            devices.push(device.try_clone()?);
        }
        let mut backings = Vec::new();
        backings
            .try_reserve_exact(self.backings.len())
            .map_err(|_| Refusal::Exhausted {
                what: "runtime capacity backings",
                bound: self.backings.len(),
            })?;
        for backing in &self.backings {
            backings.push(backing.try_clone()?);
        }
        let mut unavailable = Vec::new();
        unavailable
            .try_reserve_exact(self.unavailable.len())
            .map_err(|_| Refusal::Exhausted {
                what: "runtime capacity unavailable devices",
                bound: self.unavailable.len(),
            })?;
        for device in &self.unavailable {
            unavailable.push(device.try_clone()?);
        }
        Ok(Self {
            id,
            total: self.total,
            free: self.free,
            devices,
            cost: self.cost,
            routes: copy_capacity_values(&self.routes, "runtime capacity routes")?,
            device_routes: copy_capacity_values(
                &self.device_routes,
                "runtime capacity eligibility",
            )?,
            backings,
            unavailable,
            learned: self.learned.as_ref().map(Learned::try_clone).transpose()?,
        })
    }

    pub fn check(&self) -> Result<(), Refusal> {
        if let Some(learned) = &self.learned {
            learned.check()?;
        }
        if self.id.is_empty()
            || self.free.cpu_milli > self.total.cpu_milli
            || self.free.mem_bytes > self.total.mem_bytes
            || self.free.disk_bytes > self.total.disk_bytes
        {
            return Err(Refusal::Invalid {
                what: "runtime capacity",
                why: "pool identity is empty or free resources exceed their totals".to_owned(),
            });
        }
        if let Some(cost) = self.cost {
            cost.check()?;
            if !self
                .total
                .holds(cost.allocate(CapacityResources::default())?.resources)
            {
                return Err(invalid_cost(
                    "minimum initialized runtime charge exceeds its physical pool",
                ));
            }
        }
        crate::devices::check_ids(self.devices.iter().map(|device| device.id))?;
        // Each unavailable device is one listed above, once, with its reason.
        for (at, device) in self.unavailable.iter().enumerate() {
            device.id.check()?;
            if device.why.is_empty()
                || !self.devices.iter().any(|listed| listed.id == device.id)
                || self
                    .unavailable
                    .iter()
                    .take(at)
                    .any(|earlier| earlier.id == device.id)
            {
                return Err(Refusal::Invalid {
                    what: "runtime capacity",
                    why: "an unavailable device is unlisted, repeated or has no reason".to_owned(),
                });
            }
        }
        self.check_routes()
    }
}

fn copy_capacity_values<T: Copy>(values: &[T], what: &'static str) -> Result<Vec<T>, Refusal> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(values.len())
        .map_err(|_| Refusal::Exhausted {
            what,
            bound: values.len(),
        })?;
    copy.extend_from_slice(values);
    Ok(copy)
}

impl Capacity {
    /// Validate an already chosen fresh-admission receipt against this native owner's actual
    /// initialized offer. This verifies identity/eligibility; it never chooses another route/device.
    /// Recovery/cleanup use the original canonical record and native scope, not today's offer.
    pub fn validate_route(&self, spec: &crate::BoxSpec) -> Result<RouteOffer, Refusal> {
        self.check()?;
        spec.check()?;
        let receipt = spec
            .route
            .ok_or_else(|| invalid_route("fresh routed admission has no committed receipt"))?;
        let (at, offer) = self
            .routes
            .iter()
            .enumerate()
            .find(|(_, offer)| offer.key == receipt.key && offer.backing == receipt.backing)
            .ok_or_else(|| {
                invalid_route("committed execution is not an initialized native offer")
            })?;
        if let Some(key) = receipt.backing {
            let backing = self
                .backings
                .iter()
                .find(|backing| backing.key == key)
                .ok_or_else(|| invalid_route("committed backing is absent"))?;
            offer.check_backing(backing)?;
            if offer.key.execution == Execution::ShardsShared {
                if !spec.devices.is_empty() {
                    return Err(invalid_route(
                        "shared backing cannot grant selected physical devices",
                    ));
                }
            } else {
                BackingOffer::check_selection_parts(&backing.devices, &spec.devices)?;
            }
            if backing.phase != BackingPhase::Live {
                return Err(Refusal::Backing {
                    key,
                    cause: LifecycleCause::Pending,
                });
            }
        }
        match (offer.cost, spec.allocation) {
            (Some(cost), Some(allocation)) if allocation.cost == cost => {}
            (None, None) => {}
            _ => {
                return Err(invalid_route(
                    "committed charge differs from the initialized execution",
                ));
            }
        }
        let bit = u32::try_from(at)
            .ok()
            .and_then(|at| 1u64.checked_shl(at))
            .ok_or_else(|| invalid_route("route eligibility bit overflows"))?;
        let mut indexed = Vec::new();
        indexed
            .try_reserve_exact(self.devices.len())
            .map_err(|_| Refusal::Exhausted {
                what: "route physical eligibility entries",
                bound: crate::MAX_DEVICES,
            })?;
        indexed.extend(self.devices.iter().zip(&self.device_routes));
        indexed.sort_unstable_by_key(|(device, _)| device.id);
        for id in &spec.devices {
            let (device, mask) = indexed
                .binary_search_by_key(id, |(device, _)| device.id)
                .ok()
                .and_then(|at| indexed.get(at))
                .ok_or_else(|| {
                    invalid_route("selected physical identity is absent from the initialized route")
                })?;
            if **mask & bit == 0 {
                return Err(invalid_route(
                    "selected physical identity is ineligible for the committed execution",
                ));
            }
            if device.holder.as_ref().is_some_and(|holder| match holder {
                crate::devices::DeviceHolder::Box(id) => id != &spec.id,
                crate::devices::DeviceHolder::Backing(key) => receipt.backing != Some(*key),
            }) {
                return Err(Refusal::Lifecycle {
                    id: spec.id.try_clone()?,
                    cause: LifecycleCause::Busy,
                });
            }
        }
        Ok(*offer)
    }

    pub fn check_routes(&self) -> Result<(), Refusal> {
        Self::check_route_parts(
            self.total,
            self.devices.len(),
            &self.routes,
            &self.device_routes,
            &self.backings,
        )
    }

    /// The scheduler's normalized offer view uses this same validation without another catalog.
    pub fn check_route_parts(
        total: CapacityResources,
        device_count: usize,
        routes: &[RouteOffer],
        device_routes: &[u64],
        backings: &[BackingOffer],
    ) -> Result<(), Refusal> {
        if routes.len() > MAX_RUNTIME_ROUTES || backings.len() > MAX_RUNTIME_ROUTES {
            return Err(Refusal::Exhausted {
                what: "initialized runtime routes/backings",
                bound: MAX_RUNTIME_ROUTES,
            });
        }
        if routes.is_empty() {
            if !device_routes.is_empty() || !backings.is_empty() {
                return Err(invalid_route(
                    "legacy capacity cannot imply route or backing eligibility",
                ));
            }
            return Ok(());
        }
        if device_routes.len() != device_count {
            return Err(invalid_route(
                "route masks must pair with the canonical physical device list",
            ));
        }
        let valid_bits = if routes.len() == MAX_RUNTIME_ROUTES {
            u64::MAX
        } else {
            let count = u32::try_from(routes.len())
                .map_err(|_| invalid_route("route word width overflows"))?;
            1u64.checked_shl(count)
                .and_then(|value| value.checked_sub(1))
                .ok_or_else(|| invalid_route("route word width overflows"))?
        };
        if device_routes.iter().any(|mask| mask & !valid_bits != 0) {
            return Err(invalid_route(
                "device eligibility references an absent initialized route",
            ));
        }
        let active = |route: &RouteOffer| {
            route.backing.is_none_or(|key| {
                backings
                    .iter()
                    .find(|backing| backing.key == key)
                    .is_some_and(|backing| backing.phase == BackingPhase::Live)
            })
        };
        for (at, route) in routes.iter().enumerate() {
            route.check()?;
            if routes.iter().take(at).any(|previous| {
                (previous.key == route.key && previous.backing == route.backing)
                    || active(previous)
                        && active(route)
                        && previous.key.class == route.key.class
                        && previous.key.image == route.key.image
                        && previous.key.gpu.overlaps(route.key.gpu)
            }) {
                return Err(invalid_route(
                    "initialized execution routes have ambiguous workload predicates",
                ));
            }
            if let Some(key) = route.backing {
                let backing = backings
                    .iter()
                    .find(|backing| backing.key == key)
                    .ok_or_else(|| invalid_route("route references an absent canonical backing"))?;
                route.check_backing(backing)?;
            }
            if let Some(cost) = route.cost
                && !total.holds(cost.allocate(CapacityResources::default())?.resources)
            {
                return Err(invalid_route(
                    "route minimum charge exceeds its physical resource pool",
                ));
            }
        }
        for (at, backing) in backings.iter().enumerate() {
            let allocation = backing.allocation()?;
            if !total.holds(allocation.resources) {
                return Err(invalid_route(
                    "backing charge exceeds its physical resource pool",
                ));
            }
            if backings
                .iter()
                .take(at)
                .any(|previous| previous.key.pool == backing.key.pool)
            {
                return Err(invalid_route(
                    "one pool has multiple competing backing generations",
                ));
            }
            let mut backend = None;
            for route in routes
                .iter()
                .filter(|route| route.backing == Some(backing.key))
            {
                let actual = route.key.backend_class();
                if backend.is_some_and(|previous| previous != actual) {
                    return Err(invalid_route(
                        "backing aliases disagree about the actual backend",
                    ));
                }
                backend = Some(actual);
            }
            if backend.is_none() {
                return Err(invalid_route("backing has no initialized backend route"));
            }
        }
        Ok(())
    }
}

/// A program started in a box.
pub trait Proc: Send {
    /// Its process id on the node (0 in a simulator); a program on another host, a VM's guest, gives that host's
    /// ([`Proc::on_this_host`]).
    fn pid(&self) -> u32;

    /// Whether it is a process of this host, so its [`Proc::pid`] names it here (AC-T7.4.1). A program in a VM's guest
    /// isn't: its pid is the guest's and may name no process here, or another one, so the node never watches or
    /// signals a process by it, and the guest's host forwards it.
    fn on_this_host(&self) -> bool {
        true
    }

    /// Its end if it has ended; never blocks.
    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal>;

    /// Waits at most `timeout` for its end; `Ok(None)` when it is still running.
    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal>;

    /// Ends this program, and only it (its box and the box's other programs run on), as KILL does: its end
    /// then reads as `Exit::Signal(9)`, unless it had ended first. `Ok` once it is sent, or when the program
    /// has ended already; it never reaches a process given the program's pid since.
    fn kill(&mut self) -> Result<(), Refusal>;

    /// The caller's ends of the streams its exec asked for ([`crate::Stdio::ask`]), once: a later call gives
    /// none.
    fn streams(&mut self) -> Result<crate::Given, Refusal>;
}

/// A program and the caller's ends of the streams its exec asked for ([`crate::Stdio::make_asked`]), held
/// until [`Proc::streams`] takes them: a runtime whose programs take pipes wraps its own `Proc` in it.
pub struct Asked<P> {
    proc: P,
    given: Option<crate::Given>,
}

impl<P: Proc> Asked<P> {
    pub fn new(proc: P, given: crate::Given) -> Self {
        Self {
            proc,
            given: Some(given),
        }
    }
}

impl<P: Proc> Proc for Asked<P> {
    fn pid(&self) -> u32 {
        self.proc.pid()
    }

    fn on_this_host(&self) -> bool {
        self.proc.on_this_host()
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        self.proc.try_wait()
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        self.proc.wait_for(timeout)
    }

    fn kill(&mut self) -> Result<(), Refusal> {
        self.proc.kill()
    }

    fn streams(&mut self) -> Result<crate::Given, Refusal> {
        Ok(self.given.take().unwrap_or_default())
    }
}

impl Proc for Box<dyn Proc> {
    fn pid(&self) -> u32 {
        (**self).pid()
    }

    fn on_this_host(&self) -> bool {
        (**self).on_this_host()
    }

    fn try_wait(&mut self) -> Result<Option<Exit>, Refusal> {
        (**self).try_wait()
    }

    fn wait_for(&mut self, timeout: Duration) -> Result<Option<Exit>, Refusal> {
        (**self).wait_for(timeout)
    }

    fn kill(&mut self) -> Result<(), Refusal> {
        (**self).kill()
    }

    fn streams(&mut self) -> Result<crate::Given, Refusal> {
        (**self).streams()
    }
}

/// How a program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exit {
    /// It exited with this status.
    Code(i32),
    /// This signal ended it.
    Signal(i32),
    /// It ended where its node agent couldn't collect how: while the agent was down, or (an `oci`
    /// box's init) after a restart, when the init is no longer the agent's child.
    Unknown,
}

/// A signal to send. Its number is the Linux one; Windows holds only KILL and TERM, both ending
/// the box's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Signal(u8);

impl Signal {
    pub const HUP: Self = Self(1);
    pub const INT: Self = Self(2);
    pub const QUIT: Self = Self(3);
    pub const KILL: Self = Self(9);
    pub const USR1: Self = Self(10);
    pub const USR2: Self = Self(12);
    pub const TERM: Self = Self(15);

    /// Signal number `n`, 1 to 64.
    pub fn new(n: i64) -> Result<Self, Refusal> {
        u8::try_from(n)
            .ok()
            .filter(|n| (1..=64).contains(n))
            .map(Self)
            .ok_or(Refusal::Invalid {
                what: "signal",
                why: format!("{n} is outside 1..=64"),
            })
    }

    pub fn number(self) -> i32 {
        i32::from(self.0)
    }
}

/// Whether anything runs in a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoxState {
    /// Nothing runs in it.
    Empty,
    /// At least one process runs in it.
    Running,
    /// An `oci` box whose init waits for [`Runtime::start`].
    Created,
}

/// How a macOS `process` box holds its members, which its node reports so a caller knows which (the
/// native plan's step 2). `None` where it doesn't apply: Linux and Windows boxes, and the simulator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Membership {
    /// A process group, bounded by the node's polling watcher — the rootless class, with no helper
    /// installed (step 1).
    ProcessGroup,
    /// A user of its own from the installer's pool, started and bounded by the privileged helper (step 2).
    BoxUser,
}

/// A box as its runtime holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxInfo {
    pub id: BoxId,
    pub class: Class,
    pub state: BoxState,
    pub bounds: Bounds,
    /// The existing native record's immutable admitted charge, independent of today's profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<Allocation>,
    /// Taken back after its node agent restarted, rather than made by this one.
    pub adopted: bool,
    /// How a macOS `process` box holds its members (the helper's box user, or the rootless process
    /// group), so its inspect says which; `None` off macOS. Added after the first release, so absent on
    /// the wire means `None` (append-only, CLAUDE.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub membership: Option<Membership>,
    /// How an `oci` box's init ended, once it has: the container's own exit. `None` while it runs,
    /// and for a `process` box, which has no init. Added after the first release, so absent on the
    /// wire means `None` (append-only, CLAUDE.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
    /// Programs exec'd in the box that its node agent ended because the caller watching them went
    /// away: its connection to the node agent's socket closed while they ran. Each was signalled KILL
    /// and reaped, never left running unwatched (decision 3 of docs/design/runtime-architecture.md).
    /// Added after the first release, so absent on the wire means 0 (append-only, CLAUDE.md).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub abandoned: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<RouteReceipt>,
    /// The box's network grants as its record holds them (joint plan step 15; docs/design/network-grants.md N6):
    /// a Shards image VM's, none for any other class. After `route`, so absent means none (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<crate::Grant>,
    /// Its publish grants' ports as its boot bound them on the host, while it runs (joint plan step 15, N6).
    /// After `grants` (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published: Vec<crate::Published>,
    /// Why its node's last resort ended it, where it did (joint plan step 14, C5): what it held past its own memory
    /// budget while its fleet's VMs held more than the fleet's budget. Kept in its record, so a restarted node's
    /// inspect says it too. After `published`, so absent means none (append-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_resort: Option<LearnedEnd>,
    /// Why its run ended where it failed, in its runtime's words (joint plan step 12): a Shards image VM's whose VM, run
    /// host or guest failed, or whose own command never started (Shards' D249), or whose boot was refused, its exit then
    /// `Unknown`. Kept in its record, so a restarted node's inspect says it too. After `last_resort`, so absent means
    /// none (append-only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    /// Its credential grants as its record holds them (joint plan step 33, Shards' D242; docs/design/credential-grants.md
    /// C4): a Shards image VM's, none for any other class. After `ended`, so absent means none (append-only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<crate::CredentialGrant>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What a box has used, and holds now.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub memory_bytes: u64,
    pub memory_peak_bytes: u64,
    /// Processes the kernel killed in the box for its memory bound.
    pub oom_kills: u64,
    pub pids: u64,
    /// Forks the kernel refused the box for its process bound.
    pub pids_refused: u64,
    pub cpu_usec: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    /// A microVM box's guest time per vCPU, which the learned charge reads (joint plan steps 12 and
    /// 14): `None` where this usage's producer reads none, a box with no guest of its own and the
    /// Firecracker and hex-guest Shards VMs among them. Added after the first release, so absent on
    /// the wire means `None` (append-only, CLAUDE.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest: Option<GuestTime>,
    /// How a Shards image VM's microVM started (D253; joint plan step 12), as its run host said it as the run started,
    /// which the box's record keeps too: `None` for a box with no Shards image run. Added after `guest`, so absent on
    /// the wire means `None` (append-only, CLAUDE.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_from: Option<StartedFrom>,
}

/// How a Shards image VM's microVM started (D253), as its run host says it as the run starts: Shards' own five
/// (shards-api's `image::StartedFrom`), so Hex can say how a box came up. The node's wire tags each by its place here,
/// so append only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartedFrom {
    /// Booted, with no template to restore or save: a guest its node gave (`SHARDS_KERNEL`, `SHARDS_INIT`), or a host
    /// whose hypervisor restores no snapshot.
    Booted,
    /// Booted, saving its image's template on the way, none being saved.
    Saved,
    /// Restored from its image's template.
    Restored,
    /// Booted, saving another, its image's template having failed to restore: that one removed.
    Replaced,
    /// A warm run host's, its microVM ready before its run asked (D253's pools).
    Warm,
}

/// A microVM's guest time per vCPU, or what its backend awaits to read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GuestTime {
    /// Nanoseconds each vCPU has run the guest since the VM booted, by vCPU index from 0, one for each
    /// vCPU of the VM's size: each only grows over the VM's life (monotone), so two reads' difference
    /// is the guest time between them, and none passes the wall time the VM has run.
    PerVcpuNs(#[serde(deserialize_with = "per_vcpu")] Vec<u64>),
    /// Its backend reads none yet: what it awaits, never a made-up number.
    Awaits(Upstream),
}

impl GuestTime {
    /// One entry for every 8-bit vCPU index: a Shards VM numbers its vCPUs by 8-bit APIC id on x86_64
    /// (its MADT, at most 255; crates/vmm/src/arch/x86_64/acpi.rs) and has fewer on arm64 (123, its
    /// GIC redistributor region; crates/vmm/src/arch/aarch64/mod.rs), at Shards 482c5d78 and e82b9403. Bounds a
    /// report read off the wire, never a VM's admission, which is Shards' to refuse.
    pub const MAX_VCPUS: usize = 1 << u8::BITS;
}

fn per_vcpu<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<u64>, D::Error> {
    bounded_values(
        deserializer,
        GuestTime::MAX_VCPUS,
        "guest time per vCPU past a VM's vCPU index space",
    )
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("native backing {key:?}: {cause:?}")]
    Backing {
        key: BackingKey,
        cause: LifecycleCause,
    },

    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("no box {0}")]
    NoSuchBox(BoxId),
    #[error("box {0} exists already")]
    Exists(BoxId),
    #[error("box {0} has nothing running to {1}")]
    NotRunning(BoxId, &'static str),
    #[error("{what} isn't available on {platform}: {why}")]
    Platform {
        what: &'static str,
        platform: &'static str,
        why: String,
    },
    #[error("the node holds its most boxes ({0})")]
    Full(usize),
    /// A per-node budget other than the box count is spent: `what` names it, `bound` is its size.
    #[error("the node holds its most {what} ({bound})")]
    Exhausted { what: &'static str, bound: usize },
    #[error("{op} failed: {why}")]
    Os { op: &'static str, why: String },
    #[error("the node agent's state is unusable: {0}")]
    Broken(String),
    /// The named box retains its resources while an operation is pending or its physical result
    /// is uncertain. Busy names exclusive lifecycle ownership, never absence or completion.
    #[error("box {id}: {cause}")]
    Lifecycle { id: BoxId, cause: LifecycleCause },
    /// A CONNECT to a guest's vsock port failed (Firecracker's CONNECT/OK handshake, which Shards
    /// speaks too): its port, phase and cause stay typed, never text (AC-T7.4.1, Shards R04). It
    /// says nothing of whether the guest ended.
    #[error("connecting to the guest's port {port} failed: {failure}")]
    Connect { port: u32, failure: ConnectFailure },
    /// A create refused whose cleanup failed too (AC-T7.4.1): the box may still hold its resources, its physical
    /// termination or absence unproved, as `Lifecycle`'s `Uncertain` says, and neither cause is dropped: the
    /// create's own refusal first, then its cleanup's, each typed. Neither is itself an `Uncertain`, which the
    /// wire refuses, so its depth is one.
    #[error(
        "box {id}: physical termination or absence has not been proved: {refusal}; its cleanup: {cleanup}"
    )]
    Uncertain {
        id: BoxId,
        refusal: Box<Refusal>,
        cleanup: Box<Refusal>,
    },
    /// The call needs what another product doesn't give in the revision Hex builds against yet
    /// ([`Upstream`]): refused, never stubbed (the joint plan's rule for a missing dependency). It
    /// says nothing of any box's state.
    #[error("this needs {needs}, which the revision Hex builds against doesn't give yet")]
    Awaits { needs: Upstream },
    /// Box `id`'s run failed (joint plan step 12): its command never started, or its VM, its host or its guest failed,
    /// in its runtime's words, `why`, which its [`BoxInfo::ended`] keeps beside an `Unknown` exit. The workload never
    /// ran, or stopped running for its machine's sake, where every other refusal is the call's or its infrastructure's.
    /// A run that started and then ended, at once or not, is no failure: its exit says how it ended. After `Awaits`
    /// (append-only).
    #[error("box {id}'s run failed: {why}")]
    Failed { id: BoxId, why: String },
    /// What its node isn't configured to serve, `what`, and why it's needed (joint plan step 33): a Shards image VM's
    /// credential grants on a node its orchestrator gave no lessors ([`crate::lease::Lessors`]), whose values it could
    /// lease from no one. Its configuration's, not its platform's or its revision's: the same node given lessors serves
    /// it. After `Failed` (append-only).
    #[error("this node is configured with no {what}: {why}")]
    Unconfigured { what: &'static str, why: String },
}

/// A capability another product owes Hex, named by what it does, never by a plan step (a plan's
/// step numbers change between its versions). A call that needs one is refused
/// [`Refusal::Awaits`] until Hex builds against a revision that gives it. The node's wire tags each
/// by its place in [`Upstream::ALL`], so append only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Upstream {
    /// A Shards image VM's guest time per vCPU, with a monotone total, on HVF, KVM and WHP. Shards'
    /// guest CPU time work (221c7377, 6396a78b) gives it to the daemon's runs alone; D231's image runs
    /// don't carry it: `shards api-run` never asks its VM `GUEST_TIME`, and shards-api's image `Stats`
    /// has no field for it (Shards e82b9403).
    GuestCpuTimePerVcpu,
    /// A program started in a member of a Shards microVM: the exec of Shards' agent-control
    /// endpoint.
    MemberExec,
    /// A microVM member's UDP flows to a host's name: Shards' destinations (D236) reach a name by TCP
    /// alone.
    MemberGrantsByName,
    /// What a runtime nested under shards-init needs: a delegated cgroup subtree, CAP_SYS_ADMIN for
    /// its boxes' namespaces and DRM render nodes, granted by name. Given in part since Shards 66113379:
    /// D234's nesting grant (shards-api's `ImageSpec::nesting`) gives the subtree and the capabilities. Not given
    /// yet: its GPUs' identities and render nodes (D234's `gpus=`, which no image boot's GPU selection carries).
    NestedRuntime,
    /// The Windows host of an image's run: `shards api-run` on Windows, which Shards gives since
    /// 731b5e54 (D231). No call awaits it now; its place in [`Upstream::ALL`] stays, as the node's
    /// wire tags it.
    WindowsRunHost,
    /// Taking back an image's run after its owner's crash: a run ends with its owner in D231's
    /// first version, which owes `attach`.
    AttachAfterCrash,
    /// A signal to the members of a Shards microVM: the signal of Shards' agent-control endpoint.
    MemberSignal,
    /// A microVM member's flows to an IPv6 address, or by UDP to any address: Shards' destinations
    /// (D236) reach an IPv4 address by TCP alone.
    MemberGrantsByAddress,
    /// A microVM member's UDP reach to a service its own host serves on its loopback: Shards' destinations
    /// (D236) reach the host itself (`host.docker.internal`) by TCP alone.
    MemberHostService,
    /// A Shards guest told the room its host's budget leaves it (D188): init's `ROOM`, which
    /// shards-api's image boot (D231) has no call to send (joint plan step 14).
    GuestRoom,
    /// The host of an image's run started holding only its stdio, as every program Hex starts is. Given since
    /// Hex's vendor/patches/shards 0001 (offered upstream): shards-api starts `shards api-run` by posix_spawn with
    /// `POSIX_SPAWN_CLOEXEC_DEFAULT` on macOS and by CreateProcessW with a handle list on Windows. No call awaits
    /// it now; its place in [`Upstream::ALL`] stays, as the node's wire tags it.
    RunHostStdioOnly,
    /// The member a published port reaches, named by Shards' image boot (D231). Given since Shards 66113379
    /// (`Bound::to`), so no call awaits it now: a run is held to it as its boot returns (joint plan step 15,
    /// docs/design/network-grants.md N6). Its tag stays, the wire being append-only.
    PublishReceiver,
    /// A Shards guest's engine-ring pins (D230's long-term pinned slots) kept within the room its budget leaves it:
    /// served by Shards' ring-pins-v1, vendored at 66113379, whose pins come out of the room init is told, so no Hex
    /// call awaits it now; kept for the node's wire, whose tags only grow (joint plan step 14, C7).
    RingPinsWithinRoom,
    /// A running Shards microVM saved whole and resumed on another machine (D176), so the last resort moves a VM
    /// past its budget rather than ending it: D176's moves are Shards' own command's, which shards-api's image run
    /// (D231) doesn't offer at 66113379, whose D235 plugs directories into a running member, not the member
    /// (joint plan step 14, C5).
    LiveMove,
    /// A destination (D236: a host's name, an IPv4 address or the host itself, by TCP) kept for the members an
    /// orchestrator grants it alone. Given by Shards' D246 (7a4ef5c02, upstream from Hex's change 0002, which Hex no
    /// longer carries): each member's reach in D231's narrowing, held by the run host before the VM starts, so no call
    /// awaits it now (joint plan step 15, docs/design/network-grants.md N4). Its tag stays, the wire being
    /// append-only.
    MemberDestinations,
    /// A host's connection to a runtime it nests under shards-init: Hex's node, its init the run's own command under
    /// D234's grant (docs/design/node-under-shards-init.md §5). The node's relay listens on AF_UNIX sockets there, as
    /// the workload's filter refuses vsock. Shards' agent-control endpoint (D238) has no lane that carries a connection
    /// to such a socket, and D231's run host binds no endpoint (Shards 7a4ef5c0).
    NestedChannel,
    /// The period a Shards VM's rate holds over (D247's hold is D191's governor, its period CFS's), as Shards' API
    /// says it: shards-api exposes neither a call nor a const for it, so a learner capping a VM through its rate can't
    /// take its window from it (joint plan step 14, C1).
    RatePeriod,
}

impl Upstream {
    /// Every capability, in the order the node's wire tags them.
    pub const ALL: [Upstream; 17] = [
        Upstream::GuestCpuTimePerVcpu,
        Upstream::MemberExec,
        Upstream::MemberGrantsByName,
        Upstream::NestedRuntime,
        Upstream::WindowsRunHost,
        Upstream::AttachAfterCrash,
        Upstream::MemberSignal,
        Upstream::MemberGrantsByAddress,
        Upstream::MemberHostService,
        Upstream::GuestRoom,
        Upstream::RunHostStdioOnly,
        Upstream::PublishReceiver,
        Upstream::RingPinsWithinRoom,
        Upstream::LiveMove,
        Upstream::MemberDestinations,
        Upstream::NestedChannel,
        Upstream::RatePeriod,
    ];
}

impl std::fmt::Display for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::GuestCpuTimePerVcpu => {
                "a Shards image VM's guest time per vCPU (D231's stats; its run host asks no GUEST_TIME)"
            }
            Self::MemberExec => "a program run in a Shards member (its agent-control endpoint)",
            Self::MemberGrantsByName => {
                "a Shards member's UDP flows to a name (Shards' destinations, D236, are TCP alone)"
            }
            Self::NestedRuntime => "a runtime nested under shards-init, with its grants",
            Self::WindowsRunHost => "Shards' Windows host of an image's run (D231)",
            Self::AttachAfterCrash => {
                "taking back an image's run after its owner's crash (owed by D231)"
            }
            Self::MemberSignal => "a signal to Shards members (its agent-control endpoint)",
            Self::MemberGrantsByAddress => {
                "a Shards member's flows to an IPv6 address or by UDP (Shards' destinations, D236, are TCP to IPv4)"
            }
            Self::MemberHostService => {
                "a Shards member's UDP reach to its host's own service (Shards' destinations, D236, are TCP alone)"
            }
            Self::GuestRoom => "a Shards guest told its room (init's ROOM, owed through D231)",
            Self::RunHostStdioOnly => {
                "Shards' host of an image's run started holding only its stdio (D231)"
            }
            Self::PublishReceiver => {
                "Shards' image boot naming the member a published port reaches (given by D231's Bound::to)"
            }
            Self::RingPinsWithinRoom => {
                "a Shards guest's ring pins kept within its room (Shards' p/ring-pins-v1)"
            }
            Self::LiveMove => "a running Shards microVM moved to another machine (Shards' D176)",
            Self::MemberDestinations => {
                "a Shards destination kept for the members granted it alone (given by Hex's recorded Shards change)"
            }
            Self::NestedChannel => {
                "a connection to a runtime nested under shards-init (a lane of Shards' agent-control endpoint to its socket)"
            }
            Self::RatePeriod => {
                "the period a Shards VM's rate holds over (D191's governor), which shards-api doesn't expose"
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleCause {
    #[error("native operation is pending")]
    Pending,
    #[error("physical termination or absence has not been proved")]
    Uncertain,
    #[error("another operation owns this box's lifecycle")]
    Busy,
    /// Its backing was retired at its node's start (joint plan step 14; ROOT, 2026-11-13): recorded under a census
    /// cost, its identity is no longer its node's profile's. A box on it keeps its record, ready, until its owner's
    /// delete. Appended (wire append-only).
    #[error("its backing's identity changed with decision 9's census-free cost")]
    Retired,
}

/// Why a CONNECT handshake failed: the phase it failed in and what it saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConnectFailure {
    /// The caller's deadline passed.
    #[error("{phase:?}: handshake deadline expired")]
    Expired { phase: ConnectPhase },
    /// An I/O failure, with the OS's own code where there was one.
    #[error("{phase:?}: {kind} (OS {code:?})")]
    Io {
        phase: ConnectPhase,
        kind: IoKind,
        code: Option<i32>,
    },
    /// Closed with no answer: nothing listens on the port.
    #[error("closed unanswered (the guest does not listen there)")]
    Closed,
    /// An answer outside the CONNECT/OK grammar.
    #[error("{0}")]
    Reply(ConnectReply),
}

/// A CONNECT handshake's steps: dial the vsock socket, write `CONNECT <port>`, read the answer,
/// clear the socket's timeouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectPhase {
    Dial,
    Write,
    Read,
    ClearTimeouts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConnectReply {
    #[error("answered past the longest OK, no line")]
    TooLong,
    #[error("answered not OK")]
    NotOk,
}

/// An I/O failure's [`std::io::ErrorKind`] where Hex names it ([`IoKind::NAMED`]). Any other kind is
/// [`IoKind::UNNAMED`]: apart from every named kind, `Other` included, and never guessed back into
/// one. The OS code that travels beside it stays exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoKind(Option<std::io::ErrorKind>);

impl IoKind {
    /// Every kind stable in Rust 1.99.1 (rust-toolchain.toml), each tagged on the node's wire by its
    /// place here (hex-core's wire.rs), so append only. The unstable ones std reports, such as
    /// `Uncategorized`, `InputOutputError` and `InProgress`, can't be named.
    pub const NAMED: [std::io::ErrorKind; 39] = [
        std::io::ErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::ConnectionRefused,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::HostUnreachable,
        std::io::ErrorKind::NetworkUnreachable,
        std::io::ErrorKind::ConnectionAborted,
        std::io::ErrorKind::NotConnected,
        std::io::ErrorKind::AddrInUse,
        std::io::ErrorKind::AddrNotAvailable,
        std::io::ErrorKind::NetworkDown,
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::AlreadyExists,
        std::io::ErrorKind::WouldBlock,
        std::io::ErrorKind::NotADirectory,
        std::io::ErrorKind::IsADirectory,
        std::io::ErrorKind::DirectoryNotEmpty,
        std::io::ErrorKind::ReadOnlyFilesystem,
        std::io::ErrorKind::StaleNetworkFileHandle,
        std::io::ErrorKind::InvalidInput,
        std::io::ErrorKind::InvalidData,
        std::io::ErrorKind::TimedOut,
        std::io::ErrorKind::WriteZero,
        std::io::ErrorKind::StorageFull,
        std::io::ErrorKind::NotSeekable,
        std::io::ErrorKind::QuotaExceeded,
        std::io::ErrorKind::FileTooLarge,
        std::io::ErrorKind::ResourceBusy,
        std::io::ErrorKind::ExecutableFileBusy,
        std::io::ErrorKind::Deadlock,
        std::io::ErrorKind::CrossesDevices,
        std::io::ErrorKind::TooManyLinks,
        std::io::ErrorKind::InvalidFilename,
        std::io::ErrorKind::ArgumentListTooLong,
        std::io::ErrorKind::Interrupted,
        std::io::ErrorKind::Unsupported,
        std::io::ErrorKind::UnexpectedEof,
        std::io::ErrorKind::OutOfMemory,
        std::io::ErrorKind::Other,
    ];
    pub const UNNAMED: Self = Self(None);

    pub fn new(kind: std::io::ErrorKind) -> Self {
        if Self::NAMED.contains(&kind) {
            Self(Some(kind))
        } else {
            Self::UNNAMED
        }
    }

    pub fn named(self) -> Option<std::io::ErrorKind> {
        self.0
    }
}

impl std::fmt::Display for IoKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(kind) => write!(f, "{kind:?}"),
            None => f.write_str("an unnamed I/O kind"),
        }
    }
}

impl Refusal {
    /// An operating system call that failed, with what was being done.
    pub fn os(op: &'static str, err: impl std::fmt::Display) -> Self {
        Self::Os {
            op,
            why: err.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resources(cpu_milli: u64, mem_bytes: u64, disk_bytes: u64) -> CapacityResources {
        CapacityResources {
            cpu_milli,
            mem_bytes,
            disk_bytes,
        }
    }

    fn cost() -> RuntimeCost {
        // Fixture data exercises the formula, not measured hardware allowances.
        RuntimeCost {
            id: [1; 32],
            floor: resources(1000, 64 << 20, 0),
            ceiling: resources(4000, 256 << 20, 1 << 30),
            quantum: resources(1000, 1 << 20, 1),
            guest_overhead: resources(50, 3 << 20, 7),
            overhead: resources(25, 5 << 20, 11),
        }
    }

    /// AC-T7.4.1: admission and runtime pay guest/kernel and host/VMM costs once, preserving workload.
    #[test]
    fn runtime_cost_quantizes_guest_then_adds_host_allowance_exactly_once() {
        let cost = cost();
        let workload = resources(1500, (65 << 20) + 1, 100);
        let allocation = cost.allocate(workload).unwrap();
        assert_eq!(allocation.guest().unwrap(), resources(2000, 69 << 20, 107));
        assert_eq!(allocation.resources, resources(2025, 74 << 20, 118));
        allocation.check(workload).unwrap();
        allocation.covers(workload).unwrap();
        assert!(allocation.check(resources(1500, 65 << 20, 100)).is_err());
        assert_eq!(workload, resources(1500, (65 << 20) + 1, 100));
        let encoded = serde_json::to_vec(&allocation).unwrap();
        assert_eq!(
            serde_json::from_slice::<Allocation>(&encoded).unwrap(),
            allocation
        );
        let mut changed = allocation;
        changed.resources.mem_bytes += 1;
        assert!(changed.guest().is_err());
        assert!(changed.check(workload).is_err());
    }

    /// AC-T7.4.1: CPU-only dedicated Shards boxes pay the entire configured VM before placement.
    #[test]
    fn fixed_guest_floor_and_ceiling_refuse_workloads_that_cannot_fit_with_guest_overhead() {
        let mut cost = cost();
        cost.floor = resources(4000, 256 << 20, 1 << 30);
        cost.ceiling = cost.floor;
        let small = resources(10, 1 << 20, 0);
        let allocation = cost.allocate(small).unwrap();
        assert_eq!(allocation.guest().unwrap(), cost.floor);
        assert_eq!(
            allocation.resources,
            resources(4025, 261 << 20, (1 << 30) + 11)
        );
        assert!(cost.allocate(cost.floor).is_err());
        assert!(allocation.covers(cost.floor).is_err());
    }

    /// AC-T7.4.1: every absent/zero unit, ceiling violation and overflow is a typed refusal.
    #[test]
    fn runtime_cost_refuses_unknown_profile_bad_units_ceiling_and_arithmetic_overflow() {
        for quantum in [resources(0, 1, 1), resources(1, 0, 1), resources(1, 1, 0)] {
            let mut changed = cost();
            changed.quantum = quantum;
            assert!(matches!(
                changed.allocate(resources(1, 1, 1)),
                Err(Refusal::Invalid {
                    what: "runtime allocation",
                    ..
                })
            ));
        }
        let mut changed = cost();
        changed.id = [0; 32];
        assert!(changed.check().is_err());
        changed = cost();
        changed.floor.cpu_milli = 5000;
        assert!(changed.check().is_err());
        assert!(cost().allocate(resources(u64::MAX, 1, 1)).is_err());
        assert!(cost().allocate(resources(4000, 1, 1)).is_err());
        changed = cost();
        changed.ceiling.disk_bytes = u64::MAX;
        assert!(changed.check().is_err());
        changed.overhead.disk_bytes = 0;
        changed.guest_overhead.disk_bytes = 0;
        changed.quantum.disk_bytes = 2;
        assert!(changed.allocate(resources(1, 1, u64::MAX)).is_err());
    }

    /// AC-T7.4.1 (joint plan step 12): a microVM's size is its admitted guest envelope in whole units.
    /// Do: size the simulator's two-vCPU, 256 MiB allocation, and envelopes of a vCPU and a half, of no
    /// vCPU and of a MiB and a half. Expect: 2 vCPUs and 256 MiB, then each of the others refused.
    #[test]
    fn a_vm_is_sized_in_whole_vcpus_and_mib_of_its_allocation() {
        let sized = crate::sim::sim_vm_allocation().unwrap().vm_size().unwrap();
        assert_eq!(
            sized,
            VmSize {
                vcpus: 2,
                memory_mib: 256
            }
        );
        let profile = |cpu_quantum: u64, mem_quantum: u64| RuntimeCost {
            id: [7; 32],
            floor: CapacityResources::default(),
            ceiling: resources(8_000, 8 << 30, 0),
            quantum: resources(cpu_quantum, mem_quantum, 1),
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
        };
        for (cpu_quantum, mem_quantum, workload) in [
            (500, 1 << 20, resources(1_500, 256 << 20, 0)),
            (500, 1 << 20, resources(0, 256 << 20, 0)),
            (1_000, 1 << 19, resources(1_000, 3 << 19, 0)),
        ] {
            let allocation = profile(cpu_quantum, mem_quantum)
                .allocate(workload)
                .unwrap();
            assert!(
                matches!(
                    allocation.vm_size(),
                    Err(Refusal::Invalid {
                        what: "runtime allocation",
                        ..
                    })
                ),
                "{workload:?}"
            );
        }
    }

    /// AC-T7.4.1 (joint plan step 12): the wire tags a capability by its place in `Upstream::ALL`.
    /// Do: look each one up there, and say its `Awaits` refusal. Expect: each in one place only, and
    /// the refusal naming it in its words.
    #[test]
    fn each_upstream_capability_has_one_place_and_its_refusal_names_it() {
        for needs in Upstream::ALL {
            assert_eq!(Upstream::ALL.iter().filter(|u| **u == needs).count(), 1);
            let said = Refusal::Awaits { needs }.to_string();
            assert!(said.contains(&needs.to_string()), "{said}");
        }
    }

    #[test]
    fn a_signal_number_outside_the_kernels_range_is_refused() {
        assert!(Signal::new(0).is_err());
        assert!(Signal::new(65).is_err());
        assert!(Signal::new(-9).is_err());
        assert_eq!(Signal::new(9).unwrap(), Signal::KILL);
        assert_eq!(Signal::new(64).unwrap().number(), 64);
    }

    /// Append-only wire (CLAUDE.md): a `BoxInfo` written before `exit` existed reads as `None`, and
    /// one with no exit is written without the field, so an older reader sees what it always did.
    #[test]
    fn a_box_info_without_an_exit_reads_and_writes_as_before() {
        let old = r#"{"id":"b","class":"oci","state":"empty","bounds":{},"adopted":false}"#;
        let info: BoxInfo = serde_json::from_str(old).unwrap();
        assert_eq!(info.exit, None);
        assert!(!serde_json::to_string(&info).unwrap().contains("exit"));
        let ended = BoxInfo {
            allocation: None,
            route: None,
            membership: None,
            exit: Some(Exit::Signal(9)),
            ..info
        };
        let back: BoxInfo = serde_json::from_str(&serde_json::to_string(&ended).unwrap()).unwrap();
        assert_eq!(back.exit, Some(Exit::Signal(9)));
    }

    /// HEX-SOCK-6, append-only: a `BoxInfo` written before `abandoned` existed reads as 0, one with
    /// none abandoned is written without the field, and a count round-trips.
    #[test]
    fn a_box_info_without_abandoned_programs_reads_and_writes_as_before() {
        let old = r#"{"id":"b","class":"process","state":"running","bounds":{},"adopted":false}"#;
        let info: BoxInfo = serde_json::from_str(old).unwrap();
        assert_eq!(info.abandoned, 0);
        assert!(!serde_json::to_string(&info).unwrap().contains("abandoned"));
        let left = BoxInfo {
            membership: None,
            abandoned: 2,
            ..info
        };
        let back: BoxInfo = serde_json::from_str(&serde_json::to_string(&left).unwrap()).unwrap();
        assert_eq!(back.abandoned, 2);
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;

    /// AC-T7.4.1/R01: count/hint checks precede decoding an excessive value and fallible Vec growth.
    #[test]
    fn route_sequence_bounds_refuse_before_visiting_the_excess_value() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static VISITED: AtomicUsize = AtomicUsize::new(0);
        #[derive(Debug)]
        struct Counted {
            _value: u64,
        }
        impl<'de> Deserialize<'de> for Counted {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                VISITED.fetch_add(1, Ordering::SeqCst);
                Ok(Self {
                    _value: u64::deserialize(deserializer)?,
                })
            }
        }
        let empty: Vec<Counted> = bounded_values(
            &mut serde_json::Deserializer::from_str("[]"),
            MAX_RUNTIME_ROUTES,
            "route bound",
        )
        .unwrap();
        assert_eq!((empty.len(), empty.capacity()), (0, 0));
        for count in [MAX_RUNTIME_ROUTES, MAX_RUNTIME_ROUTES + 1] {
            VISITED.store(0, Ordering::SeqCst);
            let json = format!("[{}]", vec!["0"; count].join(","));
            let result: Result<Vec<Counted>, _> = bounded_values(
                &mut serde_json::Deserializer::from_str(&json),
                MAX_RUNTIME_ROUTES,
                "route bound",
            );
            assert_eq!(result.is_ok(), count == MAX_RUNTIME_ROUTES);
            assert_eq!(VISITED.load(Ordering::SeqCst), MAX_RUNTIME_ROUTES);
        }
        VISITED.store(0, Ordering::SeqCst);
        let hostile = serde::de::value::SeqDeserializer::<_, serde::de::value::Error>::new(
            std::iter::repeat_n(0u64, usize::MAX),
        );
        let result: Result<Vec<Counted>, _> =
            bounded_values(hostile, MAX_RUNTIME_ROUTES, "route bound");
        assert!(result.is_err());
        assert_eq!(VISITED.load(Ordering::SeqCst), 0);
    }

    /// AC-T7.4.1/R01: actual new Capacity fields use the count guard before model validation.
    #[test]
    fn capacity_route_json_fields_refuse_counts_before_materializing_a_report() {
        let route = serde_json::to_value(RouteOffer {
            key: key(ImageOs::Windows, Execution::Native, GpuEligibility::Any),
            cost: None,
            backing: None,
        })
        .unwrap();
        let backing = serde_json::to_value(BackingOffer {
            guest: Default::default(),
            key: BackingKey {
                pool: [1; 32],
                generation: [2; 16],
            },
            cost: cost(),
            phase: BackingPhase::Prepared,
            devices: Vec::new(),
        })
        .unwrap();
        for (field, value, count, cause) in [
            ("routes", route, MAX_RUNTIME_ROUTES + 1, "route count"),
            (
                "device_routes",
                serde_json::json!(0),
                crate::MAX_DEVICES + 1,
                "eligibility count",
            ),
            ("backings", backing, MAX_RUNTIME_ROUTES + 1, "backing count"),
        ] {
            let mut input = serde_json::to_value(capacity()).unwrap();
            input[field] = serde_json::Value::Array(vec![value; count]);
            let error = serde_json::from_value::<Capacity>(input).unwrap_err();
            assert!(error.to_string().contains(cause), "{field}: {error}");
        }
        let empty = serde_json::to_value(capacity()).unwrap();
        let parsed: Capacity = serde_json::from_value(empty).unwrap();
        assert!(
            parsed.routes.is_empty()
                && parsed.backings.is_empty()
                && parsed.device_routes.is_empty()
        );
    }

    /// AC-T7.4.1/R01: nested grants/offers cannot allocate an unbounded provisioning claim list.
    #[test]
    fn backing_json_claims_obey_the_same_device_address_space_before_allocation() {
        let grant = BackingGrant {
            key: BackingKey {
                pool: [1; 32],
                generation: [2; 16],
            },
            allocation: cost().allocate(CapacityResources::default()).unwrap(),
            devices: Vec::new(),
        };
        let claims: Vec<_> = (0..crate::MAX_DEVICES)
            .map(|key| serde_json::json!({"kind":"gpu","key":key}))
            .collect();
        let mut input = serde_json::to_value(&grant).unwrap();
        input["devices"] = serde_json::Value::Array(claims);
        let parsed: BackingGrant = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(parsed.devices.len(), crate::MAX_DEVICES);
        input["devices"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"kind":"gpu","key":crate::MAX_DEVICES}));
        let error = serde_json::from_value::<BackingGrant>(input.clone()).unwrap_err();
        assert!(error.to_string().contains("physical claim count"));
        let mut offer = serde_json::to_value(BackingOffer {
            guest: Default::default(),
            key: grant.key,
            cost: cost(),
            phase: BackingPhase::Prepared,
            devices: Vec::new(),
        })
        .unwrap();
        offer["devices"] = input["devices"].take();
        let error = serde_json::from_value::<BackingOffer>(offer).unwrap_err();
        assert!(error.to_string().contains("physical claim count"));
    }

    fn platform(os: ImageOs) -> ImagePlatform {
        ImagePlatform {
            os,
            arch: ImageArch::Arm64,
        }
    }
    fn key(os: ImageOs, execution: Execution, gpu: GpuEligibility) -> RouteKey {
        RouteKey {
            class: crate::Class::Oci,
            image: platform(os),
            execution,
            gpu,
        }
    }
    fn cost() -> RuntimeCost {
        let fixed = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: crate::MIN_MEMORY,
            disk_bytes: 0,
        };
        RuntimeCost {
            id: [1; 32],
            floor: fixed,
            ceiling: fixed,
            quantum: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
        }
    }
    fn capacity() -> Capacity {
        Capacity {
            id: "native-owner".into(),
            total: CapacityResources {
                cpu_milli: 4000,
                mem_bytes: crate::MIN_MEMORY * 4,
                disk_bytes: 4096,
            },
            free: CapacityResources::default(),
            devices: Vec::new(),
            cost: None,
            routes: Vec::new(),
            device_routes: Vec::new(),
            backings: Vec::new(),
            unavailable: Vec::new(),
            learned: None,
        }
    }
    /// Joint plan step 14 (C7; ROOT, 2026-11-13): a shared backing's payload fits its envelope with the guest's own
    /// overhead its runtime measured, never the census's (a census-free cost carries none); cold, its boxes' sum alone.
    /// Do check boxes summing 0.5 CPU and 512 MiB against a backing of a 1 CPU, 1 MiB-quantum envelope of 1 GiB, cold;
    /// with a guest measured at 0.25 CPU and 256 MiB; then at 0.25 CPU and 600 MiB. Expect the first two to fit and the
    /// third, its memory past the envelope with the boxes', refused.
    #[test]
    fn w14_a_shared_backings_payload_fits_with_its_guests_measured_overhead() {
        let envelope = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: 1 << 30,
            disk_bytes: 0,
        };
        let mut backing = BackingOffer {
            guest: Default::default(),
            key: BackingKey {
                pool: [2; 32],
                generation: [3; 16],
            },
            cost: RuntimeCost {
                floor: envelope,
                ceiling: envelope,
                ..cost()
            },
            phase: BackingPhase::Live,
            devices: Vec::new(),
        };
        let boxes = CapacityResources {
            cpu_milli: 500,
            mem_bytes: 512 << 20,
            disk_bytes: 0,
        };
        backing.check_inner(boxes).unwrap();
        backing.guest = LearnedOverhead {
            cpu_milli: Some(250),
            mem_bytes: Some(256 << 20),
            disk_bytes: None,
        };
        backing.check_inner(boxes).unwrap();
        backing.guest.mem_bytes = Some(600 << 20);
        assert!(backing.check_inner(boxes).is_err());
    }

    /// AC-T7.4.1: original OCI form can identify a real Linux guest on Mac and alongside Windows HCS.
    #[test]
    fn typed_oci_routes_preserve_workload_form_and_distinguish_platform_and_charge() {
        let backing = BackingOffer {
            guest: Default::default(),
            key: BackingKey {
                pool: [2; 32],
                generation: [3; 16],
            },
            cost: cost(),
            phase: BackingPhase::Prepared,
            devices: Vec::new(),
        };
        let mut c = capacity();
        c.backings.push(backing.try_clone().unwrap());
        c.routes = vec![
            RouteOffer {
                key: key(ImageOs::Windows, Execution::Native, GpuEligibility::Any),
                cost: None,
                backing: None,
            },
            RouteOffer {
                key: key(
                    ImageOs::Linux,
                    Execution::ShardsShared,
                    GpuEligibility::None,
                ),
                cost: None,
                backing: Some(backing.key),
            },
            RouteOffer {
                key: key(
                    ImageOs::Linux,
                    Execution::ShardsDedicated,
                    GpuEligibility::Required,
                ),
                cost: Some(cost()),
                backing: None,
            },
        ];
        c.check().unwrap();
        assert_eq!(c.routes[1].key.class, crate::Class::Oci);
        assert!(
            key(ImageOs::Macos, Execution::Native, GpuEligibility::Any)
                .check()
                .is_err()
        );
        assert!(
            RouteReceipt {
                key: c.routes[2].key,
                backing: None
            }
            .check(crate::Class::Oci, true)
            .is_ok()
        );
        assert!(
            RouteReceipt {
                key: c.routes[2].key,
                backing: None
            }
            .check(crate::Class::Shards, true)
            .is_err()
        );
        assert!(
            RouteReceipt {
                key: c.routes[2].key,
                backing: None
            }
            .check(crate::Class::Oci, false)
            .is_err()
        );
    }
    /// AC-T7.4.1: eligibility refers to one canonical device list; absent/high/ambiguous routes refuse.
    #[test]
    fn route_masks_backing_references_and_ambiguous_predicates_are_checked() {
        let mut c = capacity();
        let native = RouteOffer {
            key: key(ImageOs::Linux, Execution::Native, GpuEligibility::Any),
            cost: None,
            backing: None,
        };
        c.routes.push(native);
        c.device_routes.push(1);
        assert!(c.check().is_err());
        c.devices.push(crate::DeviceInfo {
            id: crate::DeviceId {
                kind: crate::DeviceKind::Gpu,
                key: 7,
            },
            model: "actual".into(),
            size: 10,
            holder: None,
        });
        c.check().unwrap();
        c.routes.push(native);
        assert!(c.check().is_err(), "duplicate exact RouteKey must refuse");
        c.routes.pop();
        c.device_routes[0] = 2;
        assert!(c.check().is_err());
        c.device_routes[0] = 1;
        c.routes.push(RouteOffer {
            key: key(
                ImageOs::Linux,
                Execution::ShardsDedicated,
                GpuEligibility::Required,
            ),
            cost: Some(cost()),
            backing: None,
        });
        assert!(c.check().is_err());
        c.routes = vec![RouteOffer {
            key: key(
                ImageOs::Linux,
                Execution::ShardsShared,
                GpuEligibility::None,
            ),
            cost: None,
            backing: Some(BackingKey {
                pool: [2; 32],
                generation: [3; 16],
            }),
        }];
        assert!(c.check().is_err());
    }
    /// AC-T7.4.1: caller-supplied execution/allocation/backing cannot grant a route before effects.
    #[test]
    fn committed_route_rejects_forged_execution_cost_and_unready_backing() {
        let mut c = capacity();
        let offered = RouteOffer {
            key: key(
                ImageOs::Linux,
                Execution::ShardsDedicated,
                GpuEligibility::Required,
            ),
            cost: Some(cost()),
            backing: None,
        };
        c.routes.push(offered);
        let id = crate::DeviceId {
            kind: crate::DeviceKind::Gpu,
            key: 7,
        };
        c.devices.push(crate::DeviceInfo {
            id,
            model: "actual".into(),
            size: 10,
            holder: None,
        });
        c.device_routes.push(1);
        let mut spec = crate::BoxSpec {
            id: crate::BoxId::new("fresh-route").unwrap(),
            class: crate::ClassSpec::Oci(Box::new(crate::OciSpec {
                rootfs: crate::Rootfs::Dir(std::env::current_dir().unwrap()),
                init: crate::Program::of(&["/bin/true"]),
                mounts: Vec::new(),
                hostname: None,
                network: crate::Network::Private,
                hosts: Vec::new(),
            })),
            bounds: crate::Bounds {
                cpu_max_milli: Some(crate::MIN_CPU_MILLI),
                memory_max: Some(crate::MIN_MEMORY),
                ..crate::Bounds::default()
            },
            owner: None,
            devices: vec![id],
            allocation: Some(cost().allocate(CapacityResources::default()).unwrap()),
            route: Some(RouteReceipt {
                key: offered.key,
                backing: None,
            }),
            band: 0,
        };
        c.validate_route(&spec).unwrap();
        let mut forged = cost();
        forged.id = [9; 32];
        spec.allocation = Some(forged.allocate(CapacityResources::default()).unwrap());
        assert!(c.validate_route(&spec).is_err());
        spec.allocation = Some(cost().allocate(CapacityResources::default()).unwrap());
        spec.route.as_mut().unwrap().key.execution = Execution::Native;
        assert!(c.validate_route(&spec).is_err());
        spec.route = Some(RouteReceipt {
            key: offered.key,
            backing: None,
        });
        c.device_routes[0] = 0;
        assert!(c.validate_route(&spec).is_err());
        c.device_routes[0] = 1;
        let backing = BackingOffer {
            guest: Default::default(),
            key: BackingKey {
                pool: [2; 32],
                generation: [3; 16],
            },
            cost: cost(),
            phase: BackingPhase::Granted,
            devices: vec![id],
        };
        c.backings.push(backing.try_clone().unwrap());
        c.routes[0].backing = Some(backing.key);
        spec.route.as_mut().unwrap().backing = Some(backing.key);
        assert!(
            matches!(c.validate_route(&spec), Err(Refusal::Backing { key, cause:LifecycleCause::Pending }) if key == backing.key)
        );
        c.backings[0].phase = BackingPhase::Live;
        c.validate_route(&spec).unwrap();
        c.backings[0].cost.id = [8; 32];
        assert!(
            c.check_routes().is_err(),
            "live unrelated cost cannot grant an observer route"
        );
        c.backings[0].cost = cost();
        c.backings[0].devices[0].key = 8;
        assert!(
            c.validate_route(&spec).is_err(),
            "free metadata cannot replace the physical grant"
        );
        c.backings[0].devices = vec![id, crate::DeviceId { key: 8, ..id }];
        c.validate_route(&spec).unwrap();
        assert_eq!(
            spec.devices,
            vec![id],
            "a source's larger grant must not enlarge the exact Box selection"
        );
    }
    /// AC-T7.4.1: current and historical incarnations use the complete existing key/backing tuple.
    #[test]
    fn qualified_route_history_and_current_selection_do_not_alias() {
        let parent = BackingKey {
            pool: [2; 32],
            generation: [3; 16],
        };
        let current = BackingKey {
            pool: [4; 32],
            generation: [5; 16],
        };
        let id = crate::DeviceId {
            kind: crate::DeviceKind::Gpu,
            key: 8,
        };
        let historical = RouteOffer {
            key: key(
                ImageOs::Linux,
                Execution::ShardsDedicated,
                GpuEligibility::Required,
            ),
            cost: Some(cost()),
            backing: Some(parent),
        };
        let fresh = RouteOffer {
            backing: Some(current),
            ..historical
        };
        let mut c = capacity();
        c.routes = vec![historical, fresh];
        c.backings = vec![
            BackingOffer {
                guest: Default::default(),
                key: parent,
                cost: cost(),
                phase: BackingPhase::Released,
                devices: vec![crate::DeviceId { key: 7, ..id }, id],
            },
            BackingOffer {
                guest: Default::default(),
                key: current,
                cost: cost(),
                phase: BackingPhase::Live,
                devices: vec![id],
            },
        ];
        c.devices.push(crate::DeviceInfo {
            id,
            model: "current actual fixture GPU".into(),
            size: 10,
            holder: Some(crate::DeviceHolder::Backing(current)),
        });
        c.device_routes = vec![2];
        c.check().unwrap();
        let mut spec = crate::BoxSpec {
            id: crate::BoxId::new("qualified-current").unwrap(),
            class: crate::ClassSpec::Oci(Box::new(crate::OciSpec {
                rootfs: crate::Rootfs::Dir(std::env::current_dir().unwrap()),
                init: crate::Program::of(&["/bin/true"]),
                mounts: Vec::new(),
                hostname: None,
                network: crate::Network::Private,
                hosts: Vec::new(),
            })),
            bounds: crate::Bounds {
                cpu_max_milli: Some(crate::MIN_CPU_MILLI),
                memory_max: Some(crate::MIN_MEMORY),
                ..crate::Bounds::default()
            },
            owner: None,
            devices: vec![id],
            allocation: Some(cost().allocate(CapacityResources::default()).unwrap()),
            route: Some(RouteReceipt {
                key: fresh.key,
                backing: fresh.backing,
            }),
            band: 0,
        };
        assert_eq!(c.validate_route(&spec).unwrap(), fresh);
        spec.route.as_mut().unwrap().backing = Some(parent);
        assert!(
            matches!(c.validate_route(&spec), Err(Refusal::Backing { key, cause: LifecycleCause::Pending }) if key == parent)
        );
        spec.route.as_mut().unwrap().backing = Some(BackingKey {
            generation: [9; 16],
            ..current
        });
        assert!(
            c.validate_route(&spec).is_err(),
            "no RouteKey-only fallback"
        );
        c.routes.push(fresh);
        assert!(
            c.check().is_err(),
            "duplicate exact qualified tuple refuses"
        );
        c.routes.pop();
        c.backings[0].phase = BackingPhase::Live;
        assert!(
            c.check().is_err(),
            "history cannot become a competing fresh predicate"
        );
        c.backings[0].phase = BackingPhase::Released;
        c.routes[1].cost.as_mut().unwrap().id = [9; 32];
        assert!(
            c.check().is_err(),
            "qualified cost still matches its actual backing"
        );
    }

    /// AC-T7.4.1: cold provisioning reserves identity refs without inventing native metadata.
    #[test]
    fn cold_backing_claims_are_explicit_unique_and_not_measured_fresh_devices() {
        let id = crate::DeviceId {
            kind: crate::DeviceKind::Gpu,
            key: 7,
        };
        let mut c = capacity();
        c.routes.push(RouteOffer {
            key: RouteKey {
                class: crate::Class::Firecracker,
                image: platform(ImageOs::Linux),
                execution: Execution::Firecracker,
                gpu: GpuEligibility::Required,
            },
            cost: Some(cost()),
            backing: Some(BackingKey {
                pool: [2; 32],
                generation: [3; 16],
            }),
        });
        let mut backing = BackingOffer {
            guest: Default::default(),
            key: BackingKey {
                pool: [2; 32],
                generation: [3; 16],
            },
            cost: cost(),
            phase: BackingPhase::Prepared,
            devices: vec![id],
        };
        c.backings.push(backing.try_clone().unwrap());
        c.check().unwrap();
        assert!(
            c.devices.is_empty(),
            "provisioning must not fabricate DeviceInfo/model/size/load"
        );
        let copied = c.try_clone().unwrap();
        assert_eq!(copied, c);
        let grant = BackingGrant {
            key: backing.key,
            allocation: backing.allocation().unwrap(),
            devices: vec![id],
        };
        grant.check().unwrap();
        assert_eq!(grant.devices, backing.devices);
        backing.devices.push(id);
        assert!(backing.allocation().is_err());
        assert!(backing.try_clone().is_err());
    }
}

#[cfg(test)]
mod subset_selection_tests {
    use super::*;

    /// AC-T7.4.1: a source observer's full grant never enlarges or replaces the Box's exact IDs.
    #[test]
    fn exact_subset_membership_keeps_full_provisioning_authority_separate() {
        let id = |key| crate::DeviceId {
            kind: crate::DeviceKind::Gpu,
            key,
        };
        let full = [id(1), id(2)];
        BackingOffer::check_selection_parts(&full, &full).unwrap();
        BackingOffer::check_selection_parts(&full, &[id(2)]).unwrap();
        for selected in [vec![], vec![id(3)], vec![id(2), id(1)], vec![id(1), id(1)]] {
            assert!(BackingOffer::check_selection_parts(&full, &selected).is_err());
        }
        assert!(BackingOffer::check_selection_parts(&[id(2), id(1)], &[id(1)]).is_err());
        assert!(BackingOffer::check_selection_parts(&[], &[id(1)]).is_err());
        assert_eq!(full, [id(1), id(2)]);
    }
}

#[cfg(test)]
mod factual_backing_receipt_tests {
    use super::*;
    use crate::{Class, DeviceId, DeviceKind};

    /// AC-T7.4.1: a factual legacy receipt is validated without widening initialized offers.
    #[test]
    fn factual_any_receipt_keeps_profile_subset_and_fresh_offer_rules_distinct() {
        let floor = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: crate::MIN_MEMORY,
            disk_bytes: 0,
        };
        let profile = RuntimeCost {
            id: [1; 32],
            floor,
            ceiling: floor,
            quantum: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: CapacityResources::default(),
            overhead: CapacityResources::default(),
        };
        let key = BackingKey {
            pool: [2; 32],
            generation: [3; 16],
        };
        let p = [7, 8].map(|key| DeviceId {
            kind: DeviceKind::Gpu,
            key,
        });
        let receipt = RouteReceipt {
            key: RouteKey {
                class: Class::Shards,
                image: ImagePlatform {
                    os: ImageOs::Linux,
                    arch: ImageArch::Amd64,
                },
                execution: Execution::ShardsDedicated,
                gpu: GpuEligibility::Any,
            },
            backing: Some(key),
        };
        receipt
            .check_backing_parts(Some(profile), key, profile, &p, &p[..1])
            .unwrap();
        let offer = RouteOffer {
            key: receipt.key,
            cost: Some(profile),
            backing: Some(key),
        };
        assert!(offer.check_backing_parts(key, profile, &p).is_err());
        RouteOffer {
            key: RouteKey {
                gpu: GpuEligibility::Required,
                ..offer.key
            },
            ..offer
        }
        .check_backing_parts(key, profile, &p)
        .unwrap();
        assert!(
            receipt
                .check_backing_parts(Some(profile), key, profile, &p, &[])
                .is_err()
        );
        assert!(
            receipt
                .check_backing_parts(
                    Some(profile),
                    key,
                    profile,
                    &p,
                    &[DeviceId {
                        kind: DeviceKind::Gpu,
                        key: 9
                    }]
                )
                .is_err()
        );
        assert!(
            receipt
                .check_backing_parts(
                    Some(profile),
                    key,
                    RuntimeCost {
                        id: [4; 32],
                        ..profile
                    },
                    &p,
                    &p[..1]
                )
                .is_err()
        );
        let shared = RouteReceipt {
            key: RouteKey {
                class: Class::Oci,
                execution: Execution::ShardsShared,
                gpu: GpuEligibility::None,
                ..receipt.key
            },
            backing: Some(key),
        };
        shared
            .check_backing_parts(None, key, profile, &[], &[])
            .unwrap();
        assert!(
            shared
                .check_backing_parts(None, key, profile, &[], &p[..1])
                .is_err()
        );
    }
}
