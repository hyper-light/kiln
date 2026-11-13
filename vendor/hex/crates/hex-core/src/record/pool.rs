//! Real admitted VM backings share the native record owner; they are never synthetic workloads.

use hex_api::{
    Allocation, BackingGrant, BackingKey, BackingOffer, BackingPhase, BoxId, Class, DeviceId,
    Execution, Exit, LifecycleCause, Refusal, RuntimeCost,
};
use serde::{Deserialize, Serialize};

use super::{BoxRecord, MAX_RECORD_BYTES, NativeScope, Phase, ProcId, RECORD_VERSION};

/// One canonical native owner. All generations of one pool serialize on its configuration identity.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OwnerKey {
    Box(BoxId),
    Pool([u8; 32]),
}

/// A stack view of the same owner, without copying a box id or allocating a wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OwnerRef<'a> {
    Box(&'a BoxId),
    Pool(&'a [u8; 32]),
}

impl OwnerKey {
    pub fn as_ref(&self) -> OwnerRef<'_> {
        match self {
            Self::Box(id) => OwnerRef::Box(id),
            Self::Pool(pool) => OwnerRef::Pool(pool),
        }
    }
    pub fn try_clone(&self) -> Result<Self, Refusal> {
        self.as_ref().try_owned()
    }
}
impl OwnerRef<'_> {
    pub fn try_owned(self) -> Result<OwnerKey, Refusal> {
        Ok(match self {
            Self::Box(id) => OwnerKey::Box(id.try_clone()?),
            Self::Pool(pool) => OwnerKey::Pool(*pool),
        })
    }
}

/// A native VM backing configuration, its pre-grant incarnation, and the exact admitted grant.
/// This is the sole Store claim, not a second resource ledger. `generation` is the Store's fresh
/// NativeScope, persisted in Prepared before the Index may grant its allocation.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolRecord {
    pub version: u32,
    pub key: BackingKey,
    pub execution: Execution,
    pub cost: RuntimeCost,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocation: Option<Allocation>,
    pub devices: Vec<DeviceId>,
    pub phase: BackingPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<ProcId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
}

fn refused(key: BackingKey, cause: LifecycleCause) -> Refusal {
    Refusal::Backing { key, cause }
}

impl PoolRecord {
    pub fn scope(&self) -> NativeScope {
        NativeScope::from_bytes(self.key.generation)
    }

    pub fn check(&self) -> Result<(), Refusal> {
        let bad = || refused(self.key, LifecycleCause::Uncertain);
        if self.version != RECORD_VERSION
            || self.execution == Execution::Native
            || (self.execution == Execution::ShardsShared && !self.devices.is_empty())
            || self.cost.floor != self.cost.ceiling
            || self.devices.len() > hex_api::MAX_DEVICES
        {
            return Err(bad());
        }
        self.cost.check()?;
        for device in &self.devices {
            device.check()?;
        }
        if self
            .devices
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left >= right))
        {
            return Err(bad());
        }
        if let Some(allocation) = self.allocation {
            allocation.check(hex_api::CapacityResources::default())?;
            if allocation != self.cost.allocate(hex_api::CapacityResources::default())? {
                return Err(bad());
            }
        }
        match self.phase {
            BackingPhase::Prepared
                if self.allocation.is_some() || self.init.is_some() || self.exit.is_some() =>
            {
                return Err(bad());
            }
            BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining
                if self.allocation.is_none() =>
            {
                return Err(bad());
            }
            BackingPhase::Live if self.init.is_none() || self.exit.is_some() => return Err(bad()),
            BackingPhase::Released if self.allocation.is_some() && self.exit.is_none() => {
                return Err(bad());
            }
            _ => {}
        }
        if self
            .init
            .is_some_and(|init| init.pid == 0 || init.start_ticks == 0)
            || (self.allocation.is_none() && (self.init.is_some() || self.exit.is_some()))
        {
            return Err(bad());
        }
        if self.exit.is_some()
            && !matches!(self.phase, BackingPhase::Draining | BackingPhase::Released)
        {
            return Err(bad());
        }
        Ok(())
    }

    pub fn offer(&self) -> Result<BackingOffer, Refusal> {
        self.check()?;
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(self.devices.len())
            .map_err(|_| Refusal::Exhausted {
                what: "native backing physical claims",
                bound: hex_api::MAX_DEVICES,
            })?;
        devices.extend_from_slice(&self.devices);
        Ok(BackingOffer {
            key: self.key,
            cost: self.cost,
            phase: self.phase,
            devices,
        })
    }

    /// A grant cannot create a generation, select another physical device, or replace its cost.
    pub fn accept_grant(&mut self, grant: &BackingGrant) -> Result<(), Refusal> {
        self.check()?;
        grant.check()?;
        if self.key != grant.key
            || self.devices != grant.devices
            || grant.allocation != self.cost.allocate(hex_api::CapacityResources::default())?
        {
            return Err(refused(self.key, LifecycleCause::Uncertain));
        }
        match (self.phase, self.allocation) {
            (BackingPhase::Prepared, None) => {
                self.allocation = Some(grant.allocation);
                self.phase = BackingPhase::Granted;
                Ok(())
            }
            (BackingPhase::Granted | BackingPhase::Live, Some(known))
                if known == grant.allocation =>
            {
                Ok(())
            }
            _ => Err(refused(self.key, LifecycleCause::Uncertain)),
        }
    }

    /// Check before writing. Cleanup authority and observed terminal facts cannot be replaced by
    /// a stale same-pool generation, an equal-cost new grant, or a rollback out of Draining.
    pub fn accepts(&self, next: &Self) -> Result<(), Refusal> {
        self.check()?;
        next.check()?;
        let bad = || refused(self.key, LifecycleCause::Uncertain);
        if self.key != next.key
            || self.execution != next.execution
            || self.cost != next.cost
            || self.devices != next.devices
            || self.version != next.version
        {
            return Err(bad());
        }
        let phase = matches!(
            (self.phase, next.phase),
            (
                BackingPhase::Prepared,
                BackingPhase::Prepared | BackingPhase::Granted | BackingPhase::Released,
            ) | (
                BackingPhase::Granted,
                BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining,
            ) | (
                BackingPhase::Live,
                BackingPhase::Live | BackingPhase::Draining
            ) | (
                BackingPhase::Draining,
                BackingPhase::Draining | BackingPhase::Released
            ) | (BackingPhase::Released, BackingPhase::Released)
        );
        if !phase {
            return Err(bad());
        }
        match (self.allocation, next.allocation) {
            (None, Some(_))
                if self.phase == BackingPhase::Prepared && next.phase == BackingPhase::Granted => {}
            (known, proposed) if known == proposed => {}
            _ => return Err(bad()),
        }
        match (self.init, next.init) {
            (None, Some(_))
                if self.phase == BackingPhase::Granted && next.phase == BackingPhase::Granted => {}
            (known, proposed) if known == proposed => {}
            _ => return Err(bad()),
        }
        if self.exit.is_some() && self.exit != next.exit {
            return Err(bad());
        }
        if next.exit.is_some()
            && !matches!(next.phase, BackingPhase::Draining | BackingPhase::Released)
        {
            return Err(bad());
        }
        Ok(())
    }

    pub fn native_commitment(&self) -> Result<[u8; 32], Refusal> {
        self.check()?;
        let allocation = self
            .allocation
            .ok_or_else(|| refused(self.key, LifecycleCause::Pending))?;
        super::commitment(&(
            self.key,
            self.execution,
            self.cost,
            allocation,
            &self.devices,
        ))
    }

    /// Count first, reserve once, then serialize these immutable scalar/array records into the
    /// exact capacity. No BoxId/String clone or grow-on-write buffer is introduced in this path.
    pub fn encode(&self) -> Result<Vec<u8>, Refusal> {
        self.check()?;
        struct Length(usize);
        impl std::io::Write for Length {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len())
                    .ok_or(std::io::ErrorKind::InvalidData)?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut length = Length(0);
        serde_json::to_writer(&mut length, self)
            .map_err(|_| refused(self.key, LifecycleCause::Uncertain))?;
        let count = length
            .0
            .checked_add(1)
            .filter(|count| *count <= MAX_RECORD_BYTES)
            .ok_or(Refusal::Exhausted {
                what: "native backing record bytes",
                bound: MAX_RECORD_BYTES,
            })?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(count)
            .map_err(|_| Refusal::Exhausted {
                what: "native backing record bytes",
                bound: count,
            })?;
        serde_json::to_writer(&mut bytes, self)
            .map_err(|_| refused(self.key, LifecycleCause::Uncertain))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Refusal> {
        if bytes.len() > MAX_RECORD_BYTES || !bytes.ends_with(b"\n") {
            return Err(Refusal::Exhausted {
                what: "native backing record bytes",
                bound: MAX_RECORD_BYTES,
            });
        }
        let record: Self = serde_json::from_slice(bytes)
            .map_err(|_| super::scope_refusal("invalid native backing record"))?;
        record.check()?;
        Ok(record)
    }
}

/// A read-only projection of the actual persisted phase, never another lifecycle authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeStage {
    Prepared,
    Creating,
    Ready,
    Deleting,
    Released,
}

#[derive(Debug, Clone, Copy)]
pub enum NativeRecord<'a> {
    Box(&'a BoxRecord),
    Pool(&'a PoolRecord),
}

impl<'a> NativeRecord<'a> {
    pub fn owner(self) -> OwnerRef<'a> {
        match self {
            Self::Box(record) => OwnerRef::Box(&record.spec.id),
            Self::Pool(record) => OwnerRef::Pool(&record.key.pool),
        }
    }
    pub fn scope(self) -> Option<NativeScope> {
        match self {
            Self::Box(record) => record.native_scope,
            Self::Pool(record) => Some(record.scope()),
        }
    }
    pub fn allocation(self) -> Option<Allocation> {
        match self {
            Self::Box(record) => record.spec.allocation,
            Self::Pool(record) => record.allocation,
        }
    }
    pub fn devices(self) -> &'a [DeviceId] {
        match self {
            Self::Box(record) => &record.spec.devices,
            Self::Pool(record) => &record.devices,
        }
    }
    pub fn class(self) -> Result<Class, Refusal> {
        match self {
            Self::Box(record) => record.spec.backend_class(),
            Self::Pool(record) => {
                record.check()?;
                Ok(match record.execution {
                    Execution::ShardsShared | Execution::ShardsDedicated => Class::Shards,
                    Execution::Firecracker => Class::Firecracker,
                    Execution::Native => {
                        return Err(refused(record.key, LifecycleCause::Uncertain));
                    }
                })
            }
        }
    }
    pub fn stage(self) -> NativeStage {
        match self {
            Self::Box(record) => match record.phase {
                Phase::Creating => NativeStage::Creating,
                Phase::Ready => NativeStage::Ready,
                Phase::Deleting => NativeStage::Deleting,
            },
            Self::Pool(record) => match record.phase {
                BackingPhase::Prepared => NativeStage::Prepared,
                BackingPhase::Granted => NativeStage::Creating,
                BackingPhase::Live => NativeStage::Ready,
                BackingPhase::Draining => NativeStage::Deleting,
                BackingPhase::Released => NativeStage::Released,
            },
        }
    }
    pub fn init(self) -> Option<ProcId> {
        match self {
            Self::Box(record) => record.init,
            Self::Pool(record) => record.init,
        }
    }
    pub fn commitment(self) -> Result<[u8; 32], Refusal> {
        match self {
            Self::Box(record) => record.native_commitment(),
            Self::Pool(record) => record.native_commitment(),
        }
    }
    pub fn refusal(self, cause: LifecycleCause) -> Refusal {
        match self {
            Self::Box(record) => match record.spec.id.try_clone() {
                Ok(id) => Refusal::Lifecycle { id, cause },
                Err(error) => error,
            },
            Self::Pool(record) => refused(record.key, cause),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared() -> PoolRecord {
        let fixed = hex_api::CapacityResources {
            cpu_milli: 1000,
            mem_bytes: hex_api::MIN_MEMORY,
            disk_bytes: 0,
        };
        PoolRecord {
            version: RECORD_VERSION,
            key: BackingKey {
                pool: [1; 32],
                generation: [2; 16],
            },
            execution: Execution::ShardsShared,
            cost: RuntimeCost {
                id: [3; 32],
                floor: fixed,
                ceiling: fixed,
                quantum: hex_api::CapacityResources {
                    cpu_milli: 1,
                    mem_bytes: 1,
                    disk_bytes: 1,
                },
                guest_overhead: hex_api::CapacityResources::default(),
                overhead: hex_api::CapacityResources::default(),
            },
            allocation: None,
            devices: Vec::new(),
            phase: BackingPhase::Prepared,
            init: None,
            exit: None,
        }
    }
    fn grant(record: &PoolRecord) -> BackingGrant {
        BackingGrant {
            key: record.key,
            allocation: record
                .cost
                .allocate(hex_api::CapacityResources::default())
                .unwrap(),
            devices: Vec::new(),
        }
    }

    /// AC-T7.4.1: decoded hostile rows obey the same topology/phase rules as lifecycle writes.
    #[test]
    fn decoded_granted_exit_and_shared_physical_claims_are_refused() {
        let mut record = prepared();
        record.accept_grant(&grant(&record)).unwrap();
        record.exit = Some(Exit::Code(0));
        let mut hostile = serde_json::to_vec(&record).unwrap();
        hostile.push(b'\n');
        assert!(PoolRecord::decode(&hostile).is_err());
        assert!(record.encode().is_err());
        record.exit = None;
        record.devices.push(DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 7,
        });
        let mut hostile = serde_json::to_vec(&record).unwrap();
        hostile.push(b'\n');
        assert!(PoolRecord::decode(&hostile).is_err());
        assert!(record.encode().is_err());
        record.execution = Execution::ShardsDedicated;
        let encoded = record.encode().unwrap();
        assert_eq!(PoolRecord::decode(&encoded).unwrap(), record);
    }

    /// AC-T7.4.1: the grant belongs to a prepared native generation, never a caller-generated replacement.
    #[test]
    fn a_grant_cannot_change_generation_or_the_fixed_profile() {
        let mut record = prepared();
        let before = record.encode().unwrap();
        let mut wrong = grant(&record);
        wrong.key.generation = [4; 16];
        assert!(record.accept_grant(&wrong).is_err());
        assert_eq!(record.encode().unwrap(), before);
        let correct = grant(&record);
        record.accept_grant(&correct).unwrap();
        assert_eq!(record.phase, BackingPhase::Granted);
        assert_eq!(record.allocation, Some(correct.allocation));
        record.accept_grant(&correct).unwrap();
    }

    /// AC-T7.4.1: a late native callback cannot replace init/terminal facts or revive a draining pool.
    #[test]
    fn lifecycle_keeps_native_identity_and_terminal_facts() {
        let mut record = prepared();
        record.accept_grant(&grant(&record)).unwrap();
        let mut published = PoolRecord::decode(&record.encode().unwrap()).unwrap();
        published.init = Some(ProcId {
            pid: 7,
            start_ticks: 11,
        });
        record.accepts(&published).unwrap();
        record = published;
        let mut wrong = PoolRecord::decode(&record.encode().unwrap()).unwrap();
        wrong.init = Some(ProcId {
            pid: 7,
            start_ticks: 12,
        });
        assert!(record.accepts(&wrong).is_err());
        let mut draining = PoolRecord::decode(&record.encode().unwrap()).unwrap();
        draining.phase = BackingPhase::Draining;
        draining.exit = Some(Exit::Code(7));
        record.accepts(&draining).unwrap();
        assert!(draining.accepts(&record).is_err());
        let mut conflict = PoolRecord::decode(&draining.encode().unwrap()).unwrap();
        conflict.exit = Some(Exit::Code(8));
        assert!(draining.accepts(&conflict).is_err());
        conflict.exit = draining.exit;
        conflict.phase = BackingPhase::Released;
        draining.accepts(&conflict).unwrap();
    }

    /// AC-T7.4.1: a backing reaches native effects only after the same admitted allocation exists.
    #[test]
    fn native_projection_and_commitment_keep_the_actual_owner() {
        let mut record = prepared();
        let native = NativeRecord::Pool(&record);
        assert_eq!(native.owner(), OwnerRef::Pool(&record.key.pool));
        assert_eq!(native.stage(), NativeStage::Prepared);
        assert_eq!(native.class().unwrap(), Class::Shards);
        assert_eq!(native.allocation(), None);
        assert!(native.commitment().is_err());
        record.accept_grant(&grant(&record)).unwrap();
        let before = record.native_commitment().unwrap();
        record.init = Some(ProcId {
            pid: 7,
            start_ticks: 11,
        });
        record.phase = BackingPhase::Live;
        assert_eq!(record.native_commitment().unwrap(), before);
        let decoded = PoolRecord::decode(&record.encode().unwrap()).unwrap();
        assert_eq!(decoded, record);
        let mut old = PoolRecord::decode(&record.encode().unwrap()).unwrap();
        old.key.generation = [9; 16];
        assert_ne!(old.native_commitment().unwrap(), before);
        assert!(old.accepts(&record).is_err());
    }
}
