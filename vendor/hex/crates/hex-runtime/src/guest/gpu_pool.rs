//! The already-admitted GPU observer VM: canonical PoolRecord holds the one
//! grant; the existing route table retains only its actual native/client owner.

use hex_api::{
    BackingGrant, BackingKey, BackingOffer, BackingPhase, LifecycleCause, Refusal, Usage,
};
use hex_core::record::{NativeRecord, OwnerRef, PoolRecord};

use super::dedicated::{Guest, Operation};
use super::pool::{self, Receipt, refused};
use super::selected::Phase;
use super::shared::{Binding, GpuBacking};
use super::{Booted, GuestRuntime, managed, observer, refusal};
use crate::devices::{self, Transaction};

impl GuestRuntime {
    pub(super) fn handoff_source_released(&self, record: &PoolRecord) -> Result<(), Refusal> {
        self.preflight_backing(record)?;
        let bad = || refused(record.key, LifecycleCause::Uncertain);
        if record.phase != BackingPhase::Released || record.exit.is_none() {
            return Err(bad());
        }
        let definition = self.gpu_definition_for_record(record)?;
        let receipt = pool::read(&definition.path)?.ok_or_else(bad)?;
        receipt.matches(record)?;
        let Phase::Released { status, .. } = receipt.phase else {
            return Err(bad());
        };
        if record.exit != Some(super::shared::exit_of(status)) {
            return Err(bad());
        }
        Ok(())
    }

    /// Service calls this only AFTER its SAME Index observes this exact old
    /// Released generation. No retained Box may reference it during rearm.
    pub(super) fn acknowledge_gpu_release(&self, key: BackingKey) -> Result<BackingOffer, Refusal> {
        let definition = self.gpu_definition_for(key)?;
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        let keys = self.turn_keys(&definition.devices)?;
        let mut reservation = self.reserve_owner(OwnerRef::Pool(&key.pool), Operation::Stopping)?;
        let offer = devices::with_pool_transaction(books, &key, &keys, |transaction| {
            let record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            self.preflight_backing(&record)?;
            if record.phase != BackingPhase::Released {
                return Err(refused(key, LifecycleCause::Pending));
            }
            for boxed in transaction.records()?.values() {
                let boxed = boxed
                    .as_ref()
                    .map_err(|_| refused(key, LifecycleCause::Uncertain))?;
                if boxed
                    .spec
                    .route
                    .is_some_and(|route| route.backing == Some(key))
                {
                    return Err(refused(key, LifecycleCause::Busy));
                }
            }
            let offer = record.offer()?;
            if let Some(receipt) = pool::read(&definition.path)? {
                receipt.matches(&record)?;
                if !matches!(receipt.phase, Phase::Released { .. }) {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                transaction.finalize_managed(receipt.released_identity()?)?;
                super::held::remove(&definition.path)?;
            }
            transaction.remove_pool(&key)?;
            Ok(offer)
        })?;
        reservation.previous.take();
        reservation.publish(None)?;
        // Only this existing Index ACK removes old metadata. The next guarded
        // preparation/capacity refresh may publish the current complement; an
        // unrelated stale proposal cannot undo this completed acknowledgment.
        Ok(offer)
    }

    /// Draining cleanup and Released Index acknowledgment are separate calls.
    /// Native finalization precedes canonical Released; metadata removal never
    /// stands in for whole-scope termination.
    pub(super) fn release_gpu_backing(&self, key: BackingKey) -> Result<BackingOffer, Refusal> {
        let snapshot = self.pool_record(key)?;
        if snapshot.phase == BackingPhase::Released {
            return self.acknowledge_gpu_release(key);
        }
        let definition = self.gpu_definition_for_record(&snapshot)?;
        let budget = managed::Budget::new(self.first_dial_bound()?)?;
        let keys = self.turn_keys(&definition.devices)?;
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        let mut reservation = self.reserve_owner(OwnerRef::Pool(&key.pool), Operation::Stopping)?;
        let offer = devices::with_pool_transaction(books, &key, &keys, |transaction| {
            let mut record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            self.preflight_backing(&record)?;
            if snapshot.phase == BackingPhase::Prepared {
                if record != snapshot {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
            } else if record.native_commitment()? != snapshot.native_commitment()?
                || record.init != snapshot.init
            {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            for boxed in transaction.records()?.values() {
                let boxed = boxed
                    .as_ref()
                    .map_err(|_| refused(key, LifecycleCause::Uncertain))?;
                if boxed.spec.route.and_then(|route| route.backing) == Some(key) {
                    return Err(refused(key, LifecycleCause::Busy));
                }
            }
            if record.phase == BackingPhase::Prepared {
                // Prepared alone is insufficient: exact no-submission also
                // requires no local Intent or retained process publication.
                if reservation.previous.is_some()
                    || pool::read(&definition.path)?.is_some()
                    || record.allocation.is_some()
                    || record.init.is_some()
                    || record.exit.is_some()
                {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                transaction.check_unsubmitted_pool()?;
                record.phase = BackingPhase::Released;
                transaction.put_pool(&record)?;
                return record.offer();
            }
            if !matches!(
                record.phase,
                BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining
            ) {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            let mut receipt = match pool::read(&definition.path)? {
                Some(receipt) => receipt,
                None if record.init.is_none() => {
                    let mut original = PoolRecord::decode(&record.encode()?)?;
                    original.phase = BackingPhase::Granted;
                    original.exit = None;
                    Receipt::retained(original, self.catalog_physical(&record.devices)?)?
                }
                None => return Err(refused(key, LifecycleCause::Uncertain)),
            };
            receipt.matches(&record)?;
            let identity = match &receipt.phase {
                Phase::Running(identity)
                | Phase::Stopping(identity)
                | Phase::Released {
                    identity: Some(identity),
                    ..
                } => {
                    managed::same_native(NativeRecord::Pool(&record), identity)?;
                    Some(super::shared::copy_identity(identity)?)
                }
                Phase::Intent | Phase::Released { identity: None, .. } => None,
            };
            record.phase = BackingPhase::Draining;
            transaction.put_pool(&record)?;
            if let Some(identity) = &identity {
                receipt.phase = Phase::Stopping(super::shared::copy_identity(identity)?);
            }
            pool::write(&definition.path, &receipt)?;
            let terminal = managed::stop_scope(
                transaction,
                identity.as_ref(),
                budget.remaining()?,
                refused(key, LifecycleCause::Uncertain),
            )?;
            receipt.phase = Phase::Released {
                identity,
                status: terminal.status,
            };
            pool::write(&definition.path, &receipt)?;
            transaction.finalize_managed(receipt.released_identity()?)?;
            record.phase = BackingPhase::Released;
            record.exit = Some(super::shared::exit_of(terminal.status));
            transaction.put_pool(&record)?;
            record.offer()
        })?;
        reservation.previous.take();
        reservation.publish(None)?;
        Ok(offer)
    }

    pub(super) fn stop_gpu_for_handoff(
        &self,
        key: BackingKey,
        boxed: &hex_core::record::BoxRecord,
        budget: managed::Budget,
    ) -> Result<BackingOffer, Refusal> {
        let definition = self.gpu_definition_for(key)?;
        if boxed.spec.route.and_then(|route| route.backing) != Some(key)
            || boxed.phase != hex_core::record::Phase::Creating
            || boxed.init.is_some()
            || boxed.run.is_some()
            || boxed.exit.is_some()
            || boxed.native_scope.is_none()
        {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        boxed
            .spec
            .route
            .ok_or(super::books::uncertain(&boxed.spec.id)?)?
            .check_backing_parts(
                boxed.spec.allocation.map(|allocation| allocation.cost),
                key,
                definition.cost,
                &definition.devices,
                &boxed.spec.devices,
            )?;
        let keys = self.turn_keys(&definition.devices)?;
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        let mut reservation = self.reserve_owner(OwnerRef::Pool(&key.pool), Operation::Stopping)?;
        let offer = devices::with_pool_transaction(books, &key, &keys, |transaction| {
            let mut record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            self.preflight_backing(&record)?;
            let current = transaction
                .read(&boxed.spec.id)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            if current.spec != boxed.spec
                || current.native_scope != boxed.native_scope
                || current.phase != hex_core::record::Phase::Creating
                || current.init.is_some()
                || current.run.is_some()
                || current.exit.is_some()
            {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            let mut receipt = pool::read(&definition.path)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            receipt.matches(&record)?;
            if record.phase == BackingPhase::Released {
                self.handoff_source_released(&record)?;
                return record.offer();
            }
            if record.phase != BackingPhase::Live && record.phase != BackingPhase::Draining {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            let identity = super::shared::copy_identity(receipt.identity()?)?;
            managed::same_native(NativeRecord::Pool(&record), &identity)?;
            record.phase = BackingPhase::Draining;
            transaction.put_pool(&record)?;
            receipt.phase = Phase::Stopping(super::shared::copy_identity(&identity)?);
            pool::write(&definition.path, &receipt)?;
            let terminal = managed::stop_scope(
                transaction,
                Some(&identity),
                budget.remaining()?,
                refused(key, LifecycleCause::Uncertain),
            )?;
            receipt.phase = Phase::Released {
                identity: Some(identity),
                status: terminal.status,
            };
            pool::write(&definition.path, &receipt)?;
            transaction.finalize_managed(receipt.released_identity()?)?;
            record.exit = Some(super::shared::exit_of(terminal.status));
            record.phase = BackingPhase::Released;
            transaction.put_pool(&record)?;
            record.offer()
        })?;
        reservation.previous.take();
        reservation.publish(None)?;
        Ok(offer)
    }

    pub(super) fn initialize_gpu_pool(&self, grant: BackingGrant) -> Result<BackingOffer, Refusal> {
        grant.check()?;
        // This one finite absolute budget is captured before grant persistence,
        // native effects, report observation, guest workload or publication.
        let budget = managed::Budget::new(self.first_dial_bound()?)?;
        let definition = self.gpu_definition_for(grant.key)?;
        if grant.devices != definition.devices
            || grant.allocation != self.configured_allocation()?
        {
            return Err(refused(grant.key, LifecycleCause::Uncertain));
        }
        let observer = self
            .gpu_observer
            .as_ref()
            .ok_or_else(|| refused(grant.key, LifecycleCause::Pending))?;
        observer.verify(&self.config)?;
        budget.remaining()?;
        let keys = self.turn_keys(&definition.devices)?;
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(grant.key, LifecycleCause::Uncertain))?;
        let mut reservation =
            self.reserve_owner(OwnerRef::Pool(&grant.key.pool), Operation::Creating)?;
        devices::with_pool_transaction(books, &grant.key, &keys, |transaction| {
            let mut record = transaction.grant_pool(&grant)?;
            self.preflight_backing(&record)?;
            let booted = match reservation.previous.as_ref() {
                Some(known) => {
                    self.matches_pool(&known.booted, &record)?;
                    let identity = known.booted.guest.identity().map_err(|why| {
                        refusal("checking the same GPU backing publication", &why)
                    })?;
                    managed::same_native(NativeRecord::Pool(&record), &identity)?;
                    self.pool_alive(&known.booted, record.key)?;
                    let receipt = pool::read(&definition.path)?
                        .ok_or_else(|| refused(record.key, LifecycleCause::Uncertain))?;
                    receipt.matches(&record)?;
                    receipt.checked_report()?;
                    self.check_pool_native_report(&receipt)?;
                    receipt.checked_work_ready()?;
                    triomphe::Arc::clone(&known.booted)
                }
                None => self.boot_gpu_pool(transaction, &definition, record, budget)?,
            };
            record = transaction
                .read_pool(&grant.key)?
                .ok_or_else(|| refused(grant.key, LifecycleCause::Uncertain))?;
            self.matches_pool(&booted, &record)?;
            let receipt = pool::read(&definition.path)?
                .ok_or_else(|| refused(grant.key, LifecycleCause::Uncertain))?;
            receipt.matches(&record)?;
            receipt.checked_work_ready()?;
            self.pool_alive(&booted, grant.key)?;
            budget.remaining()?;
            if record.phase == BackingPhase::Granted {
                record.phase = BackingPhase::Live;
                transaction.put_pool(&record)?;
            }
            if record.phase != BackingPhase::Live {
                return Err(refused(grant.key, LifecycleCause::Uncertain));
            }
            let offer = record.offer()?;
            reservation.previous = Some(Guest { booted });
            Ok(offer)
        })
        .and_then(|offer| {
            let known = reservation.previous.take();
            reservation.publish(known)?;
            Ok(offer)
        })
    }

    fn gpu_definition_for(&self, key: BackingKey) -> Result<GpuBacking, Refusal> {
        self.gpu_definition_for_record(&self.pool_record(key)?)
    }

    pub(super) fn gpu_definition_for_record(
        &self,
        record: &PoolRecord,
    ) -> Result<GpuBacking, Refusal> {
        record.check()?;
        let definition = self
            .gpu_backing_definition_for(Some(&record.devices))?
            .ok_or_else(|| refused(record.key, LifecycleCause::Uncertain))?;
        if record.execution != hex_api::Execution::ShardsDedicated
            || record.key.pool != definition.pool
            || record.cost != definition.cost
        {
            return Err(refused(record.key, LifecycleCause::Uncertain));
        }
        Ok(definition)
    }

    fn boot_gpu_pool(
        &self,
        transaction: &dyn Transaction,
        definition: &GpuBacking,
        record: PoolRecord,
        budget: managed::Budget,
    ) -> Result<triomphe::Arc<Booted>, Refusal> {
        let key = record.key;
        let retained = pool::read(&definition.path)?;
        let fresh = retained.is_none();
        let mut receipt = match retained {
            Some(receipt) => {
                receipt.matches(&record)?;
                receipt
            }
            None if record.phase == BackingPhase::Granted && record.init.is_none() => {
                Receipt::intent(
                    record,
                    self.catalog_physical(&definition.devices)?,
                    &self.config,
                )?
            }
            None => return Err(refused(key, LifecycleCause::Uncertain)),
        };
        let binding = Binding {
            key,
            commitment: receipt.original.native_commitment()?,
        };
        let mut spec = self.pool_spec(&receipt)?;
        spec.gpu = true;
        spec.gpu_devices
            .try_reserve_exact(receipt.physical.len())
            .map_err(|_| Refusal::Full(hex_api::MAX_DEVICES))?;
        spec.gpu_devices.extend_from_slice(&receipt.physical);
        let socket = self.pool_socket(key)?;
        let plan = self.managed_plan(&spec, &socket, budget)?;
        let guest = match &receipt.phase {
            Phase::Intent if fresh => {
                self.check_current_luids(
                    &receipt.original.devices,
                    &receipt.physical,
                    refused(key, LifecycleCause::Uncertain),
                )?;
                let plan = plan
                    .with_gpu_report(*receipt.original.scope().bytes(), budget.deadline())
                    .map_err(|why| refusal("preparing charged GPU observer report", &why))?;
                pool::write(&definition.path, &receipt)?;
                managed::boot(transaction, plan, budget)?
            }
            Phase::Running(identity) => {
                receipt.checked_report()?;
                self.check_pool_native_report(&receipt)?;
                receipt.checked_work_ready()?;
                let canonical = transaction
                    .read_pool(&key)?
                    .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
                managed::same_native(NativeRecord::Pool(&canonical), identity)?;
                if identity.vsock != socket {
                    return Err(refused(key, LifecycleCause::Uncertain));
                }
                managed::attach(transaction, plan, identity, budget)?
            }
            // An interrupted Intent/report/workload is uncertainty, never a
            // second VM/program launch or an automatic grant refund.
            _ => return Err(refused(key, LifecycleCause::Uncertain)),
        };
        let identity = shards_api::Guest::identity(&guest)
            .map_err(|why| refusal("reading charged GPU observer identity", &why))?;
        let canonical = transaction
            .read_pool(&key)?
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        receipt.matches(&canonical)?;
        managed::same_native(NativeRecord::Pool(&canonical), &identity)?;
        if identity.vsock != socket {
            return Err(refused(key, LifecycleCause::Uncertain));
        }
        if fresh {
            receipt.report = Some(
                guest
                    .take_gpu_report(budget.remaining()?)
                    .map_err(|why| refusal("observing same-scope full renderer ACK", &why))?,
            );
            receipt.checked_report()?;
            self.check_pool_native_report(&receipt)?;
        }
        receipt.phase = Phase::Running(identity);
        pool::write(&definition.path, &receipt)?;
        let booted = self.connect_managed(guest, None, Some(binding), budget)?;
        if fresh {
            let observer = self
                .gpu_observer
                .as_ref()
                .ok_or_else(|| refused(key, LifecycleCause::Pending))?;
            observer::observe(observer, &booted, &mut receipt, &definition.path, budget)?;
        }
        receipt.checked_work_ready()?;
        self.check_pool_native_report(&receipt)?;
        budget.remaining()?;
        Ok(booted)
    }

    fn check_pool_native_report(&self, receipt: &Receipt) -> Result<(), Refusal> {
        let report = receipt
            .checked_report()?
            .ok_or_else(|| refused(receipt.original.key, LifecycleCause::Uncertain))?;
        self.check_native_report(
            &receipt.original.devices,
            &receipt.physical,
            receipt.original.scope(),
            report,
        )
    }

    pub(super) fn recover_gpu_backing(&self) -> Result<(), Refusal> {
        if self.gpu_backing_definition()?.is_none() {
            return Ok(());
        }
        let authority = self.authority()?;
        let records = authority.pool_records()?;
        // Every retained member is checked before the first native cleanup.
        // A stale/foreign subset later in the snapshot must not be discovered
        // only after another scope has already been changed.
        for record in records
            .iter()
            .filter(|record| record.execution == hex_api::Execution::ShardsDedicated)
        {
            self.preflight_backing(record)?;
        }
        for record in records
            .iter()
            .filter(|record| record.execution == hex_api::Execution::ShardsDedicated)
        {
            let definition = self.gpu_definition_for_record(record)?;
            if matches!(
                record.phase,
                BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining
            ) && self.recover_gpu_terminal(&definition, record)?
            {
                continue;
            }
            match record.phase {
                BackingPhase::Prepared | BackingPhase::Released => {}
                BackingPhase::Live => {
                    let allocation = record
                        .allocation
                        .ok_or_else(|| refused(record.key, LifecycleCause::Uncertain))?;
                    self.initialize_gpu_pool(BackingGrant {
                        key: record.key,
                        allocation,
                        devices: self.gpu_ids(&record.devices)?,
                    })?;
                }
                _ => return Err(refused(record.key, LifecycleCause::Uncertain)),
            }
        }
        Ok(())
    }

    fn recover_gpu_terminal(
        &self,
        definition: &GpuBacking,
        snapshot: &PoolRecord,
    ) -> Result<bool, Refusal> {
        let key = snapshot.key;
        let budget = managed::Budget::new(self.first_dial_bound()?)?;
        let keys = self.turn_keys(&definition.devices)?;
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        let mut reservation = self.reserve_owner(OwnerRef::Pool(&key.pool), Operation::Loading)?;
        let released = devices::with_pool_transaction(books, &key, &keys, |transaction| {
            let mut record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            if record.native_commitment()? != snapshot.native_commitment()?
                || record.init != snapshot.init
            {
                return Err(refused(key, LifecycleCause::Uncertain));
            }
            let mut receipt = match pool::read(&definition.path)? {
                Some(receipt) => {
                    receipt.matches(&record)?;
                    receipt
                }
                None if record.phase == BackingPhase::Granted && record.init.is_none() => {
                    Receipt::retained(
                        PoolRecord::decode(&record.encode()?)?,
                        self.catalog_physical(&record.devices)?,
                    )?
                }
                None => return Err(refused(key, LifecycleCause::Uncertain)),
            };
            let identity = match &receipt.phase {
                Phase::Running(identity)
                | Phase::Stopping(identity)
                | Phase::Released {
                    identity: Some(identity),
                    ..
                } => {
                    managed::same_native(NativeRecord::Pool(&record), identity)?;
                    Some(super::shared::copy_identity(identity)?)
                }
                // Local publication can lag the canonical native init. None is
                // cleanup-only against this SAME record's scope/commitment; it
                // never reconstructs a Guest or a socket-derived identity.
                Phase::Intent | Phase::Released { identity: None, .. } => None,
            };
            let native = transaction.attach_managed(identity.as_ref())?;
            let observed = native.terminal().map_err(|why| {
                refusal("observing original GPU backing scope during recovery", &why)
            })?;
            if record.phase == BackingPhase::Live && observed.is_none() {
                // Usable recovery additionally checks original report/readiness
                // and connects the SAME native identity; this is no fresh boot.
                return Ok(false);
            }
            record.phase = BackingPhase::Draining;
            transaction.put_pool(&record)?;
            if let Some(identity) = &identity {
                receipt.phase = Phase::Stopping(super::shared::copy_identity(identity)?);
            }
            pool::write(&definition.path, &receipt)?;
            let terminal = match observed {
                Some(end) if end.scope == shards_api::TerminalScope::Released => end,
                _ => managed::stop_scope(
                    transaction,
                    identity.as_ref(),
                    budget.remaining()?,
                    refused(key, LifecycleCause::Uncertain),
                )?,
            };
            receipt.phase = Phase::Released {
                identity,
                status: terminal.status,
            };
            pool::write(&definition.path, &receipt)?;
            transaction.finalize_managed(receipt.released_identity()?)?;
            record.phase = BackingPhase::Released;
            record.exit = Some(super::shared::exit_of(terminal.status));
            transaction.put_pool(&record)?;
            Ok(true)
        })?;
        let previous = if released {
            None
        } else {
            reservation.previous.take()
        };
        reservation.publish(previous)?;
        Ok(released)
    }

    fn gpu_ids(&self, ids: &[hex_api::DeviceId]) -> Result<Vec<hex_api::DeviceId>, Refusal> {
        let mut copy = Vec::new();
        copy.try_reserve_exact(ids.len())
            .map_err(|_| Refusal::Full(hex_api::MAX_DEVICES))?;
        copy.extend_from_slice(ids);
        Ok(copy)
    }

    pub(super) fn gpu_backing_usage(&self, key: BackingKey) -> Result<Usage, Refusal> {
        let definition = self.gpu_definition_for(key)?;
        let books = self
            .books
            .get()
            .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
        let keys = self.turn_keys(&definition.devices)?;
        devices::with_pool_transaction(books, &key, &keys, |transaction| {
            let record = transaction
                .read_pool(&key)?
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            self.preflight_backing(&record)?;
            if !matches!(
                record.phase,
                BackingPhase::Granted | BackingPhase::Live | BackingPhase::Draining
            ) {
                return Err(refused(key, LifecycleCause::Pending));
            }
            transaction.managed_usage()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{Books, PoolRecords, Records};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn refused_model() -> Refusal {
        Refusal::Invalid {
            what: "cold observer model",
            why: "effects are deliberately unavailable".into(),
        }
    }

    /// Original AC-T7.4.1 retry control: local durable Released without a
    /// controller identity must permit only cleanup of the SAME canonical scope.
    /// No production preflight remedy is included in this checkpoint.
    #[test]
    fn released_gpu_receipt_without_identity_finishes_original_published_scope() {
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(
            b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\n",
        )
        .unwrap();
        let runtime = GuestRuntime::new(standalone.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        let native = triomphe::Arc::try_new(RecoveringNative {
            identity: shards_api::Identity {
                pid: 42,
                started: 7,
                vsock: "/model/v".into(),
            },
            release_on_stop: false,
            active: false,
            released: std::sync::atomic::AtomicBool::new(true),
            attaches: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        })
        .unwrap();
        let owner = Arc::new(Cold {
            row: Mutex::new(None),
            shared: Mutex::new(None),
            native_report: None,
            physical: None,
            effects: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
            native: Some(native.clone()),
            finalized: AtomicUsize::new(0),
            luid: None,
            launches: AtomicUsize::new(0),
        });
        let books: Arc<dyn Books> = owner.clone();
        runtime.bind_books(Arc::downgrade(&books)).unwrap();
        let prepared = runtime.prepare_gpu_backing().unwrap().unwrap();
        let mut canonical = Books::pool_records(owner.as_ref()).unwrap().pop().unwrap();
        canonical
            .accept_grant(&BackingGrant {
                key: prepared.key,
                allocation,
                devices: prepared.devices.clone(),
            })
            .unwrap();
        let definition = runtime.gpu_backing_definition().unwrap().unwrap();
        let mut receipt = Receipt::intent(
            PoolRecord::decode(&canonical.encode().unwrap()).unwrap(),
            definition.physical,
            &runtime.config,
        )
        .unwrap();
        receipt.phase = Phase::Released {
            identity: None,
            status: shards_api::ManagedStatus::Code(7),
        };
        pool::write(&definition.path, &receipt).unwrap();
        canonical.phase = BackingPhase::Draining;
        canonical.init = Some(hex_core::record::ProcId {
            pid: 42,
            start_ticks: 7,
        });
        *owner.row.lock().unwrap() = Some(canonical);
        // Exact crash cut: local Released durable; canonical native init is
        // already published; finalization/canonical Released has not completed.
        let retained = pool::read(&definition.path).unwrap().unwrap();
        let before = Books::pool_records(owner.as_ref()).unwrap().pop().unwrap();
        retained.matches(&before).unwrap();
        assert!(matches!(
            retained.phase,
            Phase::Released { identity: None, .. }
        ));
        assert_eq!(before.phase, BackingPhase::Draining);
        assert!(before.init.is_some());
        assert_eq!(before.key, prepared.key);
        assert_eq!(before.allocation, Some(allocation));
        let recovered = runtime.recover_gpu_backing();
        let after = Books::pool_records(owner.as_ref()).unwrap().pop().unwrap();
        assert!(
            native.attaches.load(Ordering::SeqCst) > 0,
            "Released without local identity blocked cleanup of the SAME published scope"
        );
        assert!(recovered.is_ok(), "{recovered:?}");
        assert_eq!(owner.finalized.load(Ordering::SeqCst), 1);
        assert_eq!(after.phase, BackingPhase::Released);
        assert_eq!(after.key, before.key);
        assert_eq!(after.devices, before.devices);
        assert_eq!(after.allocation, before.allocation);
        assert_eq!(after.init, before.init);
        assert_eq!(after.exit, Some(hex_api::Exit::Code(7)));
        assert_eq!(native.stops.load(Ordering::SeqCst), 0);
        assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
        assert_eq!(owner.queries.load(Ordering::SeqCst), 0);
        assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
    }

    /// AC-T7.4.1: only a still-held source and a canonical unsubmitted Box may
    /// use missing Intent as cancellation evidence. Released is never absence.
    #[test]
    fn unsubmitted_handoff_requires_held_source_no_intent_and_no_route_slot() {
        use hex_api::{
            Class, Execution, GpuEligibility, ImageOs, ImagePlatform, RouteKey, RouteReceipt,
        };
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\ngpu\t512\tunmeasured\t777\tshards-gpu\tpci:0000:02:00.0\t\n").unwrap();
        let runtime = GuestRuntime::new(standalone.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        let definition = runtime.gpu_backing_definition().unwrap().unwrap();
        let mut source = PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: definition.pool,
                generation: [7; 16],
            },
            execution: Execution::ShardsDedicated,
            cost: allocation.cost,
            allocation: Some(allocation),
            devices: definition.devices,
            phase: BackingPhase::Granted,
            init: None,
            exit: None,
        };
        let receipt = Receipt::intent(
            PoolRecord::decode(&source.encode().unwrap()).unwrap(),
            definition.physical,
            &runtime.config,
        )
        .unwrap();
        pool::write(&definition.path, &receipt).unwrap();
        source.phase = BackingPhase::Draining;
        source.init = Some(hex_core::record::ProcId {
            pid: 42,
            start_ticks: 7,
        });
        let mut spec =
            super::super::tests::oci(hex_api::Rootfs::Dir(dir.path().to_path_buf()), Vec::new());
        spec.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
        spec.bounds.memory_max = Some(hex_api::MIN_MEMORY);
        spec.devices = vec![source.devices[0]];
        spec.allocation = Some(allocation);
        spec.route = Some(RouteReceipt {
            key: RouteKey {
                class: Class::Oci,
                image: ImagePlatform {
                    os: ImageOs::Linux,
                    arch: super::super::route::host_platform().unwrap().arch,
                },
                execution: Execution::ShardsDedicated,
                gpu: GpuEligibility::Required,
            },
            backing: Some(source.key),
        });
        let mut boxed =
            hex_core::record::BoxRecord::new(spec, hex_core::record::Phase::Deleting, Vec::new());
        boxed.native_scope = Some(hex_core::record::NativeScope::from_bytes([8; 16]));
        runtime.handoff_unsubmitted(&boxed, &source).unwrap();
        let pending = runtime
            .reserve(&boxed.spec.id, Operation::Creating)
            .unwrap();
        assert!(runtime.handoff_unsubmitted(&boxed, &source).is_err());
        drop(pending);
        let path = runtime.selected_path(&boxed.spec.id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut intent = super::super::selected::Record::intent(&boxed, &runtime.config).unwrap();
        intent.physical = runtime.catalog_physical(&boxed.spec.devices).unwrap();
        super::super::selected::write(&path, &intent).unwrap();
        assert!(runtime.handoff_unsubmitted(&boxed, &source).is_err());
        super::super::held::remove(&path).unwrap();
        runtime.handoff_unsubmitted(&boxed, &source).unwrap();
        source.phase = BackingPhase::Released;
        source.exit = Some(hex_api::Exit::Code(0));
        assert!(
            runtime.handoff_unsubmitted(&boxed, &source).is_err(),
            "missing Intent after Released cannot exclude submitted native effects"
        );
        assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
        assert!(runtime.dedicated.lock().unwrap().is_empty());
    }

    /// AC-T7.4.1: the caller must choose the guarded two-borrow proof for a
    /// pending handoff. Ordinary Deleting validation deliberately remains strict;
    /// after source Released, missing Intent is never submission/absence proof.
    #[test]
    fn pending_handoff_preflight_covers_creating_deleting_and_both_held_source_phases() {
        use hex_api::{
            Class, Execution, GpuEligibility, ImageOs, ImagePlatform, RouteKey, RouteReceipt,
        };
        use std::fmt::Write as _;
        fn hex(bytes: &[u8]) -> String {
            let mut text = String::new();
            for byte in bytes {
                write!(&mut text, "{byte:02x}").unwrap();
            }
            text
        }

        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\ngpu\t512\tunmeasured\t777\tshards-gpu\tpci:0000:02:00.0\t\n").unwrap();
        let runtime = GuestRuntime::new(standalone.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        let definition = runtime.gpu_backing_definition().unwrap().unwrap();
        let mut source = PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: definition.pool,
                generation: [7; 16],
            },
            execution: Execution::ShardsDedicated,
            cost: allocation.cost,
            allocation: Some(allocation),
            devices: definition.devices,
            phase: BackingPhase::Granted,
            init: None,
            exit: None,
        };
        let mut receipt = Receipt::intent(
            PoolRecord::decode(&source.encode().unwrap()).unwrap(),
            definition.physical,
            &runtime.config,
        )
        .unwrap();
        let scope = source.scope();
        receipt.report = Some(shards_api::physical::GpuReport {
            scope: *scope.bytes(),
            probe: shards_api::GpuProbe {
                devices: receipt
                    .physical
                    .iter()
                    .copied()
                    .zip([3, 4])
                    .map(|(physical, byte)| shards_api::ResolvedGpu {
                        physical,
                        pci: Some(physical),
                        luid: None,
                        uuid: [byte; 16],
                        name: None,
                        device_local_bytes: None,
                    })
                    .collect(),
                host_visible_bytes: 1,
            },
        });
        // Model bytes exercise the actual receipt/read-only adapter, not a
        // native renderer, guest Program, driver or whole-scope witness.
        let mut inventory = String::from("hex-vulkan-guest-inventory 1 2\n");
        for (ordinal, byte) in [3u8, 4].into_iter().enumerate() {
            writeln!(
                &mut inventory,
                "device {ordinal} 4202496 22 1 1 2 2 {} 677075 76656e7573 4d657361 2 1 1 2 1",
                format!("{byte:02x}").repeat(16)
            )
            .unwrap();
            writeln!(&mut inventory, "heap {ordinal} 0 1 4096").unwrap();
        }
        let observer = runtime.gpu_observer.as_ref().unwrap();
        receipt.readiness = Some(observer::Evidence {
            scope,
            configuration: observer.identity(),
            max_queues: observer.queues(),
            report: observer::Report::parse(inventory.as_bytes(), 2, observer.queues()).unwrap(),
        });
        let mut attempt = observer::Attempt::parse(&format!(
            "{} {} complete eof {} {} eof {} -",
            hex(&observer.identity()),
            observer.queues(),
            observer.output_bytes(2).unwrap(),
            hex(inventory.as_bytes()),
            observer.stderr_bytes(),
        ))
        .unwrap()
        .unwrap();
        let exit =
            hex_core::wire::snapshot_exit(hex_api::Exit::Code(0), hex_core::wire::MAX_MESSAGE)
                .unwrap();
        attempt.parse_facts(&mut format!("observer-exit\t1\t{:016x}\t{}\t{}\nobserver-failure\tnone\nobserver-cleanup\tdeleted\n", hex_core::wire::IDENTITY, exit.len(), hex(&exit)).lines()).unwrap();
        receipt.attempt = Some(attempt);
        let identity = shards_api::Identity {
            pid: 42,
            started: 7,
            vsock: "/model/v".into(),
        };
        source.init = Some(hex_core::record::ProcId {
            pid: identity.pid,
            start_ticks: identity.started,
        });
        let mut spec =
            super::super::tests::oci(hex_api::Rootfs::Dir(dir.path().to_path_buf()), Vec::new());
        spec.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
        spec.bounds.memory_max = Some(hex_api::MIN_MEMORY);
        spec.devices = vec![source.devices[0]];
        spec.allocation = Some(allocation);
        spec.route = Some(RouteReceipt {
            key: RouteKey {
                class: Class::Oci,
                image: ImagePlatform {
                    os: ImageOs::Linux,
                    arch: super::super::route::host_platform().unwrap().arch,
                },
                execution: Execution::ShardsDedicated,
                gpu: GpuEligibility::Required,
            },
            backing: Some(source.key),
        });
        let mut boxed =
            hex_core::record::BoxRecord::new(spec, hex_core::record::Phase::Creating, Vec::new());
        boxed.native_scope = Some(hex_core::record::NativeScope::from_bytes([8; 16]));
        let path = runtime.selected_path(&boxed.spec.id).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for phase in [BackingPhase::Live, BackingPhase::Draining] {
            source.phase = phase;
            receipt.phase = if phase == BackingPhase::Live {
                Phase::Running(super::super::shared::copy_identity(&identity).unwrap())
            } else {
                Phase::Stopping(super::super::shared::copy_identity(&identity).unwrap())
            };
            pool::write(&definition.path, &receipt).unwrap();
            runtime.preflight_backing(&source).unwrap();
            for phase in [
                hex_core::record::Phase::Creating,
                hex_core::record::Phase::Deleting,
            ] {
                boxed.phase = phase;
                assert_eq!(
                    runtime.preflight_box(&boxed).is_ok(),
                    phase == hex_core::record::Phase::Creating,
                    "ordinary Deleting must not silently accept a missing receipt"
                );
                runtime.handoff_unsubmitted(&boxed, &source).unwrap();
                let mut intent =
                    super::super::selected::Record::intent(&boxed, &runtime.config).unwrap();
                intent.physical = runtime.catalog_physical(&boxed.spec.devices).unwrap();
                super::super::selected::write(&path, &intent).unwrap();
                assert!(runtime.handoff_unsubmitted(&boxed, &source).is_err());
                super::super::held::remove(&path).unwrap();
            }
        }
        source.phase = BackingPhase::Released;
        source.exit = Some(hex_api::Exit::Code(0));
        receipt.phase = Phase::Released {
            identity: Some(identity),
            status: shards_api::ManagedStatus::Code(0),
        };
        pool::write(&definition.path, &receipt).unwrap();
        runtime.preflight_backing(&source).unwrap();
        for phase in [
            hex_core::record::Phase::Creating,
            hex_core::record::Phase::Deleting,
        ] {
            boxed.phase = phase;
            assert!(runtime.handoff_unsubmitted(&boxed, &source).is_err());
        }
        assert!(runtime.preflight_box(&boxed).is_err());
        assert_eq!(boxed.spec.devices.len(), 1);
        assert_eq!(source.devices.len(), 2);
        assert_eq!(source.allocation, Some(allocation));
        assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
        assert!(runtime.dedicated.lock().unwrap().is_empty());
    }

    /// AC-T7.4.1: an unavailable VM observation cannot erase an initialized
    /// native CPU route or its already-retained backing charge. This original
    /// calls the real composition seam; production stays unchanged until run.
    #[test]
    fn native_oci_routes_and_canonical_claims_survive_qualified_shards_capacity_failure() {
        use hex_api::{
            Class, Execution, GpuEligibility, ImageOs, ImagePlatform, RouteKey, RouteOffer,
        };
        use std::fmt::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(
            b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\n",
        )
        .unwrap();
        let runtime = GuestRuntime::new(standalone.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        let native = triomphe::Arc::try_new(RecoveringNative {
            identity: shards_api::Identity {
                pid: 42,
                started: 7,
                vsock: "/model/v".into(),
            },
            release_on_stop: false,
            active: false,
            released: std::sync::atomic::AtomicBool::new(false),
            attaches: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        })
        .unwrap();
        let owner = Arc::new(Cold {
            row: Mutex::new(None),
            shared: Mutex::new(None),
            native_report: None,
            physical: None,
            effects: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
            native: Some(native.clone()),
            finalized: AtomicUsize::new(0),
            luid: None,
            launches: AtomicUsize::new(0),
        });
        let books: Arc<dyn Books> = owner.clone();
        runtime.bind_books(Arc::downgrade(&books)).unwrap();
        let prepared = runtime.prepare_gpu_backing().unwrap().unwrap();
        let mut row = owner.row.lock().unwrap().take().unwrap();
        row.accept_grant(&BackingGrant {
            key: prepared.key,
            allocation,
            devices: prepared.devices.clone(),
        })
        .unwrap();
        let definition = runtime.gpu_backing_definition().unwrap().unwrap();
        let mut receipt = Receipt::intent(
            PoolRecord::decode(&row.encode().unwrap()).unwrap(),
            definition.physical,
            &runtime.config,
        )
        .unwrap();
        let scope = row.scope();
        let physical = receipt.physical[0];
        receipt.report = Some(shards_api::physical::GpuReport {
            scope: *scope.bytes(),
            probe: shards_api::GpuProbe {
                devices: vec![shards_api::ResolvedGpu {
                    physical,
                    pci: Some(physical),
                    luid: None,
                    uuid: [3; 16],
                    name: None,
                    device_local_bytes: None,
                }],
                host_visible_bytes: 1,
            },
        });
        let inventory = format!(
            "hex-vulkan-guest-readiness 1 1\ndevice 0 4202496 22 1 1 2 2 {} 677075 76656e7573 4d657361 2 1 1 2 1\ndriver 0 {} 1 3 0 0\nheap 0 0 1 4096\nready 1\n",
            "03".repeat(16),
            "04".repeat(16)
        );
        let observer = runtime.gpu_observer.as_ref().unwrap();
        receipt.readiness = Some(observer::Evidence {
            scope,
            configuration: observer.identity(),
            max_queues: observer.queues(),
            report: observer::Report::parse(inventory.as_bytes(), 1, observer.queues()).unwrap(),
        });
        let mut configuration = String::new();
        for byte in observer.identity() {
            write!(&mut configuration, "{byte:02x}").unwrap();
        }
        let mut stdout = String::new();
        for byte in inventory.as_bytes() {
            write!(&mut stdout, "{byte:02x}").unwrap();
        }
        receipt.attempt = observer::Attempt::parse(&format!(
            "{configuration} {} complete eof {} {stdout} eof {} -",
            observer.queues(),
            observer.output_bytes(1).unwrap(),
            observer.stderr_bytes(),
        ))
        .unwrap();
        receipt.phase =
            Phase::Running(super::super::shared::copy_identity(&native.identity).unwrap());
        row.init = Some(hex_core::record::ProcId {
            pid: native.identity.pid,
            start_ticks: native.identity.started,
        });
        row.phase = BackingPhase::Live;
        pool::write(&definition.path, &receipt).unwrap();
        runtime.preflight_backing(&row).unwrap();
        receipt.checked_work_ready().unwrap();
        let original = row.encode().unwrap();
        *owner.row.lock().unwrap() = Some(row);
        let observed = runtime.physical_observations();
        assert!(
            matches!(observed, Err(Refusal::Backing { key, cause: LifecycleCause::Uncertain }) if key == prepared.key)
        );
        assert_eq!(native.attaches.load(Ordering::SeqCst), 1);

        let mut capacity = Books::capacity(owner.as_ref()).unwrap();
        let image = ImagePlatform {
            os: ImageOs::Windows,
            arch: super::super::route::host_platform().unwrap().arch,
        };
        capacity.routes = vec![
            RouteOffer {
                key: RouteKey {
                    class: Class::Process,
                    image,
                    execution: Execution::Native,
                    gpu: GpuEligibility::None,
                },
                cost: None,
                backing: None,
            },
            RouteOffer {
                key: RouteKey {
                    class: Class::Oci,
                    image,
                    execution: Execution::Native,
                    gpu: GpuEligibility::Any,
                },
                cost: None,
                backing: None,
            },
        ];
        capacity.check().unwrap();
        let prefix = capacity.try_clone().unwrap();
        let composed = runtime.add_offers(&mut capacity, Class::Shards);
        assert_eq!(native.attaches.load(Ordering::SeqCst), 2);
        assert_eq!(capacity.id, prefix.id);
        assert_eq!(capacity.total, prefix.total);
        assert_eq!(capacity.free, prefix.free);
        assert_eq!(capacity.routes.get(..2), Some(prefix.routes.as_slice()));
        let backing = capacity
            .backings
            .iter()
            .find(|backing| backing.key == prepared.key)
            .unwrap();
        assert_eq!(backing.devices, prepared.devices);
        assert_eq!(backing.cost, allocation.cost);
        assert_eq!(backing.phase, BackingPhase::Live);
        assert_eq!(
            owner
                .row
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .encode()
                .unwrap(),
            original
        );
        assert_eq!(owner.queries.load(Ordering::SeqCst), 0);
        assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
        assert_eq!(owner.finalized.load(Ordering::SeqCst), 0);
        assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
        assert!(
            composed.is_ok(),
            "qualified GPU observation failure erased an initialized native CPU report: {composed:?}"
        );
        assert!(
            capacity
                .routes
                .iter()
                .any(|offer| offer.key.gpu == GpuEligibility::Required
                    && offer.backing == Some(prepared.key))
        );
        assert!(
            capacity.devices.is_empty(),
            "no actual fresh GPU metadata was observed"
        );
        capacity.check().unwrap();
    }

    /// ORIGINAL AC-T7.4.1: the actual composite OCI entrypoint must normalize its
    /// raw native prefix through SAME Books facts/holders before strict GPU publication.
    /// Inputs are explicit native/renderer/guest models, not execution evidence.
    #[test]
    fn with_guest_normalizes_native_oci_before_checked_shards_slots() {
        use hex_api::{
            Capacity, CapacityResources, Class, DeviceHolder, DeviceInfo, Execution,
            GpuEligibility, ImageOs, ImagePlatform, RouteKey, RouteOffer, Runtime,
        };
        use std::fmt::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(
            b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\n",
        )
        .unwrap();
        let runtime = GuestRuntime::new(standalone.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        let definition = runtime.gpu_backing_definition().unwrap().unwrap();
        let key = BackingKey {
            pool: definition.pool,
            generation: [7; 16],
        };
        let factual = DeviceInfo {
            id: definition.devices[0],
            model: "same-native-provider".into(),
            size: 4096,
            holder: None,
        };
        let mut canonical_info = factual.try_clone().unwrap();
        canonical_info.holder = Some(DeviceHolder::Backing(key));
        let room = CapacityResources {
            cpu_milli: 8000,
            mem_bytes: 1 << 30,
            disk_bytes: 1 << 30,
        };
        let native = triomphe::Arc::try_new(RecoveringNative {
            identity: shards_api::Identity {
                pid: 42,
                started: 7,
                vsock: "/model/v".into(),
            },
            release_on_stop: false,
            active: true,
            released: std::sync::atomic::AtomicBool::new(false),
            attaches: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        })
        .unwrap();
        let owner = Arc::new(Cold {
            row: Mutex::new(None),
            shared: Mutex::new(None),
            native_report: Some(Capacity {
                id: "same-books-native-owner".into(),
                total: room,
                free: room,
                devices: vec![canonical_info.try_clone().unwrap()],
                routes: Vec::new(),
                device_routes: Vec::new(),
                backings: Vec::new(),
                cost: None,
                unavailable: Vec::new(),
                learned: None,
            }),
            physical: Some(vec![factual]),
            effects: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
            native: Some(native.clone()),
            finalized: AtomicUsize::new(0),
            luid: None,
            launches: AtomicUsize::new(0),
        });
        let books: Arc<dyn Books> = owner.clone();
        runtime.bind_books(Arc::downgrade(&books)).unwrap();
        let shared = runtime.prepare_backing().unwrap();
        let prepared = runtime.prepare_gpu_backing().unwrap().unwrap();
        assert_eq!(prepared.key, key);
        assert_ne!(shared.key, key);
        let mut row = owner.row.lock().unwrap().take().unwrap();
        row.accept_grant(&BackingGrant {
            key,
            allocation,
            devices: definition.devices.clone(),
        })
        .unwrap();
        let mut receipt = Receipt::intent(
            PoolRecord::decode(&row.encode().unwrap()).unwrap(),
            definition.physical.clone(),
            &runtime.config,
        )
        .unwrap();
        let scope = row.scope();
        let physical = receipt.physical[0];
        receipt.report = Some(shards_api::physical::GpuReport {
            scope: *scope.bytes(),
            probe: shards_api::GpuProbe {
                devices: vec![shards_api::ResolvedGpu {
                    physical,
                    pci: Some(physical),
                    luid: None,
                    uuid: [3; 16],
                    name: None,
                    device_local_bytes: None,
                }],
                host_visible_bytes: 1,
            },
        });
        let inventory = format!(
            "hex-vulkan-guest-readiness 1 1\ndevice 0 4202496 22 1 1 2 2 {} 677075 76656e7573 4d657361 2 1 1 2 1\ndriver 0 {} 1 3 0 0\nheap 0 0 1 4096\nready 1\n",
            "03".repeat(16),
            "04".repeat(16)
        );
        let observer = runtime.gpu_observer.as_ref().unwrap();
        receipt.readiness = Some(observer::Evidence {
            scope,
            configuration: observer.identity(),
            max_queues: observer.queues(),
            report: observer::Report::parse(inventory.as_bytes(), 1, observer.queues()).unwrap(),
        });
        let mut configuration = String::new();
        for byte in observer.identity() {
            write!(&mut configuration, "{byte:02x}").unwrap();
        }
        let mut stdout = String::new();
        for byte in inventory.as_bytes() {
            write!(&mut stdout, "{byte:02x}").unwrap();
        }
        receipt.attempt = observer::Attempt::parse(&format!(
            "{configuration} {} complete eof {} {stdout} eof {} -",
            observer.queues(),
            observer.output_bytes(1).unwrap(),
            observer.stderr_bytes(),
        ))
        .unwrap();
        receipt.phase =
            Phase::Running(super::super::shared::copy_identity(&native.identity).unwrap());
        row.init = Some(hex_core::record::ProcId {
            pid: native.identity.pid,
            start_ticks: native.identity.started,
        });
        row.phase = BackingPhase::Live;
        pool::write(&definition.path, &receipt).unwrap();
        runtime.preflight_backing(&row).unwrap();
        receipt.checked_work_ready().unwrap();
        *owner.row.lock().unwrap() = Some(row);
        // Prove the actual charged observer/provider/catalog join succeeds before
        // exercising the composite entrypoint; missing readiness is not this oracle.
        assert_eq!(
            runtime
                .checked_physical(&definition.devices, &definition.physical)
                .unwrap(),
            [canonical_info.try_clone().unwrap()]
        );
        assert_eq!(
            Books::capacity(owner.as_ref()).unwrap().devices,
            [canonical_info.try_clone().unwrap()]
        );
        let image = ImagePlatform {
            os: ImageOs::Windows,
            arch: super::super::route::host_platform().unwrap().arch,
        };
        let prefix = Capacity {
            id: "original-native-oci-pool".into(),
            total: room,
            free: CapacityResources {
                cpu_milli: 4000,
                mem_bytes: 1 << 29,
                disk_bytes: 1 << 29,
            },
            devices: vec![DeviceInfo {
                id: canonical_info.id,
                model: "unmeasured".into(),
                size: 777,
                holder: None,
            }],
            routes: vec![
                RouteOffer {
                    key: RouteKey {
                        class: Class::Process,
                        image,
                        execution: Execution::Native,
                        gpu: GpuEligibility::None,
                    },
                    cost: None,
                    backing: None,
                },
                RouteOffer {
                    key: RouteKey {
                        class: Class::Oci,
                        image,
                        execution: Execution::Native,
                        gpu: GpuEligibility::Any,
                    },
                    cost: None,
                    backing: None,
                },
            ],
            device_routes: vec![2],
            backings: Vec::new(),
            cost: None,
            unavailable: Vec::new(),
            learned: None,
        };
        prefix.check().unwrap();
        assert_ne!(
            prefix.devices,
            Books::capacity(owner.as_ref()).unwrap().devices
        );
        let node = super::super::WithGuest::configured(
            super::super::tests::native_reporting(prefix.try_clone().unwrap()),
            runtime,
            |_host, _class| Ok(super::super::Side::Host),
        );
        let result = node.capacity(Class::Oci);
        assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
        assert_eq!(owner.finalized.load(Ordering::SeqCst), 0);
        assert_eq!(native.stops.load(Ordering::SeqCst), 0);
        assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
        assert!(
            result.is_ok(),
            "actual WithGuest normalized its native prefix too late: {result:?}"
        );
        let capacity = result.unwrap().unwrap();
        assert_eq!(capacity.id, prefix.id);
        assert_eq!(capacity.total, prefix.total);
        assert_eq!(capacity.free, prefix.free);
        assert_eq!(capacity.routes[..prefix.routes.len()], prefix.routes);
        assert_eq!(capacity.device_routes[0] & 3, prefix.device_routes[0]);
        assert_eq!(capacity.devices, [canonical_info]);
        assert!(capacity.backings.iter().any(|backing| backing.key == key));
        assert!(
            capacity
                .backings
                .iter()
                .any(|backing| backing.key == shared.key)
        );
        capacity.check().unwrap();
        // An actual native owner assertion cannot be overwritten with the
        // canonical holder: conflicting Some is authority failure, not a placeholder.
        let mut conflicting = prefix;
        conflicting.devices[0].holder = Some(DeviceHolder::Box(
            hex_api::BoxId::new("foreign-native-owner").unwrap(),
        ));
        conflicting.check().unwrap();
        let conflict = super::super::WithGuest::configured(
            super::super::tests::native_reporting(conflicting),
            node.oci,
            |_host, _class| Ok(super::super::Side::Host),
        );
        assert!(conflict.capacity(Class::Oci).is_err());
        assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
        assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
    }

    /// AC-T7.4.1: family/set identity is configuration, not another selection
    /// authority. Retained exact subsets keep their own path/key; a full-set
    /// legacy record keeps its old path and cannot be transplanted onto R.
    #[test]
    fn retained_gpu_subsets_bind_exact_family_key_and_keep_legacy_full_set_path() {
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\ngpu\t512\tunmeasured\t777\tshards-gpu\tpci:0000:02:00.0\t\n").unwrap();
        let runtime = GuestRuntime::new(standalone.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        let family = runtime.gpu_backing_definition().unwrap().unwrap();
        assert_eq!(family.path, dir.path().join("guest.gpu-observer"));
        let explicit_full = runtime
            .gpu_backing_definition_for(Some(&family.devices))
            .unwrap()
            .unwrap();
        assert_eq!(family.pool, explicit_full.pool);
        assert_eq!(family.path, explicit_full.path);
        for devices in [&family.devices[..1], &family.devices[1..]] {
            let definition = runtime
                .gpu_backing_definition_for(Some(devices))
                .unwrap()
                .unwrap();
            assert_ne!(definition.pool, family.pool);
            assert_ne!(definition.path, family.path);
            assert_eq!(definition.devices, devices);
            assert_eq!(definition.cost, family.cost);
            let repeated = runtime
                .gpu_backing_definition_for(Some(devices))
                .unwrap()
                .unwrap();
            assert_eq!(definition.pool, repeated.pool);
            assert_eq!(definition.path, repeated.path);
            let mut record = PoolRecord {
                version: hex_core::record::RECORD_VERSION,
                key: BackingKey {
                    pool: definition.pool,
                    generation: [7; 16],
                },
                execution: hex_api::Execution::ShardsDedicated,
                cost: allocation.cost,
                allocation: None,
                devices: devices.to_vec(),
                phase: BackingPhase::Prepared,
                init: None,
                exit: None,
            };
            runtime.preflight_backing(&record).unwrap();
            let retained = runtime.gpu_definition_for_record(&record).unwrap();
            assert_eq!(retained.pool, definition.pool);
            assert_eq!(retained.path, definition.path);
            record.key.pool = family.pool;
            assert!(
                runtime.preflight_backing(&record).is_err(),
                "a legacy full-set identity cannot authorize a different subset"
            );
        }
        for bad in [
            Vec::new(),
            vec![family.devices[0], family.devices[0]],
            vec![family.devices[1], family.devices[0]],
            vec![hex_api::DeviceId {
                kind: hex_api::DeviceKind::Gpu,
                key: 768,
            }],
        ] {
            assert!(runtime.gpu_backing_definition_for(Some(&bad)).is_err());
        }
        assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
        assert!(runtime.dedicated.lock().unwrap().is_empty());
        assert!(!family.path.exists());
    }

    /// AC-T7.4.1: global route storage counts actual initialized aliases and
    /// historical lifetimes without querying capacity, a provider or a receipt.
    #[test]
    fn pure_route_projection_keeps_retained_gpu_aliases_and_new_cpu_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\ngpu\t512\tunmeasured\t777\tshards-gpu\tpci:0000:02:00.0\t\n").unwrap();
        let runtime = GuestRuntime::new(standalone.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        let owner = Arc::new(Cold {
            row: Mutex::new(None),
            shared: Mutex::new(None),
            native_report: None,
            physical: None,
            effects: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
            native: None,
            finalized: AtomicUsize::new(0),
            luid: None,
            launches: AtomicUsize::new(0),
        });
        let books: Arc<dyn Books> = owner.clone();
        runtime.bind_books(Arc::downgrade(&books)).unwrap();
        let definition = runtime.gpu_backing_definition().unwrap().unwrap();
        let mut parent = PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: definition.pool,
                generation: [7; 16],
            },
            execution: hex_api::Execution::ShardsDedicated,
            cost: allocation.cost,
            allocation: Some(allocation),
            devices: definition.devices,
            phase: BackingPhase::Released,
            init: Some(hex_core::record::ProcId {
                pid: 42,
                start_ticks: 7,
            }),
            exit: Some(hex_api::Exit::Code(7)),
        };
        parent.check().unwrap();
        let remaining = vec![parent.devices[1]];
        let subset = runtime
            .gpu_backing_definition_for(Some(&remaining))
            .unwrap()
            .unwrap();
        let current = PoolRecord {
            key: BackingKey {
                pool: subset.pool,
                generation: [8; 16],
            },
            allocation: None,
            devices: remaining,
            phase: BackingPhase::Prepared,
            init: None,
            exit: None,
            ..PoolRecord::decode(&parent.encode().unwrap()).unwrap()
        };
        current.check().unwrap();
        // Deliberately malformed local bytes cannot be touched by this pure
        // projection. The separate whole-owner preflight must reject them.
        super::super::held::replace(&definition.path, b"not a process receipt").unwrap();
        assert!(runtime.preflight_backing(&parent).is_err());
        let rows = vec![
            PoolRecord::decode(&parent.encode().unwrap()).unwrap(),
            current,
        ];
        let mut projection = devices::RouteProjection::new();
        runtime
            .project_routes(
                &rows,
                Some((hex_api::Execution::ShardsShared, &[])),
                &mut projection,
            )
            .unwrap();
        // Fixed dedicated CPU +two actual aliases for EACH retained GPU
        // lifetime +one new shared-CPU lifetime, not a guessed per-class cap.
        assert_eq!(projection.count().unwrap(), 6);
        runtime
            .project_routes(&rows, None, &mut projection)
            .unwrap();
        assert_eq!(projection.count().unwrap(), 6);
        parent.cost.id = [9; 32];
        assert!(
            runtime
                .project_routes(&[parent], None, &mut devices::RouteProjection::new())
                .is_err()
        );
        let foreign = [hex_api::DeviceId {
            kind: hex_api::DeviceKind::Gpu,
            key: 768,
        }];
        assert!(
            runtime
                .project_routes(
                    &rows,
                    Some((hex_api::Execution::ShardsDedicated, &foreign)),
                    &mut devices::RouteProjection::new()
                )
                .is_err()
        );
        assert_eq!(owner.queries.load(Ordering::SeqCst), 0);
        assert_eq!(owner.effects.load(Ordering::SeqCst), 0);
        assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
        assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
        assert!(owner.row.lock().unwrap().is_none());
    }

    /// AC-T7.4.1: one deterministic remainder is recomputed from canonical
    /// workload/native claims, and grows only across drain/finalize/Index-ACK.
    /// This pure control supplies phases as model inputs, never release proof.
    #[test]
    fn lazy_gpu_family_preserves_subset_parent_and_coalescing_ack_barriers() {
        use super::super::family::Proposal;
        use hex_api::{
            Class, DeviceId, DeviceKind, Execution, GpuEligibility, ImageOs, ImagePlatform,
            RouteKey, RouteReceipt,
        };
        use hex_core::record::{BoxRecord, Phase as BoxPhase};
        fn copy(row: &PoolRecord) -> PoolRecord {
            PoolRecord::decode(&row.encode().unwrap()).unwrap()
        }
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\ngpu\t512\tunmeasured\t777\tshards-gpu\tpci:0000:02:00.0\t\n").unwrap();
        let runtime = GuestRuntime::new(standalone.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        assert!(
            !runtime.uses_books(),
            "pure reconciliation cannot call Books"
        );
        let family = runtime.gpu_backing_definition().unwrap().unwrap();
        let empty = Records::new();
        assert!(
            matches!(runtime.gpu_proposal(&family, &empty, &[]).unwrap(), Proposal::Prepare { pool, devices, current: None } if devices == family.devices && pool == family.pool)
        );
        let mut parent = PoolRecord {
            version: hex_core::record::RECORD_VERSION,
            key: BackingKey {
                pool: family.pool,
                generation: [7; 16],
            },
            execution: Execution::ShardsDedicated,
            cost: allocation.cost,
            allocation: None,
            devices: family.devices.clone(),
            phase: BackingPhase::Prepared,
            init: None,
            exit: None,
        };
        assert!(
            matches!(runtime.gpu_proposal(&family, &empty, &[copy(&parent)]).unwrap(), Proposal::Prepare { devices, current: Some(_), .. } if devices == family.devices)
        );
        parent
            .accept_grant(&BackingGrant {
                key: parent.key,
                allocation,
                devices: parent.devices.clone(),
            })
            .unwrap();
        parent.init = Some(hex_core::record::ProcId {
            pid: 42,
            start_ticks: 7,
        });
        parent.phase = BackingPhase::Live;
        let mut spec =
            super::super::tests::oci(hex_api::Rootfs::Dir(dir.path().to_path_buf()), Vec::new());
        spec.id = hex_api::BoxId::new("selected-family-consumer").unwrap();
        spec.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
        spec.bounds.memory_max = Some(hex_api::MIN_MEMORY);
        spec.devices = vec![parent.devices[0]];
        spec.allocation = Some(allocation);
        let image = ImagePlatform {
            os: ImageOs::Linux,
            arch: super::super::route::host_platform().unwrap().arch,
        };
        spec.route = Some(RouteReceipt {
            key: RouteKey {
                class: Class::Oci,
                image,
                execution: Execution::ShardsDedicated,
                gpu: GpuEligibility::Required,
            },
            backing: Some(parent.key),
        });
        spec.check().unwrap();
        let mut boxed = BoxRecord::new(spec, BoxPhase::Creating, Vec::new());
        boxed.native_scope = Some(hex_core::record::NativeScope::from_bytes([8; 16]));
        let mut records = Records::new();
        records.insert(boxed.spec.id.clone(), Ok(boxed.clone()));
        // Referenced Live parent keeps FULL P; its unselected sibling cannot be
        // provisioned until that same scope is actually Released.
        assert!(matches!(
            runtime
                .gpu_proposal(&family, &records, &[copy(&parent)])
                .unwrap(),
            Proposal::Empty
        ));
        parent.phase = BackingPhase::Released;
        parent.exit = Some(hex_api::Exit::Code(7));
        let remaining = vec![parent.devices[1]];
        let (subset_pool, subset_devices) = match runtime
            .gpu_proposal(&family, &records, &[copy(&parent)])
            .unwrap()
        {
            Proposal::Prepare {
                pool,
                devices,
                current: None,
            } => (pool, devices),
            _ => panic!("Released referenced parent did not expose exact complement"),
        };
        assert_eq!(subset_devices, remaining);
        assert_ne!(subset_pool, parent.key.pool);
        let mut current = PoolRecord {
            key: BackingKey {
                pool: subset_pool,
                generation: [9; 16],
            },
            devices: subset_devices,
            phase: BackingPhase::Prepared,
            allocation: None,
            init: None,
            exit: None,
            ..copy(&parent)
        };
        assert!(
            matches!(runtime.gpu_proposal(&family, &records, &[copy(&parent), copy(&current)]).unwrap(), Proposal::Prepare { devices, current: Some(_), .. } if devices == remaining)
        );
        current
            .accept_grant(&BackingGrant {
                key: current.key,
                allocation,
                devices: current.devices.clone(),
            })
            .unwrap();
        current.init = Some(hex_core::record::ProcId {
            pid: 43,
            start_ticks: 9,
        });
        current.phase = BackingPhase::Live;
        assert_eq!(
            runtime
                .gpu_fresh_candidate(&records, &[copy(&parent), copy(&current)])
                .unwrap()
                .unwrap()
                .key,
            current.key
        );
        assert!(
            runtime
                .gpu_fresh_candidate(&empty, &[copy(&parent), copy(&current)])
                .unwrap()
                .is_none(),
            "changed R stayed fresh while waiting for an older ACK"
        );
        // Box removal grows desired R, but no-ref Released P must be ACKed
        // first. Existing live R is untouched and still reusable while P waits.
        assert!(
            matches!(runtime.gpu_proposal(&family, &empty, &[copy(&parent), copy(&current)]).unwrap(), Proposal::AwaitAck(row) if row.key == parent.key)
        );
        assert!(
            matches!(runtime.gpu_proposal(&family, &records, &[copy(&parent), copy(&current)]).unwrap(), Proposal::Prepare { current: Some(row), devices, .. } if row.key == current.key && devices == remaining)
        );
        assert!(
            matches!(runtime.gpu_proposal(&family, &empty, &[copy(&current)]).unwrap(), Proposal::Prepare { current: Some(row), pool, .. } if row.key == current.key && pool == family.pool)
        );
        current.phase = BackingPhase::Draining;
        assert!(
            matches!(runtime.gpu_proposal(&family, &empty, &[copy(&current)]).unwrap(), Proposal::Prepare { current: Some(row), .. } if row.phase == BackingPhase::Draining)
        );
        current.phase = BackingPhase::Released;
        current.exit = Some(hex_api::Exit::Code(7));
        assert!(
            matches!(runtime.gpu_proposal(&family, &empty, &[copy(&current)]).unwrap(), Proposal::AwaitAck(row) if row.key == current.key)
        );
        assert!(
            matches!(runtime.gpu_proposal(&family, &empty, &[]).unwrap(), Proposal::Prepare { current: None, pool, .. } if pool == family.pool)
        );
        // An uncharged Prepared can lose all unheld IDs to actual native work.
        // Empty is a drain/cancel proposal only, never a new GPU Pool/route.
        let mut native = boxed.clone();
        native.spec.id = hex_api::BoxId::new("actual-native-gpu-workload").unwrap();
        native.spec.devices = parent.devices.clone();
        native.spec.allocation = None;
        native.spec.route = Some(RouteReceipt {
            key: RouteKey {
                execution: Execution::Native,
                gpu: GpuEligibility::Any,
                ..boxed.spec.route.unwrap().key
            },
            backing: None,
        });
        native.spec.check().unwrap();
        let mut native_records = Records::new();
        native_records.insert(native.spec.id.clone(), Ok(native));
        let prepared = PoolRecord {
            allocation: None,
            init: None,
            exit: None,
            phase: BackingPhase::Prepared,
            ..copy(&parent)
        };
        assert!(
            matches!(runtime.gpu_proposal(&family, &native_records, &[copy(&prepared)]).unwrap(), Proposal::Drain { current, pool } if current.key == prepared.key && pool == runtime.gpu_backing_pool(allocation.cost, &[], &[]).unwrap())
        );
        let cancelled = PoolRecord {
            phase: BackingPhase::Released,
            ..copy(&prepared)
        };
        runtime.preflight_backing(&cancelled).unwrap();
        assert!(
            matches!(runtime.gpu_proposal(&family, &empty, &[copy(&cancelled)]).unwrap(), Proposal::AwaitAck(row) if row.key == cancelled.key)
        );
        assert!(
            runtime.handoff_source_released(&cancelled).is_err(),
            "unsubmitted cancellation invented a native terminal witness"
        );
        // Canonical max-one-current and max-one consumer are independent guards.
        let second = PoolRecord {
            key: BackingKey {
                pool: subset_pool,
                generation: [10; 16],
            },
            devices: vec![DeviceId {
                kind: DeviceKind::Gpu,
                key: 512,
            }],
            ..copy(&prepared)
        };
        assert!(
            runtime
                .gpu_proposal(&family, &empty, &[copy(&prepared), second])
                .is_err()
        );
        let mut other = boxed;
        other.spec.id = hex_api::BoxId::new("second-consumer").unwrap();
        records.insert(other.spec.id.clone(), Ok(other));
        assert!(
            runtime
                .gpu_proposal(&family, &records, &[copy(&parent)])
                .is_err()
        );
        assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
    }

    struct Cold {
        row: Mutex<Option<PoolRecord>>,
        shared: Mutex<Option<PoolRecord>>,
        native_report: Option<hex_api::Capacity>,
        physical: Option<Vec<hex_api::DeviceInfo>>,
        effects: AtomicUsize,
        queries: AtomicUsize,
        native: Option<triomphe::Arc<RecoveringNative>>,
        finalized: AtomicUsize,
        luid: Option<[u8; 8]>,
        launches: AtomicUsize,
    }
    impl Books for Cold {
        fn with_handoff_transaction(
            &self,
            _: &hex_api::BoxId,
            _: &hex_api::BackingKey,
            _: &[hex_api::DeviceId],
            _: &mut dyn FnMut(&dyn crate::devices::Transaction) -> Result<(), hex_api::Refusal>,
        ) -> Result<(), hex_api::Refusal> {
            panic!("existing test owner must not enter a subset handoff")
        }

        fn records(&self) -> Result<Records, Refusal> {
            Ok(Records::new())
        }
        fn pool_records(&self) -> Result<PoolRecords, Refusal> {
            let mut rows = Vec::new();
            for slot in [&self.row, &self.shared] {
                if let Some(row) = slot.lock().unwrap().as_ref() {
                    rows.push(PoolRecord::decode(&row.encode()?)?);
                }
            }
            Ok(rows)
        }
        fn prepare_pool(
            &self,
            pool: [u8; 32],
            execution: hex_api::Execution,
            cost: hex_api::RuntimeCost,
            devices: &[hex_api::DeviceId],
            family: (&[u8; 32], &[hex_api::DeviceId], Option<&PoolRecord>),
            validate: &mut crate::devices::PoolPreparationCheck<'_>,
        ) -> Result<PoolRecord, Refusal> {
            let snapshot = Books::pool_records(self)?;
            assert_eq!(snapshot.iter().find(|row| row.key.pool == pool), family.2);
            validate(&Books::records(self)?, &snapshot)?;
            let slot = if execution == hex_api::Execution::ShardsShared {
                &self.shared
            } else {
                &self.row
            };
            let mut row = slot.lock().unwrap();
            if row.is_none() {
                *row = Some(PoolRecord {
                    version: hex_core::record::RECORD_VERSION,
                    key: BackingKey {
                        pool,
                        generation: [7; 16],
                    },
                    execution,
                    cost,
                    devices: devices.to_vec(),
                    allocation: None,
                    phase: BackingPhase::Prepared,
                    init: None,
                    exit: None,
                });
            }
            PoolRecord::decode(&row.as_ref().ok_or_else(refused_model)?.encode()?)
        }
        fn capacity(&self) -> Result<hex_api::Capacity, Refusal> {
            if let Some(report) = &self.native_report {
                return report.try_clone();
            }
            let room = hex_api::CapacityResources {
                cpu_milli: 8000,
                mem_bytes: 1 << 30,
                disk_bytes: 1 << 30,
            };
            Ok(hex_api::Capacity {
                id: "cold-model".into(),
                total: room,
                free: room,
                devices: Vec::new(),
                routes: Vec::new(),
                device_routes: Vec::new(),
                backings: Vec::new(),
                cost: None,
                unavailable: Vec::new(),
                learned: None,
            })
        }
        fn physical_devices(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            self.physical
                .as_ref()
                .ok_or_else(refused_model)?
                .iter()
                .map(hex_api::DeviceInfo::try_clone)
                .collect()
        }
        fn physical_luid(&self, _: hex_api::DeviceId) -> Result<[u8; 8], Refusal> {
            self.queries.fetch_add(1, Ordering::SeqCst);
            self.luid.ok_or_else(refused_model)
        }
        fn with_transaction(
            &self,
            _: &hex_api::BoxId,
            _: &[hex_api::DeviceId],
            _: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            self.effects.fetch_add(1, Ordering::SeqCst);
            Err(refused_model())
        }
        fn with_pool_transaction(
            &self,
            key: &BackingKey,
            devices: &[hex_api::DeviceId],
            callback: &mut dyn FnMut(&dyn Transaction) -> Result<(), Refusal>,
        ) -> Result<(), Refusal> {
            self.effects.fetch_add(1, Ordering::SeqCst);
            if self.native.is_none() {
                return Err(refused_model());
            }
            {
                let row = self.row.lock().unwrap();
                let row = row.as_ref().unwrap();
                assert_eq!(row.key, *key);
                assert!(row.devices.iter().all(|id| devices.contains(id)));
            }
            callback(self)
        }
    }

    struct RecoveringNative {
        identity: shards_api::Identity,
        release_on_stop: bool,
        active: bool,
        released: std::sync::atomic::AtomicBool,
        attaches: AtomicUsize,
        stops: AtomicUsize,
    }

    impl shards_api::ManagedProcess for RecoveringNative {
        fn identity(&self) -> Result<&shards_api::Identity, shards_api::Refusal> {
            Ok(&self.identity)
        }
        fn stop(&self) -> Result<(), shards_api::Refusal> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            if self.release_on_stop {
                self.released.store(true, Ordering::SeqCst);
            }
            Ok(())
        }
        fn terminal(&self) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
            if self.active && !self.released.load(Ordering::SeqCst) {
                return Ok(None);
            }
            Ok(Some(shards_api::ManagedEnd {
                status: shards_api::ManagedStatus::Code(7),
                scope: if self.released.load(Ordering::SeqCst) {
                    shards_api::TerminalScope::Released
                } else {
                    shards_api::TerminalScope::Process
                },
            }))
        }
        fn wait_for(
            &self,
            _: std::time::Duration,
        ) -> Result<Option<shards_api::ManagedEnd>, shards_api::Refusal> {
            Ok(self
                .terminal()?
                .filter(|end| end.scope == shards_api::TerminalScope::Released))
        }
    }

    impl Transaction for Cold {
        fn check_unsubmitted_pool(&self) -> Result<(), Refusal> {
            let held = self.row.lock().unwrap();
            let row = held.as_ref().ok_or_else(refused_model)?;
            if row.phase != BackingPhase::Prepared
                || row.allocation.is_some()
                || row.init.is_some()
                || row.exit.is_some()
                || self.native.is_some()
                || self.launches.load(Ordering::SeqCst) != 0
            {
                return Err(refused_model());
            }
            Ok(())
        }
        fn records(&self) -> Result<Records, Refusal> {
            Books::records(self)
        }
        fn pool_records(&self) -> Result<PoolRecords, Refusal> {
            Books::pool_records(self)
        }
        fn read_pool(&self, key: &BackingKey) -> Result<Option<PoolRecord>, Refusal> {
            Ok(Books::pool_records(self)?
                .into_iter()
                .find(|row| row.key == *key))
        }
        fn put_pool(&self, next: &PoolRecord) -> Result<(), Refusal> {
            let mut row = self.row.lock().unwrap();
            row.as_ref().unwrap().accepts(next)?;
            *row = Some(PoolRecord::decode(&next.encode()?)?);
            Ok(())
        }
        fn grant_pool(&self, grant: &BackingGrant) -> Result<PoolRecord, Refusal> {
            let mut row = Transaction::read_pool(self, &grant.key)?.ok_or_else(refused_model)?;
            row.accept_grant(grant)?;
            Transaction::put_pool(self, &row)?;
            Ok(row)
        }
        fn remove_pool(&self, _: &BackingKey) -> Result<(), Refusal> {
            Err(refused_model())
        }
        fn read(&self, _: &hex_api::BoxId) -> Result<Option<hex_core::record::BoxRecord>, Refusal> {
            Err(refused_model())
        }
        fn put(&self, _: &hex_core::record::BoxRecord) -> Result<(), Refusal> {
            Err(refused_model())
        }
        fn remove(&self, _: &hex_api::BoxId) -> Result<(), Refusal> {
            Err(refused_model())
        }
        fn launch_managed(
            &self,
            _: &devices::ManagedLaunch<'_>,
            _: shards_api::LaunchIo,
            _: std::time::Duration,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            self.launches.fetch_add(1, Ordering::SeqCst);
            Err(refused_model())
        }
        fn attach_managed(
            &self,
            identity: Option<&shards_api::Identity>,
        ) -> Result<shards_api::ManagedReceipt, Refusal> {
            let native = self.native.as_ref().ok_or_else(refused_model)?;
            let row = self.row.lock().unwrap();
            let row = row.as_ref().unwrap();
            if row.phase == BackingPhase::Live {
                assert_eq!(identity, Some(&native.identity));
            } else {
                assert!(
                    identity.is_none(),
                    "the retained Intent has no local controller publication"
                );
            }
            managed::same_native(NativeRecord::Pool(row), &native.identity)?;
            native.attaches.fetch_add(1, Ordering::SeqCst);
            Ok(shards_api::managed_receipt(native.clone()))
        }
        fn finalize_managed(&self, identity: Option<&shards_api::Identity>) -> Result<(), Refusal> {
            let native = self.native.as_ref().ok_or_else(refused_model)?;
            assert!(identity.is_none());
            assert!(native.released.load(Ordering::SeqCst));
            self.finalized.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn managed_usage(&self) -> Result<Usage, Refusal> {
            Err(refused_model())
        }
    }

    struct NoEffects(AtomicUsize);
    impl shards_api::Guests for NoEffects {
        fn backend(&self) -> Result<shards_api::Backend, shards_api::Refusal> {
            Ok(shards_api::Backend::Kvm)
        }
        fn probe_gpu(
            &self,
            _: &shards_api::GuestSpec,
            _: std::time::Duration,
        ) -> Result<shards_api::GpuProbe, shards_api::Refusal> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(shards_api::Refusal::GpuProbe(
                shards_api::GpuProbeError::Unavailable,
            ))
        }
        fn boot(
            &self,
            _: &shards_api::GuestSpec,
        ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(shards_api::Refusal::Launch(
                shards_api::LaunchError::Observer,
            ))
        }
        fn attach(
            &self,
            _: &shards_api::Identity,
            _: &shards_api::GuestSpec,
        ) -> Result<Box<dyn shards_api::Guest>, shards_api::Refusal> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(shards_api::Refusal::Launch(
                shards_api::LaunchError::Observer,
            ))
        }
    }

    /// AC-T7.4.1: start a node whose guest serves its dedicated GPU pool alone, as a Linux node's does
    /// (its OCI boxes run natively, so its start prepares no shared backing): prepare the GPU family as
    /// its start does, then recover as its start does (recover_backing, for each retained pool).
    /// Expect Ok, the pool still Prepared, and nothing launched, attached, probed or booted.
    #[test]
    fn a_fresh_dedicated_gpu_node_recovers_with_no_shared_backing_to_recover() {
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(
            b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\n",
        )
        .unwrap();
        let runtime = GuestRuntime::new(standalone.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        let owner = Arc::new(Cold {
            row: Mutex::new(None),
            shared: Mutex::new(None),
            native_report: None,
            physical: None,
            effects: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
            native: None,
            finalized: AtomicUsize::new(0),
            luid: None,
            launches: AtomicUsize::new(0),
        });
        let books: Arc<dyn Books> = owner.clone();
        runtime.bind_books(Arc::downgrade(&books)).unwrap();
        let prepared = runtime.prepare_gpu_backing().unwrap().unwrap();
        let recovered = runtime.recover_backing();
        assert!(recovered.is_ok(), "{recovered:?}");
        let row = Books::pool_records(owner.as_ref()).unwrap().pop().unwrap();
        assert_eq!(row.key, prepared.key);
        assert_eq!(row.execution, hex_api::Execution::ShardsDedicated);
        assert_eq!(row.phase, BackingPhase::Prepared);
        assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
        assert_eq!(owner.finalized.load(Ordering::SeqCst), 0);
        assert_eq!(owner.queries.load(Ordering::SeqCst), 0);
        assert_eq!(
            standalone.0.load(Ordering::SeqCst),
            0,
            "recovery boots, attaches or probes nothing"
        );
    }

    /// AC-T7.4.1: crash after the SAME native owner publishes its controller,
    /// before local report/Running, still cleans that admitted scope. Controller
    /// exit alone retains the grant; only a whole-scope witness releases it.
    /// Original production rejection is deliberately unchanged by this control.
    #[test]
    fn published_gpu_scope_with_retained_intent_recovers_without_a_second_launch() {
        for release_on_stop in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (config, input, allocation) = observer::config::tests::fixture(dir.path());
            let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
            let catalog = devices::Catalog::parse(
                b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tpci:0000:01:00.0\t\n",
            )
            .unwrap();
            let runtime = GuestRuntime::new(standalone.clone(), config)
                .recorded_at(dir.path().join("guest"))
                .with_runtime_cost(allocation.cost)
                .unwrap()
                .with_runtime_paths(
                    shards_api::RuntimePaths::new(
                        dir.path().join("cache"),
                        dir.path().join("home"),
                    )
                    .unwrap(),
                )
                .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
                .with_gpu_observer(input)
                .unwrap();
            let native = triomphe::Arc::try_new(RecoveringNative {
                identity: shards_api::Identity {
                    pid: 42,
                    started: 7,
                    vsock: "/model/v".into(),
                },
                release_on_stop,
                active: false,
                released: std::sync::atomic::AtomicBool::new(false),
                attaches: AtomicUsize::new(0),
                stops: AtomicUsize::new(0),
            })
            .unwrap();
            let owner = Arc::new(Cold {
                row: Mutex::new(None),
                shared: Mutex::new(None),
                native_report: None,
                physical: None,
                effects: AtomicUsize::new(0),
                queries: AtomicUsize::new(0),
                native: Some(native.clone()),
                finalized: AtomicUsize::new(0),
                luid: None,
                launches: AtomicUsize::new(0),
            });
            let books: Arc<dyn Books> = owner.clone();
            runtime.bind_books(Arc::downgrade(&books)).unwrap();
            let prepared = runtime.prepare_gpu_backing().unwrap().unwrap();
            let mut original = Books::pool_records(owner.as_ref()).unwrap().pop().unwrap();
            original
                .accept_grant(&BackingGrant {
                    key: prepared.key,
                    allocation,
                    devices: prepared.devices.clone(),
                })
                .unwrap();
            let definition = runtime.gpu_backing_definition().unwrap().unwrap();
            let receipt = Receipt::intent(
                PoolRecord::decode(&original.encode().unwrap()).unwrap(),
                definition.physical,
                &runtime.config,
            )
            .unwrap();
            pool::write(&definition.path, &receipt).unwrap();
            // This is the precise publication/crash cut. No native process or
            // guest Program runs; the model's SAME row supplies canonical scope.
            original.init = Some(hex_core::record::ProcId {
                pid: native.identity.pid,
                start_ticks: native.identity.started,
            });
            *owner.row.lock().unwrap() = Some(original);
            let recovered = runtime.recover_gpu_backing();
            let row = Books::pool_records(owner.as_ref()).unwrap().pop().unwrap();
            let retained = pool::read(&definition.path).unwrap().unwrap();
            assert!(
                native.attaches.load(Ordering::SeqCst) > 0,
                "Intent must not prevent cleanup of the original published scope"
            );
            assert_eq!(native.stops.load(Ordering::SeqCst), 1);
            assert_eq!(
                standalone.0.load(Ordering::SeqCst),
                0,
                "recovery cannot boot or probe again"
            );
            assert_eq!(owner.queries.load(Ordering::SeqCst), 0);
            assert_eq!(row.devices, prepared.devices);
            assert_eq!(row.allocation, Some(allocation));
            if release_on_stop {
                assert!(recovered.is_ok());
                assert_eq!(row.phase, BackingPhase::Released);
                assert!(matches!(retained.phase, Phase::Released { .. }));
                assert_eq!(owner.finalized.load(Ordering::SeqCst), 1);
            } else {
                assert!(recovered.is_err());
                assert_eq!(row.phase, BackingPhase::Draining);
                assert!(matches!(retained.phase, Phase::Intent));
                assert_eq!(
                    owner.finalized.load(Ordering::SeqCst),
                    0,
                    "controller-only exit cannot release physical claims"
                );
            }
        }
    }

    /// AC-T7.4.1: grant a backing configured by a current-boot LUID while the
    /// SAME provider reports a stale, an unknown or the matching LUID for its
    /// stable claim; expect the launcher reached only on the match. Windows
    /// assigns LUIDs per boot, so this runs on every target.
    #[test]
    fn current_luid_join_precedes_charged_native_gpu_launch() {
        for current in [Some(10u64.to_le_bytes()), None, Some(9u64.to_le_bytes())] {
            let dir = super::super::tests::socket_fixture();
            let (config, input, allocation) = observer::config::tests::fixture(dir.path());
            let standalone = Arc::new(NoEffects(AtomicUsize::new(0)));
            let catalog = devices::Catalog::parse(
                b"hex-devices-1\ngpu\t256\tunmeasured\t777\tshards-gpu\tluid:0000000000000009\t\n",
            )
            .unwrap();
            let runtime = GuestRuntime::new(standalone.clone(), config)
                .recorded_at(dir.path().join("g"))
                .with_runtime_cost(allocation.cost)
                .unwrap()
                .with_runtime_paths(
                    shards_api::RuntimePaths::new(
                        dir.path().join("cache"),
                        dir.path().join("home"),
                    )
                    .unwrap(),
                )
                .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
                .with_gpu_observer(input)
                .unwrap();
            let owner = Arc::new(Cold {
                row: Mutex::new(None),
                shared: Mutex::new(None),
                native_report: None,
                physical: None,
                effects: AtomicUsize::new(0),
                queries: AtomicUsize::new(0),
                native: Some(
                    triomphe::Arc::try_new(RecoveringNative {
                        identity: shards_api::Identity {
                            pid: 42,
                            started: 7,
                            vsock: "/model/v".into(),
                        },
                        release_on_stop: false,
                        active: false,
                        released: std::sync::atomic::AtomicBool::new(false),
                        attaches: AtomicUsize::new(0),
                        stops: AtomicUsize::new(0),
                    })
                    .unwrap(),
                ),
                finalized: AtomicUsize::new(0),
                luid: current,
                launches: AtomicUsize::new(0),
            });
            let books: Arc<dyn Books> = owner.clone();
            runtime.bind_books(Arc::downgrade(&books)).unwrap();
            let prepared = runtime.prepare_gpu_backing().unwrap().unwrap();
            assert_eq!(
                owner.queries.load(Ordering::SeqCst),
                0,
                "cold preparation cannot query an ungranted GPU"
            );
            assert_eq!(owner.launches.load(Ordering::SeqCst), 0);
            let result = runtime.initialize_gpu_pool(BackingGrant {
                key: prepared.key,
                allocation,
                devices: prepared.devices.clone(),
            });
            let row = Books::pool_records(owner.as_ref()).unwrap().pop().unwrap();
            let allowed = current == Some(9u64.to_le_bytes());
            assert_eq!(
                owner.launches.load(Ordering::SeqCst),
                usize::from(allowed),
                "stale or unknown LUID reached the canonical native launcher"
            );
            assert_eq!(
                owner.queries.load(Ordering::SeqCst),
                1,
                "the SAME initialized provider must establish the preboot join"
            );
            if allowed {
                // The model launcher refuses after the join: no VM or readiness.
                assert_eq!(result, Err(refused_model()));
            } else {
                assert_eq!(
                    result,
                    Err(refused(prepared.key, LifecycleCause::Uncertain))
                );
                assert!(
                    pool::read(&runtime.gpu_backing_definition().unwrap().unwrap().path)
                        .unwrap()
                        .is_none(),
                    "a refused join leaves no local launch Intent"
                );
            }
            assert_eq!(row.phase, BackingPhase::Granted);
            assert_eq!(row.init, None);
            assert_eq!(row.allocation, Some(allocation));
            assert_eq!(row.devices, prepared.devices);
            assert_eq!(standalone.0.load(Ordering::SeqCst), 0);
            assert_eq!(owner.finalized.load(Ordering::SeqCst), 0);
        }
    }

    /// AC-T7.4.1: cold full-set provisioning reaches the real capacity/bootstrap
    /// entrypoints without GPU observations, a boot or invented DeviceInfo.
    #[test]
    fn cold_gpu_backing_reserves_only_configured_claims_before_any_observation() {
        use hex_api::Runtime as _;
        let dir = tempfile::tempdir().unwrap();
        let (config, input, allocation) = observer::config::tests::fixture(dir.path());
        let native = Arc::new(NoEffects(AtomicUsize::new(0)));
        let catalog = devices::Catalog::parse(b"hex-devices-1\ngpu\t256\tunmeasured-config-only\t777\tshards-gpu\tpci:0000:01:00.0\t\n").unwrap();
        let runtime = GuestRuntime::new(native.clone(), config)
            .recorded_at(dir.path().join("guest"))
            .with_runtime_cost(allocation.cost)
            .unwrap()
            .with_runtime_paths(
                shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home"))
                    .unwrap(),
            )
            .with_gpu_catalog(triomphe::Arc::try_new(catalog).unwrap())
            .with_gpu_observer(input)
            .unwrap();
        let owner = Arc::new(Cold {
            row: Mutex::new(None),
            shared: Mutex::new(None),
            native_report: None,
            physical: None,
            effects: AtomicUsize::new(0),
            queries: AtomicUsize::new(0),
            native: None,
            finalized: AtomicUsize::new(0),
            luid: None,
            launches: AtomicUsize::new(0),
        });
        let books: Arc<dyn Books> = owner.clone();
        runtime.bind_books(Arc::downgrade(&books)).unwrap();
        let prepared = runtime.prepare_gpu_backing().unwrap().unwrap();
        assert_eq!(prepared.phase, BackingPhase::Prepared);
        assert_eq!(
            prepared.devices,
            vec![hex_api::DeviceId {
                kind: hex_api::DeviceKind::Gpu,
                key: 256
            }]
        );
        let capacity = runtime.capacity(hex_api::Class::Shards).unwrap().unwrap();
        assert!(
            capacity.devices.is_empty(),
            "declared model/777 bytes are not observed facts"
        );
        assert!(
            capacity
                .routes
                .iter()
                .any(|route| route.key.gpu == hex_api::GpuEligibility::Required
                    && route.backing == Some(prepared.key))
        );
        assert_eq!(capacity.backings.len(), 1);
        assert_eq!(capacity.backings.first().unwrap().key, prepared.key);
        let bad = BackingGrant {
            key: prepared.key,
            allocation,
            devices: Vec::new(),
        };
        assert!(runtime.initialize_gpu_pool(bad).is_err());
        assert_eq!(owner.effects.load(Ordering::SeqCst), 0);
        assert_eq!(owner.queries.load(Ordering::SeqCst), 0);
        assert_eq!(native.0.load(Ordering::SeqCst), 0);
        assert!(
            owner
                .row
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .allocation
                .is_none()
        );
        assert!(
            !runtime
                .gpu_backing_definition()
                .unwrap()
                .unwrap()
                .path
                .exists()
        );
    }
}
