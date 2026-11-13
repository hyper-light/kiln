//! The initialized native owner's existing records and method/device turns. A weak
//! reference connects this adapter to Node or Recorded without another record owner.

use std::sync::Weak;

use hex_core::record::{BoxRecord, Phase};

use super::*;
use crate::devices::{self, Books};

/// A call's one read of its box's record ([`GuestRuntime::routed`]): a Shards image VM's, which its own class
/// answers, or any other's, or none, which the call carries on to its record gate.
pub(super) enum Routed {
    Image(BoxRecord),
    Other(Option<BoxRecord>),
}

pub(super) fn uncertain(id: &BoxId) -> Result<Refusal, Refusal> {
    Ok(Refusal::Lifecycle {
        id: id.try_clone()?,
        cause: hex_api::LifecycleCause::Uncertain,
    })
}

impl GuestRuntime {
    /// The same initialized immutable catalog for the canonical Box/Pool join;
    /// cloning this triomphe owner allocates no new catalog or holder cache.
    pub fn device_catalog(&self) -> Option<crate::devices::SharedCatalog> {
        self.gpu_catalog.clone()
    }

    /// Called once after the existing Arc<Node> or Arc<Recorded<WithGuest<P>>>
    /// exists. Coercing that Arc to Books and downgrading allocates no new owner.
    pub fn bind_books(&self, books: Weak<dyn Books>) -> Result<(), Refusal> {
        if books.upgrade().is_none() {
            return Err(Refusal::Broken("guest record authority is gone".into()));
        }
        if let Some(bound) = self.books.get() {
            return if Weak::ptr_eq(bound, &books) {
                Ok(())
            } else {
                Err(Refusal::Broken(
                    "guest record authority cannot be rebound".into(),
                ))
            };
        }
        self.books.set(books).or_else(|books| {
            if self
                .books
                .get()
                .is_some_and(|bound| Weak::ptr_eq(bound, &books))
            {
                Ok(())
            } else {
                Err(Refusal::Broken(
                    "guest record authority changed during initialization".into(),
                ))
            }
        })
    }

    pub fn uses_books(&self) -> bool {
        self.books.get().is_some()
    }

    /// Ordinary CPU OCI retains Recorded's existing lifecycle. Explicit Shards,
    /// including CPU-only boxes, and selected guest OCI use this same Books owner.
    pub fn owns_record(&self, spec: &BoxSpec) -> bool {
        self.uses_books()
            && match spec.route_execution() {
                Some(hex_api::Execution::ShardsDedicated) => true,
                Some(_) => false,
                // A Shards image VM is this owner's whole, its record included: a refused
                // create leaves no record of another owner's behind.
                None => {
                    matches!(spec.class, ClassSpec::Shards(_) | ClassSpec::ShardsImage(_))
                        || !spec.devices.is_empty()
                }
            }
    }

    pub fn with_runtime_cost(mut self, cost: hex_api::RuntimeCost) -> Result<Self, Refusal> {
        cost.check()?;
        if self
            .native_images
            .is_some_and(|images| images.evidence() != cost.id)
        {
            return Err(super::route::invalid(
                "native image evidence cannot change with the runtime cost",
            ));
        }
        // The explicit units and measured kernel/relay allowance must fit that
        // same configured envelope. No overhead estimate is invented here.
        self.config.checked_runtime_allocation(cost)?;
        self.runtime_cost = Some(cost);
        Ok(self)
    }

    /// Factual fixed-VM allocation from the validated configured profile. An
    /// initialized offer may describe it; only the canonical Index grant permits
    /// boot. Aggregate shared payload must pass this allocation's check(sumU).
    pub fn configured_allocation(&self) -> Result<hex_api::Allocation, Refusal> {
        self.runtime_cost
            .ok_or_else(|| Refusal::Invalid {
                what: "Shards runtime cost",
                why: "measured backend cost has not been initialized".into(),
            })?
            .allocate(hex_api::CapacityResources::default())
    }

    /// Join declarations to the SAME original measured cost before publication.
    /// This stores values only; native code still verifies actual open images.
    pub fn with_native_images(mut self, images: shards_api::NativeImages) -> Result<Self, Refusal> {
        if self
            .runtime_cost
            .is_none_or(|cost| cost.id != images.evidence())
            || self.native_images.is_some_and(|known| known != images)
        {
            return Err(super::route::invalid(
                "native images do not belong to the initialized cost evidence",
            ));
        }
        self.native_images = Some(images);
        Ok(self)
    }

    pub(super) fn check_allocation(&self, spec: &BoxSpec) -> Result<(), Refusal> {
        let cost = self.runtime_cost.ok_or_else(|| Refusal::Invalid {
            what: "Shards runtime cost",
            why: "measured backend cost has not been initialized".into(),
        })?;
        let allocation = spec.allocation.ok_or_else(|| Refusal::Invalid {
            what: "Shards allocation",
            why: "the scheduler must admit the configured VM and measured overhead before boot"
                .into(),
        })?;
        if allocation.cost != cost {
            return Err(Refusal::Invalid {
                what: "Shards allocation",
                why: "admission cost identity or measured profile changed".into(),
            });
        }
        let guest = allocation.guest()?;
        let configured = self.dedicated_resources()?;
        if guest.cpu_milli != configured.cpu_milli || guest.mem_bytes != configured.mem_bytes {
            return Err(Refusal::Invalid {
                what: "Shards allocation",
                why: "admitted guest envelope disagrees with the configured VM".into(),
            });
        }
        spec.check()
    }

    pub(super) fn authority(&self) -> Result<Arc<dyn Books>, Refusal> {
        self.books
            .get()
            .and_then(Weak::upgrade)
            .ok_or_else(|| Refusal::Broken("guest has no live native record authority".into()))
    }

    /// Read the canonical persisted route before touching a native backend. This
    /// snapshot grants no method ownership; each operation rereads under its turn.
    pub fn recorded_box(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
        if !self.uses_books() {
            return Ok(None);
        }
        self.authority()?
            .records()?
            .remove(id)
            .map(|record| {
                record.map_err(|_| Refusal::Broken("guest box record is unreadable".into()))
            })
            .transpose()
    }

    /// Box `id`'s record, read once for the call that asks ([`GuestRuntime::recorded_box`], one scan of the node's
    /// records), and routed by its class, so no call scans them twice: each operation still rereads it under its turn.
    pub(super) fn routed(&self, id: &BoxId) -> Result<Routed, Refusal> {
        Ok(match self.recorded_box(id)? {
            Some(record) if matches!(record.spec.class, ClassSpec::ShardsImage(_)) => {
                Routed::Image(record)
            }
            snapshot => Routed::Other(snapshot),
        })
    }

    pub(super) fn turn_keys(
        &self,
        devices: &[hex_api::DeviceId],
    ) -> Result<Vec<hex_api::DeviceId>, Refusal> {
        if devices.is_empty() {
            return Ok(Vec::new());
        }
        self.gpu_catalog
            .as_ref()
            .ok_or_else(|| Refusal::Broken("guest has no initialized device catalog".into()))?
            .conflict_ids(devices)
    }

    pub(super) fn create_recorded(&self, spec: BoxSpec) -> Result<BoxInfo, Refusal> {
        self.check_allocation(&spec)?;
        let budget = managed::Budget::new(self.first_dial_bound()?)?;
        let books = self.books.get().ok_or_else(|| {
            Refusal::Broken("a dedicated guest needs the native record authority".into())
        })?;
        let id = spec.id.try_clone()?;
        let source = spec
            .route
            .and_then(|route| route.backing)
            .map(|key| {
                self.authority()?
                    .pool_records()?
                    .into_iter()
                    .find(|record| record.key == key)
                    .ok_or(uncertain(&id)?)
            })
            .transpose()?;
        let conflicts = source
            .as_ref()
            .map_or(spec.devices.as_slice(), |source| source.devices.as_slice());
        let keys = self.turn_keys(conflicts)?;
        let (spec, expected_scope) = if let Some(source) = &source {
            let key = source.key;
            spec.route.ok_or(uncertain(&id)?)?.check_backing_parts(
                spec.allocation.map(|allocation| allocation.cost),
                source.key,
                source.cost,
                &source.devices,
                &spec.devices,
            )?;
            let commitment = source.native_commitment()?;
            let pending =
                devices::with_handoff_transaction(books, &id, &key, &keys, |transaction| {
                    let current = transaction.read_pool(&key)?.ok_or(uncertain(&id)?)?;
                    self.preflight_backing(&current)?;
                    if current.native_commitment()? != commitment
                        || current.phase != hex_api::BackingPhase::Live
                    {
                        return Err(uncertain(&id)?);
                    }
                    if transaction.read(&id)?.is_some() {
                        return Err(Refusal::Exists(id.try_clone()?));
                    }
                    self.prepare_selected_create(&id)?;
                    let records = transaction.records()?;
                    if records.len() >= crate::MAX_BOXES {
                        return Err(Refusal::Full(crate::MAX_BOXES));
                    }
                    let pools = transaction.pool_records()?;
                    self.gpu_catalog
                        .as_ref()
                        .ok_or(uncertain(&id)?)?
                        .prepare(&spec, &records, &pools)?;
                    budget.remaining()?;
                    transaction.put(&BoxRecord::new(spec, Phase::Creating, Vec::new()))?;
                    transaction.read(&id)?.ok_or(uncertain(&id)?)
                })?;
            // Release the Box/device turns BEFORE entering the SAME owner's
            // Pool transaction. Both canonical claims remain during handoff.
            let released = self.stop_gpu_for_handoff(key, &pending, budget)?;
            if released.key != key
                || released.phase != hex_api::BackingPhase::Released
                || released.devices != source.devices
                || released.cost != source.cost
            {
                return Err(uncertain(&id)?);
            }
            let scope = pending.native_scope.ok_or(uncertain(&id)?)?;
            (pending.spec, Some(scope))
        } else {
            (spec, None)
        };
        budget.remaining()?;
        let complete = |transaction: &dyn devices::Transaction| {
            if expected_scope.is_some() {
                let source = source.as_ref().ok_or(uncertain(&id)?)?;
                let current_source = transaction.read_pool(&source.key)?.ok_or(uncertain(&id)?)?;
                self.handoff_source_released(&current_source)?;
                if current_source.phase != hex_api::BackingPhase::Released
                    || current_source.exit.is_none()
                    || current_source.native_commitment()? != source.native_commitment()?
                {
                    return Err(uncertain(&id)?);
                }
                let current = transaction.read(&id)?.ok_or(uncertain(&id)?)?;
                if current.spec != spec
                    || current.native_scope != expected_scope
                    || current.phase != Phase::Creating
                    || current.init.is_some()
                    || current.run.is_some()
                    || current.exit.is_some()
                {
                    return Err(uncertain(&id)?);
                }
            } else if transaction.read(&id)?.is_some() {
                return Err(Refusal::Exists(id.try_clone()?));
            }
            if expected_scope.is_none() {
                self.prepare_selected_create(&id)?;
                let records = transaction.records()?;
                let pools = transaction.pool_records()?;
                if records.len() >= crate::MAX_BOXES {
                    return Err(Refusal::Full(crate::MAX_BOXES));
                }
                if !spec.devices.is_empty() {
                    let catalog = self.gpu_catalog.as_ref().ok_or_else(|| {
                        Refusal::Broken("guest has no initialized device catalog".into())
                    })?;
                    let _selection = catalog.prepare(&spec, &records, &pools)?;
                }
                // Original workload/charge/route remains canonical. The inner
                // OCI rewrite cannot mint or replace this receipt.
                budget.remaining()?;
                transaction.put(&BoxRecord::new(spec, Phase::Creating, Vec::new()))?;
            }
            let record = transaction.read(&id)?.ok_or(uncertain(&id)?)?;
            let info = self.create_selected(record.spec, transaction, budget)?;
            let mut record = transaction.read(&id)?.ok_or(uncertain(&id)?)?;
            if record.phase != Phase::Creating
                || info.id != id
                || info.class != record.spec.class.class()
                || info.allocation != record.spec.allocation
                || info.route != record.spec.route
                || record.init.is_none()
                || record.native_scope.is_none()
            {
                return Err(uncertain(&id)?);
            }
            record.phase = Phase::Ready;
            transaction.put(&record)?;
            Ok(info)
        };
        match &source {
            Some(source) => {
                devices::with_handoff_transaction(books, &id, &source.key, &keys, complete)
            }
            None => devices::with_transaction(books, &id, &keys, complete),
        }
    }

    /// `call` on box `id` through its record's gate, `snapshot` the call's one read of the record ([`Routed::Other`]).
    pub(super) fn call_recorded<T>(
        &self,
        snapshot: Option<&BoxRecord>,
        id: &BoxId,
        ready: bool,
        call: impl FnOnce() -> Result<T, Refusal>,
    ) -> Result<T, Refusal> {
        let Some(books) = self.books.get() else {
            return call();
        };
        let snapshot = snapshot.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
        if !self.owns_record(&snapshot.spec) {
            return call();
        }
        let keys = self.turn_keys(&snapshot.spec.devices)?;
        devices::with_transaction(books, id, &keys, |transaction| {
            let record = transaction
                .read(id)?
                .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if record.spec != snapshot.spec
                || record.init != snapshot.init
                || record.native_scope != snapshot.native_scope
            {
                return Err(uncertain(id)?);
            }
            if ready && record.phase != Phase::Ready {
                return Err(uncertain(id)?);
            }
            self.validate_binding(&record.spec)?;
            self.selected_running_in(id, transaction, &record)?
                .ok_or(uncertain(id)?)?;
            call()
        })
    }

    /// Deletes box `id`, which the books own, `snapshot` the call's one read of its record ([`Routed::Other`]).
    pub(super) fn delete_recorded(&self, snapshot: &BoxRecord, id: &BoxId) -> Result<(), Refusal> {
        let books = self
            .books
            .get()
            .ok_or_else(|| Refusal::Broken("guest has no initialized record authority".into()))?;
        let source = snapshot
            .spec
            .route
            .and_then(|route| route.backing)
            .map(|key| {
                self.authority()?
                    .pool_records()?
                    .into_iter()
                    .find(|record| record.key == key)
                    .ok_or(uncertain(id)?)
            })
            .transpose()?;
        let pending = source.as_ref().is_some_and(|source| {
            matches!(
                source.phase,
                hex_api::BackingPhase::Live | hex_api::BackingPhase::Draining
            )
        });
        let conflicts = if pending {
            source.as_ref().ok_or(uncertain(id)?)?.devices.as_slice()
        } else {
            snapshot.spec.devices.as_slice()
        };
        let keys = self.turn_keys(conflicts)?;
        let delete = |transaction: &dyn devices::Transaction| {
            let mut record = transaction
                .read(id)?
                .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if record.spec != snapshot.spec
                || record.init != snapshot.init
                || record.native_scope != snapshot.native_scope
            {
                return Err(uncertain(id)?);
            }
            record.phase = Phase::Deleting;
            transaction.put(&record)?;
            if pending {
                let source = source.as_ref().ok_or(uncertain(id)?)?;
                let current = transaction.read_pool(&source.key)?.ok_or(uncertain(id)?)?;
                if matches!(
                    current.phase,
                    hex_api::BackingPhase::Live | hex_api::BackingPhase::Draining
                ) {
                    self.handoff_unsubmitted(&record, &current)?;
                    // P still owns all physical claims/charge. This reserved Box
                    // was never submitted; no native call or fake release row.
                    return transaction.remove(id);
                }
            }
            // Stop uses the immutable VM receipt, even if current bindings changed.
            // Failed/uncertain native observation leaves the sole holder in Deleting.
            self.delete_selected(id, transaction)?;
            self.finalize_selected(id, transaction)?;
            transaction.remove(id)?;
            // The positive Released receipt survives any failed native finalization
            // or Store removal; remove it only after the canonical transaction did.
            self.forget_selected(id)
        };
        if pending {
            let source = source.as_ref().ok_or(uncertain(id)?)?;
            devices::with_handoff_transaction(books, id, &source.key, &keys, delete)
        } else {
            devices::with_transaction(books, id, &keys, delete)
        }
    }

    pub(super) fn native_capacity(&self) -> Result<hex_api::Capacity, Refusal> {
        let authority = self.authority()?;
        let mut capacity = authority.capacity()?;
        self.add_offers(&mut capacity, Class::Shards)?;
        Ok(capacity)
    }

    /// Dedicated VM statistics come from the same native scope, including its
    /// helpers. No relay/guest must be usable to observe those retained resources.
    pub(super) fn native_usage(
        &self,
        snapshot: Option<&BoxRecord>,
        id: &BoxId,
    ) -> Result<Option<Usage>, Refusal> {
        let Some(books) = self.books.get() else {
            return Ok(None);
        };
        let snapshot = snapshot.ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
        if !self.owns_record(&snapshot.spec) {
            return Ok(None);
        }
        let keys = self.turn_keys(&snapshot.spec.devices)?;
        devices::with_transaction(books, id, &keys, |transaction| {
            let record = transaction
                .read(id)?
                .ok_or(Refusal::NoSuchBox(id.try_clone()?))?;
            if record.native_scope.is_none()
                || record.native_scope != snapshot.native_scope
                || record.spec != snapshot.spec
                || record.init != snapshot.init
            {
                return Err(uncertain(id)?);
            }
            transaction.managed_usage().map(Some)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{Records, Transaction};
    use hex_api::{Capacity, CapacityResources};
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct Owner {
        records: Mutex<Records>,
        calls: AtomicUsize,
    }
    impl Transaction for Owner {
        fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
            Err(Refusal::Broken(
                "ordinary-box model owns no pool scope".into(),
            ))
        }
        fn pool_records(&self) -> Result<crate::devices::PoolRecords, Refusal> {
            Ok(Vec::new())
        }
        fn read_pool(
            &self,
            _: &hex_api::BackingKey,
        ) -> Result<Option<hex_core::record::PoolRecord>, Refusal> {
            panic!("box control must not read a pool")
        }
        fn put_pool(&self, _: &hex_core::record::PoolRecord) -> Result<(), Refusal> {
            panic!("box control must not write a pool")
        }
        fn grant_pool(
            &self,
            _: &hex_api::BackingGrant,
        ) -> Result<hex_core::record::PoolRecord, Refusal> {
            panic!("box control must not grant a pool")
        }
        fn remove_pool(&self, _: &hex_api::BackingKey) -> Result<(), Refusal> {
            panic!("box control must not remove a pool")
        }
        fn records(&self) -> Result<Records, Refusal> {
            Ok(self.records.lock().unwrap().clone())
        }
        fn read(&self, id: &BoxId) -> Result<Option<BoxRecord>, Refusal> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .get(id)
                .map(|row| row.as_ref().unwrap().clone()))
        }
        fn put(&self, record: &BoxRecord) -> Result<(), Refusal> {
            self.records
                .lock()
                .unwrap()
                .insert(record.spec.id.clone(), Ok(record.clone()));
            Ok(())
        }
        fn remove(&self, id: &BoxId) -> Result<(), Refusal> {
            self.records.lock().unwrap().remove(id);
            Ok(())
        }
        fn finalize_managed(&self, _: Option<&shards_api::Identity>) -> Result<(), Refusal> {
            panic!("pure record control cannot finalize native scope")
        }
        fn managed_usage(&self) -> Result<Usage, Refusal> {
            Err(Refusal::Broken(
                "this pure model has no native counters".into(),
            ))
        }
        fn launch_managed(
            &self,
            _: &crate::devices::ManagedLaunch<'_>,
            _: shards_api::LaunchIo,
            _: Duration,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            panic!("pure record control cannot launch")
        }
        fn attach_managed(
            &self,
            _: Option<&shards_api::Identity>,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            panic!("pure record control cannot attach")
        }
    }
    impl Books for Owner {
        fn with_handoff_transaction(
            &self,
            _: &hex_api::BoxId,
            _: &hex_api::BackingKey,
            _: &[hex_api::DeviceId],
            _: &mut dyn FnMut(&dyn crate::devices::Transaction) -> Result<(), hex_api::Refusal>,
        ) -> Result<(), hex_api::Refusal> {
            panic!("existing test owner must not enter a subset handoff")
        }

        fn pool_records(&self) -> Result<crate::devices::PoolRecords, Refusal> {
            Ok(Vec::new())
        }
        fn prepare_pool(
            &self,
            _: [u8; 32],
            _: hex_api::Execution,
            _: hex_api::RuntimeCost,
            _: &[hex_api::DeviceId],
            _: (
                &[u8; 32],
                &[hex_api::DeviceId],
                Option<&hex_core::record::PoolRecord>,
            ),
            _: &mut crate::devices::PoolPreparationCheck<'_>,
        ) -> Result<hex_core::record::PoolRecord, Refusal> {
            panic!("box control must not prepare a pool")
        }
        fn with_pool_transaction(
            &self,
            _: &hex_api::BackingKey,
            _: &[hex_api::DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            panic!("box control must not enter a pool")
        }
        fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
            Ok(Vec::new())
        }
        fn records(&self) -> Result<Records, Refusal> {
            Transaction::records(self)
        }
        fn capacity(&self) -> Result<Capacity, Refusal> {
            Ok(Capacity {
                id: "model-host".into(),
                total: CapacityResources {
                    cpu_milli: 8000,
                    mem_bytes: 1 << 30,
                    disk_bytes: 1 << 30,
                },
                free: CapacityResources {
                    cpu_milli: 8000,
                    mem_bytes: 1 << 30,
                    disk_bytes: 1 << 30,
                },
                devices: Vec::new(),
                cost: None,
                routes: Vec::new(),
                device_routes: Vec::new(),
                backings: Vec::new(),
                unavailable: Vec::new(),
                learned: None,
            })
        }
        fn with_transaction(
            &self,
            _: &BoxId,
            _: &[hex_api::DeviceId],
            callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            callback(self)
        }
    }

    fn runtime() -> GuestRuntime {
        GuestRuntime::new(
            Arc::new(shards_api::sim::SimGuests::new(|_| {
                shards_api::End::PowerOff
            })),
            super::super::tests::config(),
        )
    }

    fn cost(runtime: &GuestRuntime) -> hex_api::RuntimeCost {
        let floor = runtime.dedicated_resources().unwrap();
        hex_api::RuntimeCost {
            id: [1; 32],
            floor,
            ceiling: floor,
            quantum: CapacityResources {
                cpu_milli: 1000,
                mem_bytes: 1 << 20,
                disk_bytes: 1,
            },
            // Explicit model inputs for a pure test, never a native measurement.
            guest_overhead: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 0,
            },
            overhead: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 0,
            },
        }
    }

    /// AC-T7.4.1: only the same initialized record owner can be bound, and its
    /// death is uncertainty rather than a new Store or permissive empty ledger.
    #[test]
    fn weak_registration_retains_one_canonical_owner() {
        let runtime = runtime();
        let owner: Arc<dyn Books> = Arc::new(Owner::default());
        let another: Arc<dyn Books> = Arc::new(Owner::default());
        runtime.bind_books(Arc::downgrade(&owner)).unwrap();
        runtime.bind_books(Arc::downgrade(&owner)).unwrap();
        assert!(runtime.bind_books(Arc::downgrade(&another)).is_err());
        assert!(runtime.uses_books());
        drop(owner);
        assert!(
            runtime
                .recorded_box(&BoxId::new("no-owner").unwrap())
                .is_err()
        );
    }

    /// AC-T7.4.1: scheduler charge and actual configured VM must be identical;
    /// missing, forged, stale or smaller allocation is refused before a transaction.
    #[test]
    fn dedicated_cost_is_required_and_cannot_shrink_original_workload() {
        let runtime = runtime();
        let profile = cost(&runtime);
        let runtime = runtime.with_runtime_cost(profile).unwrap();
        let mut spec = super::super::tests::oci(hex_api::Rootfs::Dir("/rootfs".into()), Vec::new());
        let ClassSpec::Oci(payload) = spec.class else {
            panic!("fixture OCI")
        };
        spec.class = ClassSpec::Shards(payload);
        spec.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
        spec.bounds.memory_max = Some(hex_api::MIN_MEMORY);
        assert!(runtime.check_allocation(&spec).is_err());
        spec.allocation = Some(
            profile
                .allocate(CapacityResources {
                    cpu_milli: u64::from(hex_api::MIN_CPU_MILLI),
                    mem_bytes: hex_api::MIN_MEMORY,
                    disk_bytes: 0,
                })
                .unwrap(),
        );
        runtime.check_allocation(&spec).unwrap();
        let mut forged = spec.clone();
        forged.allocation.as_mut().unwrap().resources.cpu_milli = 1;
        assert!(runtime.check_allocation(&forged).is_err());
        let mut stale = spec.clone();
        stale.allocation.as_mut().unwrap().cost.id = [2; 32];
        assert!(runtime.check_allocation(&stale).is_err());
        spec.bounds.memory_max = Some(profile.ceiling.mem_bytes);
        assert!(
            runtime.check_allocation(&spec).is_err(),
            "guest overhead cannot shrink the original cap"
        );
    }

    /// AC-T7.4.1: host admission/execution provenance never enters the guest's
    /// native OCI namespace. The canonical original and selected receipt retain it.
    #[test]
    fn inner_oci_clears_host_allocation_and_route_without_changing_original() {
        let runtime = runtime();
        let profile = cost(&runtime);
        let mut spec = super::super::tests::oci(
            hex_api::Rootfs::Dir(std::env::current_dir().unwrap().join("rootfs.tar")),
            Vec::new(),
        );
        let ClassSpec::Oci(payload) = spec.class else {
            panic!("OCI fixture")
        };
        spec.class = ClassSpec::Shards(payload);
        spec.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
        spec.bounds.memory_max = Some(hex_api::MIN_MEMORY);
        spec.devices.push(hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 256,
        });
        spec.allocation = Some(
            profile
                .allocate(CapacityResources {
                    cpu_milli: u64::from(hex_api::MIN_CPU_MILLI),
                    mem_bytes: hex_api::MIN_MEMORY,
                    disk_bytes: 0,
                })
                .unwrap(),
        );
        spec.route = Some(hex_api::RouteReceipt {
            key: hex_api::RouteKey {
                class: Class::Shards,
                image: hex_api::ImagePlatform {
                    os: hex_api::ImageOs::Linux,
                    arch: route::host_platform().unwrap().arch,
                },
                execution: hex_api::Execution::ShardsDedicated,
                gpu: hex_api::GpuEligibility::Required,
            },
            backing: Some(hex_api::BackingKey {
                pool: [3; 32],
                generation: [4; 16],
            }),
        });
        let original = spec.clone();
        let mut record = BoxRecord::new(spec.clone(), Phase::Creating, Vec::new());
        record.native_scope = Some(hex_core::record::NativeScope::from_bytes([7; 16]));
        let retained = selected::Record::retained(&record).unwrap();
        let rewritten = runtime.rewrite_spec(spec.clone()).unwrap();
        assert!(matches!(rewritten.spec.class, ClassSpec::Oci(_)));
        assert_eq!(rewritten.spec.allocation, None);
        assert_eq!(rewritten.spec.route, None);
        assert_eq!(rewritten.spec.bounds, original.bounds);
        assert_eq!(rewritten.spec.devices, original.devices);
        assert_eq!(spec, original);
        assert_eq!(record.spec, original);
        assert_eq!(retained.route, original.route);
        let mut transplanted = original.clone();
        transplanted
            .route
            .as_mut()
            .unwrap()
            .backing
            .as_mut()
            .unwrap()
            .generation = [8; 16];
        assert!(retained.matches(&transplanted).is_err());
    }

    /// AC-T7.4.1: a fixed VM's aggregate inner payload spends the original
    /// backing once. The boundary includes both allowances and quantization.
    #[test]
    fn fixed_backing_accepts_below_exact_and_refuses_over_aggregate_payload_room() {
        let runtime = runtime();
        let profile = cost(&runtime);
        let backing = profile.allocate(CapacityResources::default()).unwrap();
        assert_eq!(
            backing.guest().unwrap(),
            runtime.dedicated_resources().unwrap()
        );
        let room = backing
            .guest()
            .unwrap()
            .checked_sub(profile.guest_overhead)
            .unwrap();
        let below = CapacityResources {
            mem_bytes: room.mem_bytes - 1,
            ..room
        };
        for aggregate in [below, room] {
            backing.check(aggregate).unwrap();
            assert_eq!(profile.allocate(aggregate).unwrap(), backing);
        }
        for aggregate in [
            CapacityResources {
                cpu_milli: room.cpu_milli + 1,
                ..room
            },
            CapacityResources {
                mem_bytes: room.mem_bytes + 1,
                ..room
            },
            CapacityResources {
                disk_bytes: room.disk_bytes + 1,
                ..room
            },
        ] {
            assert!(backing.check(aggregate).is_err());
        }
        let mut dynamic = profile;
        dynamic.ceiling.mem_bytes += dynamic.quantum.mem_bytes;
        assert!(
            runtime.with_runtime_cost(dynamic).is_err(),
            "a larger possible VM is not this configured VM"
        );
    }

    /// AC-T7.4.1: canonical recorded class/phase gates every dedicated method,
    /// while ordinary CPU OCI retains its one outer Recorded transaction.
    #[test]
    fn dedicated_methods_enter_canonical_turn_once_and_refuse_nonready_effects() {
        let runtime = runtime();
        let owner = Arc::new(Owner::default());
        let books: Arc<dyn Books> = owner.clone();
        runtime.bind_books(Arc::downgrade(&books)).unwrap();
        let mut spec = super::super::tests::oci(hex_api::Rootfs::Dir("/rootfs".into()), Vec::new());
        let id = spec.id.clone();
        owner
            .put(&BoxRecord::new(spec.clone(), Phase::Ready, Vec::new()))
            .unwrap();
        assert!(!runtime.owns_record(&spec));
        let snapshot = || runtime.recorded_box(&id).unwrap();
        assert_eq!(
            runtime.call_recorded(snapshot().as_ref(), &id, true, || Ok(7)),
            Ok(7)
        );
        assert_eq!(owner.calls.load(Ordering::SeqCst), 0);
        let ClassSpec::Oci(payload) = spec.class else {
            panic!("fixture OCI")
        };
        spec.class = ClassSpec::Shards(payload);
        owner
            .put(&BoxRecord::new(spec.clone(), Phase::Creating, Vec::new()))
            .unwrap();
        assert!(runtime.owns_record(&spec));
        assert!(matches!(
            runtime.call_recorded(snapshot().as_ref(), &id, true, || -> Result<(), Refusal> {
                panic!("non-ready effect")
            }),
            Err(Refusal::Lifecycle { .. })
        ));
        assert_eq!(owner.calls.load(Ordering::SeqCst), 1);
    }
}
